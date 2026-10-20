//! Storage keys and the per asset ring buffer. technical-doc.md Section 5.8,
//! 15.1.
//!
//! The ring is stored as one packed `Bytes` blob, not a `Vec<RingSlot>`.
//! A `Vec` of `#[contracttype]` structs encodes each `RingSlot` as an XDR
//! map of named fields, which measured about 440 bytes per slot (105,732
//! bytes for 240 slots), 61% over the current `contract_data_entry_size_bytes`
//! limit of 65,536 (see the PR's Phase 1 report). Packing each slot into
//! fixed width fields with no field names gets one slot down to
//! `SLOT_BYTES` (112, padded from 106 used bytes), 26,880 bytes of slot
//! data for 240 slots (plus the header below), comfortably under the
//! limit. This is the "Storing each slot as a contracttype map with field
//! names would be several times larger" note in Section 5.8, made
//! concrete.
//!
//! `epoch % RING_SLOTS` gives a slot's POSITION, never its identity. A
//! position can hold a slot from any epoch congruent to it mod
//! `RING_SLOTS`, or nothing at all (never written, or overturned and not
//! yet reposted). Every read that wants epoch `e`'s slot must check that
//! the slot stored at `e % RING_SLOTS` actually has `epoch == e`; if it
//! does not (wrong epoch or state `Empty`), epoch `e` is MISSING, counted
//! by `max_missing_epochs` (ADR-005), never read as if it were present.
//! `get_slot` and `get_window` below are the only ways Phase 2 code should
//! read a slot; neither returns a slot whose stored epoch does not match
//! the one asked for.

use soroban_sdk::{contracttype, Address, Bytes, BytesN, Env, Vec};
use sylox_types::{AssetConfig, EndpointStatus, RingSlot, RiskScore, SignalSet, SlotState};

use crate::score::Formula;

/// Number of slots in the per asset ring buffer: the longest v1 depeg
/// window (72h) plus the 7 day liquidity baseline before it, at the
/// default `epoch_secs` of 1 hour. technical-doc.md Section 5.8.
///
/// Frozen for v1, not a governance parameter: changing it would require
/// re-encoding every asset's existing `Ring(asset)` entry (see the header
/// layout version below), which is a migration, not a parameter change.
/// See the PR's "Spec deviations" section. Moved to `sylox_types::time`
/// (Section 5.9 S7) so `event-registry` imports the same constant
/// instead of keeping its own private copy; re-exported here under its
/// established local name so every existing call site in this module
/// is unchanged.
pub use sylox_types::time::RING_SLOTS;

/// Packed width of one `RingSlot`, in bytes:
/// epoch (8) + state (1) + pending_until (8) + peg_ratio (16) +
/// liquidity_2pct (16) + redemption_net (16) + supply (16) +
/// supply_change_bps (4) + clawback_amount (16) + auth_revocations (4) +
/// endpoint (1) = 106. Rounded up to 112 (multiple of 16) so every field
/// of every slot starts at an offset that is cheap to compute; the 6 bytes
/// of padding per slot cost 1,440 bytes across the whole buffer, far less
/// than the gap to the next size class.
///
/// Frozen for v1 alongside `RING_SLOTS`: both are baked into the header
/// below, and a mismatch is rejected rather than silently reinterpreted.
pub const SLOT_BYTES: u32 = 112;

/// Packed ring layout version. Bump this and handle the old layout
/// explicitly (migrate or reject) before ever changing `RING_SLOTS` or
/// `SLOT_BYTES`, or the byte offsets below.
///
/// Bumped to 2 for review item C6 (re-review): byte 106, previously
/// unused padding, now holds `final_announced` (see `write_slot`'s
/// doc comment). `SLOT_BYTES` itself is unchanged (112; byte 106 was
/// already inside the padded range). No contract using version 1 has
/// ever been deployed, so this is a version bump with no migration
/// code, not a live format change; documented here so that changes if
/// that ever stops being true.
const LAYOUT_VERSION: u8 = 2;

/// Header: `[0] layout version, [1..5) RING_SLOTS, [5..9) SLOT_BYTES`,
/// little endian. `HEADER_BYTES` of overhead ahead of the packed slots so
/// a future reader can detect and refuse an incompatible buffer instead of
/// misreading it as slot data.
const HEADER_BYTES: u32 = 9;

#[contracttype]
pub enum DataKey {
    Asset(Address),
    Signals(Address, u64),
    Ring(Address),
    /// Newest epoch successfully written to `Ring(asset)`, so `get_ring`
    /// can report "oldest first" without scanning for it.
    RingNewest(Address),
    /// Newest epoch known to be EFFECTIVELY final (Section 5.7, review
    /// item C5), advanced one step at a time by `lib.rs`'s
    /// `try_advance_finality`. `None` until the asset's first epoch
    /// becomes final.
    NewestFinal(Address),
    Score(Address),
    /// Review decision D1: set by `EventRegistry` via
    /// `set_event_in_progress`. While `true`, `recompute_score` forces
    /// the band to at least `Distress` (Section 6.3). `RiskOracle` never
    /// sets or clears this itself and never calls into `EventRegistry`
    /// to check it proactively; it is purely a flag `EventRegistry`
    /// pushes.
    EventInProgress(Address),
    /// `(asset, down_streak)` hysteresis counter alongside `Score(asset)`,
    /// kept separate so bumping it on every epoch that fails to move the
    /// band down does not require rewriting the whole `RiskScore`.
    DownStreak(Address),
    /// Dispute record only; the bond itself lives in `Staking` under
    /// `BondKey::SignalDispute` (Section 4.5, 7.8).
    Dispute(Address, u64),
    /// ADR-010: committee misses on signal dispute ruling deadlines.
    CommitteeMisses(Address),
    /// Review item C4: history for an overturned epoch's `SignalSet`.
    /// `resolve_signal_dispute` moves it here from `Signals(asset, epoch)`
    /// on an overturn, so `get_signals` no longer finds it and
    /// `post_signals` can accept a fresh posting for the same epoch
    /// (ADR-005: "an overturned epoch reopens for reposting"), while the
    /// overturned data stays available for audit.
    Overturned(Address, u64),
    /// Re-review item C7's `asset_stale` redesign: tracks whether the
    /// asset is currently announced stale, so `check_stale` emits
    /// `asset_stale` only on the transition into stale (flag false to
    /// true), never on every call that happens to observe an already
    /// announced stale state, and clears when a fresh, non-stale
    /// epoch is scored.
    StaleAnnounced(Address),
    /// PR #25 review: the first epoch ever successfully posted for
    /// this asset (a global, unix-time-derived epoch number, not a
    /// per-asset counter starting at 0). Set once, on the asset's
    /// first successful `post_signals` call, and never moved
    /// afterward, even if that epoch is later overturned: the point
    /// is "when did real history for this asset begin," not "what is
    /// the oldest epoch still present." Every "N epochs of history"
    /// check (the 7 day score baseline, `median_liquidity`, the Tier
    /// 1 Depeg/IssuerFreeze baselines in `EventRegistry`) must measure
    /// against this, not against the newest epoch's own absolute
    /// number, which is always large and unrelated to how long this
    /// particular asset has actually been posting.
    FirstEpoch(Address),
    Assets,
    Formula,
    /// Section 5.9 S1: this asset's current and pending `sub_epoch_secs`.
    SubEpochConfig(Address),
    /// Section 5.9 S2: the full `SignalSet` for one sub-epoch,
    /// addressed by the `(hour, sub)` pair a keeper posted it under
    /// (the posting API's own addressing). Mirrors `Signals(asset,
    /// epoch)`'s own role for the hourly path: `Sub(asset)`'s packed
    /// ring below carries only the fields needed for cheap aggregate
    /// reads (the same subset `RingSlot` already carries for the
    /// hourly ring), never `inputs_hash` or `poster`, which have no
    /// fixed-width packed encoding; this key is where those live.
    SubSignals(Address, u64, u32),
    /// Mirrors `Overturned(Address, u64)`'s own role for the sub-epoch
    /// path (ADR-005, extended to sub-epochs by the Section 5.9 S5
    /// footprint-fix review): history for an overturned sub-epoch's
    /// `SignalSet`, moved here from `SubSignals(asset, hour, sub)` by
    /// `resolve_sub_signal_dispute`'s rejection branch, so
    /// `get_sub_signals` no longer finds it and `post_sub_signals` can
    /// accept a fresh posting for the same `(hour, sub)`, while the
    /// overturned data stays available for audit.
    SubOverturned(Address, u64, u32),
    /// Section 5.9 S2 (v1.5, footprint-fix revision): set the moment
    /// `post_sub_signals` accepts a REPOST for this `(hour, sub)`
    /// (never on an ordinary first post). Checked by `overturn_sub_
    /// signals` if THIS posting is later overturned too: the one
    /// repost per original overturn rule means that second overturn
    /// must start its own `SubOverturnedRecord` already `reposted:
    /// true`, not `false`, so a third posting attempt is refused.
    /// Removed by `overturn_sub_signals` once consumed, so a FUTURE
    /// original post (after this sub-epoch's entire two-cycle history
    /// has played out and the hour has moved on) never misreads a
    /// stale flag from a previous, unrelated hour reusing this key
    /// (sub-epoch keys are never reused across different `(hour,
    /// sub)` pairs in practice, but this still keeps the key's own
    /// lifetime bounded to the one repost it describes).
    SubRepostUsed(Address, u64, u32),
    /// Section 5.9 S3: the packed sub-epoch ring, fixed at
    /// `SUB_RING_SLOTS` slots, same encoding as `Ring(asset)` but
    /// keyed by each slot's own `sub_start` identity, not `epoch`.
    Sub(Address),
    /// Newest `sub_start` successfully written to `Sub(asset)`, the
    /// sub-epoch analog of `RingNewest`.
    SubRingNewest(Address),
    /// Section 5.9 S3, S4: a disputed sub-epoch's record, copied out
    /// of `Sub(asset)` the moment it is disputed, addressed by the
    /// `(hour, sub)` pair a keeper posted it under (the posting API's
    /// own addressing, Section 5.9 S2), mirroring `Overturned`'s own
    /// move-out-of-the-ring pattern.
    SubDispute(Address, u64, u32),
    /// Section 5.9 S3: `hour`'s own sub-epoch data, copied out of
    /// `Sub(asset)` once any of its sub-epochs is disputed, so a
    /// ruling that arrives after the ring has rotated past the hour's
    /// own slots still has every OTHER sub-epoch's data to roll up
    /// from, not just the disputed one's. Cleared once the hour
    /// builds.
    HeldHour(Address, u64),
    /// Section 5.9 S2: marks which path (sub-epoch or hourly fallback)
    /// has already posted for this hour, so the other path's own
    /// `HourAlreadyPosted` guard has something to check against even
    /// after `Sub(asset)`'s own 60 slot ring has rotated the hour's
    /// sub-epochs out.
    HourPostedVia(Address, u64),
    /// Section 5.9 S1: the `sub_epoch_secs` an hour's sub-epochs were
    /// posted at, recorded by its first sub-epoch post. A new key rather
    /// than a field on an existing entry, so every entry the live
    /// contract already holds still decodes after an upgrade.
    HourSubEpochSecs(Address, u64),
}

/// Section 5.9 S2: which path posted an hour, for the one-writer-per-hour
/// rule. Persists past `Sub(asset)`'s own 60 slot rotation window, so
/// the guard still holds even once an hour's sub-epochs are long gone
/// from the sub-epoch ring.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HourPostedVia {
    SubEpoch,
    HourlyFallback,
}

#[contracttype]
#[derive(Clone, Eq, PartialEq)]
pub struct DisputeRecord {
    pub disputer: Address,
    pub alt_hash: BytesN<32>,
    /// Ledger timestamp `dispute_signals` opened this dispute at.
    /// `resolve_signal_dispute_timeout` (ADR-010) uses this, not
    /// `pending_until`, as the ruling deadline's start: the deadline
    /// is about how long the COMMITTEE may take to rule once a dispute
    /// exists, a separate clock from the original posting's own
    /// dispute window.
    pub opened_at: u64,
}

/// ADR-010: counts, per committee address, how many signal disputes
/// that committee let run out its ruling deadline without a ruling.
/// Grounds for rotating the committee through governance, the same
/// role `CommitteeMisses` plays for event ruling timeouts (ADR-002).
pub fn get_committee_misses(env: &Env, committee: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::CommitteeMisses(committee.clone()))
        .unwrap_or(0)
}

pub fn record_committee_miss(env: &Env, committee: &Address) {
    let count = get_committee_misses(env, committee) + 1;
    sylox_types::ttl::set_persistent(env, &DataKey::CommitteeMisses(committee.clone()), &count);
}

pub fn get_asset_config(env: &Env, asset: &Address) -> Option<AssetConfig> {
    env.storage()
        .persistent()
        .get(&DataKey::Asset(asset.clone()))
}

pub fn set_asset_config(env: &Env, asset: &Address, cfg: &AssetConfig) {
    sylox_types::ttl::set_persistent(env, &DataKey::Asset(asset.clone()), cfg);
}

pub fn get_signals(env: &Env, asset: &Address, epoch: u64) -> Option<SignalSet> {
    env.storage()
        .persistent()
        .get(&DataKey::Signals(asset.clone(), epoch))
}

pub fn set_signals(env: &Env, asset: &Address, epoch: u64, signals: &SignalSet) {
    sylox_types::ttl::set_history(env, &DataKey::Signals(asset.clone(), epoch), signals);
}

/// Review item C4: moves `Signals(asset, epoch)` to the `Overturned`
/// history key and removes the live entry, so `get_signals` reports it
/// missing (unblocking a repost) while the overturned data stays
/// retrievable via `get_overturned_signals`. A no-op if there was no
/// `Signals` entry for this epoch (defensive; `resolve_signal_dispute`'s
/// caller already checked one exists before calling this).
pub fn overturn_signals(env: &Env, asset: &Address, epoch: u64) {
    let key = DataKey::Signals(asset.clone(), epoch);
    if let Some(signals) = env.storage().persistent().get::<_, SignalSet>(&key) {
        sylox_types::ttl::set_history(env, &DataKey::Overturned(asset.clone(), epoch), &signals);
        env.storage().persistent().remove(&key);
    }
}

/// The history an overturned epoch's `SignalSet` was moved to by
/// `overturn_signals`. `None` if that epoch was never overturned (or was
/// reposted and overturned again, which overwrites this entry with the
/// newer attempt's data, keeping only the most recent overturn on
/// record, not a full history of every attempt).
pub fn get_overturned_signals(env: &Env, asset: &Address, epoch: u64) -> Option<SignalSet> {
    env.storage()
        .persistent()
        .get(&DataKey::Overturned(asset.clone(), epoch))
}

/// Returns epoch `epoch`'s slot, or `None` if that epoch is missing: never
/// written, overturned and not yet reposted, or the position has since
/// been overwritten by a different epoch (the ring wrapped around it).
/// Decodes exactly one slot's bytes out of the buffer, not the whole
/// buffer, per the PR's "decode only the slots you need" requirement.
///
/// This is the one place slot identity is checked: every other read in
/// this module (and every Phase 2 caller) goes through this function
/// instead of indexing the packed buffer by position directly.
pub fn get_slot(env: &Env, asset: &Address, epoch: u64) -> Option<RingSlot> {
    let packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let slot = read_slot(&packed, index);
    if slot.state != SlotState::Empty && slot.epoch == epoch {
        Some(slot)
    } else {
        None
    }
}

/// Returns the slots for `[start_epoch, start_epoch + count)`, in epoch
/// order, missing epochs included as `None`. Decodes only `count` slots.
/// `count` must be at most `RING_SLOTS`; a caller asking for a window
/// wider than the buffer would silently alias the same position to two
/// different requested epochs, which is a caller bug, not a storage
/// concern, so this panics rather than returning wrong data.
pub fn get_window(
    env: &Env,
    asset: &Address,
    start_epoch: u64,
    count: u32,
) -> Vec<Option<RingSlot>> {
    assert!(
        count <= RING_SLOTS,
        "window of {count} exceeds RING_SLOTS ({RING_SLOTS}); every position would be read more than once"
    );
    let packed = get_ring_packed(env, asset);
    let mut out = Vec::new(env);
    for i in 0..count {
        let epoch = start_epoch + i as u64;
        let slot = read_slot(&packed, position_of(epoch));
        if slot.state != SlotState::Empty && slot.epoch == epoch {
            out.push_back(Some(slot));
        } else {
            out.push_back(None);
        }
    }
    out
}

/// Review item C5: a slot is EFFECTIVELY `Final` once `now >=
/// pending_until`, even while its stored `state` still reads `Pending`
/// (nobody has made a state changing call to flip it since). This is the
/// pure function every "is this epoch final" decision in the contract
/// goes through; it never writes.
pub fn effective_state(slot: &RingSlot, now: u64) -> SlotState {
    if slot.state == SlotState::Pending && now >= slot.pending_until {
        SlotState::Final
    } else {
        slot.state
    }
}

/// Review item C5: `is_final(asset, epoch)`, exposed as a `RiskOracle`
/// read (see `lib.rs`). `false` for a missing epoch: there is nothing to
/// be final.
pub fn is_final(env: &Env, asset: &Address, epoch: u64, now: u64) -> bool {
    match get_slot(env, asset, epoch) {
        Some(slot) => effective_state(&slot, now) == SlotState::Final,
        None => false,
    }
}

/// Review item C5: a window read of EFFECTIVE state per epoch, for
/// `EventRegistry`'s Tier 1 checks (Section 8.2), which need to know
/// which epochs in a window are final without each one costing a
/// separate call. Missing epochs read `None`, the same convention as
/// `get_window`.
pub fn get_effective_window(
    env: &Env,
    asset: &Address,
    start_epoch: u64,
    count: u32,
    now: u64,
) -> Vec<Option<SlotState>> {
    let slots = get_window(env, asset, start_epoch, count);
    let mut out = Vec::new(env);
    for slot in slots.iter() {
        out.push_back(slot.map(|s| effective_state(&s, now)));
    }
    out
}

/// technical-doc.md Section 12.1: `ring(asset) -> Vec<RingSlot>`, "oldest
/// first, one storage read". Decodes every position (this is the one
/// public read that legitimately wants the whole buffer), but still
/// verifies each slot's stored epoch against the epoch that position is
/// currently expected to hold, so a caller of this function never gets a
/// stale or foreign slot either. "Oldest first" means positions
/// `[newest_position + 1 .. RING_SLOTS)` then `[0 ..= newest_position]`,
/// i.e. chronological order starting just after the most recently written
/// slot; an asset that has never posted returns `RING_SLOTS` `Empty`
/// slots in position order (there is no "oldest" yet).
pub fn get_ring(env: &Env, asset: &Address) -> Vec<RingSlot> {
    let packed = get_ring_packed(env, asset);
    let newest = get_newest_epoch(env, asset);

    let mut ring = Vec::new(env);
    match newest {
        Some(newest_epoch) => {
            let newest_index = position_of(newest_epoch);
            for step in 1..=RING_SLOTS {
                let index = (newest_index + step) % RING_SLOTS;
                ring.push_back(verified_slot_at(&packed, index));
            }
        }
        None => {
            for index in 0..RING_SLOTS {
                ring.push_back(verified_slot_at(&packed, index));
            }
        }
    }
    ring
}

/// Reads the slot physically stored at `index` and returns it only if its
/// stored epoch actually maps back to `index` (`position_of(slot.epoch) ==
/// index`); otherwise returns an `Empty` slot. Guards against a corrupted
/// or stale read being mistaken for real data at a position whose true
/// occupant does not match.
fn verified_slot_at(packed: &Bytes, index: u32) -> RingSlot {
    let slot = read_slot(packed, index);
    if slot.state != SlotState::Empty && position_of(slot.epoch) == index {
        slot
    } else {
        empty_slot()
    }
}

/// Tracks the newest epoch written so far, so `get_ring` can report
/// "oldest first" without scanning the buffer for it. Kept as its own
/// entry alongside the packed buffer rather than inside it, so a header
/// version bump never has to account for it.
fn get_newest_epoch(env: &Env, asset: &Address) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::RingNewest(asset.clone()))
}

/// Public form of `get_newest_epoch`, for `lib.rs` (`latest`, `score`,
/// `is_stale`, `median_liquidity` all need the newest posted epoch).
pub fn get_newest_epoch_pub(env: &Env, asset: &Address) -> Option<u64> {
    get_newest_epoch(env, asset)
}

/// PR #25 review: the first epoch ever successfully posted for this
/// asset, `None` before its first `post_signals` call ever succeeds.
pub fn get_first_epoch(env: &Env, asset: &Address) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::FirstEpoch(asset.clone()))
}

/// Sets `FirstEpoch(asset)` if and only if it is not already set.
/// Called from `post_signals` after a genuinely new epoch is recorded;
/// idempotent on every call after the first.
pub fn set_first_epoch_if_unset(env: &Env, asset: &Address, epoch: u64) {
    if get_first_epoch(env, asset).is_none() {
        sylox_types::ttl::set_persistent(env, &DataKey::FirstEpoch(asset.clone()), &epoch);
    }
}

/// Review item C1/C5: the newest epoch known to be effectively final.
/// `score()` reads this (and only this; it is a pure read, never a
/// scan) to know which epoch's window to have already scored.
pub fn get_newest_final(env: &Env, asset: &Address) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::NewestFinal(asset.clone()))
}

pub fn set_newest_final(env: &Env, asset: &Address, epoch: u64) {
    sylox_types::ttl::set_persistent(env, &DataKey::NewestFinal(asset.clone()), &epoch);
}

/// Writes `signals` for `epoch` into its ring slot at `position_of(epoch)`
/// and persists the whole packed buffer in one storage write.
/// technical-doc.md Section 5.3 step 4, 5.8.
///
/// Refuses to overwrite a position that currently holds a strictly newer
/// epoch than `epoch`: the ring has wrapped exactly once since `epoch`'s
/// position was last written by something newer, which can only happen if
/// `epoch` is being posted far too late (`post_signals`'s window check in
/// `lib.rs` should have already rejected it) or out of order. Returns
/// `false` in that case instead of writing, so the caller can turn it into
/// the right error rather than silently corrupting a newer slot.
pub fn write_ring_slot(
    env: &Env,
    asset: &Address,
    epoch: u64,
    signals: &SignalSet,
    pending_until: u64,
    provisional_sub_coverage: Option<u32>,
) -> bool {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let existing = read_slot(&packed, index);
    if existing.state != SlotState::Empty && existing.epoch > epoch {
        return false;
    }

    let slot = RingSlot {
        epoch,
        state: SlotState::Pending,
        pending_until,
        peg_ratio: signals.peg_ratio,
        liquidity_2pct: signals.liquidity_2pct,
        redemption_net: signals.redemption_net,
        supply: signals.supply,
        supply_change_bps: signals.supply_change_bps,
        clawback_amount: signals.issuer_actions.clawback_amount,
        auth_revocations: signals.issuer_actions.auth_revocations,
        endpoint: signals.endpoint,
        provisional_sub_coverage,
    };
    // A fresh post always starts unannounced: either this epoch has
    // never been posted before, or it is a repost after an overturn
    // (review item C4), which the re-review's C6 instructions say must
    // also start unannounced rather than inheriting whatever the
    // overturned attempt's flag happened to be.
    write_slot(&mut packed, index, &slot, false);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);

    let newest = get_newest_epoch(env, asset);
    if newest.is_none_or(|n| epoch > n) {
        sylox_types::ttl::set_persistent(env, &DataKey::RingNewest(asset.clone()), &epoch);
    }
    true
}

// -- score --

pub fn get_score(env: &Env, asset: &Address) -> Option<RiskScore> {
    env.storage()
        .persistent()
        .get(&DataKey::Score(asset.clone()))
}

pub fn set_score(env: &Env, asset: &Address, score: &RiskScore) {
    sylox_types::ttl::set_persistent(env, &DataKey::Score(asset.clone()), score);
}

/// Review decision D1.
pub fn get_event_in_progress(env: &Env, asset: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::EventInProgress(asset.clone()))
        .unwrap_or(false)
}

/// Review decision D1.
pub fn set_event_in_progress(env: &Env, asset: &Address, in_progress: bool) {
    if in_progress {
        sylox_types::ttl::set_persistent(env, &DataKey::EventInProgress(asset.clone()), &true);
    } else {
        env.storage()
            .persistent()
            .remove(&DataKey::EventInProgress(asset.clone()));
    }
}

/// Re-review item C7.
pub fn get_stale_announced(env: &Env, asset: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::StaleAnnounced(asset.clone()))
        .unwrap_or(false)
}

/// Re-review item C7.
pub fn set_stale_announced(env: &Env, asset: &Address, announced: bool) {
    if announced {
        sylox_types::ttl::set_persistent(env, &DataKey::StaleAnnounced(asset.clone()), &true);
    } else {
        env.storage()
            .persistent()
            .remove(&DataKey::StaleAnnounced(asset.clone()));
    }
}

pub fn get_down_streak(env: &Env, asset: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::DownStreak(asset.clone()))
        .unwrap_or(0)
}

pub fn set_down_streak(env: &Env, asset: &Address, streak: u32) {
    if streak == 0 {
        env.storage()
            .persistent()
            .remove(&DataKey::DownStreak(asset.clone()));
    } else {
        sylox_types::ttl::set_persistent(env, &DataKey::DownStreak(asset.clone()), &streak);
    }
}

// -- formula --

pub fn get_formula(env: &Env) -> Option<Formula> {
    env.storage().instance().get(&DataKey::Formula)
}

pub fn set_formula(env: &Env, formula: &Formula) {
    env.storage().instance().set(&DataKey::Formula, formula);
}

// -- asset list --

pub fn get_assets(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&DataKey::Assets)
        .unwrap_or_else(|| Vec::new(env))
}

pub fn add_to_asset_list(env: &Env, asset: &Address) {
    let mut assets = get_assets(env);
    if !assets.contains(asset) {
        assets.push_back(asset.clone());
        env.storage().instance().set(&DataKey::Assets, &assets);
    }
}

// -- signal disputes --

pub fn get_dispute(env: &Env, asset: &Address, epoch: u64) -> Option<DisputeRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::Dispute(asset.clone(), epoch))
}

pub fn set_dispute(env: &Env, asset: &Address, epoch: u64, record: &DisputeRecord) {
    sylox_types::ttl::set_persistent(env, &DataKey::Dispute(asset.clone(), epoch), record);
}

pub fn clear_dispute(env: &Env, asset: &Address, epoch: u64) {
    env.storage()
        .persistent()
        .remove(&DataKey::Dispute(asset.clone(), epoch));
}

/// Sets a ring slot's state directly, for `resolve_signal_dispute`
/// (Section 5.4), for `dispute_signals` opening a dispute. Preserves
/// whatever `final_announced` already held (always `false` at this
/// point: nothing is announced before a slot is actually Final). Does
/// not touch `RingNewest`: a state transition never changes which
/// epoch is newest, since the epoch itself is unchanged.
pub fn set_slot_disputed(env: &Env, asset: &Address, epoch: u64) {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let mut slot = read_slot(&packed, index);
    if slot.epoch != epoch {
        // Nothing to transition; see set_slot_final's doc comment for
        // why this can legitimately happen and is not an error.
        return;
    }
    let final_announced = read_final_announced(&packed, index);
    slot.state = SlotState::Disputed;
    write_slot(&mut packed, index, &slot, final_announced);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);
}

/// Section 5.9 S4 (v1.5): unlike `set_slot_disputed`, writes `Disputed`
/// unconditionally for `epoch`'s slot, even if it currently holds no
/// data at all (state `Empty`, the case `set_slot_disputed` treats as
/// "nothing to transition"). An hour's `Ring(asset)` slot is written
/// only by `refresh_waiting_hour`/`try_build_hour`, never by
/// `post_sub_signals` itself, so the FIRST sub-epoch ever disputed in
/// a brand-new hour can arrive while that hour's own slot has never
/// been written at all; this function is what lets that first dispute
/// still mark the hour `Disputed` rather than being silently dropped.
///
/// Unlike the old all-zero write this replaces, `signals` carries the
/// REAL provisional roll-up of this hour's other, non-disputed
/// sub-epochs (`refresh_waiting_hour`'s own `roll_up_sub_slots` call,
/// or `empty_hour_signal_set` when every posted sub-epoch happens to
/// be disputed): a disputed sub-epoch must exclude only itself, never
/// zero out its whole hour's otherwise-healthy data (the footprint-fix
/// review's own finding). `provisional_sub_coverage` is the explicit
/// marker a reader uses instead of inferring from `peg_ratio` or
/// `pending_until`: `Some(0)` means zero real coverage (every posted
/// sub-epoch disputed), `Some(n > 0)` means `n` non-disputed
/// sub-epochs contributed to `signals`. `pending_until` is set to
/// `HOUR_PENDING_SENTINEL` by the caller (`lib.rs`), never derived
/// here, matching every other waiting-hour write.
pub fn force_set_slot_disputed(
    env: &Env,
    asset: &Address,
    epoch: u64,
    pending_until: u64,
    signals: &SignalSet,
    provisional_sub_coverage: Option<u32>,
) -> bool {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let existing = read_slot(&packed, index);
    // Same ring protection write_ring_slot gives the hourly path: never
    // overwrite a position that currently holds a strictly newer
    // epoch (the ring wrapped past it since epoch's own position was
    // last written).
    if existing.state != SlotState::Empty && existing.epoch > epoch {
        return false;
    }
    let slot = RingSlot {
        epoch,
        state: SlotState::Disputed,
        pending_until,
        peg_ratio: signals.peg_ratio,
        liquidity_2pct: signals.liquidity_2pct,
        redemption_net: signals.redemption_net,
        supply: signals.supply,
        supply_change_bps: signals.supply_change_bps,
        clawback_amount: signals.issuer_actions.clawback_amount,
        auth_revocations: signals.issuer_actions.auth_revocations,
        endpoint: signals.endpoint,
        provisional_sub_coverage,
    };
    write_slot(&mut packed, index, &slot, false);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);

    let newest = get_newest_epoch(env, asset);
    if newest.is_none_or(|n| epoch > n) {
        sylox_types::ttl::set_persistent(env, &DataKey::RingNewest(asset.clone()), &epoch);
    }
    true
}

/// `resolve_signal_dispute`'s keeper-wins path (Section 5.4): the slot
/// becomes definitely `Final`. Review item C6 (re-review): sets
/// `final_announced` to `true` in the SAME write, since the caller is
/// responsible for emitting `signals_final` immediately after this
/// call returns `true` (and must not emit it if this returns `false`,
/// meaning the slot had already moved on and there is nothing to
/// announce). Returns whether the transition actually happened.
pub fn set_slot_final(env: &Env, asset: &Address, epoch: u64) -> bool {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let mut slot = read_slot(&packed, index);
    if slot.epoch != epoch {
        // The slot this epoch used to occupy has already been
        // overwritten by something newer (the ring wrapped past it).
        // A dispute resolution arriving this late is a timing issue
        // for the caller to handle, not something storage can
        // retroactively fix.
        return false;
    }
    slot.state = SlotState::Final;
    write_slot(&mut packed, index, &slot, true);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);
    true
}

/// `resolve_signal_dispute`'s disputer-wins path (Section 5.4): the
/// slot reopens to `Empty` (its `Signals` entry is moved to history by
/// `overturn_signals`, review item C4, separately). `final_announced`
/// resets to unannounced along with everything else, via `empty_slot`;
/// an `Empty` slot was never Final, so there is nothing it could have
/// announced. Does not touch `RingNewest`, matching `set_slot_final`.
pub fn set_slot_overturned(env: &Env, asset: &Address, epoch: u64) {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let slot = read_slot(&packed, index);
    if slot.epoch != epoch {
        return;
    }
    write_slot(&mut packed, index, &empty_slot(), false);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);
}

/// Updates a slot's `endpoint` in place, for `finalize_endpoint` (Section
/// 7.4) booking a late `Staking.aggregate` result after `post_signals`
/// had already written `Unknown`. A no-op if the slot's stored epoch no
/// longer matches `epoch` (same reasoning as `set_slot_final`).
pub fn set_slot_endpoint(env: &Env, asset: &Address, epoch: u64, endpoint: EndpointStatus) {
    let mut packed = get_ring_packed(env, asset);
    let index = position_of(epoch);
    let mut slot = read_slot(&packed, index);
    if slot.epoch != epoch {
        return;
    }
    let final_announced = read_final_announced(&packed, index);
    slot.endpoint = endpoint;
    write_slot(&mut packed, index, &slot, final_announced);
    sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);
}

/// `epoch`'s position in the ring. A position, not an identity: see the
/// module doc comment. `pub(crate)` so the hysteresis/overwrite
/// property tests can construct same-position epoch pairs directly
/// instead of duplicating this formula.
pub(crate) fn position_of(epoch: u64) -> u32 {
    (epoch % RING_SLOTS as u64) as u32
}

fn get_ring_packed(env: &Env, asset: &Address) -> Bytes {
    match env
        .storage()
        .persistent()
        .get::<_, Bytes>(&DataKey::Ring(asset.clone()))
    {
        Some(packed) => {
            validate_header(&packed);
            packed
        }
        None => empty_ring_packed(env),
    }
}

fn validate_header(packed: &Bytes) {
    let mut header = [0u8; HEADER_BYTES as usize];
    packed.slice(0..HEADER_BYTES).copy_into_slice(&mut header);
    let version = header[0];
    let slots = u32::from_le_bytes(header[1..5].try_into().unwrap());
    let slot_bytes = u32::from_le_bytes(header[5..9].try_into().unwrap());
    assert!(
        version == LAYOUT_VERSION && slots == RING_SLOTS && slot_bytes == SLOT_BYTES,
        "ring buffer layout mismatch: stored version {version}, RING_SLOTS {slots}, SLOT_BYTES {slot_bytes}; \
         this build expects version {LAYOUT_VERSION}, RING_SLOTS {RING_SLOTS}, SLOT_BYTES {SLOT_BYTES}"
    );
}

fn empty_ring_packed(env: &Env) -> Bytes {
    let mut header = [0u8; HEADER_BYTES as usize];
    header[0] = LAYOUT_VERSION;
    header[1..5].copy_from_slice(&RING_SLOTS.to_le_bytes());
    header[5..9].copy_from_slice(&SLOT_BYTES.to_le_bytes());

    let mut packed = Bytes::from_slice(env, &header);
    let zeros = [0u8; SLOT_BYTES as usize];
    for _ in 0..RING_SLOTS {
        packed.extend_from_slice(&zeros);
    }
    packed
}

fn empty_slot() -> RingSlot {
    RingSlot {
        epoch: 0,
        state: SlotState::Empty,
        pending_until: 0,
        peg_ratio: 0,
        liquidity_2pct: 0,
        redemption_net: 0,
        supply: 0,
        supply_change_bps: 0,
        clawback_amount: 0,
        auth_revocations: 0,
        endpoint: EndpointStatus::Unknown,
        provisional_sub_coverage: None,
    }
}

fn state_byte(state: SlotState) -> u8 {
    match state {
        SlotState::Empty => 0,
        SlotState::Pending => 1,
        SlotState::Disputed => 2,
        SlotState::Final => 3,
    }
}

fn byte_state(b: u8) -> SlotState {
    match b {
        1 => SlotState::Pending,
        2 => SlotState::Disputed,
        3 => SlotState::Final,
        _ => SlotState::Empty,
    }
}

/// Packs `RingSlot.provisional_sub_coverage` into byte 107 (one of the
/// slot's spare padding bytes, alongside byte 106's `final_announced`).
/// `0xFF` means `None`; every other byte value is the coverage count
/// itself, so the valid range is 0 to 12 (`sub_epochs_per_hour`'s own
/// maximum, `SUB_EPOCH_SECS_MIN`'s own 300s giving `3,600 / 300 = 12`).
/// Panics on a value above 12, the same "decode what was written, or
/// trap" contract `read_slot`'s own doc comment already states for
/// this file: nothing in this contract can ever WRITE more than 12
/// here, so a value above it on read means storage corruption, not a
/// case to silently coerce.
fn coverage_byte(coverage: Option<u32>) -> u8 {
    match coverage {
        None => 0xFF,
        Some(n) => {
            assert!(n <= 12, "provisional_sub_coverage out of range: {n}");
            n as u8
        }
    }
}

fn byte_coverage(b: u8) -> Option<u32> {
    if b == 0xFF {
        None
    } else {
        assert!(
            b <= 12,
            "stored provisional_sub_coverage byte out of range: {b}"
        );
        Some(b as u32)
    }
}

fn endpoint_byte(status: EndpointStatus) -> u8 {
    match status {
        EndpointStatus::Unknown => 0,
        EndpointStatus::Up => 1,
        EndpointStatus::Degraded => 2,
        EndpointStatus::Down => 3,
    }
}

fn byte_endpoint(b: u8) -> EndpointStatus {
    match b {
        1 => EndpointStatus::Up,
        2 => EndpointStatus::Degraded,
        3 => EndpointStatus::Down,
        _ => EndpointStatus::Unknown,
    }
}

/// Byte offset of slot `index`'s first byte, past the header.
fn slot_base(index: u32) -> u32 {
    HEADER_BYTES + index * SLOT_BYTES
}

/// Writes one slot's fields into `packed` at `index`, fixed width, no field
/// names. Layout (little endian) within the slot, 107 of its 112 bytes
/// used: `[0..8) epoch, [8) state, [9..17) pending_until, [17..33)
/// peg_ratio, [33..49) liquidity_2pct, [49..65) redemption_net, [65..81)
/// supply, [81..85) supply_change_bps, [85..101) clawback_amount,
/// [101..105) auth_revocations, [105) endpoint, [106) final_announced`.
///
/// `final_announced` (C6, re-review) marks whether `signals_final` has
/// already been emitted for this epoch, so the backward finality scan
/// in `lib.rs`'s `try_advance_finality` never re-announces the same
/// epoch twice. It lives in the ring slot's own byte, not a separate
/// storage key, so checking and setting it costs no extra read or
/// write beyond the slot write every other field already needed.
/// Every call site must pass it explicitly (never implicitly
/// preserved or implicitly zeroed) so each one states its own intent:
/// `write_ring_slot` always passes `false` (a fresh post, or a repost
/// after an overturn, both start unannounced); `set_slot_final`'s
/// keeper-wins transition to `Final` passes `true` in the same write
/// that makes the slot Final, rather than a separate write; every
/// other caller that is not changing this specifically must read the
/// slot's current value first and pass it straight through.
fn write_slot(packed: &mut Bytes, index: u32, slot: &RingSlot, final_announced: bool) {
    let base = slot_base(index);
    let mut buf = [0u8; SLOT_BYTES as usize];
    buf[0..8].copy_from_slice(&slot.epoch.to_le_bytes());
    buf[8] = state_byte(slot.state);
    buf[9..17].copy_from_slice(&slot.pending_until.to_le_bytes());
    buf[17..33].copy_from_slice(&slot.peg_ratio.to_le_bytes());
    buf[33..49].copy_from_slice(&slot.liquidity_2pct.to_le_bytes());
    buf[49..65].copy_from_slice(&slot.redemption_net.to_le_bytes());
    buf[65..81].copy_from_slice(&slot.supply.to_le_bytes());
    buf[81..85].copy_from_slice(&slot.supply_change_bps.to_le_bytes());
    buf[85..101].copy_from_slice(&slot.clawback_amount.to_le_bytes());
    buf[101..105].copy_from_slice(&slot.auth_revocations.to_le_bytes());
    buf[105] = endpoint_byte(slot.endpoint);
    buf[106] = final_announced as u8;
    buf[107] = coverage_byte(slot.provisional_sub_coverage);

    packed.copy_from_slice(base, &buf);
}

/// Reads slot `index`'s fields back out of `packed`. Infallible: every
/// `try_into()` below converts a fixed, known-length sub-slice of a
/// `[u8; SLOT_BYTES as usize]` array into an array of the same length, so
/// none can fail; that invariant is what makes `.unwrap()` here safe
/// rather than a defect, but we still spell out the length relationship
/// explicitly instead of trusting that the reasoning around it won't rot.
fn read_slot(packed: &Bytes, index: u32) -> RingSlot {
    let base = slot_base(index);
    let mut buf = [0u8; SLOT_BYTES as usize];
    packed
        .slice(base..base + SLOT_BYTES)
        .copy_into_slice(&mut buf);

    RingSlot {
        epoch: read_u64(&buf, 0),
        state: byte_state(buf[8]),
        pending_until: read_u64(&buf, 9),
        peg_ratio: read_i128(&buf, 17),
        liquidity_2pct: read_i128(&buf, 33),
        redemption_net: read_i128(&buf, 49),
        supply: read_i128(&buf, 65),
        supply_change_bps: read_i32(&buf, 81),
        clawback_amount: read_i128(&buf, 85),
        auth_revocations: read_u32(&buf, 101),
        endpoint: byte_endpoint(buf[105]),
        provisional_sub_coverage: byte_coverage(buf[107]),
    }
}

/// Reads slot `index`'s `final_announced` byte (106) without decoding
/// the rest of the slot. Internal only: not part of the public
/// `RingSlot` shape `ring()` returns, since it is bookkeeping for
/// `signals_final` emission, not data Section 12.1 specifies as part
/// of a slot's observable content.
fn read_final_announced(packed: &Bytes, index: u32) -> bool {
    let base = slot_base(index);
    packed.get(base + 106).unwrap_or(0) != 0
}

/// Review item C6 (re-review): per epoch, INDEPENDENT finality. Scans
/// back from `newest_posted` across at most `max_lookback` epochs
/// (never more than `RING_SLOTS`, since nothing older could still be
/// physically present), finding the single newest epoch that is
/// effectively Final (C5's `effective_state`) — a missing, Empty,
/// Disputed, or still-Pending-but-not-yet-due epoch along the way is
/// simply skipped, never a reason to stop scanning past it, unlike the
/// old sequential cursor this replaces.
///
/// At the same time, collects every epoch in the scanned range that is
/// effectively Final and not yet `final_announced`, flips that flag
/// for each one in the SAME in-memory buffer, and writes the whole
/// buffer back to storage exactly once regardless of how many flags
/// changed — "one ring write for the whole scan," per the re-review's
/// instruction. The caller (`lib.rs`) is responsible for actually
/// emitting `signals_final` for each epoch in the returned list, in
/// whatever order it chooses; this function only decides WHICH epochs
/// qualify and persists that they have been announced.
///
/// Returns `(newest_final, newly_announced)`. `newly_announced` is
/// NOT necessarily sorted by epoch and can include epochs older than
/// a later-posted one that became final earlier (a backfilled epoch),
/// which is why the re-review's report notes `signals_final` can
/// arrive out of epoch order; callers (indexers) must dedupe on
/// `(asset, epoch)`, not assume monotonic order.
pub fn advance_finality(
    env: &Env,
    asset: &Address,
    now: u64,
    newest_posted: u64,
    max_lookback: u32,
) -> (Option<u64>, Vec<u64>) {
    let lookback = max_lookback.min(RING_SLOTS) as u64;
    let oldest_candidate = newest_posted.saturating_sub(lookback.saturating_sub(1));

    let mut packed = get_ring_packed(env, asset);
    let mut newest_final: Option<u64> = None;
    let mut newly_announced: Vec<u64> = Vec::new(env);
    let mut changed = false;

    let mut epoch = newest_posted;
    loop {
        let index = position_of(epoch);
        let slot = read_slot(&packed, index);
        if slot.state != SlotState::Empty
            && slot.epoch == epoch
            && effective_state(&slot, now) == SlotState::Final
        {
            if newest_final.is_none() {
                newest_final = Some(epoch);
            }
            if !read_final_announced(&packed, index) {
                // Only the flag changes here, never the stored `state`
                // byte itself: `write_ring_slot_...` and
                // `backfill_after_an_outage_matches_no_outage` both
                // require two posting orders of the exact same
                // signals to leave an IDENTICAL ring, including the
                // stored `state` byte. Eagerly materializing `Pending`
                // to `Final` here, even though harmless to any reader
                // (every read goes through `effective_state` or this
                // same check), would make that byte's value depend on
                // whether a finality scan happened to pass over this
                // epoch, not purely on wall-clock time, breaking that
                // order-independence. `effective_state` already gives
                // every caller the right answer lazily, so there is
                // nothing to gain from writing it early.
                write_slot(&mut packed, index, &slot, true);
                newly_announced.push_back(epoch);
                changed = true;
            }
        }
        if epoch == oldest_candidate {
            break;
        }
        epoch -= 1;
    }

    if changed {
        sylox_types::ttl::set_persistent(env, &DataKey::Ring(asset.clone()), &packed);
    }

    (newest_final, newly_announced)
}

/// Reads 8 bytes at `offset` as a little endian `u64`. `offset + 8` is
/// always within `buf`'s `SLOT_BYTES` bound for every call site above, so
/// this never panics; the array conversion is infallible by construction
/// (fixed length in, fixed length out), not something that can fail at
/// runtime.
fn read_u64(buf: &[u8; SLOT_BYTES as usize], offset: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(b)
}

fn read_u32(buf: &[u8; SLOT_BYTES as usize], offset: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_le_bytes(b)
}

fn read_i32(buf: &[u8; SLOT_BYTES as usize], offset: usize) -> i32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[offset..offset + 4]);
    i32::from_le_bytes(b)
}

fn read_i128(buf: &[u8; SLOT_BYTES as usize], offset: usize) -> i128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&buf[offset..offset + 16]);
    i128::from_le_bytes(b)
}

// -- sub-epochs (technical-doc.md Section 5.9) --
//
// `Sub(asset)` is a second packed ring, same encoding as `Ring(asset)`
// (fixed width fields, no field names, `SLOT_BYTES` per slot plus a
// `HEADER_BYTES` header), fixed at `SUB_RING_SLOTS` slots. Its own
// slot identity is `sub_start` (a sub-epoch's absolute start time, a
// `u64` of seconds), not `(hour, sub)`: what `sub` means depends on
// which `sub_epoch_secs` governed that hour, so after an interval
// change `(hour, sub)` alone cannot be read back reliably, while a
// timestamp is unambiguous regardless of any later interval change
// (Section 5.9 S3). The ring's own position function is anchored to
// the fixed `SUB_EPOCH_GRID_SECS` (300 second) grid, never to the
// asset's current `sub_epoch_secs`, so the ring always spans exactly
// `SUB_RING_SLOTS * SUB_EPOCH_GRID_SECS` seconds (5 hours) of
// wall-clock time regardless of interval.

use sylox_types::time::{SUB_EPOCH_GRID_SECS, SUB_RING_SLOTS};

/// One sub-epoch's packed slot. Kept internal to this module (not
/// part of `sylox_types`, and `ring()`'s own public API gains no
/// `sub_ring(asset) -> Vec<SubRingSlot>` read per Section 12.1): only
/// the narrow reads `lib.rs` actually needs are exposed.
#[derive(Clone)]
pub struct SubRingSlot {
    /// This slot's own identity: the sub-epoch's absolute start time
    /// (`hour * EPOCH_SECS + sub * sub_epoch_secs`), not `(hour, sub)`.
    pub sub_start: u64,
    pub state: SlotState,
    pub pending_until: u64,
    pub peg_ratio: i128,
    pub liquidity_2pct: i128,
    pub redemption_net: i128,
    pub supply: i128,
    pub supply_change_bps: i32,
    pub clawback_amount: i128,
    pub auth_revocations: u32,
    pub endpoint: EndpointStatus,
}

fn empty_sub_slot() -> SubRingSlot {
    SubRingSlot {
        sub_start: 0,
        state: SlotState::Empty,
        pending_until: 0,
        peg_ratio: 0,
        liquidity_2pct: 0,
        redemption_net: 0,
        supply: 0,
        supply_change_bps: 0,
        clawback_amount: 0,
        auth_revocations: 0,
        endpoint: EndpointStatus::Unknown,
    }
}

/// `sub_start`'s position in `Sub(asset)`'s fixed ring: anchored to
/// the `SUB_EPOCH_GRID_SECS` grid, never to `sub_epoch_secs`, so a
/// `sub_epoch_secs` change never needs to re-encode this ring (Section
/// 5.9 S3). `pub(crate)` for the same reason `position_of` is: tests
/// construct same-position pairs directly.
pub(crate) fn position_of_sub(sub_start: u64) -> u32 {
    ((sub_start / SUB_EPOCH_GRID_SECS) % SUB_RING_SLOTS as u64) as u32
}

fn get_sub_ring_packed(env: &Env, asset: &Address) -> Bytes {
    match env
        .storage()
        .persistent()
        .get::<_, Bytes>(&DataKey::Sub(asset.clone()))
    {
        Some(packed) => packed,
        None => empty_sub_ring_packed(env),
    }
}

fn empty_sub_ring_packed(env: &Env) -> Bytes {
    let mut packed = Bytes::new(env);
    let zeros = [0u8; SLOT_BYTES as usize];
    for _ in 0..SUB_RING_SLOTS {
        packed.extend_from_slice(&zeros);
    }
    packed
}

fn sub_slot_base(index: u32) -> u32 {
    index * SLOT_BYTES
}

/// Same byte layout `write_slot` uses for `Ring(asset)`, with
/// `sub_start` (8 bytes) in place of `epoch` at `[0..8)`; every other
/// field keeps its existing offset. Byte 106 (`final_announced` in
/// the hourly ring) is unused here: `Sub(asset)` has no equivalent of
/// `signals_final`'s own "has this been announced yet" bookkeeping
/// (`sub_signals_final` fires directly off a state transition,
/// `lib.rs`, never off a backward scan that needs to avoid
/// re-announcing).
fn write_sub_slot(packed: &mut Bytes, index: u32, slot: &SubRingSlot) {
    let base = sub_slot_base(index);
    let mut buf = [0u8; SLOT_BYTES as usize];
    buf[0..8].copy_from_slice(&slot.sub_start.to_le_bytes());
    buf[8] = state_byte(slot.state);
    buf[9..17].copy_from_slice(&slot.pending_until.to_le_bytes());
    buf[17..33].copy_from_slice(&slot.peg_ratio.to_le_bytes());
    buf[33..49].copy_from_slice(&slot.liquidity_2pct.to_le_bytes());
    buf[49..65].copy_from_slice(&slot.redemption_net.to_le_bytes());
    buf[65..81].copy_from_slice(&slot.supply.to_le_bytes());
    buf[81..85].copy_from_slice(&slot.supply_change_bps.to_le_bytes());
    buf[85..101].copy_from_slice(&slot.clawback_amount.to_le_bytes());
    buf[101..105].copy_from_slice(&slot.auth_revocations.to_le_bytes());
    buf[105] = endpoint_byte(slot.endpoint);

    packed.copy_from_slice(base, &buf);
}

fn read_sub_slot(packed: &Bytes, index: u32) -> SubRingSlot {
    let base = sub_slot_base(index);
    let mut buf = [0u8; SLOT_BYTES as usize];
    packed
        .slice(base..base + SLOT_BYTES)
        .copy_into_slice(&mut buf);

    SubRingSlot {
        sub_start: read_u64(&buf, 0),
        state: byte_state(buf[8]),
        pending_until: read_u64(&buf, 9),
        peg_ratio: read_i128(&buf, 17),
        liquidity_2pct: read_i128(&buf, 33),
        redemption_net: read_i128(&buf, 49),
        supply: read_i128(&buf, 65),
        supply_change_bps: read_i32(&buf, 81),
        clawback_amount: read_i128(&buf, 85),
        auth_revocations: read_u32(&buf, 101),
        endpoint: byte_endpoint(buf[105]),
    }
}

/// Returns `sub_start`'s slot, verifying the stored identity matches
/// (the same stored-identity check `get_slot` makes for the hourly
/// ring): a mismatch means this sub-epoch is missing, never misread
/// as a different sub-epoch's data occupying the same grid position.
pub fn get_sub_slot(env: &Env, asset: &Address, sub_start: u64) -> Option<SubRingSlot> {
    let packed = get_sub_ring_packed(env, asset);
    let index = position_of_sub(sub_start);
    let slot = read_sub_slot(&packed, index);
    if slot.state != SlotState::Empty && slot.sub_start == sub_start {
        Some(slot)
    } else {
        None
    }
}

/// `get_sub_ring_packed`'s own one-time fetch, exposed for a caller
/// that needs to read many sub-epochs from the SAME `Sub(asset)` ring
/// in one call (`sub_peg_ratios_in_span_batch`): fetching and decoding
/// the whole packed entry once, then decoding every slot from this
/// one copy via `get_sub_slot_from_packed`, instead of a separate
/// `get_sub_slot` call (and so a separate storage fetch) per
/// sub-epoch. The packed entry is small and fixed-size (`Sub(asset)`'s
/// own 60 slot span, Section 5.9 S3) regardless of how many hours a
/// caller scans, so reading it once per call, not once per
/// sub-epoch, is both cheaper and the only read `cost_estimate()`
/// should ever attribute to this.
pub fn get_sub_ring_packed_for_batch(env: &Env, asset: &Address) -> Bytes {
    get_sub_ring_packed(env, asset)
}

/// Mirrors `get_sub_slot`'s own identity check exactly, against an
/// ALREADY-FETCHED packed entry (from `get_sub_ring_packed_for_
/// batch`) instead of fetching it again: decodes `sub_start`'s own
/// ring position and verifies its stored identity matches, the same
/// "a mismatch means this sub-epoch is missing, never misread as a
/// different sub-epoch's data" guarantee `get_sub_slot` gives.
pub fn get_sub_slot_from_packed(packed: &Bytes, sub_start: u64) -> Option<SubRingSlot> {
    let index = position_of_sub(sub_start);
    let slot = read_sub_slot(packed, index);
    if slot.state != SlotState::Empty && slot.sub_start == sub_start {
        Some(slot)
    } else {
        None
    }
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): whether `sub_start`'s
/// own ring position in `Sub(asset)` still belongs to it, i.e. is safe
/// to write a repost into directly. True if the position is `Empty`
/// (this sub-epoch's own slot, already cleared by the overturn) OR
/// still holds `sub_start`'s own identity (not yet overwritten by a
/// later sub-epoch's rotation). False if the position holds a
/// DIFFERENT `sub_start` (always a strictly newer one, by `position_
/// of_sub`'s own wraparound and the ring's existing newer-wins write
/// guard): the slot has rotated out from under this sub-epoch, and a
/// repost must go to `HeldHour` instead, never overwrite someone
/// else's data. A repost's own caller must check this BEFORE writing,
/// never write first and check after.
pub fn sub_slot_still_belongs_to(env: &Env, asset: &Address, sub_start: u64, now: u64) -> bool {
    let packed = get_sub_ring_packed(env, asset);
    let index = position_of_sub(sub_start);
    let slot = read_sub_slot(&packed, index);
    if slot.sub_start == sub_start {
        return true;
    }
    if slot.state != SlotState::Empty {
        // A DIFFERENT, non-Empty sub_start occupies this position:
        // always a strictly newer one (position_of_sub's own
        // wraparound and the ring's existing newer-wins write guard),
        // so this position never belongs to sub_start any more.
        return false;
    }
    // Empty does not, by itself, mean this position still belongs to
    // sub_start: the position's own current rotation (whichever
    // sub_start maps to it right now, via position_of_sub's modular
    // arithmetic) could belong to a LATER sub-epoch that simply was
    // never posted, not to sub_start itself. It still belongs to
    // sub_start only while sub_start's own 5 hour span
    // (SUB_RING_SLOTS * SUB_EPOCH_GRID_SECS) has not yet elapsed:
    // past that point, some other, later sub_start now legitimately
    // owns this position, even though nothing has been written to it
    // yet. A repost reaching this branch after that point must go to
    // HeldHour instead (the caller's own job), never write here.
    let span_secs = SUB_RING_SLOTS as u64 * SUB_EPOCH_GRID_SECS;
    now < sub_start + span_secs
}

/// Writes `sub_start`'s slot. Refuses to overwrite a position that
/// currently holds a strictly newer `sub_start`, the same ring
/// protection `write_ring_slot` gives the hourly ring. Returns `false`
/// in that case instead of writing.
pub fn write_sub_slot_entry(
    env: &Env,
    asset: &Address,
    sub_start: u64,
    s: &SignalSet,
    state: SlotState,
    pending_until: u64,
) -> bool {
    let mut packed = get_sub_ring_packed(env, asset);
    let index = position_of_sub(sub_start);
    let existing = read_sub_slot(&packed, index);
    if existing.state != SlotState::Empty && existing.sub_start > sub_start {
        return false;
    }

    let slot = SubRingSlot {
        sub_start,
        state,
        pending_until,
        peg_ratio: s.peg_ratio,
        liquidity_2pct: s.liquidity_2pct,
        redemption_net: s.redemption_net,
        supply: s.supply,
        supply_change_bps: s.supply_change_bps,
        clawback_amount: s.issuer_actions.clawback_amount,
        auth_revocations: s.issuer_actions.auth_revocations,
        endpoint: s.endpoint,
    };
    write_sub_slot(&mut packed, index, &slot);
    env.storage()
        .persistent()
        .set(&DataKey::Sub(asset.clone()), &packed);

    let newest = get_sub_ring_newest(env, asset);
    if newest.is_none_or(|n| sub_start > n) {
        sylox_types::ttl::set_persistent(env, &DataKey::SubRingNewest(asset.clone()), &sub_start);
    }
    true
}

/// Sets a sub-epoch's state in place (for disputing and resolving),
/// preserving every other field. A no-op if the slot's stored identity
/// no longer matches `sub_start` (the ring wrapped past it).
pub fn set_sub_slot_state(env: &Env, asset: &Address, sub_start: u64, state: SlotState) {
    let mut packed = get_sub_ring_packed(env, asset);
    let index = position_of_sub(sub_start);
    let mut slot = read_sub_slot(&packed, index);
    if slot.sub_start != sub_start {
        return;
    }
    slot.state = state;
    write_sub_slot(&mut packed, index, &slot);
    env.storage()
        .persistent()
        .set(&DataKey::Sub(asset.clone()), &packed);
}

/// Clears a sub-epoch's slot back to Empty (the disputer-wins /
/// overturned path), mirroring `set_slot_overturned`.
pub fn clear_sub_slot(env: &Env, asset: &Address, sub_start: u64) {
    let mut packed = get_sub_ring_packed(env, asset);
    let index = position_of_sub(sub_start);
    let slot = read_sub_slot(&packed, index);
    if slot.sub_start != sub_start {
        return;
    }
    write_sub_slot(&mut packed, index, &empty_sub_slot());
    env.storage()
        .persistent()
        .set(&DataKey::Sub(asset.clone()), &packed);
}

fn get_sub_ring_newest(env: &Env, asset: &Address) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::SubRingNewest(asset.clone()))
}

/// Newest `sub_start` ever written to `Sub(asset)`, for `lib.rs`'s
/// `live(asset)` read.
pub fn get_sub_ring_newest_pub(env: &Env, asset: &Address) -> Option<u64> {
    get_sub_ring_newest(env, asset)
}

/// Extends `Sub(asset)`'s own TTL. Section 5.9 S3: "extended on every
/// write, and on every `post_signals` for the asset." A new rule for
/// this new key, not a retrofit of the existing gap `Ring(asset)`,
/// `Score(asset)` etc. have today (tracked separately, issue #28);
/// `Sub(asset)` and `SubDispute` are the only keys this revision adds
/// TTL extension for.
pub fn extend_sub_ring_ttl(env: &Env, asset: &Address, threshold: u32, extend_to: u32) {
    if env.storage().persistent().has(&DataKey::Sub(asset.clone())) {
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Sub(asset.clone()), threshold, extend_to);
    }
}

// -- sub-epoch full SignalSet (Section 5.9 S2) --
//
// Mirrors `get_signals`/`set_signals`'s own role for the hourly path:
// `Sub(asset)`'s packed ring above carries only the aggregate-shaped
// subset; the full posted `SignalSet` (including `inputs_hash`,
// `poster`, `peg_ratio_p10`, none of which fit a fixed-width packed
// field) lives here, addressed by the `(hour, sub)` pair a keeper
// posted it under.

pub fn get_sub_signals(env: &Env, asset: &Address, hour: u64, sub: u32) -> Option<SignalSet> {
    env.storage()
        .persistent()
        .get(&DataKey::SubSignals(asset.clone(), hour, sub))
}

pub fn set_sub_signals(env: &Env, asset: &Address, hour: u64, sub: u32, signals: &SignalSet) {
    sylox_types::ttl::set_history(env, &DataKey::SubSignals(asset.clone(), hour, sub), signals);
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): one overturned
/// sub-epoch's own history record, mirroring `Overturned(asset,
/// epoch)`'s role for the hourly path, extended with the two fields
/// a sub-epoch's own repost window needs that an hourly epoch's
/// equivalent (ADR-005) never had to track:
/// - `overturned_at`: when the ruling landed, so a repost's own
///   window can be anchored to it (`max(original_close +
///   sub_backfill_secs, overturned_at + sub_backfill_secs)`), not
///   just to the sub-epoch's original close, which a slow (up to
///   `signal_dispute_ruling_secs`, 6 days) ruling could easily land
///   well past.
/// - `reposted`: whether this exact overturn has already been
///   reposted once. A repost's own dispute, if also overturned, is
///   its OWN new `SubOverturned` record (never this one, mutated),
///   with THIS record's `reposted` staying `true` forever, so no
///   second repost of this original overturn is ever accepted: the
///   longest a hour can wait on one sub-epoch is two dispute cycles
///   (post, dispute, overturn, repost, dispute again, overturn
///   again), never more.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubOverturnedRecord {
    pub signals: SignalSet,
    pub overturned_at: u64,
    pub reposted: bool,
}

/// Mirrors `overturn_signals`'s own move-to-history shape, for one
/// sub-epoch: moves `SubSignals(asset, hour, sub)` to the
/// `SubOverturned` history key (tagged with `overturned_at = now`,
/// `reposted = false`) and removes the live entry, so `get_sub_
/// signals` reports it missing (unblocking a repost for this exact
/// `(hour, sub)`, within its own window) while the overturned data
/// stays retrievable via `get_overturned_sub_signals`. A no-op if
/// there was no `SubSignals` entry for this sub-epoch (defensive;
/// `resolve_sub_signal_dispute`'s caller already checked one exists
/// before calling this).
pub fn overturn_sub_signals(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
    per_hour: u32,
    now: u64,
) {
    let key = DataKey::SubSignals(asset.clone(), hour, sub);
    if let Some(signals) = env.storage().persistent().get::<_, SignalSet>(&key) {
        let repost_used_key = DataKey::SubRepostUsed(asset.clone(), hour, sub);
        // One repost per ORIGINAL overturn: if the posting being
        // overturned right now was itself already a repost, this new
        // record starts `reposted: true` from the moment it is
        // written, so a third posting attempt is refused outright,
        // never reset to false the way a fresh SubOverturnedRecord
        // otherwise would be.
        let already_used_its_repost = env.storage().persistent().has(&repost_used_key);
        env.storage().persistent().remove(&repost_used_key);
        sylox_types::ttl::set_history(
            env,
            &DataKey::SubOverturned(asset.clone(), hour, sub),
            &SubOverturnedRecord {
                signals,
                overturned_at: now,
                reposted: already_used_its_repost,
            },
        );
        env.storage().persistent().remove(&key);

        // The hour can't build until the repost's own dispute cycle
        // ends: about 12.3 days after the first post, or about 13.2
        // with a late timeout (Section 5.9 S2). The build then reads
        // every sub-epoch in the hour through `hash_sub_epoch_inputs`
        // and `reward_sub_epoch_keepers`, so the siblings written at
        // the first post must outlive their own 10 days. Today a held
        // repost can't be disputed, which caps the wait at about 7.2
        // days; this keeps the hour whole once that changes.
        let storage = env.storage().persistent();
        for sibling in 0..per_hour {
            let sibling_key = DataKey::SubSignals(asset.clone(), hour, sibling);
            if sibling != sub && storage.has(&sibling_key) {
                storage.extend_ttl(
                    &sibling_key,
                    sylox_types::ttl::HISTORY_TTL_THRESHOLD,
                    sylox_types::ttl::HISTORY_TTL_EXTEND_TO,
                );
            }
        }
    }
}

/// The history record an overturned sub-epoch was moved to by
/// `overturn_sub_signals`. `None` if that sub-epoch was never
/// overturned (or a repost of it was itself overturned again, which
/// overwrites this entry with the newer attempt's own record, same
/// as `get_overturned_signals`'s own convention: only the most
/// recent overturn stays on record).
pub fn get_overturned_sub_signals(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
) -> Option<SubOverturnedRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::SubOverturned(asset.clone(), hour, sub))
}

/// Marks the overturn record for `(hour, sub)` as having now been
/// reposted once, so a second repost of the SAME overturn is refused
/// (`SubEpochAlreadyReposted`-equivalent, enforced by the caller via
/// this flag, not a separate error code: a repost is accepted or not
/// based on whether an overturn record exists and is not yet
/// reposted, so this flag's own check happens inline in `post_sub_
/// signals`, not here). A no-op if there is no overturn record for
/// this sub-epoch at all (defensive; the caller already found one
/// before calling this).
pub fn mark_sub_overturn_reposted(env: &Env, asset: &Address, hour: u64, sub: u32) {
    let key = DataKey::SubOverturned(asset.clone(), hour, sub);
    if let Some(mut record) = env
        .storage()
        .persistent()
        .get::<_, SubOverturnedRecord>(&key)
    {
        record.reposted = true;
        sylox_types::ttl::set_history(env, &key, &record);
    }
    // Tags the just-accepted repost itself, so IF it is later
    // overturned too, that overturn's own fresh record starts
    // already `reposted: true` (see `overturn_sub_signals`): the one
    // repost per original overturn rule.
    sylox_types::ttl::set_history(
        env,
        &DataKey::SubRepostUsed(asset.clone(), hour, sub),
        &true,
    );
}

// -- sub-epoch config (Section 5.9 S1) --

pub fn get_sub_epoch_config(env: &Env, asset: &Address) -> Option<sylox_types::SubEpochConfig> {
    env.storage()
        .persistent()
        .get(&DataKey::SubEpochConfig(asset.clone()))
}

pub fn set_sub_epoch_config(env: &Env, asset: &Address, cfg: &sylox_types::SubEpochConfig) {
    sylox_types::ttl::set_persistent(env, &DataKey::SubEpochConfig(asset.clone()), cfg);
}

// -- sub-epoch disputes (Section 5.9 S3, S4) --

pub fn get_sub_dispute(env: &Env, asset: &Address, hour: u64, sub: u32) -> Option<DisputeRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::SubDispute(asset.clone(), hour, sub))
}

/// `ttl_ledgers` is the caller's own computed span (mirroring
/// `Staking::storage`'s own `set_probe`/`set_probes_settled`
/// convention): long enough to outlive `SIGNAL_DISPUTE_RULING_SECS`,
/// the worst case a dispute record needs to survive before a ruling
/// or a timeout clears it.
pub fn set_sub_dispute(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
    record: &DisputeRecord,
    ttl_ledgers: u32,
) {
    let key = DataKey::SubDispute(asset.clone(), hour, sub);
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
}

pub fn clear_sub_dispute(env: &Env, asset: &Address, hour: u64, sub: u32) {
    env.storage()
        .persistent()
        .remove(&DataKey::SubDispute(asset.clone(), hour, sub));
}

// -- held hours (Section 5.9 S3, S4): an hour's own sub-epoch data,
// copied out of Sub(asset) once any of its sub-epochs is disputed --

/// Section 5.9 S3: one sub-epoch's roll-up-relevant fields, held in
/// `HeldHour(asset, hour)` once the hour has an open dispute. Narrower
/// than `SubRingSlot` (no `sub_start`: the map's own `sub` key is the
/// slot's identity here). `state`/`pending_until` ARE still carried,
/// unlike an earlier draft that stored only already-Final data: a
/// sub-epoch captured while still `Pending` (its own short dispute
/// window not yet closed, which is the common case right when a
/// SIBLING sub-epoch's dispute first triggers the copy-out) needs to
/// keep becoming effectively Final on its own over time, the same lazy
/// `now >= pending_until` computation `effective_sub_state` already
/// gives a ring-read `SubRingSlot`; nothing re-visits this entry to
/// flip a stored state once the ring itself is no longer the source
/// of truth for it.
#[contracttype]
#[derive(Clone)]
pub struct HeldSubSlot {
    pub state: SlotState,
    pub pending_until: u64,
    pub peg_ratio: i128,
    pub liquidity_2pct: i128,
    pub redemption_net: i128,
    pub supply: i128,
    pub clawback_amount: i128,
    pub auth_revocations: u32,
}

/// `hour`'s held sub-epoch data, if any dispute has opened for it.
/// `None` for an hour with no open (or ever opened) dispute: the
/// ordinary ring-read path (`sub_disposition`) is the source of truth
/// for every hour that has never needed this.
pub fn get_held_hour(
    env: &Env,
    asset: &Address,
    hour: u64,
) -> Option<soroban_sdk::Map<u32, HeldSubSlot>> {
    env.storage()
        .persistent()
        .get(&DataKey::HeldHour(asset.clone(), hour))
}

/// Copies `sub`'s current Final data into `hour`'s held map, creating
/// the map if this is the first sub-epoch of `hour` ever held.
/// Idempotent: calling it again for the same `sub` with the same data
/// (e.g. a repost after an unrelated dispute in the same hour) simply
/// overwrites that one entry. `ttl_ledgers` mirrors `set_sub_dispute`'s
/// own caller-supplied convention.
pub fn insert_held_sub_slot(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
    slot: &HeldSubSlot,
    ttl_ledgers: u32,
) {
    let key = DataKey::HeldHour(asset.clone(), hour);
    let mut map = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or(soroban_sdk::Map::new(env));
    map.set(sub, slot.clone());
    env.storage().persistent().set(&key, &map);
    env.storage()
        .persistent()
        .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
}

/// Sets `sub`'s own held entry's `state` directly: mirrors
/// `set_sub_slot_state`'s role for the ring, needed because every
/// reader of a held hour's sub-epoch disposition (`sub_disposition`,
/// `sub_peg_ratios_for_one_hour`'s own provisional read, the cover
/// gate via the batched read) now trusts THIS stored `state` alone,
/// never a separate `SubDispute` lookup, so `Sub(asset)`'s own ring
/// write is no longer the source of truth for a sub-epoch once it has
/// been copied out into `HeldHour`. A no-op if `hour` has no held
/// entry at all (the ordinary case: most disputes resolve long before
/// the ring ever needs `HeldHour` in the first place, so nothing to
/// update).
pub fn set_held_sub_slot_state(env: &Env, asset: &Address, hour: u64, sub: u32, state: SlotState) {
    let key = DataKey::HeldHour(asset.clone(), hour);
    if let Some(mut map) = env
        .storage()
        .persistent()
        .get::<_, soroban_sdk::Map<u32, HeldSubSlot>>(&key)
    {
        if let Some(mut slot) = map.get(sub) {
            slot.state = state;
            map.set(sub, slot);
            env.storage().persistent().set(&key, &map);
        }
    }
}

/// Removes `sub`'s own entry from `hour`'s held map (the disputer-wins
/// / overturned path: an overturned sub-epoch must not contribute to
/// the roll-up, the same exclusion `sub_disposition` already gives a
/// ring-visible Overturned slot). Leaves the map itself in place (with
/// `sub` simply absent) even if this was its only entry, so a later
/// insert for a DIFFERENT sub of the same hour does not need to
/// recreate it; `clear_held_hour` is the only thing that removes the
/// map entirely, once the hour is built.
pub fn remove_held_sub_slot(env: &Env, asset: &Address, hour: u64, sub: u32) {
    let key = DataKey::HeldHour(asset.clone(), hour);
    if let Some(mut map) = env
        .storage()
        .persistent()
        .get::<_, soroban_sdk::Map<u32, HeldSubSlot>>(&key)
    {
        map.remove(sub);
        env.storage().persistent().set(&key, &map);
    }
}

/// Clears `hour`'s held map entirely, once the hour has built (Final
/// or Empty) and no held data is needed any more.
pub fn clear_held_hour(env: &Env, asset: &Address, hour: u64) {
    env.storage()
        .persistent()
        .remove(&DataKey::HeldHour(asset.clone(), hour));
}

// -- one writer per hour (Section 5.9 S2) --

pub fn get_hour_posted_via(env: &Env, asset: &Address, hour: u64) -> Option<HourPostedVia> {
    env.storage()
        .persistent()
        .get(&DataKey::HourPostedVia(asset.clone(), hour))
}

pub fn set_hour_posted_via(env: &Env, asset: &Address, hour: u64, via: HourPostedVia) {
    sylox_types::ttl::set_history(env, &DataKey::HourPostedVia(asset.clone(), hour), &via);
}

pub fn get_hour_sub_epoch_secs(env: &Env, asset: &Address, hour: u64) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::HourSubEpochSecs(asset.clone(), hour))
}

pub fn set_hour_sub_epoch_secs(env: &Env, asset: &Address, hour: u64, secs: u64) {
    sylox_types::ttl::set_history(env, &DataKey::HourSubEpochSecs(asset.clone(), hour), &secs);
}

/// Test only: flips the stored layout version so the next read is
/// guaranteed to fail `validate_header`'s check. Exists purely to prove
/// that check fires; production code has no other way to corrupt a
/// `Ring(asset)` entry's header.
#[cfg(test)]
pub fn corrupt_header_for_test(env: &Env, asset: &Address) {
    let packed: Bytes = env
        .storage()
        .persistent()
        .get(&DataKey::Ring(asset.clone()))
        .expect("corrupt_header_for_test: no Ring(asset) entry to corrupt");
    let mut header = [0u8; HEADER_BYTES as usize];
    packed.slice(0..HEADER_BYTES).copy_into_slice(&mut header);
    header[0] = LAYOUT_VERSION + 1;
    let mut corrupted = Bytes::from_slice(env, &header);
    corrupted.append(&packed.slice(HEADER_BYTES..packed.len()));
    env.storage()
        .persistent()
        .set(&DataKey::Ring(asset.clone()), &corrupted);
}
