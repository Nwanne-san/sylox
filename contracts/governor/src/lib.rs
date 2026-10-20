#![no_std]

//! Governor: the multisig queue every governor-gated call in the
//! protocol goes through, the committee address, the guardian's pause
//! switches, and the parameter mirror. technical-doc.md Section 12.6,
//! 16, 17; built to docs/design/governor.md, whose decisions (D1-D26)
//! and numbered deviations from technical-doc.md this follows.
//!
//! How it reaches the targets (design note Section 1): every
//! governor-gated function in `RiskOracle`, `EventRegistry`, `Staking`,
//! `Treasury` and `MarketFactory` checks `config.governor.require_auth()`.
//! A contract address authorizes the calls it makes directly, so once a
//! target's `governor` is this contract, the only way to reach those
//! functions is through `execute`, after the action's approvals and
//! timelock have been checked here. The targets know nothing about
//! timelocks.
//!
//! Invariants (design note Section 12), each with a test:
//!
//! G1. Every governor-gated call made on another contract happens
//!     inside `execute`, for an action that reached Approved.
//! G2. An action executes at most once, never before `eta`, never
//!     after `expires_at`.
//! G3. At execution, approvals from distinct current signers
//!     `>= threshold`.
//! G4. Each action's `eta - queue time` equals its Section 4.3 timelock.
//! G5. The guardian can call `pause` and nothing else.
//! G6. A pause lapses by itself after `MAX_PAUSE_SECS`; only
//!     `Action::Pause` resets the lapse time, and the guardian cannot
//!     extend a pause that is still active.
//! G7. No pause scope blocks a claim, a withdrawal, a trigger, a
//!     dispute or a ruling (enforced in the targets, later PRs).
//! G8. The Governor holds no funds; it never calls a token contract.
//! G9. `threshold` is always within `1..=signers.len()` and `signers`
//!     has no duplicates.
//! G10. `Targets` never change except through an upgrade of the
//!      Governor itself.

mod clients;
mod error;
mod events;
mod storage;

use soroban_sdk::{contract, contractimpl, panic_with_error, Address, BytesN, Env, Symbol, Vec};
use sylox_types::{Action, ActionState, PauseScope, QueuedAction};

pub use error::Error;
pub use storage::{Config, Targets};

use clients::{
    EventRegistryClient, MarketFactoryClient, RiskOracleClient, StakingClient, TreasuryClient,
    UpgradeableClient,
};

#[contract]
pub struct Governor;

/// Bump by hand on every self-upgrade this contract ships (Section 6.3).
pub const CONTRACT_VERSION: u32 = 1;

const DAY: u64 = 86_400;
/// technical-doc.md Section 23: how long after `eta` a queued action
/// can still execute.
pub const GRACE_SECS: u64 = 14 * DAY;
/// technical-doc.md Section 23: how long a pause lasts before it lapses
/// by itself.
pub const MAX_PAUSE_SECS: u64 = 14 * DAY;

/// The only v1 `SetParam` key (D11).
pub const LIQUIDITY_COVER_RATIO: &str = "liquidity_cover_ratio";

/// Each target's own `AlreadyMigrated` code. `execute(Upgrade)` calls
/// `migrate()` right after `upgrade()` (D13: the new code runs it) and
/// accepts exactly this error, which means the new code carries no new
/// storage version to migrate to; any other error reverts the upgrade.
const ORACLE_ALREADY_MIGRATED: u32 = 116;
const REGISTRY_ALREADY_MIGRATED: u32 = 216;
const STAKING_ALREADY_MIGRATED: u32 = 322;
const FACTORY_ALREADY_MIGRATED: u32 = 406;
const TREASURY_ALREADY_MIGRATED: u32 = 704;

/// Section 4.3, every row. `Unpause` and `Pause` have no waiting time
/// but still need the full threshold.
pub fn timelock(action: &Action) -> u64 {
    match action {
        Action::OpenSeries(_)
        | Action::AddAsset(_)
        | Action::AddKeeper(_)
        | Action::AddReporter(_, _)
        | Action::DisableAsset(_)
        | Action::RemoveKeeper(_)
        | Action::RemoveReporter(_)
        | Action::SetRelatedSellers(_, _) => 2 * DAY,
        Action::UpdateAsset(_, _)
        | Action::SetParam(_, _)
        | Action::SetFormula(_, _, _)
        | Action::RegisterDefinition(_)
        | Action::TreasuryAllocate(_, _, _)
        | Action::SetSubEpochSecs(_, _)
        | Action::SetHardCap(_, _)
        | Action::SetPriceGuard(_, _) => 7 * DAY,
        Action::Upgrade(_, _)
        | Action::SetSeriesWasm(_)
        | Action::SetSigners(_, _)
        | Action::SetCommittee(_)
        | Action::TreasurySpend(_, _, _)
        | Action::SetGuardian(_)
        | Action::SetFactory(_) => 14 * DAY,
        Action::Unpause(_) | Action::Pause(_) => 0,
    }
}

#[contractimpl]
impl Governor {
    /// D1, D2, D3 (deviations 1 and 2): a constructor taking the guardian
    /// and the five targets, no `timelock_secs`. Validates and stores;
    /// calls no other contract, since the targets are deployed from
    /// precomputed addresses and may not exist yet.
    pub fn __constructor(
        env: Env,
        signers: Vec<Address>,
        threshold: u32,
        committee: Address,
        guardian: Address,
        targets: Targets,
    ) {
        if let Err(e) = validate_signers(&signers, threshold) {
            panic_with_error!(&env, e);
        }
        storage::set_config(
            &env,
            &Config {
                signers,
                threshold,
                committee,
                guardian,
                targets,
            },
        );
        // Fresh storage is already in this code's shape: the first
        // state-changing call has nothing to migrate.
        env.storage()
            .instance()
            .set(&storage::MIGRATION_VERSION_KEY, &CONTRACT_VERSION);
    }

    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    // -- the queue (Section 4) --

    /// Auth: `proposer`, which must be a current signer. Queuing is not
    /// an approval; the proposer approves like everyone else.
    pub fn queue(env: Env, proposer: Address, action: Action) -> Result<u64, Error> {
        let config = begin(&env)?;
        proposer.require_auth();
        if !config.signers.contains(&proposer) {
            return Err(Error::NotSigner);
        }
        check_at_queue(&env, &config, &action)?;

        let now = env.ledger().timestamp();
        let eta = now + timelock(&action);
        let queued = QueuedAction {
            id: storage::take_next_action_id(&env),
            action,
            proposer,
            approvals: Vec::new(&env),
            eta,
            expires_at: eta + GRACE_SECS,
            state: ActionState::Queued,
        };
        storage::set_action(&env, &queued);
        sylox_types::ttl::extend_instance(&env);
        events::ActionQueued {
            action_id: queued.id,
            action: queued.action.clone(),
            eta: queued.eta,
            expires_at: queued.expires_at,
            state: queued.state,
        }
        .publish(&env);
        Ok(queued.id)
    }

    /// Auth: `signer`, which must be a current signer. Allowed while the
    /// action is Queued or Approved, until `expires_at`, including after
    /// `eta` (D17); never executes the action itself. Approving an
    /// Approved action lets it recover after a `SetSigners` drops its
    /// current-signer approvals below the threshold (D15).
    pub fn approve(env: Env, signer: Address, action_id: u64) -> Result<(), Error> {
        let config = begin(&env)?;
        signer.require_auth();
        if !config.signers.contains(&signer) {
            return Err(Error::NotSigner);
        }
        let mut queued = load_live(&env, action_id)?;
        if queued.state != ActionState::Queued && queued.state != ActionState::Approved {
            return Err(Error::WrongState);
        }
        if queued.approvals.contains(&signer) {
            return Err(Error::AlreadyApproved);
        }
        queued.approvals.push_back(signer);
        if current_approvals(&config, &queued) >= config.threshold {
            queued.state = ActionState::Approved;
        }
        storage::set_action(&env, &queued);
        sylox_types::ttl::extend_instance(&env);
        events::ActionApproved {
            action_id,
            action: queued.action.clone(),
            eta: queued.eta,
            expires_at: queued.expires_at,
            state: queued.state,
        }
        .publish(&env);
        Ok(())
    }

    /// Anyone, for every action type (D25). The approvals are recounted
    /// against the CURRENT signer set and threshold (D15). The target
    /// call runs inside this invocation: if it fails, the whole call
    /// reverts and the action stays Approved.
    pub fn execute(env: Env, action_id: u64) -> Result<(), Error> {
        let config = begin(&env)?;
        let mut queued = load_live(&env, action_id)?;
        match queued.state {
            ActionState::Approved => {}
            ActionState::Queued => return Err(Error::ThresholdNotMet),
            _ => return Err(Error::WrongState),
        }
        if env.ledger().timestamp() < queued.eta {
            return Err(Error::TimelockActive);
        }
        if current_approvals(&config, &queued) < config.threshold {
            return Err(Error::ThresholdNotMet);
        }

        queued.state = ActionState::Executed;
        storage::set_action(&env, &queued);
        sylox_types::ttl::extend_instance(&env);
        dispatch(&env, &config, &queued.action);

        events::ActionExecuted {
            action_id,
            action: queued.action.clone(),
            eta: queued.eta,
            expires_at: queued.expires_at,
            state: queued.state,
        }
        .publish(&env);
        Ok(())
    }

    /// D6 (deviation 5). Every address in `signers` authorizes. Either
    /// the distinct current signers listed reach the threshold (Queued
    /// or Approved), or `signers` is exactly the action's own proposer,
    /// still a current signer, and the action is still Queued.
    pub fn cancel(env: Env, signers: Vec<Address>, action_id: u64) -> Result<(), Error> {
        let config = begin(&env)?;
        let mut queued = load_live(&env, action_id)?;
        if queued.state != ActionState::Queued && queued.state != ActionState::Approved {
            return Err(Error::WrongState);
        }
        // Deduplicate first: one address authorizing twice in the same
        // call is rejected by the host, and counts once anyway.
        let mut distinct: Vec<Address> = Vec::new(&env);
        for signer in signers.iter() {
            if !distinct.contains(&signer) {
                distinct.push_back(signer);
            }
        }
        for signer in distinct.iter() {
            signer.require_auth();
        }

        let by_proposer = distinct.len() == 1 && distinct.get_unchecked(0) == queued.proposer;
        if by_proposer && queued.state == ActionState::Queued {
            // The proposer withdrawing its own action alone: only while
            // it is still a current signer (D6).
            if !config.signers.contains(&queued.proposer) {
                return Err(Error::NotSigner);
            }
        } else {
            // Otherwise the threshold route: every listed address must be
            // a current signer, and they must reach the threshold. A
            // proposer acting alone on an Approved action is WrongState.
            for signer in distinct.iter() {
                if !config.signers.contains(&signer) {
                    return Err(if by_proposer {
                        Error::WrongState
                    } else {
                        Error::NotSigner
                    });
                }
            }
            if distinct.len() < config.threshold {
                return Err(if by_proposer {
                    Error::WrongState
                } else {
                    Error::ThresholdNotMet
                });
            }
        }

        queued.state = ActionState::Cancelled;
        storage::set_action(&env, &queued);
        sylox_types::ttl::extend_instance(&env);
        events::ActionCancelled {
            action_id,
            action: queued.action.clone(),
            eta: queued.eta,
            expires_at: queued.expires_at,
            state: queued.state,
        }
        .publish(&env);
        Ok(())
    }

    // -- pausing (Section 8) --

    /// Auth: the guardian, no timelock. Fails with `AlreadyPaused` while
    /// the scope is paused: the guardian cannot extend a pause (D8,
    /// deviation 7). Renewal is `Action::Pause`.
    pub fn pause(env: Env, scope: PauseScope) -> Result<(), Error> {
        let config = begin(&env)?;
        config.guardian.require_auth();
        if is_paused(&env, scope) {
            return Err(Error::AlreadyPaused);
        }
        storage::set_pause_lapse(&env, scope, env.ledger().timestamp() + MAX_PAUSE_SECS);
        sylox_types::ttl::extend_instance(&env);
        events::Paused {
            scope,
            by: config.guardian,
        }
        .publish(&env);
        Ok(())
    }

    // -- reads --

    /// Whether `scope` is paused right now. Extends the instance TTL:
    /// targets call this from their own state-changing paths (Section
    /// 3.2, D20).
    pub fn paused(env: Env, scope: PauseScope) -> bool {
        sylox_types::ttl::extend_instance(&env);
        is_paused(&env, scope)
    }

    /// The committee address, read fresh by `EventRegistry.rule` /
    /// `resolve_timeout` and `RiskOracle.resolve_*` on every call.
    /// Extends the instance TTL for the same reason as `paused`.
    pub fn committee(env: Env) -> Address {
        sylox_types::ttl::extend_instance(&env);
        require_config(&env).committee
    }

    /// The last value the Governor set for `key`, or `None` (D11). A
    /// record of what was applied; the target's own storage is
    /// authoritative.
    pub fn param(env: Env, key: Symbol) -> Option<i128> {
        storage::get_params(&env).get(key)
    }

    /// The stored action, reported as Expired once `now > expires_at`
    /// while still Queued or Approved (Section 17.1: no write needed).
    pub fn action(env: Env, id: u64) -> Option<QueuedAction> {
        let mut queued = storage::get_action(&env, id)?;
        if is_expired(&env, &queued) {
            queued.state = ActionState::Expired;
        }
        Some(queued)
    }
}

// -- helpers --

fn require_config(env: &Env) -> Config {
    match storage::get_config(env) {
        Some(config) => config,
        None => panic_with_error!(env, Error::NotInitialized),
    }
}

/// Start of every state-changing call: load `Config`, and run the
/// Governor's own pending storage migration if this code is newer than
/// the data (Section 6.3, D14). Exactly once per `CONTRACT_VERSION`.
fn begin(env: &Env) -> Result<Config, Error> {
    let config = storage::get_config(env).ok_or(Error::NotInitialized)?;
    if sylox_types::upgrade::apply_migration(env, &storage::MIGRATION_VERSION_KEY, CONTRACT_VERSION)
    {
        // No storage shape change ships with version 1; a future
        // version's migration body goes here, gated by the line above.
    }
    Ok(config)
}

fn validate_signers(signers: &Vec<Address>, threshold: u32) -> Result<(), Error> {
    if signers.is_empty() || threshold == 0 || threshold > signers.len() {
        return Err(Error::InvalidSigners);
    }
    for i in 0..signers.len() {
        for j in (i + 1)..signers.len() {
            if signers.get_unchecked(i) == signers.get_unchecked(j) {
                return Err(Error::InvalidSigners);
            }
        }
    }
    Ok(())
}

/// Section 4.4: only what can be decided without calling a target.
fn check_at_queue(env: &Env, config: &Config, action: &Action) -> Result<(), Error> {
    match action {
        Action::SetSigners(signers, threshold) => validate_signers(signers, *threshold),
        Action::SetParam(key, _) if *key != Symbol::new(env, LIQUIDITY_COVER_RATIO) => {
            Err(Error::InvalidAction)
        }
        Action::Upgrade(contract, _) => {
            if *contract == env.current_contract_address()
                || already_migrated_code(config, contract).is_some()
            {
                Ok(())
            } else {
                Err(Error::InvalidAction)
            }
        }
        _ => Ok(()),
    }
}

/// Loads an action that can still be acted on: exists, and has not
/// passed `expires_at` while Queued or Approved.
fn load_live(env: &Env, action_id: u64) -> Result<QueuedAction, Error> {
    let queued = storage::get_action(env, action_id).ok_or(Error::UnknownAction)?;
    if is_expired(env, &queued) {
        return Err(Error::ActionExpired);
    }
    storage::extend_action(env, action_id);
    Ok(queued)
}

fn is_expired(env: &Env, queued: &QueuedAction) -> bool {
    (queued.state == ActionState::Queued || queued.state == ActionState::Approved)
        && env.ledger().timestamp() > queued.expires_at
}

/// Approvals from addresses that are signers NOW (D15).
fn current_approvals(config: &Config, queued: &QueuedAction) -> u32 {
    queued
        .approvals
        .iter()
        .filter(|a| config.signers.contains(a))
        .count() as u32
}

fn is_paused(env: &Env, scope: PauseScope) -> bool {
    match storage::get_pause_lapse(env, scope) {
        Some(lapse) => env.ledger().timestamp() < lapse,
        None => false,
    }
}

/// The `AlreadyMigrated` code of the target at `contract`, or `None` if
/// it is not one of the five targets.
fn already_migrated_code(config: &Config, contract: &Address) -> Option<u32> {
    let t = &config.targets;
    if *contract == t.oracle {
        Some(ORACLE_ALREADY_MIGRATED)
    } else if *contract == t.registry {
        Some(REGISTRY_ALREADY_MIGRATED)
    } else if *contract == t.staking {
        Some(STAKING_ALREADY_MIGRATED)
    } else if *contract == t.treasury {
        Some(TREASURY_ALREADY_MIGRATED)
    } else if *contract == t.factory {
        Some(FACTORY_ALREADY_MIGRATED)
    } else {
        None
    }
}

/// Section 4.3's "Executed as" column. Any target error traps and
/// reverts the whole `execute`.
fn dispatch(env: &Env, config: &Config, action: &Action) {
    let t = &config.targets;
    match action.clone() {
        Action::SetParam(key, value) => {
            // Only `liquidity_cover_ratio` passes `check_at_queue`.
            MarketFactoryClient::new(env, &t.factory).set_liquidity_cover_ratio(&value);
            storage::set_param(env, key, value);
        }
        Action::AddAsset(cfg) => RiskOracleClient::new(env, &t.oracle).add_asset(&cfg),
        Action::UpdateAsset(asset, cfg) => {
            RiskOracleClient::new(env, &t.oracle).update_asset(&asset, &cfg)
        }
        Action::DisableAsset(asset) => RiskOracleClient::new(env, &t.oracle).disable_asset(&asset),
        Action::RegisterDefinition(def) => {
            EventRegistryClient::new(env, &t.registry).register_definition(&def);
        }
        Action::OpenSeries(terms) => {
            MarketFactoryClient::new(env, &t.factory).open_series(&terms);
        }
        Action::AddKeeper(keeper) => StakingClient::new(env, &t.staking).add_keeper(&keeper),
        Action::RemoveKeeper(keeper) => StakingClient::new(env, &t.staking).remove_keeper(&keeper),
        Action::AddReporter(reporter, region) => {
            StakingClient::new(env, &t.staking).add_reporter(&reporter, &region)
        }
        Action::RemoveReporter(reporter) => {
            StakingClient::new(env, &t.staking).remove_reporter(&reporter)
        }
        Action::SetCommittee(committee) => {
            let mut next = config.clone();
            next.committee = committee;
            storage::set_config(env, &next);
        }
        Action::SetFormula(version, weights, params) => {
            RiskOracleClient::new(env, &t.oracle).set_formula(&version, &weights, &params)
        }
        Action::SetSeriesWasm(hash) => {
            MarketFactoryClient::new(env, &t.factory).set_series_wasm(&hash)
        }
        Action::Upgrade(contract, hash) => upgrade(env, config, &contract, hash),
        Action::Unpause(scope) => {
            storage::clear_pause(env, scope);
            events::Unpaused {
                scope,
                by: env.current_contract_address(),
            }
            .publish(env);
        }
        Action::SetSigners(signers, threshold) => {
            let mut next = config.clone();
            next.signers = signers;
            next.threshold = threshold;
            storage::set_config(env, &next);
        }
        Action::TreasuryAllocate(from, to, amount) => {
            TreasuryClient::new(env, &t.treasury).allocate(&from, &to, &amount)
        }
        Action::TreasurySpend(bucket, to, amount) => {
            TreasuryClient::new(env, &t.treasury).spend(&bucket, &to, &amount)
        }
        Action::SetGuardian(guardian) => {
            let mut next = config.clone();
            next.guardian = guardian;
            storage::set_config(env, &next);
        }
        Action::Pause(scope) => {
            // D8: always sets the lapse to now + MAX_PAUSE_SECS, whether
            // the scope is paused (renewal) or not (a fresh pause).
            storage::set_pause_lapse(env, scope, env.ledger().timestamp() + MAX_PAUSE_SECS);
            events::Paused {
                scope,
                by: env.current_contract_address(),
            }
            .publish(env);
        }
        Action::SetSubEpochSecs(asset, value) => {
            RiskOracleClient::new(env, &t.oracle).set_sub_epoch_secs(&asset, &value)
        }
        Action::SetHardCap(asset, amount) => {
            MarketFactoryClient::new(env, &t.factory).set_hard_cap(&asset, &amount)
        }
        Action::SetRelatedSellers(asset, list) => {
            MarketFactoryClient::new(env, &t.factory).set_related_sellers(&asset, &list)
        }
        Action::SetPriceGuard(asset, guard) => {
            MarketFactoryClient::new(env, &t.factory).set_price_guard(&asset, &guard)
        }
        Action::SetFactory(factory) => {
            EventRegistryClient::new(env, &t.registry).set_factory(&factory)
        }
    }
}

/// Section 6. The Governor's own upgrade swaps in-process (D14,
/// deviation 9); no `upgrade` symbol is exported. A target is upgraded
/// and then migrated in the same invocation, which runs the NEW code's
/// `migrate` (D13, `d13_test`); its own `AlreadyMigrated` means the new
/// code has no new storage version and is accepted.
fn upgrade(env: &Env, config: &Config, contract: &Address, hash: BytesN<32>) {
    if *contract == env.current_contract_address() {
        sylox_types::upgrade::apply_upgrade(env, CONTRACT_VERSION, hash);
        return;
    }
    let already_migrated = match already_migrated_code(config, contract) {
        Some(code) => code,
        // `check_at_queue` only lets the five targets and the Governor
        // itself through.
        None => panic_with_error!(env, Error::InvalidAction),
    };
    let target = UpgradeableClient::new(env, contract);
    target.upgrade(&hash);
    match target.try_migrate() {
        Ok(_) => {}
        Err(Ok(e)) if e == soroban_sdk::Error::from_contract_error(already_migrated) => {}
        // Any other failure reverts the whole `execute`, upgrade included,
        // with the target's own error.
        Err(Ok(e)) => env.panic_with_error(e),
        Err(Err(_)) => panic!("migrate() failed after upgrade"),
    }
}

#[cfg(test)]
mod budget_test;
#[cfg(test)]
mod d13_test;
#[cfg(test)]
mod test;
