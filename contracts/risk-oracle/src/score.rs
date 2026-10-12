//! Risk score: the Section 6 formula, bands, hysteresis and staleness.
//! technical-doc.md Section 6, 6.5.

use soroban_sdk::{contracttype, Address, Env, Map, Symbol, Vec};
use sylox_types::{Band, EndpointStatus, RiskScore, SCALE};

use crate::error::Error;
use crate::math;
use crate::storage;

/// Epochs the 24 hour and 7 day aggregates look back, and the slots
/// `median_liquidity` (Section 11.1) uses. Both read the newest 168 of
/// the ring's 240 slots (Section 6.5, 5.8). Moved to `sylox_types::time`
/// (Section 5.9 S7); re-exported here under its established local
/// name so every existing call site in this module is unchanged.
const AGGREGATE_SLOTS_24H: u32 = 24;
pub use sylox_types::time::AGGREGATE_SLOTS_7D;

/// Epochs in the Depeg window component P's `peg_ratio_p10` is computed
/// over (the default `depeg_window_secs` = 72h at the default
/// `epoch_secs`, Section 23). `RiskOracle` has no cross contract
/// dependency on `EventRegistry`'s canonical `EventDefinition`, so this
/// is the Section 23 default, fixed, the same way `AGGREGATE_SLOTS_24H`
/// and `_7D` are; see the review's "Spec deviations" entry on this.
pub const DEPEG_WINDOW_SLOTS: u32 = 72;

/// PR #27 review (round 2): `first_epoch` alone proves calendar time
/// has elapsed since this asset's first post, not that the
/// `AGGREGATE_SLOTS_7D` window `aggregate_from_ring` reads is actually
/// populated. A keeper could post one epoch, go dark for 168+ epochs,
/// then resume with a single epoch, and the `first_epoch`-based guard
/// below would not block it even though the window it reads would
/// hold that one epoch alone. Require at least this many of the 168
/// slots to be real (`Final`): one full backfill window
/// (`WINDOW_SECS` / `EPOCH_SECS`, 72 epochs) of gap is a normal,
/// already-supported operating pattern (see
/// `check_stale_recovers_silently_then_relapses_with_a_fresh_asset_stale`),
/// so the minimum tolerates exactly one such gap and no more, rather
/// than an arbitrary ratio.
pub const MIN_FINAL_EPOCHS_7D: u32 =
    AGGREGATE_SLOTS_7D - (crate::WINDOW_SECS / crate::EPOCH_SECS) as u32;
const _: () = assert!(MIN_FINAL_EPOCHS_7D > 0);

#[contracttype]
#[derive(Clone)]
pub struct Formula {
    pub version: u32,
    /// [w_P, w_E, w_R, w_I, w_L, w_S], bps, sums to 10,000.
    pub weights: Vec<u32>,
    /// d_max, r_max, c_max, k_max, s_max_bps, l_target (per asset default;
    /// an asset specific L_target overrides this if `params` holds one
    /// keyed by the asset's own address encoded as a Symbol is not
    /// possible in Soroban, so L_target is instead passed per call from
    /// `AssetConfig` in Phase 2 — see `score_for` below).
    pub params: Map<Symbol, i128>,
}

const D_MAX: Symbol = soroban_sdk::symbol_short!("d_max");
const R_MAX: Symbol = soroban_sdk::symbol_short!("r_max");
const C_MAX: Symbol = soroban_sdk::symbol_short!("c_max");
const K_MAX: Symbol = soroban_sdk::symbol_short!("k_max");
const S_MAX_BPS: Symbol = soroban_sdk::symbol_short!("s_max_bp");

pub fn default_formula(env: &Env) -> Formula {
    let mut weights = Vec::new(env);
    // w_P, w_E, w_R, w_I, w_L, w_S. technical-doc.md Section 6.2.
    for w in [3_500u32, 2_000, 1_500, 1_500, 1_000, 500] {
        weights.push_back(w);
    }
    let mut params = Map::new(env);
    params.set(D_MAX, SCALE / 10); // 0.10
    params.set(R_MAX, SCALE / 10); // 0.10
    params.set(C_MAX, SCALE / 100); // 0.01
    params.set(K_MAX, 20);
    params.set(S_MAX_BPS, 2_000);
    Formula {
        version: 1,
        weights,
        params,
    }
}

pub fn validate_weights(weights: &Vec<u32>) -> Result<(), Error> {
    if weights.len() != 6 {
        return Err(Error::WeightsInvalid);
    }
    let sum: u32 = weights.iter().sum();
    if sum != 10_000 {
        return Err(Error::WeightsInvalid);
    }
    Ok(())
}

/// `clamp(x) = min(max(x, 0), 1)`, fixed point at `SCALE`. technical-doc.md
/// Section 6.1.
fn clamp(x: i128) -> i128 {
    x.clamp(0, SCALE)
}

/// Checked `a * b / c`, as `(a.checked_mul(b)).checked_div(c)`. Every
/// component below is this shape; this one helper keeps the overflow
/// check consistent instead of repeating `checked_mul`/`checked_div`
/// pairs six times. `MathOverflow` is also what a division by zero
/// degrades to (checked_div returns None for both overflow and
/// division by zero); every call site below guards its own divisor
/// with a `<= 0` check first, so the zero case is already excluded on
/// the callers that intend it to be a defined zero result.
fn checked_mul_div(a: i128, b: i128, c: i128) -> Result<i128, Error> {
    a.checked_mul(b)
        .and_then(|product| product.checked_div(c))
        .ok_or(Error::MathOverflow)
}

/// Component P: peg deviation. `100 * clamp(|1 - peg_ratio_p10| / d_max)`.
fn component_p(peg_ratio_p10: i128, d_max: i128) -> Result<i128, Error> {
    let deviation = SCALE
        .checked_sub(peg_ratio_p10)
        .ok_or(Error::MathOverflow)?
        .abs();
    let ratio = checked_mul_div(deviation, SCALE, d_max)?;
    checked_mul_div(100, clamp(ratio), SCALE)
}

/// Component E: endpoint health. Fixed table, not a ratio against SCALE.
fn component_e(endpoint: EndpointStatus) -> i128 {
    match endpoint {
        EndpointStatus::Up => 0,
        EndpointStatus::Unknown => 30,
        EndpointStatus::Degraded => 50,
        EndpointStatus::Down => 100,
    }
}

/// Component R: redemption pressure.
/// `100 * clamp(redemption_net_24h / (supply * r_max))`.
fn component_r(redemption_net_24h: i128, supply: i128, r_max: i128) -> Result<i128, Error> {
    if supply <= 0 {
        return Ok(0);
    }
    let denom = checked_mul_div(supply, r_max, SCALE)?;
    if denom <= 0 {
        return Ok(0);
    }
    let ratio = checked_mul_div(redemption_net_24h, SCALE, denom)?;
    checked_mul_div(100, clamp(ratio), SCALE)
}

/// Component I: issuer actions.
/// `100 * clamp(clawback_amount_7d / (supply * c_max) + auth_revocations_7d / k_max)`.
fn component_i(
    clawback_amount_7d: i128,
    auth_revocations_7d: u32,
    supply: i128,
    c_max: i128,
    k_max: i128,
) -> Result<i128, Error> {
    let clawback_term = if supply > 0 {
        let denom = checked_mul_div(supply, c_max, SCALE)?;
        if denom > 0 {
            checked_mul_div(clawback_amount_7d, SCALE, denom)?
        } else {
            0
        }
    } else {
        0
    };
    let revocation_term = if k_max > 0 {
        checked_mul_div(auth_revocations_7d as i128, SCALE, k_max)?
    } else {
        0
    };
    let sum = clawback_term
        .checked_add(revocation_term)
        .ok_or(Error::MathOverflow)?;
    checked_mul_div(100, clamp(sum), SCALE)
}

/// Component L: liquidity. `100 * clamp(1 - liquidity_2pct / L_target)`.
fn component_l(liquidity_2pct: i128, l_target: i128) -> Result<i128, Error> {
    if l_target <= 0 {
        return Ok(0);
    }
    let ratio = checked_mul_div(liquidity_2pct, SCALE, l_target)?;
    let deviation = SCALE.checked_sub(ratio).ok_or(Error::MathOverflow)?;
    checked_mul_div(100, clamp(deviation), SCALE)
}

/// Component S: supply shock.
/// `100 * clamp(|supply_change_24h_bps| / s_max_bps)`.
fn component_s(supply_change_24h_bps: i128, s_max_bps: i128) -> Result<i128, Error> {
    if s_max_bps <= 0 {
        return Ok(0);
    }
    let ratio = checked_mul_div(supply_change_24h_bps.abs(), SCALE, s_max_bps)?;
    checked_mul_div(100, clamp(ratio), SCALE)
}

/// Everything read from the ring to feed the six components, for one
/// asset at the latest Final epoch. Computed by `aggregate_from_ring`,
/// which reads only the slots it needs (`AGGREGATE_SLOTS_7D`, via
/// `storage::get_window`), never the whole buffer.
pub struct Aggregates {
    pub peg_ratio_p10: i128,
    pub liquidity_2pct: i128,
    pub endpoint: EndpointStatus,
    pub supply: i128,
    pub redemption_net_24h: i128,
    pub clawback_amount_7d: i128,
    pub auth_revocations_7d: u32,
    pub supply_change_24h_bps: i128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateResult {
    pub span_epochs: u64,
    pub epochs_until_span_ready: u64,
    pub present_epochs_7d: u32,
    pub newest_slot_present: bool,
    pub passes: bool,
}

pub fn history_gate(
    env: &Env,
    asset: &Address,
    newest_final: u64,
    first_epoch: u64,
) -> (GateResult, Vec<Option<sylox_types::RingSlot>>) {
    let span_epochs = newest_final.saturating_add(1).saturating_sub(first_epoch);
    let target_span = AGGREGATE_SLOTS_7D as u64;
    let epochs_until_span_ready = target_span.saturating_sub(span_epochs);

    if span_epochs < target_span {
        return (
            GateResult {
                span_epochs,
                epochs_until_span_ready,
                present_epochs_7d: 0,
                newest_slot_present: false,
                passes: false,
            },
            Vec::new(env),
        );
    }

    let start = newest_final.saturating_add(1).saturating_sub(target_span);
    let window = storage::get_window(env, asset, start, AGGREGATE_SLOTS_7D);

    let present_epochs_7d = window.iter().filter(|slot| slot.is_some()).count() as u32;
    let newest_slot_present = window.get(AGGREGATE_SLOTS_7D - 1).flatten().is_some();

    let passes = present_epochs_7d >= MIN_FINAL_EPOCHS_7D && newest_slot_present;

    (
        GateResult {
            span_epochs,
            epochs_until_span_ready,
            present_epochs_7d,
            newest_slot_present,
            passes,
        },
        window,
    )
}

/// Reads the newest `AGGREGATE_SLOTS_7D` ring slots ending at `latest_epoch`
/// and folds them into the aggregates every score component needs.
/// `latest_epoch` must be a Final epoch (the caller's responsibility; this
/// function only reads, it does not check finality beyond what
/// `storage::get_window` already does by treating a non-matching or empty
/// slot as missing).
///
/// `first_epoch` is the asset's own `storage::get_first_epoch`: the
/// epoch its first signal was EVER posted at. PR #25 review: comparing
/// `latest_epoch` directly against `AGGREGATE_SLOTS_7D` only guards the
/// subtraction below from underflowing, not a real history check — on
/// a real network every epoch number is unix-time-derived and far
/// larger than 168, so that comparison alone never rejects a
/// brand-new asset. The real requirement is that this asset has been
/// posting for at least `AGGREGATE_SLOTS_7D` epochs of its OWN
/// history, measured from `first_epoch`.
///
/// Every failure here is `AggregationFailed` (110), never `SanityBoundFailed`
/// (104): the latter is about one posted `SignalSet` failing a bound check,
/// this is about the ring not having enough history or data to score from,
/// a different condition a caller may want to handle differently (review
/// item "Aggregation failures must not reuse SanityBoundFailed").
pub fn aggregate_from_ring(
    env: &Env,
    asset: &Address,
    latest_epoch: u64,
    first_epoch: u64,
) -> Result<Aggregates, Error> {
    let (gate, window) = history_gate(env, asset, latest_epoch, first_epoch);
    if !gate.passes {
        return Err(Error::AggregationFailed);
    }

    let latest = window
        .get(AGGREGATE_SLOTS_7D - 1)
        .flatten()
        .ok_or(Error::AggregationFailed)?;

    let mut redemption_net_24h: i128 = 0;
    for i in (AGGREGATE_SLOTS_7D - AGGREGATE_SLOTS_24H)..AGGREGATE_SLOTS_7D {
        if let Some(slot) = window.get(i).flatten() {
            redemption_net_24h = redemption_net_24h
                .checked_add(slot.redemption_net)
                .ok_or(Error::MathOverflow)?;
        }
    }

    let mut clawback_amount_7d: i128 = 0;
    let mut auth_revocations_7d: u32 = 0;
    for i in 0..AGGREGATE_SLOTS_7D {
        if let Some(slot) = window.get(i).flatten() {
            clawback_amount_7d = clawback_amount_7d
                .checked_add(slot.clawback_amount)
                .ok_or(Error::MathOverflow)?;
            auth_revocations_7d = auth_revocations_7d
                .checked_add(slot.auth_revocations)
                .ok_or(Error::MathOverflow)?;
        }
    }

    // supply_change_24h_bps: the latest epoch's posted supply_change_bps
    // is a one-epoch figure (Section 11.3); the 24h component instead
    // compares supply now against supply 24 slots ago, matching S's
    // definition ("supply_change_24h_bps") rather than reusing the
    // one-epoch field under a 24h sounding name.
    let supply_change_24h_bps = match window
        .get(AGGREGATE_SLOTS_7D - AGGREGATE_SLOTS_24H)
        .flatten()
    {
        Some(slot) if slot.supply > 0 => {
            let diff = latest
                .supply
                .checked_sub(slot.supply)
                .ok_or(Error::MathOverflow)?;
            let scaled = diff.checked_mul(10_000).ok_or(Error::MathOverflow)?;
            scaled.checked_div(slot.supply).ok_or(Error::MathOverflow)?
        }
        _ => 0,
    };

    // Review item C3: component P's peg_ratio_p10 is computed onchain as
    // the 10th percentile of peg_ratio across the Depeg window (the
    // newest DEPEG_WINDOW_SLOTS of the 7 day window already read above),
    // with missing epochs excluded, not the keeper posted per-epoch field
    // of the same name (Section 4.1). A single epoch's wick therefore
    // cannot move the band through P: see
    // percentile_p10_ignores_a_single_epoch_wick in test.rs.
    let mut window_ratios: Vec<i128> = Vec::new(env);
    for i in (AGGREGATE_SLOTS_7D - DEPEG_WINDOW_SLOTS)..AGGREGATE_SLOTS_7D {
        if let Some(slot) = window.get(i).flatten() {
            window_ratios.push_back(slot.peg_ratio);
        }
    }
    if window_ratios.is_empty() {
        // Every epoch in the Depeg window is missing: nothing to compute
        // P from. This is a data availability failure, not a bound
        // violation on any one posting.
        return Err(Error::AggregationFailed);
    }
    let peg_ratio_p10 = math::percentile_10(&window_ratios);

    Ok(Aggregates {
        peg_ratio_p10,
        liquidity_2pct: latest.liquidity_2pct,
        endpoint: latest.endpoint,
        supply: latest.supply,
        redemption_net_24h,
        clawback_amount_7d,
        auth_revocations_7d,
        supply_change_24h_bps,
    })
}

/// Section 6.2's combined score, plus whether Section 6.3's "forced to
/// at least Warning if P = 100 or E = 100" override applies. Kept
/// alongside the score (rather than recomputed from it) because once
/// collapsed into the weighted sum, a component reading exactly 100
/// cannot be reliably recovered from the final rounded total.
pub struct ScoreResult {
    pub score: u32,
    pub forced_warning: bool,
}

/// Combines the six components into the Section 6.2 score, `w_P P + w_E E
/// + ... + w_S S`, rounded to the nearest whole point.
pub fn combined_score(
    formula: &Formula,
    aggregates: &Aggregates,
    l_target: i128,
) -> Result<ScoreResult, Error> {
    let d_max = formula.params.get(D_MAX).unwrap_or(SCALE / 10);
    let r_max = formula.params.get(R_MAX).unwrap_or(SCALE / 10);
    let c_max = formula.params.get(C_MAX).unwrap_or(SCALE / 100);
    let k_max = formula.params.get(K_MAX).unwrap_or(20);
    let s_max_bps = formula.params.get(S_MAX_BPS).unwrap_or(2_000);

    let p = component_p(aggregates.peg_ratio_p10, d_max)?;
    let e = component_e(aggregates.endpoint);
    let r = component_r(aggregates.redemption_net_24h, aggregates.supply, r_max)?;
    let i = component_i(
        aggregates.clawback_amount_7d,
        aggregates.auth_revocations_7d,
        aggregates.supply,
        c_max,
        k_max,
    )?;
    let l = component_l(aggregates.liquidity_2pct, l_target)?;
    let s = component_s(aggregates.supply_change_24h_bps, s_max_bps)?;

    let weighted_term = |weight_index: u32, component: i128| -> Result<i128, Error> {
        let weight = formula.weights.get(weight_index).unwrap() as i128;
        weight.checked_mul(component).ok_or(Error::MathOverflow)
    };
    let weighted = weighted_term(0, p)?
        .checked_add(weighted_term(1, e)?)
        .ok_or(Error::MathOverflow)?
        .checked_add(weighted_term(2, r)?)
        .ok_or(Error::MathOverflow)?
        .checked_add(weighted_term(3, i)?)
        .ok_or(Error::MathOverflow)?
        .checked_add(weighted_term(4, l)?)
        .ok_or(Error::MathOverflow)?
        .checked_add(weighted_term(5, s)?)
        .ok_or(Error::MathOverflow)?;

    // Weighted sum is in bps-of-score units (weights sum to 10,000); divide
    // back down and round to the nearest point.
    let rounded = weighted
        .checked_add(5_000)
        .ok_or(Error::MathOverflow)?
        .checked_div(10_000)
        .ok_or(Error::MathOverflow)?;
    let score: u32 = rounded
        .clamp(0, 100)
        .try_into()
        .map_err(|_| Error::MathOverflow)?;

    // Section 6.3: "Forced to at least Warning if P = 100 or E = 100",
    // checked on the components themselves (each already clamped to
    // 0..=100 before the weighted sum), not on the rounded total: a
    // component pinned at its max can still land the final weighted
    // score below the Warning range once scaled by its weight, which is
    // exactly the case this override exists to catch.
    let forced_warning = p == 100 || e == 100;

    Ok(ScoreResult {
        score,
        forced_warning,
    })
}

/// Section 6.3's score-range bands, before any override or hysteresis.
pub fn band_for_score(score: u32) -> Band {
    match score {
        0..=24 => Band::Normal,
        25..=49 => Band::Watch,
        50..=74 => Band::Warning,
        _ => Band::Distress,
    }
}

/// Section 6.4 hysteresis, applied to a freshly computed `(score, raw_band)`
/// against the asset's stored `RiskScore`. An upward move (toward
/// Distress) applies immediately; a downward move only applies once the
/// new, lower band has qualified for `band_down_epochs` consecutive
/// epochs in a row. `down_streak` is the running count of consecutive
/// epochs the raw band has qualified for a lower band than what is
/// currently stored; the caller persists the returned streak alongside
/// the score.
///
/// Does not implement the Section 6.3 "forced to Distress if an event is
/// Proposed, Challenged or Escalated" override: review decision D1 adds
/// that as a read-time floor in `RiskOracle::score`, deliberately
/// outside this function and outside the stored `RiskScore`, so the
/// hysteresis streak computed here only ever reflects genuine new-epoch
/// evidence, never an `EventRegistry` flag flip (see `score`'s doc
/// comment and the PR's "Review fixes" section, D1). The "forced to at
/// least Warning if P = 100 or E = 100" override is applied by the
/// caller (`recompute_score` in lib.rs) to `raw_band` before it ever
/// reaches this function, using `ScoreResult::forced_warning` from
/// `combined_score`, so an upward move from that override goes through
/// the same "applies immediately" path below as any other raw band
/// increase. The sticky `Event` band (via
/// `set_event_band`/`clear_event_band`) is handled by the caller
/// skipping this function entirely while it is set (see
/// `recompute_score`'s own `Band::Event` check).
pub fn apply_hysteresis(
    current: &RiskScore,
    raw_band: Band,
    down_streak: u32,
    band_down_epochs: u32,
) -> (Band, u32) {
    if current.band == Band::Event {
        // Event is sticky until clear_event_band; a freshly computed band
        // never overrides it (handled by the caller not calling this at
        // all while Event is set, but kept here too as a defensive
        // invariant rather than relying solely on call order).
        return (Band::Event, 0);
    }
    if raw_band >= current.band {
        (raw_band, 0)
    } else {
        let streak = down_streak + 1;
        if streak >= band_down_epochs {
            (raw_band, 0)
        } else {
            (current.band, streak)
        }
    }
}
