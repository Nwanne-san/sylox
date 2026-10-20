#![no_std]

//! Staking: keeper bonds, reporter stakes and probes, signal dispute
//! bonds, event proposal and challenge bonds; executes slashing on
//! instruction from RiskOracle and EventRegistry. technical-doc.md
//! Section 7, 12.3, ADR-004.
//!
//! Scope (feat/treasury): `EventRegistry`, `MarketFactory`, `Series`
//! and `Governor` are not implemented here. Where this contract needs
//! them, it uses the address passed at `initialize` (checked via
//! `require_auth()` on calls FROM them) and mocks in tests stand in
//! for calls made ON them (`EventRegistry`'s bond calls).
//!
//! Lead decision (feat/treasury, task brief): one home for each kind
//! of money. This contract now holds ONLY participant funds: keeper
//! bonds, reporter stakes, locked dispute bonds, and `Claimable`
//! balances owed to a participant (refunds, winnings). Every protocol
//! fund (fees, slashed funds, keeper and reporter rewards) lives in
//! `Treasury` instead (Section 12.7), called through `clients.rs`.
//! feat/staking's own local reward pool (`RewardPool`/
//! `AccruedReward`/`fund_rewards`/`claim_rewards`) was a stand-in for
//! until `Treasury` existed; it is removed in this PR, now that it
//! does.
//!
//! Accounting invariants (task brief), tested in `test::property`
//! and noted again at each function below that could threaten them:
//!
//! S1. USDC balance of this contract EQUALS total keeper bonds +
//!     total reporter stake (including stake in cooldown) + total
//!     locked bonds + unclaimed `claimable` balances, apart from
//!     direct donations (tightened from `>=` to exact equality in
//!     feat/treasury, now that every protocol-fund credit has moved
//!     out to `Treasury`: nothing left in this contract's own
//!     tracked liabilities is anything OTHER than a participant's
//!     own funds, so there is no remaining reason for the real
//!     balance to run ahead of what is tracked). Every function that
//!     moves USDC (`stake`, `unstake`, `withdraw_keeper_bond`,
//!     `lock_bond`, `release_bond`/`forfeit_bond`'s `claimable`
//!     credit, `slash`, `claim`) keeps this contract's own real
//!     balance and its tracked liabilities moving together.
//! S2. A bond (`storage::BondRecord`, keyed by `BondKey`) is either
//!     Locked (present in storage), Released or Forfeited (absent,
//!     its value moved to `claimable`); `lock_bond` rejects a key
//!     that already exists (`BondExists`), and `release_bond`/
//!     `forfeit_bond` both clear the record before crediting
//!     anything, so the same key can satisfy at most one of them.
//! S3. No function here moves a participant's stake or bond to any
//!     address other than that participant itself (`unstake`,
//!     `withdraw_keeper_bond`, `claim`), or a named dispute winner
//!     (`forfeit_bond`'s `winner`, `slash`'s `winner`). The treasury
//!     half of a `forfeit_bond`/`slash` split now leaves via
//!     `Treasury.deposit` (feat/treasury), never credited as
//!     `Claimable` inside this contract; see
//!     `deposit_treasury_share`. No function here takes an arbitrary
//!     destination address as a parameter.
//! S4. `aggregate` never writes: it and the pure `aggregation`
//!     module it calls take only already-read data and return a
//!     computed status, with no call to any storage write function
//!     anywhere in that path.

mod aggregation;
mod clients;
mod error;
mod events;
mod params;
mod storage;

use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env, Symbol, Vec};
use sylox_types::{BondKey, EndpointStatus, KeeperInfo, ProbeReport, ReporterInfo, TreasuryBucket};

use clients::TreasuryClient;
use error::Error;
use storage::{BondRecord, StoredProbe};

#[contract]
pub struct Staking;

#[derive(Clone)]
#[soroban_sdk::contracttype]
struct Config {
    governor: Address,
    oracle: Address,
    registry: Address,
    treasury: Address,
    usdc: Address,
}

const CONFIG_KEY: Symbol = soroban_sdk::symbol_short!("CONFIG");

/// `set_governor`'s own one-time lock, same shape as EventRegistry's
/// `FACTORY_SET_KEY`: absent until the first successful `set_governor`
/// call, then permanently present, so a second call always fails.
const GOVERNOR_SET_KEY: Symbol = soroban_sdk::symbol_short!("GOV_SET");

/// Issue #12. Bump by hand on every `upgrade()` this contract ships.
const CONTRACT_VERSION: u32 = 2;
/// Issue #12. Instance key for `sylox_types::upgrade::do_migrate`'s own
/// "which version has this contract's storage already been migrated to"
/// marker.
const MIGRATED_TO_KEY: Symbol = soroban_sdk::symbol_short!("MIGRATE");

/// Section 7.8's 50/50 split, rounding the remainder (if `amount` is
/// odd) to the treasury side rather than the winner's, so the two
/// halves always sum back to exactly `amount` with no dust.
fn split_half(amount: i128) -> (i128, i128) {
    let to_winner = amount / 2;
    let to_treasury = amount - to_winner;
    (to_winner, to_treasury)
}

#[contractimpl]
impl Staking {
    /// Issue #21: a constructor, not a separate `initialize` call -
    /// see `RiskOracle::__constructor`'s own doc comment for why. Only
    /// stores addresses and config; never calls another contract (the
    /// other four may not exist yet when this runs, since every
    /// address here is computed in advance from a deterministic salt,
    /// deploy/deploy.sh's own "Constructor flow" section).
    pub fn __constructor(
        env: Env,
        governor: Address,
        oracle: Address,
        registry: Address,
        treasury: Address,
        usdc: Address,
    ) {
        sylox_types::ttl::extend_instance(&env);
        env.storage().instance().set(
            &CONFIG_KEY,
            &Config {
                governor,
                oracle,
                registry,
                treasury,
                usdc,
            },
        );
    }

    /// Issue #12. The currently deployed code's own version, bumped by
    /// hand alongside each `upgrade()` this contract ships (see deploy/
    /// README.md's upgrade procedure). Lets anyone confirm what is
    /// actually deployed without guessing from a wasm hash.
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// technical-doc.md Section 17.3: "Core contracts expose
    /// upgrade(wasm_hash), callable only by the governor." `Staking`
    /// holds funds, so Section 17.2 assigns `Upgrade` a 14 day timelock
    /// through `Governor` - NOT carried here, since `Governor` is not
    /// built yet (known gap, see deploy/README.md). This is the same
    /// direct, immediate governor-gated call every other call here
    /// already uses.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        sylox_types::upgrade::do_upgrade(&env, &config.governor, CONTRACT_VERSION, new_wasm_hash);
        Ok(())
    }

    /// Issue #12. Storage migration hook for a future `upgrade()`: runs
    /// at most once per `CONTRACT_VERSION`. Takes no argument - always
    /// migrates to this code's OWN current version, never a
    /// caller-chosen one (a governor could otherwise mark a future
    /// version "already migrated" before the code ever reaches it,
    /// permanently blocking that migration). Empty for now - no storage
    /// shape change ships with this PR - but every future upgrade that
    /// DOES change a stored type's shape migrates existing data here,
    /// gated the same way, with its own storage test (existing keys
    /// decode under the new types) alongside it.
    pub fn migrate(env: Env) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        if !sylox_types::upgrade::do_migrate(
            &env,
            &config.governor,
            &MIGRATED_TO_KEY,
            CONTRACT_VERSION,
        ) {
            return Err(Error::AlreadyMigrated);
        }
        Ok(())
    }

    /// One-time governor handover, for the Governor deployment cutover:
    /// callable only by the CURRENT governor, exactly once. Same
    /// one-time-lock pattern as `EventRegistry::set_factory` (a storage
    /// flag checked before the write, set after it), so a second call
    /// always fails rather than letting the governor be reassigned
    /// again later through this same path.
    pub fn set_governor(env: Env, new_admin: Address) -> Result<(), Error> {
        let mut config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if env.storage().instance().has(&GOVERNOR_SET_KEY) {
            return Err(Error::GovernorAlreadySet);
        }
        config.governor = new_admin;
        env.storage().instance().set(&CONFIG_KEY, &config);
        env.storage().instance().set(&GOVERNOR_SET_KEY, &true);
        Ok(())
    }

    // -- membership (Section 7.6, 7.7, 12.3) --

    pub fn add_keeper(env: Env, keeper: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if storage::get_keeper(&env, &keeper).is_some() {
            return Err(Error::AlreadyRegistered);
        }
        if storage::get_reporter(&env, &keeper).is_some() {
            // Review fix S6: one role per address, so slash (and
            // every other keeper-or-reporter branch) always has
            // exactly one target.
            return Err(Error::RoleConflict);
        }
        storage::set_keeper(
            &env,
            &keeper,
            &KeeperInfo {
                bond: 0,
                fault_times: Vec::new(&env),
                suspended: false,
                unstake_requested_at: None,
                removed_at: None,
                open_dispute_count: 0,
            },
        );
        Ok(())
    }

    /// Deactivates the keeper immediately (Section 7.7's `Staked ->
    /// Suspended`-style exit; `is_active_keeper` reads `false` from
    /// this point on, so `RiskOracle.post_signals` rejects it). Does
    /// NOT release the bond: `withdraw_keeper_bond` does that, once
    /// `KEEPER_EXIT_DELAY_SECS` has passed AND every signal dispute
    /// naming this keeper has resolved (lead decision, feat/staking:
    /// removal is not an escape hatch from an open dispute).
    pub fn remove_keeper(env: Env, keeper: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let mut info = storage::get_keeper(&env, &keeper).ok_or(Error::NotKeeper)?;
        let now = env.ledger().timestamp();
        info.removed_at = Some(now);
        storage::set_keeper(&env, &keeper, &info);
        events::KeeperRemoved {
            keeper: keeper.clone(),
            removed_at: now,
            withdrawable_at: now + params::KEEPER_EXIT_DELAY_SECS,
        }
        .publish(&env);
        Ok(())
    }

    pub fn add_reporter(env: Env, reporter: Address, region: Symbol) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if storage::get_reporter(&env, &reporter).is_some() {
            return Err(Error::AlreadyRegistered);
        }
        if storage::get_keeper(&env, &reporter).is_some() {
            // Review fix S6: see add_keeper's own check above.
            return Err(Error::RoleConflict);
        }
        storage::set_reporter(
            &env,
            &reporter,
            &ReporterInfo {
                stake: 0,
                region,
                fault_times: Vec::new(&env),
                suspended: false,
                unstake_requested_at: None,
                removed_at: None,
            },
        );
        storage::add_to_all_reporters(&env, &reporter);
        Ok(())
    }

    /// Deactivates the reporter immediately (no new `submit_probe`).
    /// Does NOT release the stake: it stays locked, and slashable,
    /// until `REPORTER_EXIT_DELAY_SECS` after removal (lead decision,
    /// feat/staking), via the same `unstake` path a voluntary exit
    /// uses, since `unstake` already checks `removed_at` when present
    /// (see `unstake` below).
    pub fn remove_reporter(env: Env, reporter: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let mut info = storage::get_reporter(&env, &reporter).ok_or(Error::NotReporter)?;
        let now = env.ledger().timestamp();
        info.removed_at = Some(now);
        storage::set_reporter(&env, &reporter, &info);
        events::ReporterRemoved {
            reporter: reporter.clone(),
            removed_at: now,
            withdrawable_at: now + params::REPORTER_EXIT_DELAY_SECS,
        }
        .publish(&env);
        Ok(())
    }

    // -- keeper bonds and reporter stakes (Section 12.3) --

    pub fn stake(env: Env, who: Address, amount: i128) -> Result<(), Error> {
        who.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if amount <= 0 {
            return Err(Error::StakeTooLow);
        }
        let config = Self::require_config(&env)?;
        let usdc = token::TokenClient::new(&env, &config.usdc);
        let here = env.current_contract_address();

        if let Some(mut keeper) = storage::get_keeper(&env, &who) {
            if keeper.removed_at.is_some() {
                return Err(Error::NotKeeper);
            }
            usdc.transfer(&who, soroban_sdk::MuxedAddress::from(here.clone()), &amount);
            keeper.bond = keeper.bond.checked_add(amount).ok_or(Error::MathOverflow)?;
            storage::set_keeper(&env, &who, &keeper);
            events::Staked {
                who: who.clone(),
                amount,
                total_after: keeper.bond,
            }
            .publish(&env);
            return Ok(());
        }
        if let Some(mut reporter) = storage::get_reporter(&env, &who) {
            if reporter.removed_at.is_some() {
                return Err(Error::NotReporter);
            }
            usdc.transfer(&who, soroban_sdk::MuxedAddress::from(here.clone()), &amount);
            reporter.stake = reporter
                .stake
                .checked_add(amount)
                .ok_or(Error::MathOverflow)?;
            storage::set_reporter(&env, &who, &reporter);
            events::Staked {
                who: who.clone(),
                amount,
                total_after: reporter.stake,
            }
            .publish(&env);
            return Ok(());
        }
        Err(Error::NotKeeper)
    }

    /// Starts the voluntary unstake cooldown for `amount` of `who`'s
    /// bond or stake. v1 has no partial unstake tracking beyond this:
    /// a second `unstake_request` while one is already pending is
    /// rejected (`UnstakePending`), matching Section 12.3's "starts
    /// cooldown" being a one-shot action per cycle.
    pub fn unstake_request(env: Env, who: Address, amount: i128) -> Result<(), Error> {
        who.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if amount <= 0 {
            return Err(Error::StakeTooLow);
        }
        Self::require_config(&env)?;
        let now = env.ledger().timestamp();

        if let Some(mut keeper) = storage::get_keeper(&env, &who) {
            if keeper.unstake_requested_at.is_some() {
                return Err(Error::UnstakePending);
            }
            if amount > keeper.bond {
                return Err(Error::StakeTooLow);
            }
            keeper.unstake_requested_at = Some(now);
            storage::set_keeper(&env, &who, &keeper);
            return Ok(());
        }
        if let Some(mut reporter) = storage::get_reporter(&env, &who) {
            if reporter.unstake_requested_at.is_some() {
                return Err(Error::UnstakePending);
            }
            if amount > reporter.stake {
                return Err(Error::StakeTooLow);
            }
            reporter.unstake_requested_at = Some(now);
            storage::set_reporter(&env, &who, &reporter);
            return Ok(());
        }
        Err(Error::NotKeeper)
    }

    /// Pays out `who`'s full bond or stake after its cooldown
    /// (voluntary, `UNSTAKE_COOLDOWN_SECS`, or removal triggered,
    /// `KEEPER_EXIT_DELAY_SECS`/`REPORTER_EXIT_DELAY_SECS`: whichever
    /// applies). A keeper's own open dispute count must also be zero
    /// (lead decision, feat/staking); `withdraw_keeper_bond` is the
    /// dedicated entry point for the removal path's own extra check,
    /// but a VOLUNTARY `unstake` (not preceded by `remove_keeper`)
    /// still goes through this function, so it also checks
    /// `open_dispute_count` here, not only there.
    pub fn unstake(env: Env, who: Address) -> Result<i128, Error> {
        who.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;
        let now = env.ledger().timestamp();
        let here = env.current_contract_address();
        let usdc = token::TokenClient::new(&env, &config.usdc);

        if let Some(mut keeper) = storage::get_keeper(&env, &who) {
            let requested_at = keeper
                .unstake_requested_at
                .ok_or(Error::NoUnstakeRequested)?;
            if now < requested_at + params::UNSTAKE_COOLDOWN_SECS {
                return Err(Error::UnstakeCooldown);
            }
            if keeper.open_dispute_count > 0 {
                // Review fix S7: a dedicated code, distinct from
                // Suspended (a fault/evidence outcome). This keeper
                // is not suspended; its funds are still needed as
                // collateral for an open dispute.
                return Err(Error::DisputesOpen);
            }
            let amount = keeper.bond;
            keeper.bond = 0;
            keeper.unstake_requested_at = None;
            storage::set_keeper(&env, &who, &keeper);
            if amount > 0 {
                usdc.transfer(&here, soroban_sdk::MuxedAddress::from(who.clone()), &amount);
            }
            events::Unstaked {
                who: who.clone(),
                amount,
                total_after: 0,
            }
            .publish(&env);
            return Ok(amount);
        }
        if let Some(mut reporter) = storage::get_reporter(&env, &who) {
            let requested_at = reporter
                .unstake_requested_at
                .ok_or(Error::NoUnstakeRequested)?;
            if now < requested_at + params::UNSTAKE_COOLDOWN_SECS {
                return Err(Error::UnstakeCooldown);
            }
            let amount = reporter.stake;
            reporter.stake = 0;
            reporter.unstake_requested_at = None;
            storage::set_reporter(&env, &who, &reporter);
            if amount > 0 {
                usdc.transfer(&here, soroban_sdk::MuxedAddress::from(who.clone()), &amount);
            }
            events::Unstaked {
                who: who.clone(),
                amount,
                total_after: 0,
            }
            .publish(&env);
            return Ok(amount);
        }
        Err(Error::NotKeeper)
    }

    /// The removal triggered exit path (lead decision, feat/staking):
    /// permissionless, pays a removed keeper's bond (less any
    /// slashes already applied to it) back to the keeper once BOTH
    /// `now >= removed_at + KEEPER_EXIT_DELAY_SECS` (every posting's
    /// dispute window has closed) and `open_dispute_count == 0`
    /// (nothing still open could slash it) hold. Distinct from
    /// `unstake`, which is the keeper's OWN voluntary exit while still
    /// registered; this is governor initiated removal's exit, with no
    /// `unstake_request` step since `remove_keeper` already started
    /// its own clock.
    pub fn withdraw_keeper_bond(env: Env, keeper: Address) -> Result<i128, Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let mut info = storage::get_keeper(&env, &keeper).ok_or(Error::NotKeeper)?;
        let removed_at = info.removed_at.ok_or(Error::NotKeeper)?;
        let now = env.ledger().timestamp();
        if now < removed_at + params::KEEPER_EXIT_DELAY_SECS {
            return Err(Error::UnstakeCooldown);
        }
        if info.open_dispute_count > 0 {
            // Review fix S7: see unstake's own comment above.
            return Err(Error::DisputesOpen);
        }
        let amount = info.bond;
        info.bond = 0;
        storage::set_keeper(&env, &keeper, &info);
        if amount > 0 {
            let usdc = token::TokenClient::new(&env, &config.usdc);
            usdc.transfer(
                &env.current_contract_address(),
                soroban_sdk::MuxedAddress::from(keeper.clone()),
                &amount,
            );
        }
        events::KeeperBondWithdrawn {
            keeper: keeper.clone(),
            amount,
        }
        .publish(&env);
        Ok(amount)
    }

    // -- probes (Section 7.3) --

    /// technical-doc.md Section 7.3: one report per reporter per asset
    /// per epoch. The report's own `region` field is ignored (Section
    /// 7.3's field notes this build makes concrete): the reporter's
    /// REGISTERED region is snapshotted into storage at submission
    /// time instead (lead decision, feat/staking), so `aggregate`/
    /// `settle_probes` never need a live registration lookup.
    ///
    /// Accepted only for the current epoch or the just closed epoch,
    /// within `PROBE_GRACE_SECS` of its close (new parameter,
    /// feat/staking; Section 7.3 called for a grace period without
    /// naming one). Every probe write extends its own TTL, and the
    /// (asset, epoch) submitter index's TTL, to `PROBE_TTL_LEDGERS`,
    /// which the `params_test` module proves covers the full
    /// settlement window with margin.
    pub fn submit_probe(env: Env, reporter: Address, report: ProbeReport) -> Result<(), Error> {
        reporter.require_auth();
        sylox_types::ttl::extend_instance(&env);
        Self::require_config(&env)?;
        let info = storage::get_reporter(&env, &reporter).ok_or(Error::NotReporter)?;
        if info.suspended || info.removed_at.is_some() {
            return Err(Error::Suspended);
        }
        if info.stake < params::REPORTER_STAKE {
            // Section 7.7: "below reporter_stake is not active." Not
            // `Suspended` (that is a fault/evidence outcome, a
            // distinct reason); `StakeTooLow` is the code
            // `stake`/`unstake_request` already use for the same
            // underlying condition.
            return Err(Error::StakeTooLow);
        }

        let now = env.ledger().timestamp();
        let current_epoch = now / params::EPOCH_SECS;
        let epoch_close = (report.epoch + 1) * params::EPOCH_SECS;
        let is_current = report.epoch == current_epoch;
        let is_just_closed_within_grace =
            report.epoch + 1 == current_epoch || now <= epoch_close + params::PROBE_GRACE_SECS;
        if !is_current && !is_just_closed_within_grace {
            return Err(Error::ProbeWindowClosed);
        }

        if storage::get_probe(&env, &report.asset, report.epoch, &reporter).is_some() {
            return Err(Error::DuplicateProbe);
        }

        let stored = StoredProbe {
            report: report.clone(),
            region_at_submission: info.region.clone(),
        };
        storage::set_probe(
            &env,
            &report.asset,
            report.epoch,
            &reporter,
            &stored,
            params::PROBE_TTL_LEDGERS,
        );
        let added = storage::add_submitter(
            &env,
            &report.asset,
            report.epoch,
            &reporter,
            params::MAX_SUBMITTERS_PER_EPOCH,
            params::PROBE_TTL_LEDGERS,
        );
        if !added {
            // MAX_SUBMITTERS_PER_EPOCH reached (DuplicateProbe already
            // ruled out above): refuse rather than silently excluding
            // this reporter from the aggregate it just paid gas to
            // submit into.
            return Err(Error::ProbeWindowClosed);
        }

        events::ProbeSubmitted {
            asset: report.asset.clone(),
            reporter,
            epoch: report.epoch,
            status: report.status,
            region: info.region,
        }
        .publish(&env);
        Ok(())
    }

    /// technical-doc.md Section 7.4. A READ with no side effects (S4):
    /// never calls any storage write, directly or through
    /// `aggregation::compute`, which is itself pure. Uses the
    /// submitter index, never the live reporter set (lead decision,
    /// feat/staking), so a removed reporter's already submitted probe
    /// still counts here exactly as it would have before removal.
    pub fn aggregate(env: Env, asset: Address, epoch: u64) -> EndpointStatus {
        let (_, reports) = Self::collect_reports(&env, &asset, epoch);
        aggregation::compute(&env, &reports)
    }

    /// Reads every still present probe for (asset, epoch) via the
    /// submitter index (lead decision, feat/staking: never the live
    /// reporter set). Returns the reporter addresses and their
    /// `StoredProbe`s as two parallel vectors (same index in each),
    /// rather than a single `Vec` of pairs, since `aggregation`'s pure
    /// functions only ever need the probes, not who submitted them.
    fn collect_reports(env: &Env, asset: &Address, epoch: u64) -> (Vec<Address>, Vec<StoredProbe>) {
        let submitters = storage::get_submitters(env, asset, epoch);
        let mut addresses = Vec::new(env);
        let mut reports = Vec::new(env);
        for reporter in submitters.iter() {
            if let Some(probe) = storage::get_probe(env, asset, epoch, &reporter) {
                addresses.push_back(reporter);
                reports.push_back(probe);
            }
        }
        (addresses, reports)
    }

    /// technical-doc.md Section 7.4, 7.5, 12.3 (named `settle_probes`
    /// there and in the `RiskOracle` client this exact signature must
    /// match; the task's own notes call the same function
    /// `settle_epoch`, but `RiskOracle.finalize_endpoint` already
    /// calls `Staking.settle_probes` by that name, so this build keeps
    /// the name `RiskOracle` depends on).
    ///
    /// Lead decision (settlement window): permissionless, callable
    /// exactly once per (asset, epoch), and ONLY while every probe for
    /// that epoch is guaranteed to still exist: not before
    /// `PROBE_GRACE_SECS` has elapsed since the epoch closed
    /// (`SettlementNotOpen`), and not after `SETTLE_WINDOW_SECS` more
    /// has elapsed on top of that (`SettlementWindowExpired`). An
    /// epoch nobody settles within its window simply never settles:
    /// no faults, no rewards, nothing reserved for it, the reward pool
    /// untouched; this is the intended behavior (see the PR report),
    /// not an error state, because settling on a set of probes that
    /// cannot be guaranteed complete would let an attacker who can
    /// extend their OWN probe's TTL (any address can call
    /// `extend_ttl`-backed functions on data they do not own, the
    /// write just failed to re-extend another entry, not inspect
    /// one) manufacture a majority by letting everyone else's probe
    /// expire first.
    pub fn settle_probes(env: Env, asset: Address, epoch: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        if storage::get_probes_settled(&env, &asset, epoch) {
            return Err(Error::AlreadySettled);
        }

        let now = env.ledger().timestamp();
        let epoch_close = (epoch + 1) * params::EPOCH_SECS;
        let window_opens = epoch_close + params::PROBE_GRACE_SECS;
        let window_closes = window_opens + params::SETTLE_WINDOW_SECS;
        if now < window_opens {
            return Err(Error::SettlementNotOpen);
        }
        if now > window_closes {
            return Err(Error::SettlementWindowExpired);
        }

        let (addresses, reports) = Self::collect_reports(&env, &asset, epoch);
        let aggregate = aggregation::compute(&env, &reports);
        let majority_size = aggregation::majority_size(&reports, aggregate);

        let mut rewarded: u32 = 0;
        let mut faulted: u32 = 0;
        let per_reporter_reward = if majority_size > 0 {
            params::REPORTER_REWARD_PER_EPOCH / majority_size as i128
        } else {
            0
        };

        for i in 0..reports.len() {
            let stored = reports.get(i).unwrap();
            let reporter = addresses.get(i).unwrap();
            let matched = stored.report.status == aggregate;
            if matched {
                if majority_size > 0 && per_reporter_reward > 0 {
                    // feat/treasury: accrues from Treasury's
                    // ReporterRewards bucket, not a local pool.
                    // Treasury.accrue_reward itself caps the accrual
                    // at what the bucket holds and emits
                    // RewardShortfall if it could not cover the full
                    // request (Section 12.7, T2); Staking does not
                    // duplicate that capping or that event.
                    TreasuryClient::new(&env, &config.treasury).accrue_reward(
                        &reporter,
                        &TreasuryBucket::ReporterRewards,
                        &per_reporter_reward,
                    );
                }
                rewarded += 1;
            } else if majority_size >= params::FAULT_MAJORITY_THRESHOLD {
                Self::record_fault(&env, &config, &reporter, now);
                faulted += 1;
            }
        }

        storage::set_probes_settled(&env, &asset, epoch, params::PROBE_TTL_LEDGERS);
        events::ProbesSettled {
            asset,
            epoch,
            aggregate,
            rewarded,
            faulted,
        }
        .publish(&env);
        Ok(())
    }

    /// Records one fault for `reporter` at `now`, pruning fault times
    /// outside `FAULT_WINDOW_SECS` first, then slashes and suspends if
    /// the pruned, incremented count exceeds `REPORTER_MAX_FAULTS`
    /// (Section 7.5).
    fn record_fault(env: &Env, config: &Config, reporter: &Address, now: u64) {
        let Some(mut info) = storage::get_reporter(env, reporter) else {
            return;
        };
        let mut fault_times = Vec::new(env);
        for t in info.fault_times.iter() {
            if now.saturating_sub(t) <= params::FAULT_WINDOW_SECS {
                fault_times.push_back(t);
            }
        }
        fault_times.push_back(now);
        info.fault_times = fault_times;

        if info.fault_times.len() > params::REPORTER_MAX_FAULTS && !info.suspended {
            // `slash_amount` is a percentage (REPORTER_SLASH_BPS <=
            // 10_000) of `info.stake` itself, so it can never exceed
            // `info.stake`: no `min`/overpay risk here the way
            // `slash`'s caller-supplied `amount` has (review fix S5).
            // `checked_sub` still replaces the previous
            // `saturating_sub`, per the review's "remove
            // saturating_sub from every money path" instruction, even
            // though it cannot actually underflow given the above.
            let slash_amount = info.stake * params::REPORTER_SLASH_BPS / 10_000;
            info.stake = info
                .stake
                .checked_sub(slash_amount)
                .expect("slash_amount is bounded by info.stake; cannot underflow");
            info.suspended = true;
            storage::set_reporter(env, reporter, &info);
            // No bonded counterparty for a fault slash (Section 7.5
            // gives the split as 50/50 winner/Treasury, but a fault is
            // not a dispute with a winner): the whole amount goes to
            // the Treasury contract's Slashed bucket, mirroring the
            // "no bonded counterparty... 100% goes to the Treasury"
            // rule Section 7.8 states for the analogous case on bond
            // forfeits. feat/treasury: a real Treasury.deposit call,
            // not a local Claimable credit (one home for each kind of
            // money: this is a protocol fund, not owed to any
            // participant).
            Self::deposit_treasury_share(env, config, slash_amount);
            events::Slashed {
                who: reporter.clone(),
                amount: slash_amount,
                requested_amount: slash_amount,
                winner: None,
                to_treasury: slash_amount,
                reason: BytesN::from_array(env, &[0u8; 32]),
                suspended: true,
            }
            .publish(env);
        } else {
            storage::set_reporter(env, reporter, &info);
        }
    }

    // -- bond escrow and slashing (Section 7.8, 12.3) --

    /// technical-doc.md Section 12.3: auth `RiskOracle` for
    /// `SignalDispute` keys, `EventRegistry` for `EventProposal`/
    /// `EventChallenge` keys. `subject` (lead decision, feat/staking):
    /// `Some(keeper)` required for `SignalDispute` (the keeper whose
    /// posting is being disputed, so a later `remove_keeper` can
    /// withhold bond release while this stays open), `None` required
    /// for the event kinds. A mismatch is `InvalidBondSubject`, not
    /// silently ignored.
    pub fn lock_bond(
        env: Env,
        key: BondKey,
        owner: Address,
        amount: i128,
        subject: Option<Address>,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_bond_caller(&env, &config, &key)?;
        Self::require_valid_subject(&key, &subject)?;
        if amount <= 0 {
            // PR #7 approval fix: without this, a zero (or negative)
            // bond would still increment the subject keeper's
            // open_dispute_count below, with no real collateral ever
            // locked to justify it.
            return Err(Error::InvalidAmount);
        }
        if storage::get_bond(&env, &key).is_some() {
            return Err(Error::BondExists);
        }

        let usdc = token::TokenClient::new(&env, &config.usdc);
        usdc.transfer(
            &owner,
            soroban_sdk::MuxedAddress::from(env.current_contract_address()),
            &amount,
        );
        storage::set_bond(
            &env,
            &key,
            &BondRecord {
                owner: owner.clone(),
                amount,
                subject: subject.clone(),
            },
        );
        if let Some(keeper) = &subject {
            Self::increment_open_disputes(&env, keeper);
        }

        events::BondLocked { owner, key, amount }.publish(&env);
        Ok(())
    }

    /// Credits the full bond back to its owner's claimable balance.
    /// Auth: whichever contract locked the bond (read from the
    /// record's `owner`'s... no: Section 12.3 says "the contract that
    /// locked it", which for every `BondKey` kind this contract
    /// handles is always `oracle` for `SignalDispute` and `registry`
    /// for the event kinds, the same mapping `lock_bond` itself
    /// checks, so `release_bond` and `forfeit_bond` reuse
    /// `require_bond_caller` rather than trusting the stored record to
    /// say who may act on it.
    pub fn release_bond(env: Env, key: BondKey) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_bond_caller(&env, &config, &key)?;
        let record = storage::get_bond(&env, &key).ok_or(Error::UnknownBond)?;
        storage::clear_bond(&env, &key);
        storage::add_claimable(&env, &record.owner, record.amount);
        if let Some(keeper) = &record.subject {
            Self::decrement_open_disputes(&env, keeper);
        }
        events::BondReleased {
            owner: record.owner,
            key,
            amount: record.amount,
        }
        .publish(&env);
        Ok(())
    }

    /// 50% to `winner` (if any), 50% to `Treasury`'s `Slashed` bucket
    /// (Section 7.8). With no bonded counterparty (`winner = None`),
    /// 100% goes to `Treasury` instead, per the same section.
    /// feat/treasury: the treasury half is a real `Treasury.deposit`
    /// call (`deposit_treasury_share`), never a local `Claimable`
    /// credit; only the winner's own half, if any, stays `Claimable`
    /// here (S3: a participant's own funds).
    pub fn forfeit_bond(env: Env, key: BondKey, winner: Option<Address>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_bond_caller(&env, &config, &key)?;
        let record = storage::get_bond(&env, &key).ok_or(Error::UnknownBond)?;
        storage::clear_bond(&env, &key);

        let (to_winner, to_treasury) = split_half(record.amount);
        if let Some(winner_addr) = &winner {
            storage::add_claimable(&env, winner_addr, to_winner);
            Self::deposit_treasury_share(&env, &config, to_treasury);
        } else {
            Self::deposit_treasury_share(&env, &config, record.amount);
        }
        if let Some(keeper) = &record.subject {
            Self::decrement_open_disputes(&env, keeper);
        }

        let to_treasury_final = if winner.is_some() {
            to_treasury
        } else {
            record.amount
        };
        events::BondForfeited {
            owner: record.owner,
            key,
            amount: record.amount,
            winner,
            to_treasury: to_treasury_final,
        }
        .publish(&env);
        Ok(())
    }

    /// technical-doc.md Section 7.5, 12.3: auth `RiskOracle` or
    /// `EventRegistry`, or the committee (via `Governor`, not wired in
    /// this build; see the PR's "Spec deviations") for false probe
    /// evidence. This build checks `oracle` or `registry` auth only;
    /// a committee initiated slash for false evidence is not callable
    /// yet, since there is no committee address this contract knows
    /// about independent of `RiskOracle`'s own (and `RiskOracle`'s
    /// `resolve_signal_dispute` is the one call site this build's
    /// `slash` actually serves).
    pub fn slash(
        env: Env,
        who: Address,
        amount: i128,
        winner: Option<Address>,
        reason: BytesN<32>,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_slash_caller(&env, &config)?;
        if amount <= 0 {
            // A non-positive amount is never a real slash (review fix
            // S5, found while fixing the overpay bug): without this
            // check, `amount.min(remaining)` below would return a
            // negative `actual`, and `bond -= actual` would INCREASE
            // the bond rather than reduce it.
            return Err(Error::StakeTooLow);
        }
        let now = env.ledger().timestamp();

        let suspended;
        // S1 (review fix S5): never deduct or pay out more than `who`
        // actually holds. `actual` is the amount genuinely taken;
        // every downstream credit (`to_winner`/`to_treasury`) is
        // computed from `actual`, never from the caller's requested
        // `amount`, so a request exceeding the remaining balance is
        // capped down rather than overpaid from other participants'
        // funds.
        let actual;
        if let Some(mut keeper) = storage::get_keeper(&env, &who) {
            actual = amount.min(keeper.bond);
            keeper.bond -= actual;
            // Section 7.5, 7.8: `keeper_slash` on a lost signal
            // dispute counts as one fault; suspension follows
            // `KEEPER_MAX_FAULTS` in `FAULT_WINDOW_SECS`, the same
            // pruned rolling window `record_fault` uses for reporters.
            let mut fault_times = Vec::new(&env);
            for t in keeper.fault_times.iter() {
                if now.saturating_sub(t) <= params::FAULT_WINDOW_SECS {
                    fault_times.push_back(t);
                }
            }
            fault_times.push_back(now);
            keeper.fault_times = fault_times;
            if keeper.fault_times.len() > params::KEEPER_MAX_FAULTS {
                keeper.suspended = true;
            }
            suspended = keeper.suspended;
            storage::set_keeper(&env, &who, &keeper);
        } else if let Some(mut reporter) = storage::get_reporter(&env, &who) {
            actual = amount.min(reporter.stake);
            reporter.stake -= actual;
            suspended = reporter.suspended;
            storage::set_reporter(&env, &who, &reporter);
        } else {
            return Err(Error::NotKeeper);
        }

        let (to_winner, to_treasury) = split_half(actual);
        if let Some(winner_addr) = &winner {
            storage::add_claimable(&env, winner_addr, to_winner);
            Self::deposit_treasury_share(&env, &config, to_treasury);
        } else {
            Self::deposit_treasury_share(&env, &config, actual);
        }

        let to_treasury_final = if winner.is_some() {
            to_treasury
        } else {
            actual
        };
        events::Slashed {
            who,
            amount: actual,
            requested_amount: amount,
            winner,
            to_treasury: to_treasury_final,
            reason,
            suspended,
        }
        .publish(&env);
        Ok(())
    }

    /// technical-doc.md Section 12.3, issue #11 fix (feat/treasury).
    /// Auth: `RiskOracle`. Accrues `keeper_reward * epochs` from
    /// `Treasury`'s `KeeperRewards` bucket via `Treasury.accrue_reward`,
    /// only if `keeper` is currently active (added, bonded, not
    /// removed) and not suspended; otherwise a no-op returning `Ok(0)`,
    /// not an error, since `RiskOracle`'s own caller (the finality
    /// scan, grouping newly Final epochs by poster) must not have its
    /// whole call fail just because one keeper among several became
    /// ineligible between posting and this call. `Treasury.accrue_reward`
    /// itself caps the accrual at what the bucket holds and emits
    /// `RewardShortfall` if it falls short (Section 12.7, "never accrue
    /// unfunded rewards").
    pub fn reward_keeper(env: Env, keeper: Address, epochs: u32) -> Result<i128, Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_oracle_caller(&env, &config)?;
        let Some(info) = storage::get_keeper(&env, &keeper) else {
            return Err(Error::NotKeeper);
        };
        if info.suspended || info.removed_at.is_some() || info.bond < params::KEEPER_BOND {
            return Ok(0);
        }
        if epochs == 0 {
            return Ok(0);
        }
        let amount = params::KEEPER_REWARD_PER_ACCEPTED_EPOCH
            .checked_mul(epochs as i128)
            .ok_or(Error::MathOverflow)?;
        let accrued = TreasuryClient::new(&env, &config.treasury).accrue_reward(
            &keeper,
            &TreasuryBucket::KeeperRewards,
            &amount,
        );
        Ok(accrued)
    }

    /// technical-doc.md Section 5.9 S6 (v1.5): pays per accepted
    /// sub-epoch at `keeper_reward * sub_epoch_secs / 3,600`, so the
    /// total paid per hour of real coverage is unchanged regardless of
    /// `sub_epoch_secs`. Additive to `reward_keeper`, not a replacement:
    /// the hourly reward path (`reward_keeper`, called from
    /// `RiskOracle`'s existing finality scan) is untouched, satisfying
    /// I22 ("existing suite passes unchanged at `sub_epoch_secs =
    /// 3,600`") by construction, since nothing calls this new function
    /// unless an asset is actually using sub-epoch posting.
    pub fn reward_keeper_sub_epochs(
        env: Env,
        keeper: Address,
        sub_epoch_count: u32,
        sub_epoch_secs: u64,
    ) -> Result<i128, Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        Self::require_oracle_caller(&env, &config)?;
        let Some(info) = storage::get_keeper(&env, &keeper) else {
            return Err(Error::NotKeeper);
        };
        if info.suspended || info.removed_at.is_some() || info.bond < params::KEEPER_BOND {
            return Ok(0);
        }
        if sub_epoch_count == 0 {
            return Ok(0);
        }
        // keeper_reward * sub_epoch_secs * sub_epoch_count / 3,600:
        // every multiplication happens before the one division, so a
        // full hour's worth of sub-epochs (sub_epoch_count ==
        // 3,600 / sub_epoch_secs) always pays EXACTLY one hourly
        // reward, no matter how finely the interval divides the
        // hour. Dividing once per sub-epoch first (computing a
        // per-sub-epoch rate, then multiplying by the count) would
        // truncate on every division instead of once overall: at the
        // 300s default, 12 separate truncations lose up to 11 more
        // units than this one combined division does, measurably
        // underpaying keepers every single hour.
        let amount = params::KEEPER_REWARD_PER_ACCEPTED_EPOCH
            .checked_mul(sub_epoch_secs as i128)
            .ok_or(Error::MathOverflow)?
            .checked_mul(sub_epoch_count as i128)
            .ok_or(Error::MathOverflow)?
            / params::EPOCH_SECS as i128;
        let accrued = TreasuryClient::new(&env, &config.treasury).accrue_reward(
            &keeper,
            &TreasuryBucket::KeeperRewards,
            &amount,
        );
        Ok(accrued)
    }

    /// technical-doc.md Section 7.8, 12.3: pays refunds and winnings
    /// from bond settlement. Probe and keeper rewards are claimed
    /// from `Treasury.claim_reward` instead (feat/treasury): a
    /// different balance, a different funding source, and no longer
    /// this contract's concern now that `Treasury` exists.
    pub fn claim(env: Env, who: Address) -> Result<i128, Error> {
        who.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;
        let amount = storage::get_claimable(&env, &who);
        if amount <= 0 {
            return Err(Error::NothingToClaim);
        }
        storage::clear_claimable(&env, &who);
        let usdc = token::TokenClient::new(&env, &config.usdc);
        usdc.transfer(
            &env.current_contract_address(),
            soroban_sdk::MuxedAddress::from(who.clone()),
            &amount,
        );
        events::Claimed { who, amount }.publish(&env);
        Ok(amount)
    }

    // -- reads (Section 12.3) --

    pub fn keeper(env: Env, keeper: Address) -> Option<KeeperInfo> {
        storage::get_keeper(&env, &keeper)
    }

    pub fn is_active_keeper(env: Env, keeper: Address) -> bool {
        match storage::get_keeper(&env, &keeper) {
            Some(info) => {
                !info.suspended && info.removed_at.is_none() && info.bond >= params::KEEPER_BOND
            }
            None => false,
        }
    }

    pub fn reporter(env: Env, reporter: Address) -> Option<ReporterInfo> {
        storage::get_reporter(&env, &reporter)
    }

    pub fn bond(env: Env, key: BondKey) -> Option<(Address, i128)> {
        storage::get_bond(&env, &key).map(|record| (record.owner, record.amount))
    }

    pub fn claimable(env: Env, who: Address) -> i128 {
        storage::get_claimable(&env, &who)
    }

    /// Returns every still present probe for (asset, epoch), decoded
    /// back to the plain `ProbeReport` shape Section 12.3 specifies
    /// (the region snapshot, lead decision feat/staking, is internal
    /// bookkeeping, not part of this read's contract).
    pub fn probes(env: Env, asset: Address, epoch: u64) -> Vec<ProbeReport> {
        let (_, reports) = Self::collect_reports(&env, &asset, epoch);
        let mut out = Vec::new(&env);
        for stored in reports.iter() {
            out.push_back(stored.report.clone());
        }
        out
    }

    // -- internal helpers --

    fn require_bond_caller(_env: &Env, config: &Config, key: &BondKey) -> Result<(), Error> {
        match key {
            BondKey::SignalDispute(_, _) => {
                config.oracle.require_auth();
                Ok(())
            }
            BondKey::EventProposal(_) | BondKey::EventChallenge(_) => {
                config.registry.require_auth();
                Ok(())
            }
        }
    }

    fn require_valid_subject(key: &BondKey, subject: &Option<Address>) -> Result<(), Error> {
        match key {
            BondKey::SignalDispute(_, _) => {
                if subject.is_none() {
                    return Err(Error::InvalidBondSubject);
                }
            }
            BondKey::EventProposal(_) | BondKey::EventChallenge(_) => {
                if subject.is_some() {
                    return Err(Error::InvalidBondSubject);
                }
            }
        }
        Ok(())
    }

    /// technical-doc.md Section 12.3 names the caller set as
    /// `RiskOracle` OR `EventRegistry`. Soroban's `require_auth()`
    /// traps on the first address that did not actually authorize the
    /// call, with no non-panicking variant and no portable "who
    /// invoked this call" read; checking two candidate addresses in
    /// sequence is therefore not possible without the wrong one
    /// trapping first, not failing gracefully into the next check.
    /// This build requires `oracle`'s auth only: `EventRegistry` is
    /// not implemented in this PR, so there is no real call site to
    /// support the other branch against yet. Flagged in the PR's
    /// "Spec deviations" section as a real gap `EventRegistry`'s own
    /// build needs to close, most likely by giving `Staking` a way to
    /// distinguish its caller (for example, two separate functions,
    /// or a kind argument this call already effectively has via
    /// `BondKey`'s own variant, extended to `slash` too).
    fn require_slash_caller(env: &Env, config: &Config) -> Result<(), Error> {
        let _ = env;
        config.oracle.require_auth();
        Ok(())
    }

    fn require_oracle_caller(env: &Env, config: &Config) -> Result<(), Error> {
        let _ = env;
        config.oracle.require_auth();
        Ok(())
    }

    fn increment_open_disputes(env: &Env, keeper: &Address) {
        if let Some(mut info) = storage::get_keeper(env, keeper) {
            info.open_dispute_count += 1;
            storage::set_keeper(env, keeper, &info);
        }
    }

    fn decrement_open_disputes(env: &Env, keeper: &Address) {
        if let Some(mut info) = storage::get_keeper(env, keeper) {
            info.open_dispute_count = info
                .open_dispute_count
                .checked_sub(1)
                .expect("open_dispute_count underflow: a bond settled twice for the same subject");
            storage::set_keeper(env, keeper, &info);
        }
    }

    fn require_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&CONFIG_KEY)
            .ok_or(Error::NotInitialized)
    }

    /// feat/treasury: deposits `amount` of this contract's own USDC
    /// into `Treasury`'s `Slashed` bucket, the treasury half of a
    /// `slash`/`forfeit_bond` split (lead decision: one home for each
    /// kind of money, protocol funds live only in `Treasury`).
    ///
    /// markets design note, Section 9: the authorize-as-self logic
    /// this needs (Soroban auto-authorizes only a contract's own
    /// DIRECT calls, never `Treasury.deposit`'s own second-hop SAC
    /// `transfer`) moved into `sylox_types::treasury_deposit`, shared
    /// with `Series.buy_cover`'s own fee deposit (technical-doc.md
    /// Section 12.7), so this wrapper stays bucket-specific while the
    /// mechanism itself has one implementation, not two.
    fn deposit_treasury_share(env: &Env, config: &Config, amount: i128) {
        sylox_types::treasury_deposit::deposit_treasury_share(
            env,
            &config.usdc,
            &config.treasury,
            TreasuryBucket::Slashed,
            amount,
        );
    }
}

#[cfg(test)]
mod test;
