//! Storage keys for EventRegistry. technical-doc.md Section 15.1.

use soroban_sdk::{contracttype, Address, Env};
use sylox_types::{AssetEventStatus, EventDefinition, EventKind, EventRecord};

#[contracttype]
pub struct Config {
    pub governor: Address,
    pub oracle: Address,
    pub staking: Address,
    /// `MarketFactory` is not built; stored for the day it is
    /// (`register_definition`'s own `DefinitionInUse` check, and
    /// `propose_tier1`'s `version_has_live_cover`, design note
    /// Section 2, would both call into it), never read in this build.
    pub factory: Address,
    pub usdc: Address,
}

#[contracttype]
pub enum DataKey {
    Config,
    /// Monotonic event id counter; the next id to assign.
    NextEventId,
    /// The current canonical version for (asset, kind), 0 if none
    /// registered yet.
    Canonical(Address, EventKind),
    /// `EventDefinition`, stored by (asset, kind, version). Never
    /// edited once written (ADR-001).
    Def(Address, EventKind, u32),
    /// `EventRecord`, by id.
    Event(u64),
    /// Status of (asset, kind) for its CANONICAL version only
    /// (ADR-001); a non-canonical version's own live event is tracked
    /// separately, by `LiveEvent`, below.
    Status(Address, EventKind),
    /// The live (non-terminal) event id for (asset, kind, version), if
    /// any (invariant E3). Cleared when the event leaves Proposed,
    /// Escalated, Cured or Rejected into a state a new proposal for
    /// the same key may follow (Declared is terminal and never
    /// cleared; Cured/Rejected clear immediately, since re-proposal
    /// is gated by data freshness, not by this key staying occupied,
    /// design note Section 2a).
    LiveEvent(Address, EventKind, u32),
    /// Count of events (any kind, canonical version only, design note
    /// Section 2 review item D3b) currently Proposed, Challenged or
    /// Escalated for this asset. Drives the push to
    /// `RiskOracle.set_event_in_progress` (design note Section 6).
    ActiveCount(Address),
    /// Ruling deadlines a committee address let pass without a ruling
    /// (ADR-002, mirroring `RiskOracle.CommitteeMisses`).
    CommitteeMisses(Address),
    /// The time (asset, kind, version)'s event last left Proposed or
    /// Escalated into Cured or Rejected, for the next proposal's own
    /// `window_start > left_at` test (design note Section 2a). Not
    /// stored at all once a proposal with a fresh `window_start`
    /// supersedes it (cleared alongside `LiveEvent` above), since a
    /// stale `left_at` with no live event in the way is otherwise
    /// indistinguishable from "no prior event ever existed here,"
    /// which is exactly the state a fresh key with no `left_at` at all
    /// should read as.
    LeftAt(Address, EventKind, u32),
    /// PR #15 review, finding F2: a Depeg event's own cure-window
    /// progress recorded so far, by event id. `RiskOracle`'s ring
    /// only holds `RING_SLOTS` epochs; a cure-window epoch that
    /// becomes Final or permanently missing while still inside the
    /// ring must be recorded before it rotates out, or its result is
    /// lost and the epoch reads as missing forever after — see
    /// `CureProgress`'s own doc comment.
    CureProgress(u64),
}

/// PR #15 review, finding F2. Bit `i` of `recorded` is set once the
/// cure-window epoch `first_cure_epoch + i` has been observed Final or
/// PermanentlyMissing at least once; `any_below_threshold`/
/// `any_missing` are the OR of every such observation so far. A `u128`
/// bitmap comfortably covers the up-to-72 cure epochs `challenge_secs`
/// can specify (finding F3's own bound), with headroom to spare.
/// Deliberately never clears a bit once set: a disposition, once
/// observed Final or PermanentlyMissing, cannot change (an epoch that
/// is genuinely Final never reverts to Pending or Disputed, and
/// PermanentlyMissing is a statement about elapsed time, which never
/// un-elapses), so recording is monotonic and `checkpoint_cure` is
/// naturally idempotent.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CureProgress {
    pub recorded: u128,
    pub any_below_threshold: bool,
    pub any_missing: bool,
}

pub fn get_cure_progress(env: &Env, event_id: u64) -> CureProgress {
    env.storage()
        .persistent()
        .get(&DataKey::CureProgress(event_id))
        .unwrap_or(CureProgress {
            recorded: 0,
            any_below_threshold: false,
            any_missing: false,
        })
}

pub fn set_cure_progress(env: &Env, event_id: u64, progress: &CureProgress) {
    sylox_types::ttl::set_persistent(env, &DataKey::CureProgress(event_id), progress);
}

pub fn get_config(env: &Env) -> Option<Config> {
    env.storage().instance().get(&DataKey::Config)
}

pub fn set_config(env: &Env, config: &Config) {
    env.storage().instance().set(&DataKey::Config, config);
}

pub fn next_event_id(env: &Env) -> u64 {
    let id: u64 = env
        .storage()
        .instance()
        .get(&DataKey::NextEventId)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::NextEventId, &(id + 1));
    id
}

pub fn get_canonical(env: &Env, asset: &Address, kind: EventKind) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::Canonical(asset.clone(), kind))
        .unwrap_or(0)
}

pub fn set_canonical(env: &Env, asset: &Address, kind: EventKind, version: u32) {
    sylox_types::ttl::set_persistent(env, &DataKey::Canonical(asset.clone(), kind), &version);
}

pub fn get_def(
    env: &Env,
    asset: &Address,
    kind: EventKind,
    version: u32,
) -> Option<EventDefinition> {
    env.storage()
        .persistent()
        .get(&DataKey::Def(asset.clone(), kind, version))
}

pub fn set_def(env: &Env, def: &EventDefinition) {
    sylox_types::ttl::set_persistent(
        env,
        &DataKey::Def(def.asset.clone(), def.kind, def.version),
        def,
    );
}

pub fn get_event(env: &Env, id: u64) -> Option<EventRecord> {
    env.storage().persistent().get(&DataKey::Event(id))
}

pub fn set_event(env: &Env, record: &EventRecord) {
    sylox_types::ttl::set_persistent(env, &DataKey::Event(record.id), record);
}

pub fn get_status(env: &Env, asset: &Address, kind: EventKind) -> AssetEventStatus {
    env.storage()
        .persistent()
        .get(&DataKey::Status(asset.clone(), kind))
        .unwrap_or(AssetEventStatus::None)
}

pub fn set_status(env: &Env, asset: &Address, kind: EventKind, status: &AssetEventStatus) {
    sylox_types::ttl::set_persistent(env, &DataKey::Status(asset.clone(), kind), status);
}

pub fn get_live_event(env: &Env, asset: &Address, kind: EventKind, version: u32) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::LiveEvent(asset.clone(), kind, version))
}

pub fn set_live_event(env: &Env, asset: &Address, kind: EventKind, version: u32, id: u64) {
    sylox_types::ttl::set_persistent(env, &DataKey::LiveEvent(asset.clone(), kind, version), &id);
}

pub fn clear_live_event(env: &Env, asset: &Address, kind: EventKind, version: u32) {
    env.storage()
        .persistent()
        .remove(&DataKey::LiveEvent(asset.clone(), kind, version));
}

pub fn get_active_count(env: &Env, asset: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::ActiveCount(asset.clone()))
        .unwrap_or(0)
}

pub fn set_active_count(env: &Env, asset: &Address, count: u32) {
    sylox_types::ttl::set_persistent(env, &DataKey::ActiveCount(asset.clone()), &count);
}

pub fn get_committee_misses(env: &Env, committee: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::CommitteeMisses(committee.clone()))
        .unwrap_or(0)
}

pub fn increment_committee_misses(env: &Env, committee: &Address) -> u32 {
    let next = get_committee_misses(env, committee) + 1;
    sylox_types::ttl::set_persistent(env, &DataKey::CommitteeMisses(committee.clone()), &next);
    next
}

pub fn get_left_at(env: &Env, asset: &Address, kind: EventKind, version: u32) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::LeftAt(asset.clone(), kind, version))
}

pub fn set_left_at(env: &Env, asset: &Address, kind: EventKind, version: u32, at: u64) {
    sylox_types::ttl::set_persistent(env, &DataKey::LeftAt(asset.clone(), kind, version), &at);
}

pub fn clear_left_at(env: &Env, asset: &Address, kind: EventKind, version: u32) {
    env.storage()
        .persistent()
        .remove(&DataKey::LeftAt(asset.clone(), kind, version));
}
