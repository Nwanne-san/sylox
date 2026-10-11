//! Time-derived constants shared across contracts, so a number baked
//! into the hourly ring, the Depeg window or the cure-window bitmap
//! is never silently duplicated or allowed to drift between crates.
//! technical-doc.md Section 5.9 S7. Mirrors `network_limits`'s own
//! style: one constant per line, a doc comment naming its spec
//! source, no logic.
//!
//! Before this module existed, `RING_SLOTS` was duplicated privately
//! in both `risk-oracle/src/storage.rs` and `event-registry/src/lib.rs`,
//! and `AGGREGATE_SLOTS_7D`, `MAX_CURE_EPOCHS`, the Depeg missing-epoch
//! tolerance default, and the `challenge_secs` bound each lived as a
//! private constant or bare literal in one contract's own crate. Both
//! crates now import these from here instead. `EPOCH_SECS` itself was
//! the same story (duplicated privately in `risk-oracle`,
//! `event-registry` and `staking`): centralized here too, for the same
//! reason and so the `demo-time` feature below has exactly one
//! definition to scale.
//!
//! **The `demo-time` feature**: every window in this build is a fixed
//! number of epochs (`RING_SLOTS`, `AGGREGATE_SLOTS_7D`,
//! `MAX_CURE_EPOCHS`, and so on, none of which change below), built
//! entirely out of how many SECONDS one epoch actually is. `EPOCH_SECS`
//! and `SUB_EPOCH_GRID_SECS` are the only two constants in this whole
//! module with a real "seconds" unit, and they are the only two this
//! feature touches, both divided by the same factor (300, the largest
//! that keeps `SUB_EPOCH_GRID_SECS` a whole number of seconds) so their
//! ratio - and every window expressed as a multiple of either one -
//! comes out identical in both builds. A contract built with this
//! feature plays through the exact same sequence of epochs and windows
//! as the normal build, just over minutes instead of weeks, for a live
//! demo deployment. It must never reach a real deployment: see
//! `RiskOracle::demo_build()`.

use crate::network_limits::CONTRACT_DATA_ENTRY_SIZE_BYTES;

/// One epoch, in seconds. technical-doc.md's own default throughout:
/// the hourly ring, the Depeg/cure windows, and every bound below are
/// all stated as a number of these. Scaled by the `demo-time` feature;
/// see this module's own doc comment.
#[cfg(not(feature = "demo-time"))]
pub const EPOCH_SECS: u64 = 3_600;
#[cfg(feature = "demo-time")]
pub const EPOCH_SECS: u64 = 12;

/// Number of slots in the per asset hourly ring buffer: the longest
/// v1 depeg window (72h) plus the 7 day liquidity baseline before it,
/// at the default `epoch_secs` of 1 hour. technical-doc.md Section
/// 5.8. Frozen for v1, not a governance parameter: changing it would
/// require re-encoding every asset's existing `Ring(asset)` entry,
/// which is a migration, not a parameter change.
pub const RING_SLOTS: u32 = 240;

/// Epochs the 24 hour and 7 day score aggregates look back, and the
/// slots `median_liquidity` uses: the newest 168 of the ring's 240
/// slots. technical-doc.md Section 6.5, 11.1.
pub const AGGREGATE_SLOTS_7D: u32 = 168;

/// The largest number of cure-window epochs (`challenge_secs /
/// EPOCH_SECS`) a Depeg `EventDefinition` may register, 72 hours'
/// worth. Keeps `CureProgress`'s own `recorded: u128` bitmap
/// comfortably sized for every definition this build can register,
/// with headroom to spare. technical-doc.md Section 8.8, PR #15
/// review finding F3.
pub const MAX_CURE_EPOCHS: u64 = 72;

/// `EventDefinition.max_missing_epochs`'s own documented default for
/// a Depeg definition: 6 of the 72 window epochs may be missing
/// without failing the check. technical-doc.md Section 8.2, ADR-005,
/// Section 23 `max_missing_epochs` default.
pub const DEPEG_MISSING_EPOCH_TOLERANCE_DEFAULT: u32 = 6;

/// `challenge_secs` must be a whole number of epochs, at least one
/// epoch long. technical-doc.md Section 8.8, PR #15 review finding
/// F3.
pub const CHALLENGE_SECS_MIN_EPOCHS: u64 = 1;

/// `challenge_secs` must be at most `MAX_CURE_EPOCHS` epochs long, so
/// `CureProgress`'s `u128` bitmap always has room for every cure-window
/// epoch a registered definition could need. technical-doc.md Section
/// 8.8, PR #15 review finding F3.
pub const CHALLENGE_SECS_MAX_EPOCHS: u64 = MAX_CURE_EPOCHS;

/// Fixed number of slots in the per asset sub-epoch ring, `Sub(asset)`.
/// technical-doc.md Section 5.9 S3: sized to outlive the worst case a
/// sub-epoch dispute needs (`sub_backfill_secs + signal_dispute_secs +
/// 3,600` seconds, 5 hours at the defaults) on a fixed 300 second
/// grid, `5 * 3,600 / 300 = 60`. Never resized when `sub_epoch_secs`
/// changes: a slower interval uses fewer of these 60 slots per
/// rotation, never spans more wall-clock time.
pub const SUB_RING_SLOTS: u32 = 60;

/// The fixed wall-clock grid `Sub(asset)`'s ring position is anchored
/// to, in seconds: the 5 minute `sub_epoch_secs` floor. technical-doc.md
/// Section 5.9 S3. Every allowed `sub_epoch_secs` value is a multiple
/// of this, so every sub-epoch's own start time always falls exactly
/// on this grid, and a `sub_epoch_secs` change never needs to
/// re-encode `Sub(asset)`. Scaled by the `demo-time` feature, by the
/// same factor as `EPOCH_SECS`; see this module's own doc comment.
#[cfg(not(feature = "demo-time"))]
pub const SUB_EPOCH_GRID_SECS: u64 = 300;
#[cfg(feature = "demo-time")]
pub const SUB_EPOCH_GRID_SECS: u64 = 1;

/// Build-time check: `EPOCH_SECS` is exactly 12 `SUB_EPOCH_GRID_SECS`
/// units, in both builds. This is the "in proportion" guarantee the
/// `demo-time` feature rests on - every other window in this module
/// (and every contract's own window built as a multiple of either
/// constant) comes out scaled by the identical factor as a result,
/// never needing its own feature gate.
const _: () = assert!(EPOCH_SECS == 12 * SUB_EPOCH_GRID_SECS);

/// `sylox_types::assets::RingSlot`'s own packed width in `risk-oracle`
/// and `Sub(asset)`'s own packed slot width (same encoding, Section
/// 5.9 S3): 112 bytes. Duplicated here as a plain number (not a
/// cross-crate re-export of `risk-oracle::storage::SLOT_BYTES`, which
/// stays private to that module's own packing) purely so the
/// build-time assertions below can check against it without creating
/// a dependency from `sylox_types` back onto `risk-oracle`.
const PACKED_SLOT_BYTES: u64 = 112;

/// The packed ring's header overhead, ahead of the packed slots:
/// layout version (1) + slot count (4) + slot width (4). Mirrors
/// `risk-oracle::storage::HEADER_BYTES`.
const PACKED_HEADER_BYTES: u64 = 9;

/// Build-time check (Section 5.9 S7, bullet 1): the hourly ring still
/// fits `contract_data_entry_size_bytes`. Unaffected by this revision,
/// which does not touch `RING_SLOTS` or the packed slot width; restated
/// here against the shared constants so a future change to either one
/// fails the build instead of silently reintroducing the Phase 1
/// overflow this packing was built to avoid.
const _: () = assert!(
    RING_SLOTS as u64 * PACKED_SLOT_BYTES + PACKED_HEADER_BYTES <= CONTRACT_DATA_ENTRY_SIZE_BYTES
);

/// Build-time check (Section 5.9 S7, bullet 2): `Sub(asset)` fits
/// `contract_data_entry_size_bytes` at its fixed 60 slot size (6,729
/// bytes, about 10.3% of the limit).
const _: () = assert!(
    SUB_RING_SLOTS as u64 * PACKED_SLOT_BYTES + PACKED_HEADER_BYTES
        <= CONTRACT_DATA_ENTRY_SIZE_BYTES
);

/// Build-time check (Section 5.9 S7, bullet 3): the cure window still
/// fits `CureProgress`'s own `u128` bitmap, one bit per cure epoch.
const _: () = assert!(MAX_CURE_EPOCHS <= 128);

/// The largest `depeg_window_secs / EPOCH_SECS` a Depeg
/// `EventDefinition` may register (`register_definition`'s own
/// `window_epochs + BASELINE_EPOCHS <= RING_SLOTS` check,
/// `BASELINE_EPOCHS == AGGREGATE_SLOTS_7D`), and the one, SHARED
/// source for that limit: `register_definition` and the build-time
/// assertion below both read this constant, never a separate copy of
/// the number, so the two can never silently drift apart. Also the
/// default depeg window (`DEFAULT_DEPEG_WINDOW_SECS / EPOCH_SECS`,
/// `event-registry/src/lib.rs`), which is set at this same ceiling.
/// technical-doc.md Section 5.9 S5, 9.4, 23.
pub const MAX_DEPEG_WINDOW_EPOCHS: u64 = (RING_SLOTS - AGGREGATE_SLOTS_7D) as u64;

/// `EventRegistry.cover_gate`'s own `RecentDepeg` check: the most
/// unbuilt hours in the trailing depeg window `depeg_check` may scan
/// before it must instead return `UnbuiltBacklog` (Section 5.9 S5,
/// R10), rather than making its own batched read at all.
///
/// Set at `MAX_DEPEG_WINDOW_EPOCHS + 1 = 73`: the structural maximum
/// a Depeg definition's own window can ever produce (one more than
/// `MAX_DEPEG_WINDOW_EPOCHS`, for `depeg_check`'s own inclusive loop
/// bound). Review finding: this means `UnbuiltBacklog` can NEVER
/// actually be reached while `register_definition` enforces
/// `MAX_DEPEG_WINDOW_EPOCHS` (no registered definition, nor the
/// default window, can ever produce more than this many unbuilt
/// hours in one scan) — the build-time assertion below is what keeps
/// this true, failing the build rather than letting the two drift
/// apart if either is ever changed without re-measuring the other.
/// `depeg_check` still checks this cap at runtime (`event-registry/
/// src/lib.rs`'s own doc comment there explains why: a defensive
/// guard that fails closed if the limits ever do drift, not dead
/// code to remove), it is just never reachable TODAY.
///
/// Since the Section 5.9 S5 footprint-fix revision (the gate reads
/// `Sub(asset)`, never `HeldHour`, plus `Ring(asset)`'s own
/// `provisional_sub_coverage`-tagged roll-up outside the 5 hour
/// span), `cover_gate`'s own footprint stays a FLAT 9 entries
/// regardless of how many hours are unbuilt or disputed (measured at
/// 1, 12, 72 and 73 unbuilt hours identically; see
/// `event-registry/src/budget_test.rs`'s own
/// `budget_cover_gate_footprint_is_constant_from_1_to_72_unbuilt_
/// hours`), so footprint no longer constrains this cap at all.
/// Memory is the one dimension that still scales with hour count, and
/// is what actually sets this cap's own real margin: 73 unbuilt
/// hours, every one also under an open dispute (the costlier of the
/// two per-hour paths this cap has to account for), measures
/// 1,762,761 bytes (re-measured after `sub_peg_ratios_in_span_batch`'s
/// own single-fetch review fix), about 4.2% of `TX_MEMORY_LIMIT_BYTES`
/// (41,943,040), a roughly 23.8x margin — comfortable, and the
/// binding constraint, not footprint or instructions (instructions:
/// 9,602,712, about 2.4% of `TX_MAX_INSTRUCTIONS`, a roughly 41.7x
/// margin, the least binding of the three). See
/// `event-registry/src/budget_test.rs`'s own
/// `budget_cover_gate_at_the_structural_cap_every_hour_disputed`.
pub const MAX_UNBUILT_HOURS_SCANNED_BY_COVER_GATE: u32 = 73;

/// Build-time check: the cap above matches `MAX_DEPEG_WINDOW_EPOCHS +
/// 1`, the SAME shared constant `register_definition` enforces, so
/// the two can never silently drift apart if either changes. If a
/// future revision raises `MAX_DEPEG_WINDOW_EPOCHS` without
/// re-measuring and raising this cap to match, the build fails here
/// instead of `cover_gate` silently scanning past what was ever
/// measured. The cap's own real memory margin under
/// `TX_MEMORY_LIMIT_BYTES` (about 23.8x, see this constant's own doc
/// comment) is empirical, not a formula a build-time assertion can
/// check; that number is re-verified by `event-registry/src/budget_
/// test.rs`'s own `budget_cover_gate_at_the_structural_cap_every_
/// hour_disputed` every time the test suite runs, which is the actual
/// guard against a regression here.
const _: () =
    assert!(MAX_UNBUILT_HOURS_SCANNED_BY_COVER_GATE as u64 == MAX_DEPEG_WINDOW_EPOCHS + 1);
