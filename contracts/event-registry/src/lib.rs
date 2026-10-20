#![no_std]

//! EventRegistry: event definitions, Tier 1 proposals, challenges, the
//! committee ruling deadline, final event status. technical-doc.md
//! Section 8 (all), 12.2, 13, 14, 15, 21; ADR-001, 002, 003, 005, 006,
//! 007, 008, 012. Design: `docs/design/event-registry.md`.
//!
//! Scope of this build (feat/event-registry): `register_definition`,
//! `propose_tier1` for Depeg and IssuerFreeze, `challenge`, `finalize`
//! (including Depeg cure), `rule`, `resolve_timeout`, every Section
//! 12.2 read, the `RiskOracle` push calls, bonds via `Staking`.
//! `propose_tier2` (WithdrawalHalt), `propose_tier3` (Insolvency) and
//! MintWithoutBacking are each a known-gap issue, not stubbed here.
//!
//! **Lead decision (design note Section 2): `propose_tier1` takes an
//! explicit `version`.** Deviates from Section 8.1/8.2/8.8 and ADR-001,
//! which take only (asset, kind) and always the canonical version. A
//! series pins the version canonical when it opened; a newer version
//! can supersede that one while the series is still live, and a real
//! failure discovered after that point must still be provable against
//! the OLDER, still-pinned version. `version` is accepted only if it
//! is canonical, or `version_has_live_cover` reports a live series
//! still pins it (always `false` in this build, `MarketFactory` is
//! out of scope: every proposal in this build is canonical-only, in
//! practice).
//!
//! **Lead decision (design note Section 6): `ActiveCount(asset)`
//! replaces a single boolean push to `RiskOracle.set_event_in_progress`.**
//! Several (kind, version) events can be live for one asset at once;
//! a boolean cannot represent that correctly. Asset-wide effects
//! (this count, the oracle push, `set_event_band`) fire only for a
//! CANONICAL-version event.
//!
//! **No compliance-action exclusion mechanism.**
//! An earlier draft of the design note invented one; it was an
//! unbonded, unilateral governor veto over an IssuerFreeze payout and
//! is dropped. Section 8.2's own requirement ("no governance flag
//! marks the issuer's action as a declared compliance action") is
//! satisfied by the existing `challenge`/`rule`/`resolve_timeout` path:
//! anyone who believes a counted action was a legitimate compliance
//! action challenges the proposal with evidence, and the committee
//! rules.
//!
//! **No time-based cooldown after Cured/Rejected.**
//! `propose_tier1` instead accepts a new proposal for the same
//! (asset, kind, version) once its own freshly computed `window_start`
//! is strictly after the prior event's own `left_at` (the
//! `storage::get_left_at`/`LeftAt` check inside `propose_tier1`
//! below); see the design note's own Section 2a for the argument
//! this still rejects re-proposing unchanged data.
//!
//! **Cure is strict.** Any permanently missing
//! epoch anywhere in the cure window means no cure at all, routing to
//! Declared unconditionally; there is no partial credit for a window
//! a keeper (who may also have sold cover on the series) can
//! selectively leave incomplete. "Permanently missing" tests the
//! slot's own state (`Empty`), never elapsed time
//! alone; a `Disputed` epoch is "not ready" until resolved or timed
//! out, which `guaranteed_finalizable_at`'s own
//! worst-case formula accounts for (see `finalize` and its tests).

mod clients;
mod error;
mod events;
mod storage;

use soroban_sdk::{contract, contractimpl, symbol_short, Address, BytesN, Env, Symbol, Vec};
use sylox_types::{
    AssetEventStatus, BondKey, CoverGate, EventDefinition, EventKind, EventRecord, EventState,
    IssuerFlags, SlotState,
};

pub use error::Error;
use storage::Config;

use clients::{GovernorClient, MarketFactoryClient, RiskOracleClient, StakingClient};
use sylox_types::time::EPOCH_SECS;

/// Backfill window: how long after an epoch's own close a keeper may
/// still post it (ADR-005). Mirrors `RiskOracle`'s own `WINDOW_SECS`:
/// both read the same shared `EPOCH_SECS`, so the two can never
/// silently drift apart. 72 epochs.
const WINDOW_SECS: u64 = 72 * EPOCH_SECS;

/// IssuerFreeze's own fixed 7 day window (Section 8.2), and the Depeg
/// liquidity baseline immediately before its own window (ADR-005):
/// the same 168 epoch span, used for two different purposes.
const BASELINE_EPOCHS: u64 = 7 * 24;

/// `check_depeg`'s own liquidity baseline
/// loop below has no minimum-count check of its own, only
/// `liquidity_values.is_empty()` (catches zero real epochs, not
/// "mostly missing"). Do NOT reuse `max_missing_epochs`: that field
/// tolerates gaps in the 72h Depeg WINDOW, a different, intentionally
/// separate tolerance from this 168 epoch BASELINE. Require at least
/// this many Final epochs within the baseline instead: one full
/// backfill window (`WINDOW_SECS` / `EPOCH_SECS`, 72 epochs) of gap is
/// a normal, already-supported operating pattern, so the minimum
/// tolerates exactly one such gap and no more, rather than an
/// arbitrary ratio.
const MIN_BASELINE_FINAL_EPOCHS: u32 = BASELINE_EPOCHS as u32 - (WINDOW_SECS / EPOCH_SECS) as u32;
const _: () = assert!(MIN_BASELINE_FINAL_EPOCHS > 0);

/// The largest number of cure-window
/// epochs (`challenge_secs / EPOCH_SECS`) `validate_definition_params`
/// will accept, 72 hours' worth. Keeps `storage::CureProgress`'s own
/// `recorded: u128` bitmap comfortably sized for every definition
/// this build can register, with headroom to spare. Moved to
/// `sylox_types::time` (Section 5.9 S7).
use sylox_types::time::MAX_CURE_EPOCHS;

// `SIGNAL_DISPUTE_SECS` and `SIGNAL_DISPUTE_RULING_SECS` (ADR-010)
// are not needed by this contract's own production logic (it only
// ever classifies each cure-window epoch's CURRENT state, never
// predicts when every epoch will settle); the test modules that
// verify `guaranteed_finalizable_at`'s own worst-case formula
// each define their own local copies.

#[contract]
pub struct EventRegistry;

/// Issue #12. Bump by hand on every `upgrade()` this contract ships.
/// Bumped to 2 for this upgrade: `version_has_live_cover` goes from a
/// stand-in to a real `MarketFactory` call, and `set_factory` is new
/// (markets design note Section 10, deviation 14).
const CONTRACT_VERSION: u32 = 2;
/// Issue #12. Instance key for `sylox_types::upgrade::do_migrate`'s own
/// "which version has this contract's storage already been migrated to"
/// marker.
const MIGRATED_TO_KEY: Symbol = symbol_short!("MIGRATE");
/// markets design note Section 10, deviation 14: instance key for the
/// one-time lock on `set_factory` - present (always `true`) once set,
/// absent beforehand. A dedicated flag, never a comparison against
/// the placeholder address's own value, so this works identically on
/// every network regardless of what that redeploy's placeholder was.
const FACTORY_SET_KEY: Symbol = symbol_short!("FACT_SET");

/// `set_governor`'s own one-time lock, same shape as `FACTORY_SET_KEY`
/// above: absent until the first successful `set_governor` call, then
/// permanently present, so a second call always fails.
const GOVERNOR_SET_KEY: Symbol = symbol_short!("GOV_SET");

#[contractimpl]
impl EventRegistry {
    /// Issue #21: a constructor, not a separate `initialize` call -
    /// see `RiskOracle::__constructor`'s own doc comment for why. Only
    /// stores addresses and config; never calls another contract (the
    /// other three may not exist yet when this runs, since every
    /// address here is computed in advance from a deterministic salt,
    /// deploy/deploy.sh's own "Constructor flow" section).
    pub fn __constructor(
        env: Env,
        governor: Address,
        oracle: Address,
        staking: Address,
        factory: Address,
        usdc: Address,
    ) {
        sylox_types::ttl::extend_instance(&env);
        storage::set_config(
            &env,
            &Config {
                governor,
                oracle,
                staking,
                factory,
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
    /// upgrade(wasm_hash), callable only by the governor." After 12
    /// months on mainnet, Section 17.3 also notes governance plans to
    /// remove `upgrade` from `EventRegistry` entirely (deploying v2
    /// alongside instead); not relevant yet on testnet.
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

    /// markets design note Section 10, deviation 14: the real
    /// `MarketFactory` address, governor-only, exactly once. `factory`
    /// (set at construction) is a placeholder until `MarketFactory`
    /// is deployed; this call gives it the real address and then
    /// permanently locks - a second call, naming any address
    /// including the same one, is rejected. Taking the address as a
    /// call argument (never a hardcoded constant) means this same
    /// upgrade works on every network regardless of what address each
    /// one's own placeholder happened to be.
    pub fn set_factory(env: Env, factory: Address) -> Result<(), Error> {
        let mut config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if env.storage().instance().has(&FACTORY_SET_KEY) {
            return Err(Error::FactoryAlreadySet);
        }
        config.factory = factory;
        storage::set_config(&env, &config);
        env.storage().instance().set(&FACTORY_SET_KEY, &true);
        Ok(())
    }

    /// One-time governor handover, for the Governor deployment cutover:
    /// callable only by the CURRENT governor, exactly once. Same
    /// one-time-lock pattern as `set_factory` above (a storage flag
    /// checked before the write, set after it), so a second call
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
        storage::set_config(&env, &config);
        env.storage().instance().set(&GOVERNOR_SET_KEY, &true);
        Ok(())
    }

    /// technical-doc.md Section 8.8. `def.version` is ignored on input
    /// (the caller cannot pick a version number); the stored version is
    /// always the previous canonical version plus one, starting at 1.
    pub fn register_definition(env: Env, def: EventDefinition) -> Result<u32, Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);

        let asset_config = RiskOracleClient::new(&env, &config.oracle)
            .asset_config(&def.asset)
            .ok_or(Error::InvalidDefinition)?;
        if def.reference != asset_config.reference {
            return Err(Error::InvalidDefinition);
        }
        validate_definition_params(&def)?;
        if def.kind == EventKind::IssuerFreeze
            && !issuer_freeze_possible(&asset_config.issuer_flags)
        {
            return Err(Error::FreezeImpossible);
        }

        let previous_version = storage::get_canonical(&env, &def.asset, def.kind);
        let version = previous_version + 1;

        // An event for (asset, kind) in progress blocks a new version
        // (Section 8.8): the canonical definition cannot shift under a
        // proposal that is still being decided.
        if matches!(
            storage::get_status(&env, &def.asset, def.kind),
            AssetEventStatus::InProgress(_)
        ) {
            return Err(Error::EventInProgress);
        }

        // `DefinitionInUse`: a live series still pinning `previous_version`
        // blocks registering a new one over it (Section 8.8). Wired
        // through the same shared `version_has_live_cover` stand-in
        // `propose_tier1` uses (design note Section 2); it always
        // returns `false` in this build (no `MarketFactory` exists to
        // report a live series at all), so this check is unreachable
        // for now, but genuinely enforces the rule the moment
        // `version_has_live_cover` has something real to report,
        // with no further change needed here.
        if previous_version > 0
            && Self::version_has_live_cover(&env, &config, &def.asset, def.kind, previous_version)
        {
            return Err(Error::DefinitionInUse);
        }

        let mut stored = def.clone();
        stored.version = version;
        storage::set_def(&env, &stored);
        storage::set_canonical(&env, &def.asset, def.kind, version);

        events::DefinitionRegistered {
            asset: def.asset,
            kind: def.kind,
            version,
            previous_version,
        }
        .publish(&env);

        Ok(version)
    }

    /// technical-doc.md Section 8.2, as amended by this build's lead
    /// decision (design note Section 2).
    pub fn propose_tier1(
        env: Env,
        caller: Address,
        asset: Address,
        kind: EventKind,
        version: u32,
    ) -> Result<u64, Error> {
        // Nothing pays a Tier 1 proposer in
        // this build, but `caller` is still stored as `proposer` and
        // published in `EventProposed`, a public record an indexer
        // will display. Without this, anyone could propose in someone
        // else's name.
        caller.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;
        let def = storage::get_def(&env, &asset, kind, version).ok_or(Error::UnknownDefinition)?;

        let canonical = storage::get_canonical(&env, &asset, kind);
        let is_canonical = version == canonical;
        if !is_canonical && !Self::version_has_live_cover(&env, &config, &asset, kind, version) {
            return Err(Error::VersionNotCovered);
        }

        if storage::get_live_event(&env, &asset, kind, version).is_some() {
            return Err(Error::EventInProgress);
        }

        let now = env.ledger().timestamp();
        let oracle = RiskOracleClient::new(&env, &config.oracle);
        let window_start = match kind {
            EventKind::Depeg => check_depeg(&env, &oracle, &asset, &def)?,
            EventKind::IssuerFreeze => check_issuer_freeze(&env, &oracle, &asset, &def)?,
            _ => return Err(Error::UnknownDefinition),
        };

        // Gated by new data, not a time
        // cooldown. If a prior event for this exact (asset, kind,
        // version) left Proposed/Escalated into Cured or Rejected,
        // the new proposal's own freshly computed `window_start` must
        // be strictly after that departure time.
        if let Some(left_at) = storage::get_left_at(&env, &asset, kind, version) {
            if window_start <= left_at {
                return Err(Error::EventInProgress);
            }
        }

        let id = storage::next_event_id(&env);
        let record = EventRecord {
            id,
            asset: asset.clone(),
            kind,
            def_version: version,
            tier: 1,
            state: EventState::Proposed,
            window_start,
            proposed_at: now,
            escalated_at: None,
            declared_at: None,
            evidence_hash: BytesN::from_array(&env, &[0u8; 32]),
            proposer: caller.clone(),
            bond: 0,
        };
        storage::set_event(&env, &record);
        storage::set_live_event(&env, &asset, kind, version, id);
        storage::clear_left_at(&env, &asset, kind, version);

        if is_canonical {
            storage::set_status(&env, &asset, kind, &AssetEventStatus::InProgress(id));
            Self::enter_active(&env, &oracle, &asset);
        }

        events::EventProposed {
            asset,
            event_id: id,
            kind,
            def_version: version,
            tier: 1,
            window_start,
            proposer: caller,
            evidence: BytesN::from_array(&env, &[0u8; 32]),
        }
        .publish(&env);

        Ok(id)
    }

    pub fn challenge(
        env: Env,
        challenger: Address,
        event_id: u64,
        evidence: BytesN<32>,
    ) -> Result<(), Error> {
        challenger.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;
        let mut record = storage::get_event(&env, event_id).ok_or(Error::UnknownEvent)?;
        if record.state != EventState::Proposed {
            return Err(Error::WrongState);
        }
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)
            .ok_or(Error::UnknownDefinition)?;
        let now = env.ledger().timestamp();
        if now >= record.proposed_at + def.challenge_secs {
            return Err(Error::ChallengeWindowClosed);
        }

        let bond = challenge_bond_placeholder();
        StakingClient::new(&env, &config.staking).lock_bond(
            &BondKey::EventChallenge(event_id),
            &challenger,
            &bond,
            &None,
        );

        record.state = EventState::Escalated;
        record.escalated_at = Some(now);
        storage::set_event(&env, &record);

        events::EventChallenged {
            asset: record.asset.clone(),
            event_id,
            challenger,
            evidence: evidence.clone(),
        }
        .publish(&env);
        events::EventEscalated {
            asset: record.asset,
            event_id,
            escalated_at: now,
            ruling_deadline: now + def.ruling_deadline_secs,
        }
        .publish(&env);

        Ok(())
    }

    /// technical-doc.md Section 8.1, corrected for the cure window, a
    /// three-way epoch state, and cure strictness. See
    /// `docs/design/event-registry.md` Section 5.
    pub fn finalize(env: Env, event_id: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let mut record = storage::get_event(&env, event_id).ok_or(Error::UnknownEvent)?;
        if record.state != EventState::Proposed {
            return Err(Error::WrongState);
        }
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)
            .ok_or(Error::UnknownDefinition)?;
        let now = env.ledger().timestamp();
        if now < record.proposed_at + def.challenge_secs {
            return Err(Error::ChallengeWindowOpen);
        }

        let outcome = if record.kind == EventKind::Depeg {
            cure_outcome(&env, &config, &record, &def)?
        } else {
            CureOutcome::Declared
        };

        match outcome {
            CureOutcome::NotReady => Err(Error::DataNotFinal),
            CureOutcome::Cured => {
                record.state = EventState::Cured;
                Self::leave_active(&env, &config, &record, now)?;
                events::EventCured {
                    asset: record.asset.clone(),
                    event_id,
                }
                .publish(&env);
                storage::set_event(&env, &record);
                Ok(())
            }
            CureOutcome::Declared => {
                record.state = EventState::Declared;
                record.declared_at = Some(now);
                Self::declare(&env, &config, &record)?;
                storage::set_event(&env, &record);
                Ok(())
            }
        }
    }

    /// Permissionless (no `require_auth`):
    /// anyone, including the keeper service, may call this to record
    /// a Depeg event's cure-window progress before an epoch that is
    /// already decidable rotates out of `RiskOracle`'s own ring.
    /// Unlike `finalize`, this never errors on "not ready" — recording
    /// whatever is currently decidable, and keeping that record, is
    /// itself the successful outcome; there being more to record later
    /// is not a failure. Calling it twice in a row is a no-op the
    /// second time (`record_cure_progress`'s own recorded-bit check).
    pub fn checkpoint_cure(env: Env, event_id: u64) -> Result<storage::CureProgress, Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let record = storage::get_event(&env, event_id).ok_or(Error::UnknownEvent)?;
        if record.state != EventState::Proposed {
            return Err(Error::WrongState);
        }
        if record.kind != EventKind::Depeg {
            // IssuerFreeze has no cure path at all (`finalize` always
            // decides it as `CureOutcome::Declared`); there is nothing
            // for this function to record.
            return Err(Error::WrongState);
        }
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)
            .ok_or(Error::UnknownDefinition)?;
        let oracle = RiskOracleClient::new(&env, &config.oracle);
        Ok(record_cure_progress(&env, &oracle, &record, &def))
    }

    pub fn rule(env: Env, event_id: u64, declare: bool, reason: BytesN<32>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        let committee = GovernorClient::new(&env, &config.governor).committee();
        committee.require_auth();
        sylox_types::ttl::extend_instance(&env);

        let mut record = storage::get_event(&env, event_id).ok_or(Error::UnknownEvent)?;
        if record.state != EventState::Escalated {
            return Err(Error::WrongState);
        }
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)
            .ok_or(Error::UnknownDefinition)?;
        let now = env.ledger().timestamp();
        let escalated_at = record.escalated_at.ok_or(Error::WrongState)?;
        if now >= escalated_at + def.ruling_deadline_secs {
            return Err(Error::RulingDeadlinePassed);
        }

        let staking = StakingClient::new(&env, &config.staking);
        let key = BondKey::EventChallenge(event_id);
        if declare {
            staking.forfeit_bond(&key, &None);
            record.state = EventState::Declared;
            record.declared_at = Some(now);
            Self::declare(&env, &config, &record)?;
        } else {
            staking.release_bond(&key);
            record.state = EventState::Rejected;
            Self::leave_active(&env, &config, &record, now)?;
            events::EventRejected {
                asset: record.asset.clone(),
                event_id,
                reason,
            }
            .publish(&env);
        }
        storage::set_event(&env, &record);
        Ok(())
    }

    pub fn resolve_timeout(env: Env, event_id: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let mut record = storage::get_event(&env, event_id).ok_or(Error::UnknownEvent)?;
        if record.state != EventState::Escalated {
            return Err(Error::WrongState);
        }
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)
            .ok_or(Error::UnknownDefinition)?;
        let now = env.ledger().timestamp();
        let escalated_at = record.escalated_at.ok_or(Error::WrongState)?;
        if now < escalated_at + def.ruling_deadline_secs {
            return Err(Error::RulingDeadlineNotReached);
        }

        // ADR-002: on timeout every bond is refunded, unconditionally,
        // regardless of the default outcome.
        StakingClient::new(&env, &config.staking).release_bond(&BondKey::EventChallenge(event_id));

        let committee = GovernorClient::new(&env, &config.governor).committee();
        let misses_after = storage::increment_committee_misses(&env, &committee);

        // Tier 1 default: Declared (Section 8.9).
        let declared = true;
        record.state = EventState::Declared;
        record.declared_at = Some(now);
        Self::declare(&env, &config, &record)?;
        storage::set_event(&env, &record);

        events::RulingTimedOut {
            asset: record.asset,
            event_id,
            outcome_declared: declared,
            committee,
            misses_after,
        }
        .publish(&env);
        Ok(())
    }

    // -- reads --

    pub fn definition(
        env: Env,
        asset: Address,
        kind: EventKind,
        version: u32,
    ) -> Option<EventDefinition> {
        storage::get_def(&env, &asset, kind, version)
    }

    pub fn current_version(env: Env, asset: Address, kind: EventKind) -> u32 {
        storage::get_canonical(&env, &asset, kind)
    }

    pub fn event(env: Env, event_id: u64) -> Option<EventRecord> {
        storage::get_event(&env, event_id)
    }

    pub fn event_status(env: Env, asset: Address, kind: EventKind) -> AssetEventStatus {
        storage::get_status(&env, &asset, kind)
    }

    pub fn in_progress(env: Env, asset: Address) -> bool {
        storage::get_active_count(&env, &asset) > 0
    }

    pub fn has_declared(env: Env, asset: Address) -> bool {
        for kind in ALL_KINDS {
            if matches!(
                storage::get_status(&env, &asset, kind),
                AssetEventStatus::Declared(..)
            ) {
                return true;
            }
        }
        false
    }

    /// technical-doc.md Section 9.4 step 2, ADR-003. First-match order.
    /// Thresholds and windows come from the asset's own canonical
    /// Depeg definition where one is registered; the Section 23
    /// default applies otherwise (matching Section 9.4's own stated
    /// fallback rule verbatim). WithdrawalHalt is out of scope for
    /// this build (no canonical definition of that kind can exist
    /// yet), so its own window always uses the Section 23 default.
    pub fn cover_gate(env: Env, asset: Address) -> Result<CoverGate, Error> {
        let config = Self::require_config(&env)?;
        if storage::get_active_count(&env, &asset) > 0 {
            return Ok(CoverGate::EventInProgress);
        }

        let oracle = RiskOracleClient::new(&env, &config.oracle);
        let ring = oracle.ring(&asset);
        let newest_epoch = match oracle.newest_epoch(&asset) {
            Some(e) => e,
            None => return Ok(CoverGate::Clear),
        };

        let depeg_canonical = storage::get_canonical(&env, &asset, EventKind::Depeg);
        let (depeg_window_secs, depeg_threshold) =
            match storage::get_def(&env, &asset, EventKind::Depeg, depeg_canonical) {
                Some(def) => (def.depeg_window_secs, def.depeg_threshold),
                None => (DEFAULT_DEPEG_WINDOW_SECS, DEFAULT_DEPEG_THRESHOLD),
            };
        let halt_window_secs = DEFAULT_HALT_WINDOW_SECS;

        let depeg_epochs = (depeg_window_secs / EPOCH_SECS) as u32;
        match depeg_check(
            &env,
            &oracle,
            &asset,
            &ring,
            newest_epoch,
            depeg_epochs,
            depeg_threshold,
        ) {
            DepegCheck::Depeg => return Ok(CoverGate::RecentDepeg),
            DepegCheck::UnbuiltBacklog => return Ok(CoverGate::UnbuiltBacklog),
            DepegCheck::Clear => {}
        }

        let halt_epochs = (halt_window_secs / EPOCH_SECS) as u32;
        if any_epoch_endpoint_down(&ring, newest_epoch, halt_epochs) {
            return Ok(CoverGate::RecentEndpointOutage);
        }

        if any_epoch_with_issuer_action(&ring, newest_epoch, BASELINE_EPOCHS as u32) {
            return Ok(CoverGate::RecentIssuerAction);
        }

        Ok(CoverGate::Clear)
    }

    /// technical-doc.md Section 8.6.
    pub fn covers(env: Env, event_id: u64, def_version: u32, start: u64, expiry: u64) -> bool {
        let Some(record) = storage::get_event(&env, event_id) else {
            return false;
        };
        if record.def_version != def_version {
            return false;
        }
        if record.window_start < start || record.window_start > expiry {
            return false;
        }
        let window_len =
            match storage::get_def(&env, &record.asset, record.kind, record.def_version) {
                Some(def) => match record.kind {
                    EventKind::Depeg => def.depeg_window_secs,
                    EventKind::IssuerFreeze => BASELINE_EPOCHS * EPOCH_SECS,
                    _ => 0,
                },
                None => 0,
            };
        record.proposed_at <= expiry + window_len
    }

    pub fn ruling_deadline(env: Env, event_id: u64) -> Option<u64> {
        let record = storage::get_event(&env, event_id)?;
        let def = storage::get_def(&env, &record.asset, record.kind, record.def_version)?;
        record
            .escalated_at
            .map(|escalated_at| escalated_at + def.ruling_deadline_secs)
    }

    pub fn committee_misses(env: Env, committee: Address) -> u32 {
        storage::get_committee_misses(&env, &committee)
    }

    pub fn active_event_count(env: Env, asset: Address) -> u32 {
        storage::get_active_count(&env, &asset)
    }

    // -- internal --

    fn require_config(env: &Env) -> Result<Config, Error> {
        storage::get_config(env).ok_or(Error::NotInitialized)
    }

    /// Design note Section 2: the single function routing "does a live
    /// series still pin this version" through one place. markets
    /// design note Section 10 closes this: `MarketFactory` owns the
    /// pinned-version bookkeeping itself (`Pins`/`LiveCoverCount`), so
    /// this is one cross-contract call, not an iteration over
    /// `Series` instances.
    fn version_has_live_cover(
        env: &Env,
        config: &Config,
        asset: &Address,
        kind: EventKind,
        version: u32,
    ) -> bool {
        MarketFactoryClient::new(env, &config.factory).has_live_cover(asset, &kind, &version)
    }

    fn enter_active(env: &Env, oracle: &RiskOracleClient, asset: &Address) {
        let count = storage::get_active_count(env, asset);
        storage::set_active_count(env, asset, count + 1);
        if count == 0 {
            oracle.set_event_in_progress(asset, &true);
        }
    }

    fn leave_active(
        env: &Env,
        config: &Config,
        record: &EventRecord,
        left_at: u64,
    ) -> Result<(), Error> {
        let canonical = storage::get_canonical(env, &record.asset, record.kind);
        storage::clear_live_event(env, &record.asset, record.kind, record.def_version);
        if record.def_version == canonical {
            storage::set_status(env, &record.asset, record.kind, &AssetEventStatus::None);
            let count = storage::get_active_count(env, &record.asset);
            let next = count.saturating_sub(1);
            storage::set_active_count(env, &record.asset, next);
            if next == 0 {
                RiskOracleClient::new(env, &config.oracle)
                    .set_event_in_progress(&record.asset, &false);
            }
        }
        storage::set_left_at(env, &record.asset, record.kind, record.def_version, left_at);
        Ok(())
    }

    /// technical-doc.md Section 8.5. Asset-wide
    /// effects (`set_event_band`, clearing the active count) fire only
    /// for a canonical-version event.
    fn declare(env: &Env, config: &Config, record: &EventRecord) -> Result<(), Error> {
        let canonical = storage::get_canonical(env, &record.asset, record.kind);
        let is_canonical = record.def_version == canonical;
        if is_canonical {
            RiskOracleClient::new(env, &config.oracle).set_event_band(&record.asset);
        }
        // E2: Declared is terminal. `LiveEvent` is deliberately NOT
        // cleared here, unlike the Cured/Rejected path in
        // `leave_active`: clearing it would let a later
        // `propose_tier1` call for this exact (asset, kind, version)
        // succeed again (E3's own "no live event" check would read
        // as free), creating a SECOND event whose own later
        // resolution (Cured or Rejected) would then overwrite
        // `Status(asset, kind)` back to `None` via `leave_active`,
        // destroying the record that this version was ever Declared
        // even though the oracle's own sticky `Event` band (set just
        // above) is never cleared to match. `LiveEvent` staying set
        // to this Declared event's own id is what keeps E1/E2/E3 all
        // consistent with each other for the rest of this version's
        // existence.
        if is_canonical {
            storage::set_status(
                env,
                &record.asset,
                record.kind,
                &AssetEventStatus::Declared(
                    record.id,
                    record.def_version,
                    record.window_start,
                    record.declared_at.unwrap_or_default(),
                ),
            );
            let count = storage::get_active_count(env, &record.asset);
            let next = count.saturating_sub(1);
            storage::set_active_count(env, &record.asset, next);
            if next == 0 {
                RiskOracleClient::new(env, &config.oracle)
                    .set_event_in_progress(&record.asset, &false);
            }
        }
        events::EventDeclared {
            asset: record.asset.clone(),
            event_id: record.id,
            kind: record.kind,
            def_version: record.def_version,
            window_start: record.window_start,
            declared_at: record.declared_at.unwrap_or_default(),
        }
        .publish(env);
        Ok(())
    }
}

const ALL_KINDS: [EventKind; 5] = [
    EventKind::Depeg,
    EventKind::IssuerFreeze,
    EventKind::MintWithoutBacking,
    EventKind::WithdrawalHalt,
    EventKind::Insolvency,
];

enum CureOutcome {
    NotReady,
    Cured,
    Declared,
}

/// A fixed placeholder challenge bond amount for this phase. Tier 1
/// defines no `challenge_bond` field of its own on `EventDefinition`
/// distinct from Tier 2's `claim_bond` (Section 8.3); this build uses
/// the same `signal_dispute_bond` default (Section 23) every other
/// bonded dispute in this workspace uses, the same pattern
/// `RiskOracle`'s own private `signal_dispute_bond()` helper follows
/// for its own, unrelated dispute bond.
fn challenge_bond_placeholder() -> i128 {
    10_000_000_000
}

fn issuer_freeze_possible(flags: &IssuerFlags) -> bool {
    flags.auth_revocable || flags.clawback_enabled
}

fn validate_definition_params(def: &EventDefinition) -> Result<(), Error> {
    match def.kind {
        EventKind::Depeg => {
            if def.freeze_pct_bps != 0
                || def.auth_revocation_threshold != 0
                || def.mint_spike_bps != 0
                || def.halt_window_secs != 0
            {
                return Err(Error::InvalidDefinition);
            }
            if def.depeg_window_secs == 0 || def.depeg_threshold == 0 || def.cure_threshold == 0 {
                return Err(Error::InvalidDefinition);
            }
            let window_epochs = def.depeg_window_secs / EPOCH_SECS;
            // The ONE shared source for this ceiling:
            // sylox_types::time::MAX_DEPEG_WINDOW_EPOCHS, never a
            // separate copy of RING_SLOTS - BASELINE_EPOCHS computed
            // here, so this check and MAX_UNBUILT_HOURS_SCANNED_BY_
            // COVER_GATE's own build-time assertion can never drift
            // apart.
            if window_epochs > sylox_types::time::MAX_DEPEG_WINDOW_EPOCHS {
                return Err(Error::InvalidDefinition);
            }
        }
        EventKind::IssuerFreeze => {
            if def.depeg_threshold != 0
                || def.depeg_window_secs != 0
                || def.max_missing_epochs != 0
                || def.cure_threshold != 0
                || def.mint_spike_bps != 0
                || def.halt_window_secs != 0
            {
                return Err(Error::InvalidDefinition);
            }
            if def.freeze_pct_bps == 0 && def.auth_revocation_threshold == 0 {
                return Err(Error::InvalidDefinition);
            }
        }
        _ => return Err(Error::InvalidDefinition),
    }
    if def.ruling_deadline_secs == 0 {
        return Err(Error::InvalidDefinition);
    }
    // `challenge_secs` must be a whole
    // number of epochs, between 1 and `MAX_CURE_EPOCHS`. Without the
    // lower bound, a short `challenge_secs` landing inside a single
    // epoch makes `cure_outcome`'s own cure-window loop run zero
    // times, which reads as "no failure observed" and cures with no
    // data at all. The upper bound keeps the cure window's own
    // `CureProgress::recorded` bitmap (`u128`) comfortably
    // sized for every cure epoch it must address.
    if def.challenge_secs == 0
        || !def.challenge_secs.is_multiple_of(EPOCH_SECS)
        || def.challenge_secs / EPOCH_SECS > MAX_CURE_EPOCHS
    {
        return Err(Error::InvalidDefinition);
    }
    Ok(())
}

/// Finds the `RingSlot` for `epoch` within `ring`'s own "oldest
/// first" layout (technical-doc.md Section 12.1's own doc comment on
/// `ring()`), without a linear scan: `ring`'s last element is always
/// the newest epoch the oracle has ever stored a slot for (or, if the
/// asset never posted, 0, an `Empty` placeholder), and every element
/// before it is exactly one epoch older, in order.
/// Finds the slot holding `epoch`'s own data, if any. `ring()`'s own
/// "oldest first" layout (Section 12.1) is a fixed rotation of ring
/// POSITIONS relative to the newest write, not "the N most recent
/// epochs by number": if a gap in posting left some position
/// unwritten since an EARLIER epoch occupied it, that position still
/// holds that earlier epoch's own, already-verified-real data, which
/// the naive "offset back from newest" arithmetic alone would
/// misattribute to whatever epoch number the arithmetic expects at
/// that offset. Checking the found slot's own `.epoch` field against
/// what was actually asked for is what catches this: a mismatch
/// means `epoch` was never written into the live span this `ring()`
/// snapshot covers, the same as if the slot were `Empty`.
fn slot_for_epoch(
    ring: &Vec<sylox_types::RingSlot>,
    newest_epoch: u64,
    epoch: u64,
) -> Option<sylox_types::RingSlot> {
    if epoch > newest_epoch {
        return None;
    }
    let back = newest_epoch - epoch;
    if back as u32 >= ring.len() {
        return None;
    }
    let index = ring.len() - 1 - back as u32;
    let slot = ring.get(index).unwrap();
    if slot.state != SlotState::Empty && slot.epoch != epoch {
        return None;
    }
    Some(slot)
}

fn effective_state_of(slot: &sylox_types::RingSlot, now: u64) -> SlotState {
    if slot.state == SlotState::Pending && now >= slot.pending_until {
        SlotState::Final
    } else {
        slot.state
    }
}

/// design note Section 5.
/// `target_epoch` is the epoch actually being asked about, independent
/// of whether `slot` carries any real data for it: `slot_for_epoch`
/// returns `None` both when `target_epoch` is outside this `ring()`
/// snapshot's own live span, AND when a position-matching slot exists
/// but holds a DIFFERENT, stale epoch's data (a gap, see
/// `slot_for_epoch`'s own doc comment) — both cases mean `target_epoch`
/// itself was never written, exactly like a verified `Empty` slot.
fn epoch_disposition(
    slot: Option<&sylox_types::RingSlot>,
    target_epoch: u64,
    now: u64,
) -> EpochDisposition {
    let is_empty = match slot {
        None => true,
        Some(slot) => slot.state == SlotState::Empty,
    };
    if is_empty {
        let epoch_close = (target_epoch + 1) * EPOCH_SECS;
        return if now > epoch_close + WINDOW_SECS {
            EpochDisposition::PermanentlyMissing
        } else {
            EpochDisposition::NotReady
        };
    }
    let slot = slot.unwrap();
    let effective = effective_state_of(slot, now);
    if effective == SlotState::Final {
        EpochDisposition::Final(slot.peg_ratio)
    } else {
        // Pending (not yet past pending_until) or Disputed.
        EpochDisposition::NotReady
    }
}

enum EpochDisposition {
    Final(i128),
    PermanentlyMissing,
    NotReady,
}

/// technical-doc.md Section 8.2, design note Section 3.
fn check_depeg(
    env: &Env,
    oracle: &RiskOracleClient,
    asset: &Address,
    def: &EventDefinition,
) -> Result<u64, Error> {
    let ring = oracle.ring(asset);
    let newest_epoch = oracle.newest_epoch(asset).ok_or(Error::Tier1CheckFailed)?;
    let now = env.ledger().timestamp();

    let window_epochs = def.depeg_window_secs / EPOCH_SECS;
    if newest_epoch + 1 < window_epochs {
        return Err(Error::Tier1CheckFailed);
    }
    let window_start_epoch = newest_epoch + 1 - window_epochs;

    let mut missing: u32 = 0;
    for epoch in window_start_epoch..=newest_epoch {
        let slot = slot_for_epoch(&ring, newest_epoch, epoch);
        match epoch_disposition(slot.as_ref(), epoch, now) {
            EpochDisposition::Final(peg_ratio) => {
                if peg_ratio >= def.depeg_threshold {
                    return Err(Error::Tier1CheckFailed);
                }
            }
            EpochDisposition::PermanentlyMissing | EpochDisposition::NotReady => {
                missing += 1;
            }
        }
    }
    if missing > def.max_missing_epochs {
        return Err(Error::Tier1CheckFailed);
    }

    // 7 day baseline, strictly before the window.
    // `window_start_epoch < BASELINE_EPOCHS` alone only guards the
    // subtraction below from underflowing; on a real network
    // `window_start_epoch` is always far larger than `BASELINE_EPOCHS`
    // (168), so that comparison never actually requires the asset to
    // have 168 epochs of its OWN history. Measure against
    // `first_epoch` instead: the baseline must fall entirely within
    // real history, not merely within absolute-epoch-number room.
    let first_epoch = oracle.first_epoch(asset).ok_or(Error::Tier1CheckFailed)?;
    if window_start_epoch < first_epoch + BASELINE_EPOCHS {
        return Err(Error::Tier1CheckFailed);
    }
    let baseline_start = window_start_epoch - BASELINE_EPOCHS;
    let mut liquidity_values: Vec<i128> = Vec::new(env);
    for epoch in baseline_start..window_start_epoch {
        let slot = slot_for_epoch(&ring, newest_epoch, epoch);
        if let EpochDisposition::Final(_) = epoch_disposition(slot.as_ref(), epoch, now) {
            liquidity_values.push_back(slot.unwrap().liquidity_2pct);
        }
    }
    // `first_epoch` alone proves calendar
    // time has elapsed since this asset's first post, not that the
    // baseline is actually populated. A keeper could post once, go
    // dark for 168+ epochs, then resume right before the Depeg
    // window starts, clearing the calendar guard above while leaving
    // the baseline almost entirely empty. `liquidity_values.is_empty()`
    // alone only catches the all-missing extreme; see
    // `MIN_BASELINE_FINAL_EPOCHS`'s own doc comment for why this is a
    // distinct minimum from `max_missing_epochs`.
    if liquidity_values.len() < MIN_BASELINE_FINAL_EPOCHS {
        return Err(Error::Tier1CheckFailed);
    }
    let median_liquidity = median(&liquidity_values);
    let min_liquidity = oracle
        .asset_config(asset)
        .map(|c| c.min_liquidity)
        .unwrap_or(0);
    if median_liquidity < min_liquidity {
        return Err(Error::Tier1CheckFailed);
    }

    Ok(window_start_epoch * EPOCH_SECS)
}

/// technical-doc.md Section 8.2, design note Section 3.
fn check_issuer_freeze(
    env: &Env,
    oracle: &RiskOracleClient,
    asset: &Address,
    def: &EventDefinition,
) -> Result<u64, Error> {
    let ring = oracle.ring(asset);
    let newest_epoch = oracle.newest_epoch(asset).ok_or(Error::Tier1CheckFailed)?;
    let now = env.ledger().timestamp();

    // Deliberately no `first_epoch`/history
    // requirement here, unlike `check_depeg`'s liquidity baseline.
    // Unlike that baseline, a sparse window here cannot produce a
    // FALSE trigger: `clawback_sum`/`revocation_sum` only accumulate
    // real data that exists, so a sparse window can only undercount
    // actions and make the trigger harder to reach, never easier.
    // "Enough history to rely on this asset" is a cover-sale concern
    // (MarketFactory refuses sales while score() reads stale), not a
    // payout-trigger concern; gating a real freeze on history only
    // hurts people who already bought cover. This guard is purely an
    // underflow guard on `window_start_epoch`'s own subtraction below
    // (never fires on a real network, where `newest_epoch` is always
    // far larger than `BASELINE_EPOCHS`; it only matters for a
    // brand-new asset in test, epoch numbers starting near 0). See
    // propose_tier1_issuer_freeze_succeeds_with_fewer_than_168_epochs_
    // of_history in test.rs.
    let window_epochs: u64 = BASELINE_EPOCHS;
    if newest_epoch + 1 < window_epochs {
        return Err(Error::Tier1CheckFailed);
    }
    let window_start_epoch = newest_epoch + 1 - window_epochs;

    let mut clawback_sum: i128 = 0;
    let mut revocation_sum: u32 = 0;
    let mut earliest_action_epoch: Option<u64> = None;
    let mut latest_supply: i128 = 0;
    for epoch in window_start_epoch..=newest_epoch {
        let slot = slot_for_epoch(&ring, newest_epoch, epoch);
        if let EpochDisposition::Final(_) = epoch_disposition(slot.as_ref(), epoch, now) {
            let slot = slot.unwrap();
            latest_supply = slot.supply;
            if slot.clawback_amount > 0 || slot.auth_revocations > 0 {
                clawback_sum = clawback_sum
                    .checked_add(slot.clawback_amount)
                    .ok_or(Error::MathOverflow)?;
                revocation_sum = revocation_sum
                    .checked_add(slot.auth_revocations)
                    .ok_or(Error::MathOverflow)?;
                if earliest_action_epoch.is_none() {
                    earliest_action_epoch = Some(epoch);
                }
            }
        }
    }

    let passes_clawback = if def.freeze_pct_bps > 0 && latest_supply > 0 {
        let scaled = clawback_sum
            .checked_mul(10_000)
            .ok_or(Error::MathOverflow)?;
        scaled / latest_supply >= def.freeze_pct_bps as i128
    } else {
        false
    };
    let passes_revocation =
        def.auth_revocation_threshold > 0 && revocation_sum > def.auth_revocation_threshold;
    if !passes_clawback && !passes_revocation {
        return Err(Error::Tier1CheckFailed);
    }

    let window_start = earliest_action_epoch.ok_or(Error::Tier1CheckFailed)? * EPOCH_SECS;
    Ok(window_start)
}

fn median(values: &Vec<i128>) -> i128 {
    let mut sorted: soroban_sdk::Vec<i128> = values.clone();
    // Simple insertion sort; `values` is bounded by 168 entries
    // (the 7 day baseline), so this is cheap in practice.
    let len = sorted.len();
    for i in 1..len {
        let key = sorted.get(i).unwrap();
        let mut j = i;
        while j > 0 && sorted.get(j - 1).unwrap() > key {
            let prev = sorted.get(j - 1).unwrap();
            sorted.set(j, prev);
            j -= 1;
        }
        sorted.set(j, key);
    }
    let mid = len / 2;
    if len.is_multiple_of(2) {
        (sorted.get(mid - 1).unwrap() + sorted.get(mid).unwrap()) / 2
    } else {
        sorted.get(mid).unwrap()
    }
}

/// design note Section 5. Returns the cure outcome for a Depeg event's
/// cure window, `[proposed_at, proposed_at + challenge_secs)` by close
/// time.
/// Scans the cure window and records the
/// result of every epoch whose disposition is currently Final or
/// PermanentlyMissing into `event_id`'s own persisted `CureProgress`,
/// merging with whatever was already recorded (recording is monotonic,
/// see `CureProgress`'s own doc comment). An epoch whose disposition
/// is NotReady is skipped, not stopped on: a later epoch in the window
/// disputed or still Pending must never block recording an EARLIER
/// epoch that is already decidable, because that earlier epoch is the
/// one at risk of rotating out of the ring first. Returns the merged
/// progress; never errors on "nothing new to record."
fn record_cure_progress(
    env: &Env,
    oracle: &RiskOracleClient,
    record: &EventRecord,
    def: &EventDefinition,
) -> storage::CureProgress {
    let ring = oracle.ring(&record.asset);
    let now = env.ledger().timestamp();
    let newest_epoch = oracle.newest_epoch(&record.asset).unwrap_or(0);

    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch_close = record.proposed_at + def.challenge_secs;
    let last_cure_epoch = last_cure_epoch_close / EPOCH_SECS;

    let mut progress = storage::get_cure_progress(env, record.id);
    let mut epoch = first_cure_epoch;
    let mut bit = 0u32;
    while epoch < last_cure_epoch {
        let mask = 1u128 << bit;
        if progress.recorded & mask == 0 {
            let slot = slot_for_epoch(&ring, newest_epoch, epoch);
            match epoch_disposition(slot.as_ref(), epoch, now) {
                EpochDisposition::NotReady => {}
                EpochDisposition::PermanentlyMissing => {
                    progress.recorded |= mask;
                    progress.any_missing = true;
                }
                EpochDisposition::Final(peg_ratio) => {
                    progress.recorded |= mask;
                    if peg_ratio < def.cure_threshold {
                        progress.any_below_threshold = true;
                    }
                }
            }
        }
        epoch += 1;
        bit += 1;
    }
    storage::set_cure_progress(env, record.id, &progress);
    progress
}

/// design note Section 5: the
/// cure window, `[proposed_at, proposed_at + challenge_secs)` by close
/// time, is checked against PERSISTED progress
/// (`record_cure_progress`), not a live re-scan alone, because the
/// oracle's own ring only holds `RING_SLOTS` epochs — a cure-window
/// epoch that becomes decidable while still inside the ring must be
/// recorded before it rotates out, or a late `finalize` call
/// misreads it as missing.
///
/// Decision order: an already-recorded failure (missing or below
/// threshold) declares immediately, even while other epochs are
/// still NotReady — one failure already rules out a cure, so there is
/// nothing to gain by waiting on the rest. Only once there is no
/// recorded failure does an unrecorded NotReady epoch block the
/// decision. Cured requires every cure-window epoch recorded and
/// clean.
fn cure_outcome(
    env: &Env,
    config: &Config,
    record: &EventRecord,
    def: &EventDefinition,
) -> Result<CureOutcome, Error> {
    let oracle = RiskOracleClient::new(env, &config.oracle);
    let progress = record_cure_progress(env, &oracle, record, def);

    if progress.any_missing || progress.any_below_threshold {
        return Ok(CureOutcome::Declared);
    }

    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + def.challenge_secs) / EPOCH_SECS;
    let cure_epochs = last_cure_epoch.saturating_sub(first_cure_epoch) as u32;
    // Finding F3: `validate_definition_params` guarantees at least one
    // cure epoch; this is a defensive second line, never reachable
    // for a definition registered through this build.
    if cure_epochs == 0 {
        return Ok(CureOutcome::Declared);
    }
    let all_recorded = if cure_epochs >= 128 {
        false
    } else {
        let full_mask = (1u128 << cure_epochs) - 1;
        progress.recorded & full_mask == full_mask
    };
    if all_recorded {
        Ok(CureOutcome::Cured)
    } else {
        Ok(CureOutcome::NotReady)
    }
}

/// technical-doc.md Section 23 defaults, used when the asset has no
/// registered definition of that kind (Section 9.4's own stated
/// fallback rule). Both windows are 72 epochs.
const DEFAULT_DEPEG_WINDOW_SECS: u64 = 72 * EPOCH_SECS;
const DEFAULT_DEPEG_THRESHOLD: i128 = 9_500_000;
const DEFAULT_HALT_WINDOW_SECS: u64 = 72 * EPOCH_SECS;

enum DepegCheck {
    Clear,
    Depeg,
    UnbuiltBacklog,
}

/// Section 5.9 S4/S5 (v1.5), R10: `RecentDepeg`'s own check across
/// `[newest_epoch - window_epochs, newest_epoch]`, exactly ONE source
/// per hour, and NEVER `HeldHour`: `HeldHour` is a write-path,
/// build/dispute-only concern, so the gate's own footprint never
/// depends on how many disputes are open, only on the fixed set of
/// keys below):
///
/// Each hour in the window is classified into exactly one of three
/// buckets (Section 5.9 S5):
///
/// - **No signal**: `slot_for_epoch` returns `None` (before the
///   asset's own history, or a stale-slot gap it cannot verify), or
///   the slot it returns is `Empty`. A built-Empty hour (below
///   `min_sub_coverage_bps`) is BUILT in the sense that matters here:
///   its own sub-epochs, if any ever posted, must never be read
///   again, the same guarantee a built Final hour already gets. This
///   can never misclassify a genuinely waiting hour, since posting
///   even one sub-epoch moves the hour's own slot to `Pending`, never
///   leaves it `Empty` (S4). Not counted toward the cap, and never
///   sent to the batched read.
/// - **Built** (`effective_state_of` reports the slot Final): reads
///   from the ring alone, at its own built, averaged `peg_ratio`,
///   exactly as before v1.5. A wick a built hour's own roll-up
///   already absorbed into its average can never be read back out at
///   sub-epoch granularity once that hour builds, keeping the gate's
///   own sensitivity fixed at every `sub_epoch_secs`.
/// - **Unbuilt**: everything else — a waiting hour on the sub-epoch
///   path (`Pending` or `Disputed`, `provisional_sub_coverage` set),
///   or a `Pending`/`Disputed` hour on the hourly fallback path.
///   Every UNBUILT hour in the window is batched into ONE
///   `RiskOracle.sub_peg_ratios_in_span_batch` call, touching exactly
///   one ledger key (`Sub(asset)`) regardless of how many hours are
///   requested. If more unbuilt hours sit in the window than
///   `MAX_UNBUILT_HOURS_SCANNED_BY_COVER_GATE` allows, this returns
///   `UnbuiltBacklog` without making that call at all: blocking new
///   cover is the safe failure, not guessing or risking the call
///   itself exceeding the network's own transaction memory limit.
///   Review finding: this check can NEVER actually trigger while
///   `register_definition` enforces `sylox_types::time::
///   MAX_DEPEG_WINDOW_EPOCHS` (no registered definition, nor the
///   default window, can ever produce more unbuilt hours than the
///   cap allows) — it stays here as a defensive guard that fails
///   closed if those two limits are ever changed out of step, not
///   dead code to remove. The build-time assertion tying
///   `MAX_UNBUILT_HOURS_SCANNED_BY_COVER_GATE` to `MAX_DEPEG_WINDOW_
///   EPOCHS + 1` (`sylox_types::time`) is what actually prevents that
///   drift; this runtime check is the second, defense-in-depth layer
///   underneath it.
/// - For each unbuilt hour the batch call returns real sub-epoch data
///   for (still inside `Sub(asset)`'s own 5 hour span), those
///   sub-epoch values are the check, read at sub-epoch granularity so
///   a depeg is visible within one `sub_epoch_secs` of starting, not
///   diluted into the hour's own provisional mean.
/// - For an unbuilt hour the batch call returns nothing for at all
///   (its sub-epochs have rotated out of that 5 hour span), its own
///   `Ring(asset)` provisional roll-up is the check instead, using
///   `provisional_sub_coverage` (never `peg_ratio` or `pending_until`)
///   to decide whether that roll-up holds real data:
///     - `Some(0)`: every posted sub-epoch is currently disputed, zero
///       real coverage; treated as no signal, same as a missing hour.
///     - `Some(n > 0)`: `n` non-disputed sub-epochs' real roll-up,
///       read REGARDLESS of `state` (even `Disputed`, since the
///       footprint-fix review's own roll-up fix means a `Disputed`
///       sub-epoch-path slot's `peg_ratio` already excludes the
///       disputed sub-epoch itself, not the whole hour).
///     - `None`: this hour was posted (if at all) through the hourly
///       fallback path, so `peg_ratio` IS the one real, possibly
///       contested reading itself; today's rule applies unchanged
///       (`state != Empty && state != Disputed`), since a `Disputed`
///       hourly-fallback slot's own fields are the contested value,
///       not a roll-up that already excludes it.
fn depeg_check(
    env: &Env,
    oracle: &RiskOracleClient,
    asset: &Address,
    ring: &Vec<sylox_types::RingSlot>,
    newest_epoch: u64,
    window_epochs: u32,
    threshold: i128,
) -> DepegCheck {
    let start = newest_epoch.saturating_sub(window_epochs as u64);
    let now = env.ledger().timestamp();

    let mut unbuilt_hours: Vec<u64> = Vec::new(env);
    let mut epoch = start;
    while epoch <= newest_epoch {
        let slot = slot_for_epoch(ring, newest_epoch, epoch);
        match &slot {
            // Before the asset's own history, or a gap `slot_for_
            // epoch` could not verify: never posted at all, nothing
            // to read. Not an unbuilt hour: there is no sub-epoch
            // data to batch, so it must never occupy a slot in the
            // batched read or count toward the cap.
            None => {}
            Some(s) if s.state == SlotState::Empty => {
                // Built Empty (below min_sub_coverage_bps, or the
                // hourly fallback's own missing-epoch write) or
                // genuinely never posted: either way, BUILT in the
                // sense that matters here — its own sub-epochs (if
                // any ever existed) must never be read again, the
                // same rule a built Final hour already gets. An
                // unbuilt hour with any real posts is never Empty
                // (posting makes it Pending, S4), so this can never
                // misclassify a genuinely waiting hour as built.
            }
            Some(s) => {
                let is_built = effective_state_of(s, now) == SlotState::Final;
                if is_built {
                    if s.peg_ratio < threshold {
                        return DepegCheck::Depeg;
                    }
                } else {
                    unbuilt_hours.push_back(epoch);
                }
            }
        }
        epoch += 1;
    }

    if unbuilt_hours.len() > sylox_types::time::MAX_UNBUILT_HOURS_SCANNED_BY_COVER_GATE {
        return DepegCheck::UnbuiltBacklog;
    }
    if unbuilt_hours.is_empty() {
        return DepegCheck::Clear;
    }

    let batch = oracle.sub_peg_ratios_in_span_batch(asset, &unbuilt_hours);
    for (i, hour) in unbuilt_hours.iter().enumerate() {
        let ratios = batch.get(i as u32).unwrap_or(Vec::new(env));
        let any_sub_epoch_data = ratios.iter().flatten().count() > 0;
        if any_sub_epoch_data {
            if ratios.iter().flatten().any(|p| p < threshold) {
                return DepegCheck::Depeg;
            }
        } else if let Some(s) = slot_for_epoch(ring, newest_epoch, hour) {
            let is_depeg = match s.provisional_sub_coverage {
                Some(0) => false,
                Some(_) => s.peg_ratio < threshold,
                None => {
                    s.state != SlotState::Empty
                        && s.state != SlotState::Disputed
                        && s.peg_ratio < threshold
                }
            };
            if is_depeg {
                return DepegCheck::Depeg;
            }
        }
    }
    DepegCheck::Clear
}

fn any_epoch_endpoint_down(
    ring: &Vec<sylox_types::RingSlot>,
    newest_epoch: u64,
    window_epochs: u32,
) -> bool {
    let start = newest_epoch.saturating_sub(window_epochs as u64);
    let mut epoch = start;
    while epoch <= newest_epoch {
        if let Some(slot) = slot_for_epoch(ring, newest_epoch, epoch) {
            if matches!(
                slot.endpoint,
                sylox_types::EndpointStatus::Down | sylox_types::EndpointStatus::Degraded
            ) {
                return true;
            }
        }
        epoch += 1;
    }
    false
}

fn any_epoch_with_issuer_action(
    ring: &Vec<sylox_types::RingSlot>,
    newest_epoch: u64,
    window_epochs: u32,
) -> bool {
    let start = newest_epoch.saturating_sub(window_epochs as u64);
    let mut epoch = start;
    while epoch <= newest_epoch {
        if let Some(slot) = slot_for_epoch(ring, newest_epoch, epoch) {
            if slot.clawback_amount > 0 || slot.auth_revocations > 0 {
                return true;
            }
        }
        epoch += 1;
    }
    false
}

#[cfg(test)]
mod budget_test;
#[cfg(test)]
mod test;
