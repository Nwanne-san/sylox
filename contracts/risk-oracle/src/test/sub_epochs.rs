//! Tests for spec v1.5 Section 5.9: sub-epochs and the hourly roll-up.
//! Split into its own module, mirroring `golden_vectors.rs`/
//! `property.rs`'s own split-out pattern, given the size of this
//! feature. Reuses `test.rs`'s own `setup`/`setup_with_asset`,
//! `signal_set`, `time_at_epoch` and `REALISTIC_EPOCH_BASE`
//! conventions, never hand-computing a timestamp or epoch boundary.

use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, Address, Env};
use sylox_types::SlotState;

use super::{setup_with_asset, signal_set};
use crate::mocks::MockGovernor;
use crate::{RiskOracle, RiskOracleClient};

/// A bespoke setup with a real, *registered* `MockGovernor` (unlike
/// `setup_with_asset`'s own plain `Address::generate` governor), for
/// tests that call `resolve_sub_signal_dispute` and so need
/// `Governor.committee()` to actually resolve. Mirrors the pattern
/// `test.rs`'s own `resolve_signal_dispute` tests use (e.g.
/// `an_overturned_epoch_is_never_rewarded`), since `Fixture`'s shared
/// `setup` deliberately keeps the governor unregistered for every test
/// that never needs to call back into it.
fn setup_with_governor_and_asset(env: &Env) -> (RiskOracleClient<'_>, Address, Address) {
    env.mock_all_auths();
    let staking = env.register(crate::mocks::MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(env, &contract_id);

    let asset = Address::generate(env);
    let issuer = Address::generate(env);
    client.add_asset(&super::asset_config(env, &asset, &issuer));

    (client, governor, asset)
}

const REALISTIC_EPOCH_BASE: u64 = 497_000;
const EPOCH_SECS: u64 = 3_600;
const SUB_EPOCH_SECS_DEFAULT: u64 = 300;

fn hour_close(hour: u64) -> u64 {
    (hour + 1) * EPOCH_SECS
}

fn sub_start(hour: u64, sub: u32, sub_epoch_secs: u64) -> u64 {
    hour * EPOCH_SECS + sub as u64 * sub_epoch_secs
}

#[allow(clippy::too_many_arguments)]
fn post_sub(
    env: &Env,
    client: &crate::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    hour: u64,
    sub: u32,
    sub_epoch_secs: u64,
    peg_ratio: i128,
) {
    let start = sub_start(hour, sub, sub_epoch_secs);
    env.ledger().set_timestamp(start + sub_epoch_secs);
    let mut s = signal_set(env, hour, peg_ratio);
    s.liquidity_2pct = 500_000_000_000;
    s.supply_change_bps = 0;
    client.post_sub_signals(keeper, asset, &hour, &sub, &s);
}

/// S1: `set_sub_epoch_secs` rejects any value outside the allowed set.
#[test]
fn set_sub_epoch_secs_rejects_values_outside_the_allowed_set() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    for bad in [0u64, 1, 299, 301, 3_601, 7_200] {
        let result = fx.client.try_set_sub_epoch_secs(&asset, &bad);
        assert_eq!(
            result,
            Err(Ok(crate::Error::InvalidSubEpochInterval)),
            "sub_epoch_secs {bad} must be rejected"
        );
    }
    for good in [300u64, 600, 900, 1_200, 1_800, 3_600] {
        assert!(fx.client.try_set_sub_epoch_secs(&asset, &good).is_ok());
    }
}

/// S1: a `sub_epoch_secs` change takes effect only from the next hour
/// boundary, never mid-hour.
#[test]
fn changing_sub_epoch_secs_takes_effect_only_at_the_next_hour_boundary() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    let hour = REALISTIC_EPOCH_BASE;
    // Post sub 0 at the 5 minute default.
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        0,
        SUB_EPOCH_SECS_DEFAULT,
        10_000_000,
    );

    // Change to 600s mid-hour; must not apply to this hour.
    fx.client.set_sub_epoch_secs(&asset, &600);
    let cfg = fx.client.sub_epoch_config(&asset).unwrap();
    assert_eq!(cfg.pending_sub_epoch_secs, Some(600));
    assert_eq!(cfg.effective_from_hour, Some(hour + 1));

    // Sub 1 of THIS hour must still be addressed at the 300s grid
    // (sub 1 covers [300, 600)), not the new 600s one (which would
    // make sub 1 invalid, since 3600/600 = 6 sub-epochs, sub index 1
    // still exists either way, but at a different start time).
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        1,
        SUB_EPOCH_SECS_DEFAULT,
        10_000_000,
    );
    let (live_sub_epoch, _, _) = fx.client.live(&asset).unwrap();
    assert_eq!(
        live_sub_epoch.sub, 1,
        "still addressed at the old interval within this hour"
    );
}

/// What `sub_epoch_config` says is in force for `hour`: the pending
/// value from `effective_from_hour` on, the current value before it.
fn in_force_per_config(cfg: &sylox_types::SubEpochConfig, hour: u64) -> u64 {
    match (cfg.pending_sub_epoch_secs, cfg.effective_from_hour) {
        (Some(pending), Some(from)) if hour >= from => pending,
        _ => cfg.sub_epoch_secs,
    }
}

/// `sub_epoch_secs_at` and `sub_epoch_config` agree for every hour
/// around each change, before any change, while one is pending, and
/// once it is in force.
#[test]
fn sub_epoch_config_and_sub_epoch_secs_at_agree() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let check = |label: &str| {
        let cfg = fx.client.sub_epoch_config(&asset);
        for h in hour - 2..hour + 6 {
            let expected = cfg
                .as_ref()
                .map(|c| in_force_per_config(c, h))
                .unwrap_or(SUB_EPOCH_SECS_DEFAULT);
            assert_eq!(
                fx.client.sub_epoch_secs_at(&asset, &h),
                expected,
                "{label}, hour {h}"
            );
        }
    };

    env.ledger().set_timestamp(hour * EPOCH_SECS + 60);
    check("no config yet");
    fx.client.set_sub_epoch_secs(&asset, &600);
    check("600 pending");
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &hour), 300);
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &(hour + 1)), 600);

    env.ledger().set_timestamp((hour + 2) * EPOCH_SECS + 60);
    check("600 in force");
    env.ledger()
        .set_timestamp((hour + 1 + 240) * EPOCH_SECS + 60);
    fx.client.set_sub_epoch_secs(&asset, &900);
    check("900 pending after 600 took effect");
}

/// A second change 239 hours after the first took effect is rejected:
/// hours from before the first change could still be read, and the one
/// stored transition can't describe both.
#[test]
fn a_second_change_239_hours_after_the_first_takes_effect_is_rejected() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let hour = REALISTIC_EPOCH_BASE;

    env.ledger().set_timestamp(hour * EPOCH_SECS + 60);
    fx.client.set_sub_epoch_secs(&asset, &600);
    let took_effect = hour + 1;

    env.ledger()
        .set_timestamp((took_effect + 239) * EPOCH_SECS + 3_599);
    assert_eq!(
        fx.client.try_set_sub_epoch_secs(&asset, &900),
        Err(Ok(crate::Error::SubEpochChangeTooSoon))
    );
    let cfg = fx.client.sub_epoch_config(&asset).unwrap();
    assert_eq!(cfg.pending_sub_epoch_secs, Some(600), "config unchanged");
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &hour), 300);
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &took_effect), 600);
}

/// At 240 hours the second change is accepted. The length in force is
/// promoted to current, the hour of the change and those after the first
/// change keep 600, the next hour gets 900, and the hours before the
/// first change resolve to 600 only now that they are over 240 hours
/// old (before the second change they still resolved to 300).
#[test]
fn at_240_hours_a_second_change_is_accepted_and_old_hours_resolve_correctly() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;

    env.ledger().set_timestamp(hour * EPOCH_SECS + 60);
    fx.client.set_sub_epoch_secs(&asset, &600);
    let took_effect = hour + 1;
    let second_hour = took_effect + 240;

    // Sub 0 of the second change's hour, at 600, before the change.
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        second_hour,
        0,
        600,
        10_000_000,
    );
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &hour), 300);
    fx.client.set_sub_epoch_secs(&asset, &900);

    let cfg = fx.client.sub_epoch_config(&asset).unwrap();
    assert_eq!(
        cfg.sub_epoch_secs, 600,
        "the value in force is promoted to current"
    );
    assert_eq!(cfg.pending_sub_epoch_secs, Some(900));
    assert_eq!(cfg.effective_from_hour, Some(second_hour + 1));
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &took_effect), 600);
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &second_hour), 600);
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &(second_hour + 1)), 900);
    assert!(
        second_hour - hour > 240,
        "the pre-change hour is over 240 hours old"
    );
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &hour), 600);

    // The rest of the current hour is still addressed at 600: sub 1
    // covers [600, 1200), and `live` reads it back as sub 1.
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        second_hour,
        1,
        600,
        10_000_000,
    );
    let (live, _, _) = fx.client.live(&asset).unwrap();
    assert_eq!((live.hour, live.sub), (second_hour, 1));

    // The next hour uses 900: four sub-epochs, sub 3 the last.
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        second_hour + 1,
        3,
        900,
        10_000_000,
    );
    let (live, _, _) = fx.client.live(&asset).unwrap();
    assert_eq!((live.hour, live.sub), (second_hour + 1, 3));
}

/// A change still pending may be replaced, in the same hour, as often
/// as needed; the length in force for the current hour is untouched.
#[test]
fn replacing_a_still_pending_change_works() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let hour = REALISTIC_EPOCH_BASE;

    env.ledger().set_timestamp(hour * EPOCH_SECS + 60);
    fx.client.set_sub_epoch_secs(&asset, &600);
    env.ledger().set_timestamp(hour * EPOCH_SECS + 3_000);
    fx.client.set_sub_epoch_secs(&asset, &900);
    fx.client.set_sub_epoch_secs(&asset, &1_800);

    let cfg = fx.client.sub_epoch_config(&asset).unwrap();
    assert_eq!(cfg.sub_epoch_secs, 300);
    assert_eq!(cfg.pending_sub_epoch_secs, Some(1_800));
    assert_eq!(cfg.effective_from_hour, Some(hour + 1));
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &hour), 300);
    assert_eq!(fx.client.sub_epoch_secs_at(&asset, &(hour + 1)), 1_800);
}

/// Section 5.9 S1: an hour is resolved with the length it was posted at,
/// at any age. Hour H is posted at 300 while a change to 900 is pending
/// for H + 1, left unbuilt, and a second change (to 600) lands 293 hours
/// later, promoting 900 over the config's history. Building H then must
/// still use 300 and roll up all 12 of its sub-epochs; resolved through
/// the config alone it would read H at 900 and roll up only 4.
#[test]
fn an_old_unbuilt_hour_builds_with_the_length_it_was_posted_at() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;

    env.ledger().set_timestamp(hour * EPOCH_SECS + 60);
    fx.client.set_sub_epoch_secs(&asset, &900);
    let mut sum: i128 = 0;
    for sub in 0..12u32 {
        let peg = 9_000_000 + sub as i128 * 10_000;
        sum += peg;
        post_sub(&env, &fx.client, &keeper, &asset, hour, sub, 300, peg);
    }
    assert!(!fx.client.is_final(&asset, &hour), "left unbuilt");

    env.ledger()
        .set_timestamp(hour * EPOCH_SECS + 293 * EPOCH_SECS);
    fx.client.set_sub_epoch_secs(&asset, &600);
    let cfg = fx.client.sub_epoch_config(&asset).unwrap();
    assert_eq!(
        cfg.sub_epoch_secs, 900,
        "900 promoted over the original 300"
    );
    assert_eq!(
        fx.client.sub_epoch_secs_at(&asset, &hour),
        300,
        "recorded at its first post"
    );

    fx.client.build_hour(&asset, &hour);
    assert!(fx.client.is_final(&asset, &hour));
    let built = fx.client.signals(&asset, &hour).unwrap();
    assert_eq!(
        built.peg_ratio,
        sum / 12,
        "every one of the 12 sub-epochs rolled up"
    );
}

/// S2: an hour is posted through exactly one path; a sub-epoch post
/// against an hour already posted through the hourly fallback is
/// rejected, and the reverse.
#[test]
fn hour_already_posted_rejects_mixing_the_two_posting_paths() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;

    // Post the hour through the hourly fallback path first.
    env.ledger().set_timestamp(hour_close(hour));
    let mut s = signal_set(&env, hour, 10_000_000);
    s.liquidity_2pct = 500_000_000_000;
    s.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s);

    // A sub-epoch post for the SAME hour must now be rejected.
    let start = sub_start(hour, 0, SUB_EPOCH_SECS_DEFAULT);
    env.ledger().set_timestamp(start + SUB_EPOCH_SECS_DEFAULT);
    let mut sub_s = signal_set(&env, hour, 10_000_000);
    sub_s.liquidity_2pct = 500_000_000_000;
    sub_s.supply_change_bps = 0;
    let result = fx
        .client
        .try_post_sub_signals(&keeper, &asset, &hour, &0u32, &sub_s);
    assert_eq!(result, Err(Ok(crate::Error::HourAlreadyPosted)));

    // The reverse: a fresh hour posted via sub-epochs first, then an
    // hourly fallback attempt for the SAME hour must be rejected too.
    let hour2 = hour + 1;
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour2,
        0,
        SUB_EPOCH_SECS_DEFAULT,
        10_000_000,
    );
    env.ledger().set_timestamp(hour_close(hour2));
    let mut s2 = signal_set(&env, hour2, 10_000_000);
    s2.liquidity_2pct = 500_000_000_000;
    s2.supply_change_bps = 0;
    let result2 = fx.client.try_post_signals(&keeper, &asset, &s2);
    assert_eq!(result2, Err(Ok(crate::Error::HourAlreadyPosted)));
}

/// S4: a waiting hour never auto-promotes to Final before it is
/// built, even once the last POSTED sub-epoch's own dispute window
/// has closed, as long as at least one sub-epoch in the hour is
/// still unaccounted for (within its own backfill window).
#[test]
fn waiting_hour_never_auto_promotes_before_every_sub_epoch_is_decided() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;

    // Post sub 0 only; leave every other sub-epoch of this hour
    // unposted (there are 3,600 / 300 = 12 of them).
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        0,
        sub_epoch_secs,
        10_000_000,
    );

    // Advance well past sub 0's own pending_until (2 hours,
    // SIGNAL_DISPUTE_SECS), far enough that a naive "pending_until
    // derived from posted sub-epochs alone" implementation would
    // have auto-promoted the hour to Final by now.
    env.ledger()
        .set_timestamp(sub_start(hour, 0, sub_epoch_secs) + sub_epoch_secs + 7_200 + 3_600);

    // is_final and effective_window must report NOT Final throughout.
    assert!(
        !fx.client.is_final(&asset, &hour),
        "the hour must not read Final while sub-epochs 1..12 are still undecided"
    );
    let window = fx.client.effective_window(&asset, &hour, &1u32);
    assert_ne!(
        window.get(0).flatten(),
        Some(SlotState::Final),
        "effective_window must not report this hour Final either"
    );
}

/// S4: while waiting, the cover gate (via `live`) reads the hour's
/// provisional roll-up, not a zeroed slot.
#[test]
fn waiting_hour_holds_a_provisional_rollup_not_zeros() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;

    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        0,
        sub_epoch_secs,
        9_000_000, // a genuinely low peg_ratio, distinguishable from 0
    );

    let (_, signals, state) = fx.client.live(&asset).unwrap();
    assert_eq!(state, SlotState::Pending);
    assert_eq!(
        signals.peg_ratio, 9_000_000,
        "the provisional roll-up must reflect the one posted sub-epoch's real peg_ratio, not zero"
    );
}

/// Found on testnet (not caught by `waiting_hour_holds_a_provisional_
/// rollup_not_zeros` above, which only exercises `live()`'s own
/// per-sub-epoch read, never `Ring(asset)`'s own write path):
/// `Ring(asset)`'s own provisional slot for a waiting hour must hold
/// the real mean of whatever sub-epochs have POSTED (Pending or
/// Final), not an all-zero roll-up, the instant nothing has cleared
/// its own dispute window yet. Before the fix, `refresh_waiting_hour`
/// only fed Final sub-epochs into the provisional roll-up, so a
/// freshly posted (still Pending) sub-epoch's real data never reached
/// `Ring(asset)` at all until SIGNAL_DISPUTE_SECS later, and any
/// reader of `ring()` directly (`EventRegistry.cover_gate`'s own
/// `any_epoch_below_threshold`) saw peg_ratio 0, a false depeg.
#[test]
fn waiting_hour_s_ring_slot_holds_real_values_before_any_sub_epoch_clears_its_window() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;

    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        0,
        sub_epoch_secs,
        9_900_000,
    );
    post_sub(
        &env,
        &fx.client,
        &keeper,
        &asset,
        hour,
        1,
        sub_epoch_secs,
        10_100_000,
    );

    // Neither sub-epoch has cleared its own SIGNAL_DISPUTE_SECS
    // window yet (post_sub leaves `now` at each sub-epoch's own
    // close, never its own pending_until): both are still Pending.
    let ring = fx.client.ring(&asset);
    let slot = ring
        .iter()
        .find(|s| s.epoch == hour)
        .expect("a waiting hour with at least one posted sub-epoch must have a ring slot");
    assert_eq!(slot.state, SlotState::Pending);
    assert_eq!(
        slot.peg_ratio,
        (9_900_000 + 10_100_000) / 2,
        "the provisional roll-up must be the mean of the two POSTED (still Pending) \
         sub-epochs, not zero"
    );
}

/// Companion to the test above: with NOTHING posted for an hour at
/// all yet, its `Ring(asset)` slot must stay genuinely absent
/// (`epoch != hour`, the same representation a never-touched hour
/// already has), never a `Pending` slot holding an all-zero roll-up
/// that a reader could mistake for real (if implausible) data.
#[test]
fn an_hour_with_nothing_posted_yet_has_no_ring_slot_at_all() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let hour = REALISTIC_EPOCH_BASE;

    let ring = fx.client.ring(&asset);
    assert!(
        ring.iter().all(|s| s.epoch != hour),
        "an hour nobody has posted anything for must not appear in the ring at all"
    );
}

/// S4: with full coverage, `build_hour` writes Final with the
/// correctly computed roll-up for each field.
#[test]
fn build_hour_writes_final_with_full_coverage_and_correct_rollup() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12

    // Post all 12 sub-epochs with increasing peg_ratio so the mean is
    // easy to hand-compute: 10_000_000 + sub * 100_000, mean over
    // sub=0..12 is 10_000_000 + (0+1+...+11)/12 * 100_000
    // = 10_000_000 + 550_000 = 10_550_000.
    for sub in 0..per_hour {
        let peg_ratio = 10_000_000 + sub as i128 * 100_000;
        post_sub(
            &env,
            &fx.client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            peg_ratio,
        );
    }
    // Let the last sub-epoch's own dispute window close.
    env.ledger()
        .set_timestamp(sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + 7_200 + 1);
    fx.client.build_hour(&asset, &hour);

    assert!(fx.client.is_final(&asset, &hour));
    let ring = fx.client.ring(&asset);
    let slot = ring
        .iter()
        .find(|s| s.epoch == hour)
        .expect("built hour must be in the ring");
    assert_eq!(slot.state, SlotState::Final);
    assert_eq!(
        slot.peg_ratio, 10_550_000,
        "mean peg_ratio across all 12 sub-epochs"
    );
}

/// Section 21.3: the roll-up's correctness checked against an
/// INDEPENDENT offchain recomputation (`independent_rollup`, below),
/// not against a hand-simplified special case like the arithmetic
/// sequence `build_hour_writes_final_with_full_coverage_and_correct_
/// rollup` already covers. Every field varies independently per
/// sub-epoch here (not all equal, not a simple arithmetic progression),
/// so a bug that only cancels out under a symmetric input would still
/// be caught.
#[test]
fn build_hour_rollup_matches_an_independent_offchain_recomputation() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12

    // Deliberately irregular per sub-epoch data: peg_ratio oscillates,
    // liquidity_2pct is given in an order that is neither sorted nor
    // reverse sorted (so median actually has to sort), supply strictly
    // increases (so "last by sub order" is distinguishable from "max"
    // or "min"), redemption_net/clawback_amount/auth_revocations vary
    // so the sums are non-trivial.
    let peg_ratios: [i128; 12] = [
        10_000_000, 9_800_000, 10_200_000, 9_950_000, 10_100_000, 9_900_000, 10_050_000,
        10_000_000, 9_975_000, 10_025_000, 9_990_000, 10_010_000,
    ];
    let liquidity: [i128; 12] = [
        500_000_000_000,
        300_000_000_000,
        900_000_000_000,
        100_000_000_000,
        700_000_000_000,
        200_000_000_000,
        800_000_000_000,
        400_000_000_000,
        600_000_000_000,
        1_000_000_000_000,
        950_000_000_000,
        50_000_000_000,
    ];
    let redemption_net: [i128; 12] = [
        100, -50, 200, -300, 400, -500, 600, -700, 800, -900, 1_000, -1_100,
    ];
    let clawback_amount: [i128; 12] = [0, 0, 5_000, 0, 0, 0, 10_000, 0, 0, 0, 0, 2_000];
    let auth_revocations: [u32; 12] = [0, 1, 0, 0, 2, 0, 0, 0, 1, 0, 0, 1];
    let supply: [i128; 12] = [
        10_000_000_000_000,
        10_001_000_000_000,
        10_002_000_000_000,
        10_003_000_000_000,
        10_004_000_000_000,
        10_005_000_000_000,
        10_006_000_000_000,
        10_007_000_000_000,
        10_008_000_000_000,
        10_009_000_000_000,
        10_010_000_000_000,
        10_011_000_000_000,
    ];

    for sub in 0..per_hour as usize {
        let start = sub_start(hour, sub as u32, sub_epoch_secs);
        env.ledger().set_timestamp(start + sub_epoch_secs);
        let mut s = signal_set(&env, hour, peg_ratios[sub]);
        s.liquidity_2pct = liquidity[sub];
        s.redemption_net = redemption_net[sub];
        s.supply = supply[sub];
        s.supply_change_bps = 0;
        s.issuer_actions.clawback_amount = clawback_amount[sub];
        s.issuer_actions.auth_revocations = auth_revocations[sub];
        fx.client
            .post_sub_signals(&keeper, &asset, &hour, &(sub as u32), &s);
    }
    env.ledger()
        .set_timestamp(sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + 7_200 + 1);
    fx.client.build_hour(&asset, &hour);
    assert!(fx.client.is_final(&asset, &hour));

    let expected = independent_rollup(
        &peg_ratios,
        &liquidity,
        &redemption_net,
        &clawback_amount,
        &auth_revocations,
        supply[11], // last by sub order
        None,       // no previous built hour: supply_change_bps must be 0
    );

    let ring = fx.client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.peg_ratio, expected.peg_ratio);
    assert_eq!(slot.liquidity_2pct, expected.liquidity_2pct);
    assert_eq!(slot.redemption_net, expected.redemption_net);
    assert_eq!(slot.supply, expected.supply);
    assert_eq!(slot.supply_change_bps, expected.supply_change_bps);
    assert_eq!(slot.clawback_amount, expected.clawback_amount);
    assert_eq!(slot.auth_revocations, expected.auth_revocations);
}

/// An independent, offchain recomputation of the Section 5.9 S4
/// roll-up table, deliberately NOT sharing any code with
/// `lib.rs`'s own `roll_up_sub_slots`: this is a from-the-spec
/// reimplementation, so a bug specific to the contract's own
/// arithmetic (e.g. integer division order, a wrong sort) would not
/// be mirrored here and so would actually be caught.
struct ExpectedRollup {
    peg_ratio: i128,
    liquidity_2pct: i128,
    redemption_net: i128,
    supply: i128,
    supply_change_bps: i32,
    clawback_amount: i128,
    auth_revocations: u32,
}

#[allow(clippy::too_many_arguments)]
fn independent_rollup(
    peg_ratios: &[i128],
    liquidity: &[i128],
    redemption_net: &[i128],
    clawback_amount: &[i128],
    auth_revocations: &[u32],
    last_supply: i128,
    previous_built_supply: Option<i128>,
) -> ExpectedRollup {
    let n = peg_ratios.len() as i128;
    let peg_ratio = peg_ratios.iter().sum::<i128>() / n;

    let mut sorted_liquidity = liquidity.to_vec();
    sorted_liquidity.sort();
    let liquidity_2pct = sorted_liquidity[sorted_liquidity.len() / 2];

    let redemption_net_sum = redemption_net.iter().sum::<i128>();
    let clawback_amount_sum = clawback_amount.iter().sum::<i128>();
    let auth_revocations_sum = auth_revocations.iter().sum::<u32>();

    let supply_change_bps = match previous_built_supply {
        Some(prev) if prev > 0 => (((last_supply - prev) * 10_000) / prev) as i32,
        _ => 0,
    };

    ExpectedRollup {
        peg_ratio,
        liquidity_2pct,
        redemption_net: redemption_net_sum,
        supply: last_supply,
        supply_change_bps,
        clawback_amount: clawback_amount_sum,
        auth_revocations: auth_revocations_sum,
    }
}

/// S4: below `min_sub_coverage_bps`, the built hour's slot is Empty,
/// not Final.
#[test]
fn build_hour_writes_empty_below_min_sub_coverage_bps() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12

    // min_sub_coverage_bps is 7,500 (9 of 12); post only 8, leaving
    // the rest to go permanently missing past their own backfill
    // window.
    for sub in 0..8u32 {
        post_sub(
            &env,
            &fx.client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    // Advance past every remaining sub-epoch's own backfill window
    // (sub_backfill_secs, 2 hours) so they read PermanentlyMissing,
    // and past the last posted sub-epoch's own dispute window.
    let last_missing_close = sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs;
    env.ledger().set_timestamp(last_missing_close + 7_200 + 1);

    fx.client.build_hour(&asset, &hour);
    assert!(
        !fx.client.is_final(&asset, &hour),
        "8 of 12 (6,666 bps) is below the 7,500 bps coverage threshold"
    );
}

/// `SCORE_STALE_AFTER_EPOCHS` (not exported; mirrors this module's own
/// `SIGNAL_DISPUTE_SECS` comment at the top of the file): an asset
/// whose newest Final hour was built through the sub-epoch path, not
/// posted via the hourly fallback, reads fresh at the normal
/// structural lag that path carries (hour close, then the last
/// sub-epoch's own dispute window, then the time it takes a build to
/// land) and stale once that lag exceeds it - the exact gap this
/// revision's own `SCORE_STALE_AFTER_EPOCHS` was sized to cover.
/// Builds 168 epochs of real history through the cheap hourly
/// fallback path, then builds ONE further hour through sub-epochs
/// (the realistic shape: an asset that historically posted hourly,
/// like testnet USDC before the live poster's own build_hour fix),
/// and checks `score().stale` on both sides of the boundary against
/// that sub-epoch-built hour specifically, not an hourly one.
#[test]
fn score_reads_fresh_then_stale_for_a_sub_epoch_built_hour_at_the_score_threshold() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    // 168 epochs of ordinary hourly-fallback history, so scoring is
    // already active by the time the sub-epoch-built hour under test
    // arrives (same baseline every other test in this suite uses).
    super::fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        9_900_000,
        500_000_000_000,
    );

    let hour = 170u64;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12
    for sub in 0..per_hour {
        post_sub(
            &env,
            &fx.client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            9_900_000,
        );
    }
    // The hour's own close, plus the last sub-epoch's dispute window:
    // exactly the moment build_hour can first succeed.
    let built_at = sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + 7_200 + 1;
    env.ledger().set_timestamp(built_at);
    fx.client.build_hour(&asset, &hour);
    assert!(fx.client.is_final(&asset, &hour));
    assert_eq!(
        fx.client.score(&asset).epoch,
        hour,
        "score must have advanced to this sub-epoch-built hour"
    );
    assert!(
        !fx.client.score(&asset).stale,
        "fresh immediately after the sub-epoch-built hour lands Final"
    );

    // SCORE_STALE_AFTER_EPOCHS (5, not exported - see this test's own
    // doc comment): current_epoch - hour > 5 is the real condition;
    // one epoch before that boundary must still read fresh.
    env.ledger().set_timestamp((hour + 5) * EPOCH_SECS);
    assert!(
        !fx.client.score(&asset).stale,
        "must still read fresh exactly at the SCORE_STALE_AFTER_EPOCHS boundary"
    );

    env.ledger().set_timestamp((hour + 6) * EPOCH_SECS + 1);
    assert!(
        fx.client.score(&asset).stale,
        "must read stale once the gap exceeds SCORE_STALE_AFTER_EPOCHS"
    );
}

/// The same boundary as above, but for an hourly-posted asset (no
/// sub-epochs at all), proving `SCORE_STALE_AFTER_EPOCHS` applies
/// identically regardless of which posting path produced the newest
/// Final hour - this revision changes the threshold's VALUE, never
/// makes it path dependent.
#[test]
fn score_reads_fresh_then_stale_for_an_hourly_posted_asset_at_the_score_threshold() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    super::fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        169,
        9_900_000,
        500_000_000_000,
    );
    let last_epoch = fx.client.score(&asset).epoch;
    assert!(!fx.client.score(&asset).stale);

    env.ledger().set_timestamp((last_epoch + 5) * EPOCH_SECS);
    assert!(
        !fx.client.score(&asset).stale,
        "must still read fresh exactly at the SCORE_STALE_AFTER_EPOCHS boundary"
    );

    env.ledger()
        .set_timestamp((last_epoch + 6) * EPOCH_SECS + 1);
    assert!(
        fx.client.score(&asset).stale,
        "must read stale once the gap exceeds SCORE_STALE_AFTER_EPOCHS"
    );
}

/// S4, I23: a sub-epoch resolved Overturned never contributes to the
/// built hour, and the hour still builds correctly from the remaining
/// Final sub-epochs.
#[test]
fn a_sub_epoch_resolved_overturned_never_contributes_to_the_built_hour() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;

    for sub in 0..per_hour {
        // Sub 0 posts a wildly wrong peg_ratio (1); every other
        // sub-epoch posts 10_000_000, so if sub 0's overturned data
        // leaked into the roll-up, the mean would be obviously wrong.
        let peg_ratio = if sub == 0 { 1 } else { 10_000_000 };
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            peg_ratio,
        );
    }
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(
        &asset,
        &hour,
        &0u32,
        &false, // disputer wins: sub 0 is overturned
        &dummy_hash(&env),
    );

    env.ledger()
        .set_timestamp(sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + 7_200 + 1);
    client.build_hour(&asset, &hour);

    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    // Coverage: 11 of 12 Final (sub 0 overturned, never reposted, now
    // PermanentlyMissing) = 9,166 bps, still above the 7,500 threshold.
    assert_eq!(slot.state, SlotState::Final);
    assert_eq!(
        slot.peg_ratio, 10_000_000,
        "sub 0's overturned peg_ratio (1) must never enter the mean"
    );
}

/// Section 21.3, Section 5.9 S3: a disputed sub-epoch outliving the
/// 60 slot ring (5 hours of wall-clock span), with the committee's
/// ruling arriving after `WINDOW_SECS` (72h) and a cure-style
/// checkpoint read in between, UPHOLDING the keeper. `HeldHour`
/// preserves every one of the hour's OTHER 11 sub-epochs' own data
/// (copied out the moment the dispute opened) and the disputed one's
/// own data too (captured then, in case the ruling upholds it), so
/// even though `Sub(asset)`'s ring has long since rotated past this
/// entire hour's footprint by the time the ruling lands, the hour
/// still builds Final with all 12 values once the dispute resolves,
/// matching an independent offchain recomputation exactly.
#[test]
fn a_dispute_outliving_the_ring_upheld_at_day_6_still_builds_final_with_all_12() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12
    let hour = REALISTIC_EPOCH_BASE;

    let peg_ratios: [i128; 12] = [
        10_000_000, 9_950_000, 10_050_000, 9_900_000, 10_100_000, 9_975_000, 10_025_000, 9_990_000,
        10_010_000, 9_960_000, 10_040_000, 10_000_000,
    ];
    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            peg_ratios[sub as usize],
        );
    }
    post_sub(
        &env,
        &client,
        &keeper,
        &asset,
        hour,
        0,
        sub_epoch_secs,
        peg_ratios[0],
    );
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    assert!(!client.is_final(&asset, &hour));

    // Rotate Sub(asset) a full 5 hours past hour's own slots, entirely
    // via later hours' own prompt, un-backfilled posts.
    for later_hour in (hour + 1)..=(hour + 5) {
        for sub in 0..per_hour {
            post_sub(
                &env,
                &client,
                &keeper,
                &asset,
                later_hour,
                sub,
                sub_epoch_secs,
                10_000_000,
            );
        }
    }
    // Sub(asset)'s own ring no longer holds ANY of hour's own slots
    // (all 12 rotated out together, 5 hours later). sub_peg_ratios
    // must still report every UNDISPUTED sub-epoch's real value,
    // through HeldHour (captured when sub 0's dispute first opened);
    // only sub 0 itself, currently Disputed, reads None.
    let ratios = client.sub_peg_ratios(&asset, &hour);
    for sub in 1..per_hour {
        assert_eq!(
            ratios.get(sub).flatten(),
            Some(peg_ratios[sub as usize]),
            "sub {sub}'s own value must still read through HeldHour, not go blind just \
             because Sub(asset)'s ring has rotated past it"
        );
    }
    assert_eq!(
        ratios.get(0).flatten(),
        None,
        "sub 0 is currently Disputed; its value must read None while the ruling is pending"
    );

    // A cure-style checkpoint read in between, well before the
    // ruling: still Disputed/not-Final, never prematurely decided.
    let window_before = client.effective_window(&asset, &hour, &1u32);
    assert_ne!(window_before.get(0).flatten(), Some(SlotState::Final));

    // The ruling arrives at day 6 (well past WINDOW_SECS's 72h, still
    // inside SIGNAL_DISPUTE_RULING_SECS's 6 day deadline), upholding
    // the keeper.
    env.ledger()
        // One second inside the ruling deadline (D2: a ruling at the deadline itself is rejected).
        .set_timestamp(sub_start(hour, 0, sub_epoch_secs) + sub_epoch_secs + 6 * 86_400 - 1);
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &true, &dummy_hash(&env));

    assert!(
        client.is_final(&asset, &hour),
        "HeldHour must give build_hour every sub-epoch's data back, including the upheld one"
    );
    let expected = independent_rollup(
        &peg_ratios,
        &[500_000_000_000i128; 12],
        &[0i128; 12],
        &[0i128; 12],
        &[0u32; 12],
        10_000_000_000_000,
        None,
    );
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.state, SlotState::Final);
    assert_eq!(slot.peg_ratio, expected.peg_ratio);
    assert_eq!(slot.liquidity_2pct, expected.liquidity_2pct);

    // Never flips: calling build_hour again changes nothing further.
    client.build_hour(&asset, &hour);
    let ring_again = client.ring(&asset);
    let slot_again = ring_again.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot_again.peg_ratio, slot.peg_ratio);
    assert_eq!(slot_again.state, SlotState::Final);
}

/// Section 21.3, Section 5.9 S2, S3: the same scenario, but the
/// ruling at day 6 OVERTURNS the disputed sub-epoch. `HeldHour`'s own
/// entry for it is removed on resolution (I23: an overturned
/// sub-epoch never contributes), but the hour does NOT build the
/// instant it is overturned: the overturned sub-epoch's own repost
/// window (anchored to the overturn itself, not its long-past
/// original close, Section 5.9 S2) is still open, so it reads
/// AwaitingRepost, not PermanentlyMissing, and the hour must wait.
/// Only once that window closes with no repost does it settle to
/// PermanentlyMissing and the hour builds from the remaining 11 —
/// still above the 7,500 bps coverage threshold (11 of 12 = 9,166
/// bps) — so it reads Final, built from 11 values, not 12, and not
/// Empty.
#[test]
fn a_dispute_outliving_the_ring_overturned_at_day_6_builds_from_the_other_11() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12
    let hour = REALISTIC_EPOCH_BASE;

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    // Sub 0 posts a wildly wrong value; if it leaked into the roll-up
    // despite being overturned, the mean would be obviously wrong.
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));

    for later_hour in (hour + 1)..=(hour + 5) {
        for sub in 0..per_hour {
            post_sub(
                &env,
                &client,
                &keeper,
                &asset,
                later_hour,
                sub,
                sub_epoch_secs,
                10_000_000,
            );
        }
    }
    assert!(!client.is_final(&asset, &hour));

    env.ledger()
        // One second inside the ruling deadline (D2: a ruling at the deadline itself is rejected).
        .set_timestamp(sub_start(hour, 0, sub_epoch_secs) + sub_epoch_secs + 6 * 86_400 - 1);
    // Disputer wins: sub 0 is overturned. The hour does NOT build yet
    // (Section 5.9 S2, footprint-fix revision): sub 0 is now
    // AwaitingRepost, not PermanentlyMissing, since its own repost
    // window (anchored to this overturn, not its long-past original
    // close) has just opened.
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    assert!(
        !client.is_final(&asset, &hour),
        "the hour must wait out sub 0's own repost window before building, not build \
         the instant it is overturned"
    );

    // Past sub 0's own repost window (sub_backfill_secs past the
    // overturn) with no repost: sub 0 settles to PermanentlyMissing,
    // and the hour is free to build without it.
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 7_200 + 1);
    client.build_hour(&asset, &hour);

    assert!(
        client.is_final(&asset, &hour),
        "11 of 12 (9,166 bps) is still above the 7,500 bps coverage threshold"
    );
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.state, SlotState::Final);
    assert_eq!(
        slot.peg_ratio, 10_000_000,
        "sub 0's overturned value (1) must never enter the mean, HeldHour copy or not"
    );

    // Never flips.
    client.build_hour(&asset, &hour);
    let ring_again = client.ring(&asset);
    let slot_again = ring_again.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot_again.peg_ratio, 10_000_000);
    assert_eq!(slot_again.state, SlotState::Final);
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): a FAST ruling, well
/// within `sub_backfill_secs` of both the original close and the
/// overturn itself, so the repost lands while `Sub(asset)`'s own ring
/// still holds sub 0's position. The repost goes into the ring
/// directly (not `HeldHour`), and the hour builds with the corrected
/// value once the repost's own Pending window clears.
#[test]
fn a_fast_ruling_repost_lands_in_the_ring_and_the_hour_builds_corrected() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;

    // Sub 0 posts wrong first (earliest timestamp), then subs 1..11
    // post normally: the clock only ever moves forward, the same
    // convention every other test in this file relies on.
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    assert!(
        !client.is_final(&asset, &hour),
        "sub 0 is awaiting repost; the hour must not build yet"
    );

    // Repost immediately (0 seconds past the overturn, well within
    // sub_backfill_secs), still earlier than sub 1's own close, so
    // the clock still only moves forward from here.
    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    client.post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);
    assert!(
        !client.is_final(&asset, &hour),
        "the repost is freshly Pending; the hour must wait for it too"
    );

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + crate::SIGNAL_DISPUTE_SECS + 1);
    client.build_hour(&asset, &hour);

    assert!(client.is_final(&asset, &hour));
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(
        slot.peg_ratio, 10_000_000,
        "all 12 sub-epochs, including the corrected repost, must contribute"
    );
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): a SLOW ruling,
/// landing well after `Sub(asset)`'s own 5 hour ring span has rotated
/// past sub 0's own position. The repost must go into `HeldHour`
/// instead (the ring position now belongs to someone else), and the
/// hour still builds with the corrected value once the repost's own
/// Pending window clears.
#[test]
fn a_slow_ruling_repost_lands_in_heldhour_and_the_hour_builds_corrected() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;
    let hour = REALISTIC_EPOCH_BASE;

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));

    // Rotate Sub(asset) a full 5 hours past hour's own slots via
    // later hours' own prompt posts, exactly the convention the
    // existing "outliving the ring" tests already use.
    for later_hour in (hour + 1)..=(hour + 5) {
        for sub in 0..per_hour {
            post_sub(
                &env,
                &client,
                &keeper,
                &asset,
                later_hour,
                sub,
                sub_epoch_secs,
                10_000_000,
            );
        }
    }

    // The ruling lands at day 6, well past the ring's own 5 hour span
    // AND past sub 0's own original close + sub_backfill_secs.
    env.ledger()
        // One second inside the ruling deadline (D2: a ruling at the deadline itself is rejected).
        .set_timestamp(sub_start(hour, 0, sub_epoch_secs) + sub_epoch_secs + 6 * 86_400 - 1);
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    assert!(!client.is_final(&asset, &hour));

    // Repost immediately (0 seconds past the overturn): accepted,
    // since the window is anchored to the overturn, not the
    // long-past original close.
    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    client.post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);

    env.ledger()
        .set_timestamp(env.ledger().timestamp() + crate::SIGNAL_DISPUTE_SECS + 1);
    client.build_hour(&asset, &hour);

    assert!(client.is_final(&asset, &hour));
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(
        slot.peg_ratio, 10_000_000,
        "the HeldHour repost must contribute the corrected value, same as a ring repost would"
    );
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): no repost arrives
/// at all. The hour waits out sub 0's own repost window
/// (`overturned_at + sub_backfill_secs`), then builds without it,
/// from the other 11.
#[test]
fn no_repost_the_hour_builds_without_the_overturned_sub_epoch_once_its_window_closes() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;

    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    let overturned_at = env.ledger().timestamp();
    assert!(
        !client.is_final(&asset, &hour),
        "awaiting repost, must not build yet"
    );

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    // Past every other sub's own pending_until AND sub 0's own
    // repost window (the later of the two): both must have cleared
    // before the hour is free to build.
    let last_sub_pending_until =
        sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + crate::SIGNAL_DISPUTE_SECS;
    let deadline = (overturned_at + 7_200).max(last_sub_pending_until);

    env.ledger().set_timestamp(deadline);
    assert!(
        !client.is_final(&asset, &hour),
        "right at the deadline (not yet past it): must not build yet"
    );

    env.ledger().set_timestamp(deadline + 1);
    client.build_hour(&asset, &hour);
    assert!(
        client.is_final(&asset, &hour),
        "every other sub is settled and sub 0's own repost window has closed with no \
         repost; the hour must build without it"
    );
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.peg_ratio, 10_000_000);
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): a repost attempted
/// AFTER its own window has closed is rejected, not silently
/// accepted into a slot the build has already moved past.
#[test]
fn a_repost_after_its_own_deadline_is_rejected() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    let overturned_at = env.ledger().timestamp();

    env.ledger().set_timestamp(overturned_at + 7_200 + 1);
    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    let result = client.try_post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);
    assert!(
        result.is_err(),
        "a repost past its own window must be rejected, not accepted"
    );
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): the repost itself
/// is overturned a second time. No further repost is accepted for
/// this sub-epoch: the longest the hour can wait on it is two dispute
/// cycles (post, dispute, overturn, repost, dispute, overturn again),
/// never more.
#[test]
fn a_repost_overturned_a_second_time_gets_no_further_repost() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));

    // First repost: also wrong, immediately disputed and overturned
    // again.
    let mut wrong_again = signal_set(&env, hour, 2);
    wrong_again.liquidity_2pct = 500_000_000_000;
    wrong_again.supply_change_bps = 0;
    client.post_sub_signals(&keeper, &asset, &hour, &0u32, &wrong_again);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    let second_overturned_at = env.ledger().timestamp();

    // A second repost attempt, immediately, well within what would
    // have been a fresh window: must still be rejected, since this
    // overturn's own record belongs to the REPOST, which has no
    // repost of its own.
    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    let result = client.try_post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);
    assert!(
        result.is_err(),
        "the repost's own overturn gets no second repost"
    );

    let last_sub_pending_until =
        sub_start(hour, per_hour - 1, sub_epoch_secs) + sub_epoch_secs + crate::SIGNAL_DISPUTE_SECS;
    env.ledger()
        .set_timestamp((second_overturned_at + 7_200).max(last_sub_pending_until) + 1);
    client.build_hour(&asset, &hour);
    assert!(
        client.is_final(&asset, &hour),
        "the hour must build without sub 0 once its own (second) repost window closes"
    );
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.peg_ratio, 10_000_000);
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): the slot-ownership
/// check (`storage::sub_slot_still_belongs_to`) must correctly refuse
/// a direct ring write once a DIFFERENT, newer sub-epoch has rotated
/// into that exact ring position. Verified directly against the
/// storage layer (not through `post_sub_signals`, which never
/// reaches this unsafe path once the ownership check is in place):
/// this test is the one that would fail loudly if that check were
/// ever removed or bypassed.
#[test]
fn slot_ownership_check_refuses_a_write_into_a_rotated_slot() {
    let env = Env::default();
    let (client, _governor, asset) = setup_with_governor_and_asset(&env);
    let contract_id = client.address.clone();
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let hour = REALISTIC_EPOCH_BASE;
    let original_sub_start = sub_start(hour, 0, sub_epoch_secs);

    env.as_contract(&contract_id, || {
        // Nothing posted yet, still well within the original
        // sub-epoch's own 5 hour span: the position is Empty, and
        // belongs to the original sub-epoch (nothing else could own
        // it yet).
        assert!(crate::storage::sub_slot_still_belongs_to(
            &env,
            &asset,
            original_sub_start,
            original_sub_start,
        ));

        // A newer sub-epoch, one full ring rotation (5 hours) later,
        // claims the SAME ring position.
        let rotated_sub_start = original_sub_start + 5 * EPOCH_SECS;
        let mut s = signal_set(&env, hour, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        let wrote = crate::storage::write_sub_slot_entry(
            &env,
            &asset,
            rotated_sub_start,
            &s,
            SlotState::Pending,
            rotated_sub_start + sub_epoch_secs,
        );
        assert!(wrote, "the newer sub-epoch's own write must succeed");

        assert!(
            !crate::storage::sub_slot_still_belongs_to(
                &env,
                &asset,
                original_sub_start,
                rotated_sub_start,
            ),
            "the original sub-epoch's own position now belongs to the newer one; a \
             repost must never be allowed to overwrite it"
        );
    });
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): the ownership check
/// must also refuse a position that is still `Empty` but whose own
/// 5 hour span has already moved on to a LATER sub-epoch that simply
/// was never posted. `HeldHour` keeps the repost safe either way
/// (this is never a data-loss case), but the ring write itself must
/// never land on a position that no longer belongs to the sub-epoch
/// being reposted, posted or not.
#[test]
fn slot_ownership_check_refuses_an_empty_position_whose_span_has_moved_on() {
    let env = Env::default();
    let (client, _governor, asset) = setup_with_governor_and_asset(&env);
    let contract_id = client.address.clone();
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let hour = REALISTIC_EPOCH_BASE;
    let original_sub_start = sub_start(hour, 0, sub_epoch_secs);

    env.as_contract(&contract_id, || {
        // Nothing was ever posted to the position a later sub-epoch
        // (one full 5 hour rotation on) would occupy: it reads
        // Empty, same as it always has, but by the time `now` is
        // past the ORIGINAL sub-epoch's own 5 hour span, this
        // position no longer belongs to it.
        let now = original_sub_start + 5 * EPOCH_SECS + 1;
        assert!(
            !crate::storage::sub_slot_still_belongs_to(&env, &asset, original_sub_start, now),
            "an Empty position whose span has already moved on to a later, \
             never-posted sub-epoch must not be treated as still belonging to the \
             original one"
        );
    });
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): the end-to-end
/// version of the Empty-but-span-elapsed case above, through `post_
/// sub_signals` itself rather than the storage layer directly. A
/// slow ruling lands well past sub 0's own 5 hour ring span, but
/// NOTHING was ever posted to the later sub-epoch that position now
/// belongs to (unlike `a_slow_ruling_repost_lands_in_heldhour_and_
/// the_hour_builds_corrected`, which rotates the ring with real,
/// posted data): the position reads Empty, not occupied by a
/// different sub_start, and the repost must still go to `HeldHour`,
/// never write into that Empty-but-no-longer-owned position.
#[test]
fn a_slow_ruling_repost_still_uses_heldhour_when_the_newer_position_was_never_posted() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;
    let hour = REALISTIC_EPOCH_BASE;

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));

    // Advance the clock directly past sub 0's own 5 hour ring span
    // and well into the ruling deadline, WITHOUT posting anything
    // else: the position sub 0 occupied is still Empty, just no
    // longer within its own span.
    let sub0_start = sub_start(hour, 0, sub_epoch_secs);
    env.ledger().set_timestamp(sub0_start + 6 * 86_400);
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));
    assert!(!client.is_final(&asset, &hour));

    // Repost immediately (0 seconds past the overturn): accepted, and
    // must land in HeldHour, not overwrite whatever that Empty
    // position's own current rotation would otherwise suggest.
    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    client.post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);

    env.ledger()
        .set_timestamp(env.ledger().timestamp() + crate::SIGNAL_DISPUTE_SECS + 1);
    client.build_hour(&asset, &hour);

    assert!(client.is_final(&asset, &hour));
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(
        slot.peg_ratio, 10_000_000,
        "the HeldHour repost must contribute the corrected value even when the ring \
         position it would have otherwise used was merely Empty, not occupied by a \
         different sub-epoch"
    );
}

/// Section 5.9 S6: keeper pay for an overturned-then-reposted
/// sub-epoch. The overturned post is never paid (its own keeper is
/// slashed, not rewarded); the repost is paid exactly once, when the
/// hour builds with it included.
#[test]
fn keeper_pay_never_pays_the_overturned_post_and_pays_the_repost_once() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(crate::mocks::MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&super::asset_config(&env, &asset, &issuer));
    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);

    let hour = REALISTIC_EPOCH_BASE;
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;
    let keeper = Address::generate(&env);

    post_sub(&env, &client, &keeper, &asset, hour, 0, sub_epoch_secs, 1);
    let disputer = Address::generate(&env);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &dummy_hash(&env));

    let mut corrected = signal_set(&env, hour, 10_000_000);
    corrected.liquidity_2pct = 500_000_000_000;
    corrected.supply_change_bps = 0;
    client.post_sub_signals(&keeper, &asset, &hour, &0u32, &corrected);

    for sub in 1..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + crate::SIGNAL_DISPUTE_SECS + 1);
    client.build_hour(&asset, &hour);
    assert!(client.is_final(&asset, &hour));

    assert_eq!(
        staking_client.reward_keeper_sub_epoch_count(&keeper),
        per_hour,
        "exactly {per_hour} sub-epochs paid: 11 original posts plus the one repost, \
         never the overturned original on top of it"
    );
}

/// Section 5.9 S3: two sub-epochs of the SAME hour disputed at once,
/// both outliving the ring, ruled in opposite directions. `HeldHour`
/// holds both plus every undisputed sub-epoch; each dispute's own
/// resolution only ever touches its OWN entry.
#[test]
fn two_disputes_in_the_same_hour_resolve_independently() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12
    let hour = REALISTIC_EPOCH_BASE;

    for sub in 2..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    // Sub 0: will be upheld. Sub 1: will be overturned (wildly wrong).
    post_sub(
        &env,
        &client,
        &keeper,
        &asset,
        hour,
        0,
        sub_epoch_secs,
        9_000_000,
    );
    post_sub(&env, &client, &keeper, &asset, hour, 1, sub_epoch_secs, 1);

    let disputer_0 = Address::generate(&env);
    let disputer_1 = Address::generate(&env);
    client.dispute_sub_signals(&disputer_0, &asset, &hour, &0u32, &dummy_hash(&env));
    client.dispute_sub_signals(&disputer_1, &asset, &hour, &1u32, &dummy_hash(&env));

    for later_hour in (hour + 1)..=(hour + 5) {
        for sub in 0..per_hour {
            post_sub(
                &env,
                &client,
                &keeper,
                &asset,
                later_hour,
                sub,
                sub_epoch_secs,
                10_000_000,
            );
        }
    }
    assert!(!client.is_final(&asset, &hour));

    env.ledger()
        // One second inside the ruling deadline (D2: a ruling at the deadline itself is rejected).
        .set_timestamp(sub_start(hour, 1, sub_epoch_secs) + sub_epoch_secs + 6 * 86_400 - 1);
    // Sub 0 upheld (keeper wins); the hour must still wait on sub 1.
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &true, &dummy_hash(&env));
    assert!(
        !client.is_final(&asset, &hour),
        "sub 1's own dispute is still open; the hour must not build yet"
    );

    // Sub 1 overturned (disputer wins). The hour does NOT build yet
    // (Section 5.9 S2, footprint-fix revision): sub 1 is now
    // AwaitingRepost, not PermanentlyMissing, since its own repost
    // window (anchored to this overturn) has just opened.
    client.resolve_sub_signal_dispute(&asset, &hour, &1u32, &false, &dummy_hash(&env));
    assert!(
        !client.is_final(&asset, &hour),
        "sub 1 is awaiting repost; the hour must wait out its own repost window first"
    );

    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 7_200 + 1);
    client.build_hour(&asset, &hour);

    assert!(
        client.is_final(&asset, &hour),
        "11 of 12 (sub 1 excluded) is still above the 7,500 bps coverage threshold"
    );
    let ring = client.ring(&asset);
    let slot = ring.iter().find(|s| s.epoch == hour).unwrap();
    assert_eq!(slot.state, SlotState::Final);
    // Mean of ten 10_000_000 values plus sub 0's upheld 9_000_000,
    // over 11 sub-epochs: (10 * 10_000_000 + 9_000_000) / 11.
    assert_eq!(slot.peg_ratio, (10 * 10_000_000 + 9_000_000) / 11);
}

/// Section 5.9 S3: the copy-out at dispute-open time (`ensure_hour_
/// held`'s own ring scan plus the disputed sub-epoch's own insert)
/// measured against `sylox_types::network_limits`, the same
/// `cost_estimate` convention `budget_test.rs` uses elsewhere. Scans
/// at most 12 sub-epochs (the 300s default's own `per_hour`) and
/// writes one `HeldHour` entry; this is a one-time cost paid only the
/// first time any dispute opens against a given hour, not on every
/// sub-epoch post.
#[test]
fn budget_dispute_copy_out_at_the_300s_default() {
    let env = Env::default();
    let (client, governor, asset) = setup_with_governor_and_asset(&env);
    let keeper = Address::generate(&env);
    let committee = Address::generate(&env);
    crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    let sub_epoch_secs = SUB_EPOCH_SECS_DEFAULT;
    let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32; // 12
    let hour = REALISTIC_EPOCH_BASE;

    for sub in 0..per_hour {
        post_sub(
            &env,
            &client,
            &keeper,
            &asset,
            hour,
            sub,
            sub_epoch_secs,
            10_000_000,
        );
    }
    let disputer = Address::generate(&env);

    // The measured call: the FIRST dispute against this hour, which
    // pays the full ensure_hour_held scan (every other sub-epoch
    // already Final gets copied in the same call).
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &dummy_hash(&env));

    let estimate = env.cost_estimate();
    let resources = estimate.resources();
    extern crate std;
    std::println!(
        "--- dispute_sub_signals, first dispute against a full 12 sub-epoch hour (copy-out) ---"
    );
    std::println!("  instructions:      {}", resources.instructions);
    std::println!("  write_entries:     {}", resources.write_entries);
    std::println!("  write_bytes:       {}", resources.write_bytes);
    assert!(
        resources.write_bytes < sylox_types::network_limits::CONTRACT_DATA_ENTRY_SIZE_BYTES as u32,
        "HeldHour's own entry (at most 12 HeldSubSlot-sized rows) must fit a single ledger entry"
    );
}

/// A fixture for the history-TTL tests below: a registered
/// `MockGovernor` and `MockStaking`, one keeper per sub-epoch, and an
/// `at` that moves time and ledger sequence together (5 seconds per
/// ledger), so TTLs are checked against real elapsed time.
struct TtlFx<'a> {
    env: &'a Env,
    client: RiskOracleClient<'a>,
    contract_id: Address,
    staking: crate::mocks::MockStakingClient<'a>,
    governor: Address,
    committee: Address,
    asset: Address,
    keepers: std::vec::Vec<Address>,
    disputer: Address,
}

extern crate std;

const TTL_HOUR: u64 = REALISTIC_EPOCH_BASE;

impl<'a> TtlFx<'a> {
    fn new(env: &'a Env) -> Self {
        env.mock_all_auths();
        let staking = env.register(crate::mocks::MockStaking, ());
        let governor = env.register(MockGovernor, ());
        let contract_id = env.register(
            RiskOracle,
            (governor.clone(), Address::generate(env), staking.clone()),
        );
        let client = RiskOracleClient::new(env, &contract_id);
        let asset = Address::generate(env);
        client.add_asset(&super::asset_config(env, &asset, &Address::generate(env)));
        let per_hour = (EPOCH_SECS / SUB_EPOCH_SECS_DEFAULT) as usize;
        TtlFx {
            env,
            client,
            contract_id,
            staking: crate::mocks::MockStakingClient::new(env, &staking),
            governor,
            committee: Address::generate(env),
            asset,
            keepers: (0..per_hour).map(|_| Address::generate(env)).collect(),
            disputer: Address::generate(env),
        }
    }

    fn at(&self, t: u64) {
        self.env.ledger().set_timestamp(t);
        self.env.ledger().set_sequence_number((t / 5) as u32);
        // MockGovernor keeps its committee in temporary storage, which
        // the test host expires as the ledger ages (test setup only).
        crate::mocks::MockGovernorClient::new(self.env, &self.governor)
            .set_committee(&self.committee);
    }

    fn signals(&self, peg_ratio: i128, tag: u8) -> sylox_types::SignalSet {
        let mut s = signal_set(self.env, TTL_HOUR, peg_ratio);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        s.inputs_hash = soroban_sdk::BytesN::from_array(self.env, &[tag; 32]);
        s
    }

    /// Every sub-epoch of the hour at its own close, sub 0 wrong. Each
    /// gets inputs hash `[sub + 1; 32]`.
    fn post_hour(&self) {
        for sub in 0..self.keepers.len() as u32 {
            self.at(sub_start(TTL_HOUR, sub, SUB_EPOCH_SECS_DEFAULT) + SUB_EPOCH_SECS_DEFAULT);
            let peg_ratio = if sub == 0 { 1 } else { 10_000_000 };
            self.client.post_sub_signals(
                &self.keepers[sub as usize],
                &self.asset,
                &TTL_HOUR,
                &sub,
                &self.signals(peg_ratio, sub as u8 + 1),
            );
        }
    }

    fn sub_signals_ttl(&self, sub: u32) -> u32 {
        use soroban_sdk::testutils::storage::Persistent as _;
        let key = crate::storage::DataKey::SubSignals(self.asset.clone(), TTL_HOUR, sub);
        self.env.as_contract(&self.contract_id, || {
            self.env.storage().persistent().get_ttl(&key)
        })
    }
}

/// The longest wait the code can reach today on one contested
/// sub-epoch: a second dispute cycle needs the repost to land in
/// `Sub(asset)`'s ring (a held repost can't be disputed), so the first
/// ruling must come within the ring's 5 hour span; the repost's dispute
/// then times out as late as `SubDispute` lives. The hour builds about
/// 7.2 days after its first post, with every sibling still live, the
/// full inputs hash, and every keeper behind a Final sub-epoch paid.
#[test]
fn a_double_contested_hour_builds_in_full_at_a_late_timeout_about_7_2_days_later() {
    use soroban_sdk::{Bytes, BytesN};

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let hour = TTL_HOUR;
    let per_hour = fx.keepers.len() as u32;
    fx.post_hour();
    let first_post = sub_start(hour, 0, SUB_EPOCH_SECS_DEFAULT) + SUB_EPOCH_SECS_DEFAULT;
    let ring_span_end = sub_start(hour, 0, SUB_EPOCH_SECS_DEFAULT) + crate::SUB_RING_SPAN_SECS;

    // Cycle 1: disputed at the hour's close, overturned late enough that
    // the repost only just lands in the ring.
    fx.at(hour_close(hour));
    fx.client
        .dispute_sub_signals(&fx.disputer, &fx.asset, &hour, &0u32, &dummy_hash(&env));
    fx.at(ring_span_end - 1_200);
    fx.client
        .resolve_sub_signal_dispute(&fx.asset, &hour, &0u32, &false, &dummy_hash(&env));

    // Cycle 2: the repost, disputed, then timed out as late as
    // `SubDispute` (the ruling window plus 1 day) still allows.
    let reposter = Address::generate(&env);
    fx.at(ring_span_end - 600);
    fx.client.post_sub_signals(
        &reposter,
        &fx.asset,
        &hour,
        &0u32,
        &fx.signals(10_000_000, 100),
    );
    let redisputed_at = ring_span_end - 540;
    fx.at(redisputed_at);
    fx.client
        .dispute_sub_signals(&fx.disputer, &fx.asset, &hour, &0u32, &dummy_hash(&env));
    let built_at = redisputed_at + crate::SIGNAL_DISPUTE_RULING_SECS + 86_400 - 600;
    fx.at(built_at);
    let days = (built_at - first_post) as f64 / 86_400.0;
    assert!(
        days > 7.15,
        "the build lands {days:.2} days after the first post"
    );

    for sub in 1..per_hour {
        assert!(
            fx.sub_signals_ttl(sub) > 0,
            "SubSignals sub {sub} expired before the build, {days:.2} days after its post"
        );
    }

    fx.client
        .resolve_sub_dispute_timeout(&fx.asset, &hour, &0u32);
    assert!(
        fx.client.is_final(&fx.asset, &hour),
        "the hour builds at the timeout"
    );

    // The full inputs hash: the repost's, then every sibling's, in order.
    let mut combined = Bytes::new(&env);
    combined.append(&Bytes::from_array(&env, &[100u8; 32]));
    for sub in 1..per_hour {
        combined.append(&Bytes::from_array(&env, &[sub as u8 + 1; 32]));
    }
    let expected: BytesN<32> = env.crypto().sha256(&combined).into();
    assert_eq!(
        fx.client.signals(&fx.asset, &hour).unwrap().inputs_hash,
        expected
    );

    // Every keeper behind a Final sub-epoch is paid once; the
    // overturned first post is not.
    for sub in 1..per_hour {
        assert_eq!(
            fx.staking
                .reward_keeper_sub_epoch_count(&fx.keepers[sub as usize]),
            1,
            "keeper of sub {sub}"
        );
    }
    assert_eq!(fx.staking.reward_keeper_sub_epoch_count(&reposter), 1);
    assert_eq!(fx.staking.reward_keeper_sub_epoch_count(&fx.keepers[0]), 0);
}

/// An overturn re-extends every other `SubSignals` entry of the hour to
/// the full history lifetime, the safeguard for the spec's two-cycle
/// case (Section 5.9 S2). Overturned near its 6 day ruling deadline,
/// the siblings are down to about 4 days, below the 8 day threshold.
#[test]
fn an_overturn_re_extends_every_sibling_sub_signals_to_the_history_lifetime() {
    use sylox_types::ttl::{HISTORY_TTL_EXTEND_TO, HISTORY_TTL_THRESHOLD};

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let hour = TTL_HOUR;
    let per_hour = fx.keepers.len() as u32;
    fx.post_hour();

    let disputed_at = hour_close(hour);
    fx.at(disputed_at);
    fx.client
        .dispute_sub_signals(&fx.disputer, &fx.asset, &hour, &0u32, &dummy_hash(&env));
    fx.at(disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS - 60);
    for sub in 1..per_hour {
        assert!(
            fx.sub_signals_ttl(sub) < HISTORY_TTL_THRESHOLD,
            "sub {sub} must be below the threshold before the overturn"
        );
    }

    fx.client
        .resolve_sub_signal_dispute(&fx.asset, &hour, &0u32, &false, &dummy_hash(&env));

    for sub in 1..per_hour {
        assert_eq!(
            fx.sub_signals_ttl(sub),
            HISTORY_TTL_EXTEND_TO,
            "sub {sub} must be re-extended by the overturn"
        );
    }
    let overturned = env.as_contract(&fx.contract_id, || {
        crate::storage::get_sub_signals(&env, &fx.asset, hour, 0)
    });
    assert!(
        overturned.is_none(),
        "the overturned sub-epoch itself moved to history"
    );
}

// -- held repost disputes (#92, docs/design/held-repost-disputes.md) --

impl<'a> TtlFx<'a> {
    fn calls(&self, name: &str) -> u32 {
        self.staking
            .call_count(&soroban_sdk::Symbol::new(self.env, name))
    }

    fn held_state(&self, sub: u32) -> Option<SlotState> {
        self.env.as_contract(&self.contract_id, || {
            crate::storage::get_held_hour(self.env, &self.asset, TTL_HOUR)
                .and_then(|m| m.get(sub))
                .map(|h| h.state)
        })
    }

    fn ttl_of(&self, key: crate::storage::DataKey) -> u32 {
        use soroban_sdk::testutils::storage::Persistent as _;
        self.env.as_contract(&self.contract_id, || {
            self.env.storage().persistent().get_ttl(&key)
        })
    }

    /// The full hour posted (sub 0 wrong), sub 0 disputed at the hour's
    /// close and overturned a minute before its 6-day deadline, then
    /// reposted a minute before its repost window closes, long past the
    /// ring span, so the repost lands in `HeldHour` only. Returns the
    /// repost's address and time.
    fn held_repost(&self) -> (Address, u64) {
        self.post_hour();
        let disputed_at = hour_close(TTL_HOUR);
        self.at(disputed_at);
        self.client.dispute_sub_signals(
            &self.disputer,
            &self.asset,
            &TTL_HOUR,
            &0u32,
            &dummy_hash(self.env),
        );
        self.at(disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS - 60);
        self.client.resolve_sub_signal_dispute(
            &self.asset,
            &TTL_HOUR,
            &0u32,
            &false,
            &dummy_hash(self.env),
        );
        let reposter = Address::generate(self.env);
        let reposted_at = disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS - 60 + 7_200 - 60;
        self.at(reposted_at);
        self.client.post_sub_signals(
            &reposter,
            &self.asset,
            &TTL_HOUR,
            &0u32,
            &self.signals(10_000_000, 100),
        );
        let sub_start0 = sub_start(TTL_HOUR, 0, SUB_EPOCH_SECS_DEFAULT);
        let in_ring = self.env.as_contract(&self.contract_id, || {
            crate::storage::get_sub_slot(self.env, &self.asset, sub_start0)
        });
        assert!(
            in_ring.is_none(),
            "the repost is past the ring span, so held only"
        );
        (reposter, reposted_at)
    }

    fn dispute_repost(&self, at: u64) {
        self.at(at);
        self.client.dispute_sub_signals(
            &self.disputer,
            &self.asset,
            &TTL_HOUR,
            &0u32,
            &dummy_hash(self.env),
        );
    }

    /// Asserts the built hour's inputs hash is the repost's then every
    /// sibling's (`with_repost`), or the siblings' alone.
    fn assert_inputs_hash(&self, with_repost: bool) {
        use soroban_sdk::{Bytes, BytesN};
        let mut combined = Bytes::new(self.env);
        if with_repost {
            combined.append(&Bytes::from_array(self.env, &[100u8; 32]));
        }
        for sub in 1..self.keepers.len() as u32 {
            combined.append(&Bytes::from_array(self.env, &[sub as u8 + 1; 32]));
        }
        let expected: BytesN<32> = self.env.crypto().sha256(&combined).into();
        assert_eq!(
            self.client
                .signals(&self.asset, &TTL_HOUR)
                .unwrap()
                .inputs_hash,
            expected
        );
    }

    fn assert_paid(&self, reposter: &Address, reposter_paid: u32) {
        for sub in 1..self.keepers.len() {
            assert_eq!(
                self.staking
                    .reward_keeper_sub_epoch_count(&self.keepers[sub]),
                1,
                "keeper of sub {sub}"
            );
        }
        assert_eq!(
            self.staking.reward_keeper_sub_epoch_count(reposter),
            reposter_paid
        );
        assert_eq!(
            self.staking.reward_keeper_sub_epoch_count(&self.keepers[0]),
            0,
            "the overturned first post is never paid"
        );
    }
}

/// #92's day-6 probe, now passing (D1): a repost that lives only in
/// `HeldHour` is disputed. Its held entry reads `Disputed`, its
/// `SubDispute` exists and the disputer's bond is locked.
#[test]
fn a_held_repost_can_be_disputed() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    let (_, reposted_at) = fx.held_repost();
    assert_eq!(fx.held_state(0), Some(SlotState::Pending));

    let locks_before = fx.calls("lock_bond");
    fx.dispute_repost(reposted_at + 60);
    assert_eq!(
        fx.calls("lock_bond"),
        locks_before + 1,
        "the disputer's bond is locked"
    );
    assert_eq!(fx.held_state(0), Some(SlotState::Disputed));
    let dispute = env.as_contract(&fx.contract_id, || {
        crate::storage::get_sub_dispute(&env, &fx.asset, TTL_HOUR, 0)
    });
    assert!(dispute.is_some(), "SubDispute exists");
}

/// The double-contested hour through a held repost, keeper wins: the
/// ruling on the repost's dispute builds the hour about 12.1 days after
/// the first post, with the repost's value, the full inputs hash and
/// every keeper behind a Final sub-epoch paid once.
#[test]
fn keeper_wins_on_a_held_repost_builds_the_hour_in_full_about_12_1_days_later() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    let (reposter, reposted_at) = fx.held_repost();
    let disputed_at = reposted_at + 60;
    fx.dispute_repost(disputed_at);

    let built_at = disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS - 60;
    fx.at(built_at);
    fx.client
        .resolve_sub_signal_dispute(&fx.asset, &TTL_HOUR, &0u32, &true, &dummy_hash(&env));
    let first_post = sub_start(TTL_HOUR, 0, SUB_EPOCH_SECS_DEFAULT) + SUB_EPOCH_SECS_DEFAULT;
    let days = (built_at - first_post) as f64 / 86_400.0;
    assert!(days > 12.1, "builds {days:.2} days after the first post");

    assert!(fx.client.is_final(&fx.asset, &TTL_HOUR));
    assert_eq!(
        fx.client.signals(&fx.asset, &TTL_HOUR).unwrap().peg_ratio,
        10_000_000
    );
    fx.assert_inputs_hash(true);
    fx.assert_paid(&reposter, 1);
}

/// The held repost is overturned again: the reposter is slashed and
/// never paid, no further repost is accepted, and the hour builds
/// without sub 0 in that same call (the overturn's record starts
/// `reposted`, so sub 0 is permanently missing at once).
#[test]
fn a_held_repost_overturned_again_builds_without_it() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    let (reposter, reposted_at) = fx.held_repost();
    let disputed_at = reposted_at + 60;
    fx.dispute_repost(disputed_at);

    fx.at(disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS - 60);
    let slashes_before = fx.calls("slash");
    fx.client
        .resolve_sub_signal_dispute(&fx.asset, &TTL_HOUR, &0u32, &false, &dummy_hash(&env));
    assert_eq!(
        fx.calls("slash"),
        slashes_before + 1,
        "the reposter is slashed"
    );
    assert!(
        fx.client.is_final(&fx.asset, &TTL_HOUR),
        "builds in the same call"
    );
    fx.assert_inputs_hash(false);
    fx.assert_paid(&reposter, 0);
    assert!(
        fx.client
            .try_post_sub_signals(
                &reposter,
                &fx.asset,
                &TTL_HOUR,
                &0u32,
                &fx.signals(10_000_000, 101)
            )
            .is_err(),
        "a repost overturned again gets no further repost"
    );
}

/// The double-contested hour through a held repost, closed by the
/// timeout at the last ledger of the repost's `SubDispute` (D4): about
/// 13.1 days after the first post. Every sibling `SubSignals` and
/// `HeldHour` are still live, and the hour builds in full.
#[test]
fn a_late_timeout_on_a_held_repost_builds_the_hour_in_full_about_13_1_days_later() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    let (reposter, reposted_at) = fx.held_repost();
    let disputed_at = reposted_at + 60;
    fx.dispute_repost(disputed_at);

    // The last ledger `SubDispute` is live on: it was extended to
    // SUB_DISPUTE_TTL_LEDGERS at the dispute.
    let last = disputed_at + (crate::SUB_DISPUTE_TTL_LEDGERS as u64) * 5 - 5;
    fx.at(last);
    let first_post = sub_start(TTL_HOUR, 0, SUB_EPOCH_SECS_DEFAULT) + SUB_EPOCH_SECS_DEFAULT;
    let days = (last - first_post) as f64 / 86_400.0;
    assert!(days > 13.1, "builds {days:.2} days after the first post");
    assert!(
        fx.ttl_of(crate::storage::DataKey::SubDispute(
            fx.asset.clone(),
            TTL_HOUR,
            0
        )) > 0
    );
    assert!(
        fx.ttl_of(crate::storage::DataKey::HeldHour(
            fx.asset.clone(),
            TTL_HOUR
        )) > 0
    );
    for sub in 1..fx.keepers.len() as u32 {
        assert!(fx.sub_signals_ttl(sub) > 0, "sibling {sub} is live");
    }

    fx.client
        .resolve_sub_dispute_timeout(&fx.asset, &TTL_HOUR, &0u32);
    assert!(fx.client.is_final(&fx.asset, &TTL_HOUR));
    fx.assert_inputs_hash(true);
    fx.assert_paid(&reposter, 1);
}

/// A held repost's own dispute window applies: at its `pending_until`
/// the dispute is refused.
#[test]
fn a_held_repost_cannot_be_disputed_after_its_pending_until() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    let (_, reposted_at) = fx.held_repost();
    fx.at(reposted_at + crate::SIGNAL_DISPUTE_SECS);
    assert_eq!(
        fx.client.try_dispute_sub_signals(
            &fx.disputer,
            &fx.asset,
            &TTL_HOUR,
            &0u32,
            &dummy_hash(&env)
        ),
        Err(Ok(crate::Error::DisputeWindowClosed))
    );
}

/// D1: once an hour is held, `HeldHour` is the only source. A sub-epoch
/// with no held entry is `WrongEpoch`, even while its ring slot is
/// still Pending and inside its own dispute window.
#[test]
fn in_a_held_hour_a_sub_epoch_with_no_held_entry_is_wrong_epoch() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    fx.post_hour();
    fx.at(hour_close(TTL_HOUR));
    fx.client
        .dispute_sub_signals(&fx.disputer, &fx.asset, &TTL_HOUR, &0u32, &dummy_hash(&env));
    env.as_contract(&fx.contract_id, || {
        crate::storage::remove_held_sub_slot(&env, &fx.asset, TTL_HOUR, 5)
    });
    let ring = env.as_contract(&fx.contract_id, || {
        crate::storage::get_sub_slot(
            &env,
            &fx.asset,
            sub_start(TTL_HOUR, 5, SUB_EPOCH_SECS_DEFAULT),
        )
    });
    assert_eq!(
        ring.map(|s| s.state),
        Some(SlotState::Pending),
        "the ring still has it"
    );
    assert_eq!(
        fx.client.try_dispute_sub_signals(
            &fx.disputer,
            &fx.asset,
            &TTL_HOUR,
            &5u32,
            &dummy_hash(&env)
        ),
        Err(Ok(crate::Error::WrongEpoch))
    );
}

/// D3: `HeldHour` outlives the hour's open `SubDispute` by one day.
#[test]
fn held_hour_outlives_the_last_open_sub_dispute_by_one_day() {
    let env = Env::default();
    let fx = TtlFx::new(&env);
    fx.post_hour();
    fx.at(hour_close(TTL_HOUR));
    fx.client
        .dispute_sub_signals(&fx.disputer, &fx.asset, &TTL_HOUR, &3u32, &dummy_hash(&env));
    let dispute = fx.ttl_of(crate::storage::DataKey::SubDispute(
        fx.asset.clone(),
        TTL_HOUR,
        3,
    ));
    let held = fx.ttl_of(crate::storage::DataKey::HeldHour(
        fx.asset.clone(),
        TTL_HOUR,
    ));
    assert_eq!(dispute, crate::SUB_DISPUTE_TTL_LEDGERS);
    assert_eq!(held, dispute + 17_280);
}

/// D2, sub-epoch path: a ruling one second before the deadline is
/// accepted, while the timeout is not yet open; at the deadline the
/// ruling is refused and the timeout closes the dispute.
#[test]
fn sub_epoch_rulings_close_at_the_deadline_and_the_timeout_opens_there() {
    let deadline = |fx: &TtlFx| {
        fx.post_hour();
        let disputed_at = hour_close(TTL_HOUR);
        fx.at(disputed_at);
        fx.client.dispute_sub_signals(
            &fx.disputer,
            &fx.asset,
            &TTL_HOUR,
            &0u32,
            &dummy_hash(fx.env),
        );
        disputed_at + crate::SIGNAL_DISPUTE_RULING_SECS
    };

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let d = deadline(&fx);
    fx.at(d - 1);
    assert_eq!(
        fx.client
            .try_resolve_sub_dispute_timeout(&fx.asset, &TTL_HOUR, &0u32),
        Err(Ok(crate::Error::RulingDeadlineNotReached))
    );
    fx.client
        .resolve_sub_signal_dispute(&fx.asset, &TTL_HOUR, &0u32, &true, &dummy_hash(&env));

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let d = deadline(&fx);
    fx.at(d);
    assert_eq!(
        fx.client.try_resolve_sub_signal_dispute(
            &fx.asset,
            &TTL_HOUR,
            &0u32,
            &true,
            &dummy_hash(&env)
        ),
        Err(Ok(crate::Error::RulingDeadlinePassed))
    );
    fx.client
        .resolve_sub_dispute_timeout(&fx.asset, &TTL_HOUR, &0u32);
}

/// D2, hourly path: the same boundary for `resolve_signal_dispute`.
#[test]
fn hourly_rulings_close_at_the_deadline_and_the_timeout_opens_there() {
    let epoch = TTL_HOUR;
    let deadline = |fx: &TtlFx| {
        fx.at((epoch + 1) * EPOCH_SECS);
        fx.client.post_signals(
            &fx.keepers[0],
            &fx.asset,
            &signal_set(fx.env, epoch, 9_900_000),
        );
        fx.client
            .dispute_signals(&fx.disputer, &fx.asset, &epoch, &dummy_hash(fx.env));
        (epoch + 1) * EPOCH_SECS + crate::SIGNAL_DISPUTE_RULING_SECS
    };

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let d = deadline(&fx);
    fx.at(d - 1);
    assert_eq!(
        fx.client
            .try_resolve_signal_dispute_timeout(&fx.asset, &epoch),
        Err(Ok(crate::Error::RulingDeadlineNotReached))
    );
    fx.client
        .resolve_signal_dispute(&fx.asset, &epoch, &true, &dummy_hash(&env));

    let env = Env::default();
    let fx = TtlFx::new(&env);
    let d = deadline(&fx);
    fx.at(d);
    assert_eq!(
        fx.client
            .try_resolve_signal_dispute(&fx.asset, &epoch, &true, &dummy_hash(&env)),
        Err(Ok(crate::Error::RulingDeadlinePassed))
    );
    fx.client.resolve_signal_dispute_timeout(&fx.asset, &epoch);
}

fn dummy_hash(env: &Env) -> soroban_sdk::BytesN<32> {
    soroban_sdk::BytesN::from_array(env, &[1u8; 32])
}

// -- Property test: random sequences of sub-epoch posts, disputes,
// rulings, missed posts, builds and sub_epoch_secs changes, checking
// I21, I23 and "a built hour never changes." --

mod property {
    extern crate std;
    use super::*;
    use proptest::prelude::*;

    const PROPERTY_HOUR: u64 = REALISTIC_EPOCH_BASE;
    const MAX_SUBS_AT_300S: u32 = 12;

    /// Which sub-epoch a step targets. Resolved against RUNTIME
    /// state (which sub was most recently overturned or reposted),
    /// never decided at strategy-generation time: `proptest`
    /// generates every step's own data upfront, before the sequence
    /// runs, so there is no way for a literal `sub: u32` field to
    /// "know" what happened in an earlier step. `Focus` defers that
    /// decision to the execution loop instead.
    #[derive(Clone, Copy, Debug)]
    enum SubChoice {
        /// Target whichever sub-epoch the execution loop's own
        /// `focus_sub` currently points at (the one most recently
        /// overturned or successfully reposted), falling back to
        /// `fallback` if nothing has been focused on yet.
        Focus { fallback: u32 },
        /// Target this exact sub-epoch, ignoring any current focus.
        Explicit(u32),
    }

    /// One step of the random sequence. A literal `sub: u32` field
    /// (via `SubChoice::Explicit`) is taken modulo whatever `per_hour`
    /// actually is at the time the step runs (the strategy below
    /// generates it in `0..MAX_SUBS_AT_300S`, the widest range any
    /// allowed interval needs, so a step is never silently impossible
    /// to generate regardless of the current interval).
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// Post `sub` with `peg_ratio`, if it has not already been
        /// posted for the current hour/interval and the window
        /// accepts it.
        Post { sub: SubChoice, peg_ratio: i128 },
        /// Dispute `sub`, if it is currently Pending and within its
        /// own dispute window.
        Dispute { sub: SubChoice },
        /// Resolve `sub`'s own open dispute, if any, via committee
        /// ruling.
        Rule { sub: SubChoice, keeper_wins: bool },
        /// Resolve `sub`'s own open dispute via the timeout path
        /// (ADR-010's default-favors-the-data outcome), if its ruling
        /// deadline has passed.
        Timeout { sub: SubChoice },
        /// Advance the ledger clock forward by `secs`.
        AdvanceTime { secs: u64 },
        /// Call the permissionless `build_hour` trigger directly.
        Build,
        /// Change `sub_epoch_secs` to one of the 6 allowed values.
        ChangeInterval { secs_index: u8 },
    }

    /// `Dispute`/`Rule`/`Timeout`'s own `fallback` (used only until
    /// `focus_sub` is set by a first overturn or repost) draws from
    /// this much narrower pool (not the full `0..MAX_SUBS_AT_300S`):
    /// a dispute opened on one random sub-epoch and a later ruling
    /// against a SEPARATELY, independently random sub-epoch almost
    /// never target the same one across a short sequence, which
    /// starves this property of the exact cross-step correlation
    /// (dispute, then a later rule/timeout against THAT SAME sub) it
    /// exists to exercise. A pool of 3 keeps the generator still
    /// exploring which of several sub-epochs gets disputed before any
    /// focus exists, while making a dispute/rule or two-disputes-in-
    /// one-hour collision likely within a 40 step sequence. `Post`
    /// keeps the full range for its own fallback: an un-posted,
    /// never-disputed sub-epoch (one of the other 9 at the 300s
    /// default) is exactly how `MissingWithinBackfill`/
    /// `PermanentlyMissing` get exercised.
    const DISPUTE_SUB_POOL: u32 = 3;

    /// How often `Dispute`/`Rule`/`Post` target `focus_sub` (the most
    /// recently overturned or reposted sub-epoch) instead of a fresh,
    /// independently random one: weighted 70/30 via `prop_oneof!`'s
    /// own relative-weight mechanism (7 against 3), matching the
    /// engineer's own "with high probability (say 70%)" instruction.
    /// Without this bias, reaching the specific 6-step chain a
    /// repost's own second overturn needs (dispute sub X, reject,
    /// repost X, dispute X again, reject again, attempt a third post)
    /// requires the SAME sub to be independently redrawn for
    /// `Dispute`/`Rule` twice in a row out of the 3-sub fallback pool,
    /// which empirically never happened within 300 cases of up to 80
    /// steps each (verified by hand, reintroducing the exact
    /// one-repost-per-overturn bug and confirming this test did NOT
    /// catch it before this bias was added).
    fn sub_choice(fallback_pool: u32) -> impl Strategy<Value = SubChoice> {
        prop_oneof![
            19 => (0..fallback_pool).prop_map(|fallback| SubChoice::Focus { fallback }),
            1 => (0..fallback_pool).prop_map(SubChoice::Explicit),
        ]
    }

    /// `Rule`'s own `keeper_wins` outcome, biased toward `false`
    /// (the disputer winning, overturning the sub-epoch) at the same
    /// 70/30 ratio as `sub_choice`'s own Focus bias: the repost
    /// double-overturn chain needs TWO rejecting rulings in a row
    /// against the focused sub, not just two rulings, so leaving
    /// `keeper_wins` at a uniform 50/50 would still make that chain
    /// roughly `0.7^4 * 0.5^2 ≈ 6%` likely per attempt (needing
    /// Focus to hit 4 times AND both rulings to land `false`), far
    /// below what reliably reproduces within the default case count.
    fn keeper_wins_choice() -> impl Strategy<Value = bool> {
        prop_oneof![
            3 => Just(true),
            7 => Just(false),
        ]
    }

    /// Named jumps, each tied to a specific protocol boundary, rather
    /// than a continuous random range: a uniform range wide enough to
    /// ever reach `SIGNAL_DISPUTE_RULING_SECS` (6 days) would almost
    /// always overshoot every short window (a sub-epoch's own
    /// `pending_until`, 2h) in a single step, and real sequences
    /// accumulate MANY small steps before the generator happens to
    /// pick a large one, compounding past short windows even with a
    /// heavily-skewed continuous distribution. Named jumps instead
    /// let the generator land EXACTLY where behavior changes: just
    /// inside a window, just past it, past the whole ring, past the
    /// ruling deadline. `Tiny` keeps a little genuine jitter (several
    /// of these in a row still plausibly stay inside a 2h window);
    /// every other variant is a single deliberate jump to one exact
    /// boundary (or just past it).
    fn advance_time_secs() -> impl Strategy<Value = u64> {
        prop_oneof![
            // A few minutes: several in a row still land inside a
            // sub-epoch's own 2h dispute window.
            3 => 0u64..=300u64,
            // Just past a sub-epoch's own pending_until (2h): the
            // moment a Pending sub-epoch becomes effectively Final.
            3 => Just(crate::SIGNAL_DISPUTE_SECS + 1),
            // Just past Sub(asset)'s own 5 hour ring span: the
            // moment a sub-epoch posted this far back would have
            // rotated out.
            2 => Just(5 * EPOCH_SECS + 1),
            // Just past SIGNAL_DISPUTE_RULING_SECS (6 days): a
            // timeout resolution becomes callable.
            2 => Just(crate::SIGNAL_DISPUTE_RULING_SECS + 1),
        ]
    }

    /// `AdvanceTime` itself is weighted far below the other step
    /// types (weight 2, against 6 each for `Post`/`Dispute`/`Rule`):
    /// most of an 80 step sequence should be post/dispute/rule
    /// traffic with the clock essentially still, so a `Dispute`
    /// immediately after its own `Post` (and a `Rule` immediately
    /// after its own `Dispute`) are common, not rare, outcomes. The
    /// FEW `AdvanceTime` steps that do land are what carry the clock
    /// through a window boundary when the sequence needs to.
    fn step_strategy() -> impl Strategy<Value = Step> {
        prop_oneof![
            6 => (sub_choice(MAX_SUBS_AT_300S), 9_000_000i128..=10_100_000i128)
                .prop_map(|(sub, peg_ratio)| Step::Post { sub, peg_ratio }),
            6 => sub_choice(DISPUTE_SUB_POOL).prop_map(|sub| Step::Dispute { sub }),
            6 => (sub_choice(DISPUTE_SUB_POOL), keeper_wins_choice())
                .prop_map(|(sub, keeper_wins)| Step::Rule { sub, keeper_wins }),
            3 => sub_choice(DISPUTE_SUB_POOL).prop_map(|sub| Step::Timeout { sub }),
            2 => advance_time_secs().prop_map(|secs| Step::AdvanceTime { secs }),
            3 => Just(Step::Build),
            2 => (0u8..6u8).prop_map(|secs_index| Step::ChangeInterval { secs_index }),
        ]
    }

    /// A built (or built-Empty) hour's own observable fields, snapshot
    /// at the moment it was first observed decided. Compared against
    /// the SAME read taken after every later step: any difference at
    /// all is a violated "a built hour never changes" property.
    #[derive(Clone, Debug, PartialEq)]
    struct Decided {
        is_final: bool,
        peg_ratio: i128,
        liquidity_2pct: i128,
        supply: i128,
        redemption_net: i128,
    }

    fn snapshot_hour(client: &RiskOracleClient, asset: &Address, hour: u64) -> Option<Decided> {
        let is_final = client.is_final(asset, &hour);
        let ring = client.ring(asset);
        let slot = ring.iter().find(|s| s.epoch == hour);
        match slot {
            Some(s) if s.state == SlotState::Final || s.state == SlotState::Empty => {
                Some(Decided {
                    is_final,
                    peg_ratio: s.peg_ratio,
                    liquidity_2pct: s.liquidity_2pct,
                    supply: s.supply,
                    redemption_net: s.redemption_net,
                })
            }
            // epoch == 0 (an Empty, built-via-overturn-to-Empty slot
            // clears its own identity back to 0, matching
            // storage.rs's own empty_slot) is its own form of
            // "decided Empty," distinct from "never built at all":
            // but since PROPERTY_HOUR is never 0, a genuine
            // never-built hour and a built-Empty one are only
            // distinguishable by whether build_hour has ever
            // succeeded; is_final alone already tells decided-Final
            // apart from everything else, and the Empty case is
            // covered by the explicit None-epoch slot check in
            // never_flips_once_decided's own caller instead.
            _ => None,
        }
    }

    /// This test's own model of one sub-epoch's current disposition,
    /// kept independently of the contract's own storage, precisely
    /// enough to predict whether `post_sub_signals` will accept or
    /// reject the NEXT `Post` step against it, and what value a
    /// built hour should end up reading if this sub-epoch is Final
    /// when it builds.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum ModelSub {
        /// Never posted, or posted and then overturned with its
        /// repost window closed and either no repost or the repost
        /// itself overturned again: settled, permanently excluded,
        /// but a fresh `Post` is only ever ACCEPTED again while still
        /// genuinely `NeverPosted` (an overturned-and-settled sub
        /// never reopens).
        NeverPosted,
        /// Currently has a live `SubSignals` entry (Pending,
        /// Disputed, or effectively Final): a further `Post` must be
        /// rejected (`EpochAlreadyPosted`), regardless of which of
        /// those three states it is actually in right now, since all
        /// three share the same "already posted" rejection.
        /// `was_repost` mirrors `storage::SubRepostUsed` exactly:
        /// true if THIS live posting is itself a repost, so that IF
        /// it is later overturned too, the model's own `Rule` handler
        /// knows to start that new `Overturned` record already
        /// `reposted: true` (never resetting to `false` the way a
        /// model that only inspected the PRIOR state's own `reposted`
        /// flag would, which is exactly the shape of bug this model
        /// exists to catch, and the shape it would otherwise share
        /// with the one found by hand).
        Live { value: i128, was_repost: bool },
        /// Overturned (a dispute the disputer won), its own repost
        /// window still open or already closed, `reposted` tracking
        /// whether the ONE allowed repost has already been used.
        Overturned {
            sub_close: u64,
            overturned_at: u64,
            reposted: bool,
        },
    }

    impl ModelSub {
        /// Mirrors `check_sub_epoch_window`/`check_sub_epoch_repost_
        /// window`/the `EpochAlreadyPosted`/`HourAlreadyPosted` checks
        /// `post_sub_signals` makes, in that order, against THIS
        /// model's own view of the sub-epoch (never the contract's),
        /// so a mismatch between this prediction and the real result
        /// is exactly the kind of bug the hand-found repost-limit gap
        /// was: the model accepting (or rejecting) something the
        /// real contract does the opposite of.
        fn predict_post_accepted(&self, sub_close: u64, now: u64, hour_is_final: bool) -> bool {
            if hour_is_final {
                return false;
            }
            match self {
                ModelSub::NeverPosted => {
                    sub_close <= now && now.saturating_sub(sub_close) <= crate::SUB_BACKFILL_SECS
                }
                ModelSub::Live { .. } => false,
                ModelSub::Overturned {
                    overturned_at,
                    reposted,
                    ..
                } => {
                    if *reposted {
                        false
                    } else {
                        let anchor = sub_close.max(*overturned_at);
                        now.saturating_sub(anchor) <= crate::SUB_BACKFILL_SECS
                    }
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

        /// I21, I23, "a built hour never changes", and now the exact
        /// repost accept/reject boundary: a random sequence of up to
        /// 80 steps against ONE hour, mixing posts, disputes
        /// (including ones left open long enough to outlast
        /// `Sub(asset)`'s own 5 hour ring, per `AdvanceTime`'s wide
        /// range), rulings (both outcomes, via both the committee and
        /// timeout paths), missed sub-epochs (any `sub` nobody ever
        /// posts), manual `build_hour` calls at arbitrary points, and
        /// `sub_epoch_secs` changes mid-sequence. Every `Post` step is
        /// checked against this test's OWN independent model
        /// (`ModelSub::predict_post_accepted`) before being applied:
        /// a model that always predicts rejection, or one with no
        /// repost limit at all, would both have passed the OLD
        /// version of this test (which only ever ignored the result
        /// via `let _ =`), the exact shape of the one-repost-per-
        /// overturn bug found by hand while writing the deterministic
        /// tests. Checked after EVERY step: once the hour is first
        /// observed decided (Final or built-Empty), every later
        /// step's own snapshot must be byte-for-byte identical (I21's
        /// "never changes an hour that is already built" and the
        /// brief's own "a built hour never changes," together), and
        /// a Final hour's own `peg_ratio` must equal the mean of
        /// whichever sub-epochs this test's own model believes are
        /// currently live, not a value recomputed from stale,
        /// overturned data (I23, generalized from "the sentinel never
        /// leaks" into "the built value matches the model exactly").
        #[test]
        fn random_sequences_never_violate_i21_i23_or_a_built_hour_changing(
            steps in prop::collection::vec(step_strategy(), 1..80),
        ) {
            let env = Env::default();
            let (client, governor, asset) = setup_with_governor_and_asset(&env);
            let committee = Address::generate(&env);
            crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
            let keeper = Address::generate(&env);
            let disputer = Address::generate(&env);
            let hour = PROPERTY_HOUR;
            // Starts right at hour's own close (the earliest moment
            // any of its sub-epochs can be posted at the 300s
            // default): without this, Env::default()'s own starting
            // timestamp (near 0) sits far earlier than hour * 3,600,
            // and check_sub_epoch_window would reject every post
            // until enough AdvanceTime steps happened to sum past the
            // gap, which 40 steps of at most 600,000s each cannot
            // realistically reach.
            env.ledger().set_timestamp(hour_close(hour));

            let mut model: [ModelSub; MAX_SUBS_AT_300S as usize] =
                [ModelSub::NeverPosted; MAX_SUBS_AT_300S as usize];
            // Tracked independently of the contract's own SubDispute
            // storage, from this test's own view of which dispute
            // calls it made succeed: an open dispute must always
            // block the hour from reading Final, regardless of how
            // Sub(asset)'s own ring has rotated underneath it. This
            // is what actually catches sub_disposition checking
            // HeldHour/the ring before SubDispute (the ring-vs-
            // dispute bug this whole mechanism exists to fix):
            // removing that check does not necessarily change the
            // FINAL VALUE a built hour ends up with (so the
            // never-flips and I23 checks alone can miss it under
            // many sequences), but it DOES let the hour build Final
            // while this test's own bookkeeping still considers a
            // dispute open, which this check catches directly.
            let mut open_disputes = [false; MAX_SUBS_AT_300S as usize];
            let mut decided: Option<Decided> = None;
            // The sub-epoch most recently overturned or successfully
            // reposted: what SubChoice::Focus resolves to. None until
            // the first overturn happens, in which case Focus falls
            // back to its own, separately random `fallback` sub.
            let mut focus_sub: Option<u32> = None;

            for step in steps {
                let now = env.ledger().timestamp();
                let sub_epoch_secs = client
                    .sub_epoch_config(&asset)
                    .map(|c| {
                        match (c.pending_sub_epoch_secs, c.effective_from_hour) {
                            (Some(pending), Some(eff)) if hour >= eff => pending,
                            _ => c.sub_epoch_secs,
                        }
                    })
                    .unwrap_or(SUB_EPOCH_SECS_DEFAULT);
                let per_hour = (EPOCH_SECS / sub_epoch_secs) as u32;
                let resolve = |choice: SubChoice| -> u32 {
                    match choice {
                        SubChoice::Focus { fallback } => focus_sub.unwrap_or(fallback),
                        SubChoice::Explicit(sub) => sub,
                    }
                };

                match step {
                    Step::Post { sub, peg_ratio } => {
                        let sub = resolve(sub);
                        if sub >= per_hour {
                            continue;
                        }
                        let sub_close = sub_start(hour, sub, sub_epoch_secs) + sub_epoch_secs;
                        let hour_is_final = client.is_final(&asset, &hour);
                        let was_overturned =
                            matches!(model[sub as usize], ModelSub::Overturned { .. });
                        let predicted =
                            model[sub as usize].predict_post_accepted(sub_close, now, hour_is_final);

                        let mut s = signal_set(&env, hour, peg_ratio);
                        s.liquidity_2pct = 500_000_000_000;
                        s.supply_change_bps = 0;
                        let result = client.try_post_sub_signals(&keeper, &asset, &hour, &sub, &s);

                        prop_assert_eq!(
                            result.is_ok(),
                            predicted,
                            "sub {}: post_sub_signals returned {:?}, model predicted \
                             accepted = {}, model state = {:?}",
                            sub,
                            result,
                            predicted,
                            model[sub as usize],
                        );
                        if result.is_ok() {
                            // On a successful repost, this REPLACES
                            // the model's own prior Overturned entry
                            // (never keeps tagging it as overturned):
                            // the final check below must know the
                            // real, current value, not stale history.
                            model[sub as usize] = ModelSub::Live {
                                value: peg_ratio,
                                was_repost: was_overturned,
                            };
                            if was_overturned {
                                // A successful REPOST (not a
                                // genuinely fresh first post) keeps
                                // this sub-epoch in focus, so the
                                // generator's own bias toward Focus
                                // naturally keeps producing a SECOND
                                // dispute/rule against the SAME sub
                                // the repost just landed on.
                                focus_sub = Some(sub);
                            }
                        }
                    }
                    Step::Dispute { sub } => {
                        let sub = resolve(sub);
                        if sub >= per_hour {
                            continue;
                        }
                        let r = client.try_dispute_sub_signals(
                            &disputer,
                            &asset,
                            &hour,
                            &sub,
                            &dummy_hash(&env),
                        );
                        if r.is_ok() {
                            open_disputes[sub as usize] = true;
                        }
                    }
                    Step::Rule { sub, keeper_wins } => {
                        let sub = resolve(sub);
                        if sub >= per_hour {
                            continue;
                        }
                        let result = client.try_resolve_sub_signal_dispute(
                            &asset,
                            &hour,
                            &sub,
                            &keeper_wins,
                            &dummy_hash(&env),
                        );
                        if result.is_ok() {
                            open_disputes[sub as usize] = false;
                        }
                        if result.is_ok() {
                            if keeper_wins {
                                // Upheld: stays Live with its own
                                // already-recorded value (nothing
                                // else changes it).
                            } else {
                                // Overturned: the sub's repost window
                                // opens now, anchored to whichever is
                                // later, its own original close or
                                // this overturn. If the LIVE posting
                                // being overturned right now was
                                // itself already a repost (mirroring
                                // storage::SubRepostUsed, which is
                                // exactly what the real
                                // overturn_sub_signals checks, never
                                // the PRIOR Overturned record's own
                                // stale `reposted` flag, which is
                                // exactly the gap that let the real
                                // one-repost-per-original-overturn
                                // bug through undetected), the new
                                // record starts already `reposted:
                                // true` too, so no further repost is
                                // predicted to be accepted.
                                let sub_close =
                                    sub_start(hour, sub, sub_epoch_secs) + sub_epoch_secs;
                                let already_used_its_repost = matches!(
                                    model[sub as usize],
                                    ModelSub::Live { was_repost: true, .. }
                                );
                                model[sub as usize] = ModelSub::Overturned {
                                    sub_close,
                                    overturned_at: now,
                                    reposted: already_used_its_repost,
                                };
                                // Keep the generator's own Focus bias
                                // pointed at this sub, so a repost
                                // attempt (Post with Focus) and a
                                // second dispute/rule against THAT
                                // SAME sub are both likely next,
                                // without needing the fallback pool
                                // to happen to redraw it.
                                focus_sub = Some(sub);
                            }
                        }
                    }
                    Step::Timeout { sub } => {
                        let sub = resolve(sub);
                        if sub >= per_hour {
                            continue;
                        }
                        // The timeout path always favors the data
                        // (ADR-010): never overturns, so the model
                        // never moves to Overturned here; it stays
                        // Live with whatever value it already had.
                        let tr = client.try_resolve_sub_dispute_timeout(&asset, &hour, &sub);
                        if tr.is_ok() {
                            open_disputes[sub as usize] = false;
                        }
                    }
                    Step::AdvanceTime { secs } => {
                        env.ledger().set_timestamp(now + secs);
                    }
                    Step::Build => {
                        let _ = client.try_build_hour(&asset, &hour);
                    }
                    Step::ChangeInterval { secs_index } => {
                        let value = crate::SUB_EPOCH_SECS_ALLOWED[secs_index as usize % 6];
                        let _ = client.try_set_sub_epoch_secs(&asset, &value);
                    }
                }

                // I21: a sub_epoch_secs change never renumbers this
                // hour or touches an already-built slot. Checked
                // structurally: the ring's own epoch field for
                // `hour`'s position, once Final, must still read
                // `hour` (write_ring_slot's own newer-wins guard is
                // the mechanism; this just confirms the guarantee
                // holds after every step, not only at the end).
                if let Some(current) = snapshot_hour(&client, &asset, hour) {
                    match &decided {
                        None => decided = Some(current),
                        Some(previous) => {
                            prop_assert_eq!(
                                &current,
                                previous,
                                "a decided hour's own snapshot changed after a later step"
                            );
                        }
                    }
                }

                // I23 (generalized): a Final hour's own peg_ratio
                // must equal the mean of exactly the sub-epochs this
                // test's own model currently believes are Live, never
                // a value that leaked stale, overturned data (the
                // old sentinel check) and never one that is simply
                // wrong for some other reason (the new, stronger
                // check this model enables).
                if client.is_final(&asset, &hour) {
                    // The hour must never read Final while this
                    // test's own bookkeeping still considers ANY
                    // sub-epoch's dispute open: this is the direct
                    // check for sub_disposition/refresh_waiting_hour
                    // checking SubDispute before anything Sub(asset)'s
                    // ring or HeldHour currently shows (Section 5.9
                    // S3/S4's own design), independent of whether a
                    // violation would also change the built value.
                    for sub in 0..per_hour {
                        prop_assert!(
                            !open_disputes[sub as usize],
                            "hour read Final while sub {sub}'s own dispute was still open"
                        );
                    }

                    let live_values: std::vec::Vec<i128> = (0..per_hour)
                        .filter_map(|sub| match model[sub as usize] {
                            ModelSub::Live { value, .. } => Some(value),
                            _ => None,
                        })
                        .collect();
                    if !live_values.is_empty() {
                        let expected_mean: i128 =
                            live_values.iter().sum::<i128>() / live_values.len() as i128;
                        let ring = client.ring(&asset);
                        if let Some(slot) = ring.iter().find(|s| s.epoch == hour) {
                            prop_assert_eq!(
                                slot.peg_ratio,
                                expected_mean,
                                "the built hour's own peg_ratio must match the mean of \
                                 exactly the sub-epochs this model believes are live"
                            );
                        }
                    }
                }
            }
        }
    }
}
