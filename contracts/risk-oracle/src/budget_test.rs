//! Resource spike: measures `post_signals` and `score` (the real Tier 1
//! style read path, not a placeholder) against a FULL 240 slot ring
//! buffer, and prints the numbers against the current network limits.
//! See the PR description for the full report; this file is the source
//! of every number quoted there.
//!
//! Run with: `cargo test -p risk-oracle --lib budget -- --nocapture`

extern crate std;

use soroban_sdk::{
    testutils::{cost_estimate::CostEstimate, Address as _, Ledger as _},
    Address, BytesN, Env,
};
use std::println;
use sylox_types::network_limits::{
    TX_MAX_INSTRUCTIONS, TX_MAX_READ_LEDGER_ENTRIES, TX_MAX_WRITE_BYTES,
    TX_MAX_WRITE_LEDGER_ENTRIES,
};
use sylox_types::{AssetConfig, EndpointStatus, IssuerActions, Reference, SignalSet};

use crate::mocks::MockStaking;
use crate::storage::RING_SLOTS;
use crate::{RiskOracle, RiskOracleClient};

fn asset_config(env: &Env, asset: &Address, issuer: &Address) -> AssetConfig {
    AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: soroban_sdk::String::from_str(env, "example.com"),
        amm_adapters: soroban_sdk::Vec::new(env),
        fx_adapter: None,
        min_liquidity: 100_000_000_000,
        issuer_flags: sylox_types::IssuerFlags::default(),
        enabled: true,
    }
}

fn signal_set(env: &Env, epoch: u64, peg_ratio: i128) -> SignalSet {
    SignalSet {
        epoch,
        posted_at: 0,
        peg_ratio,
        peg_ratio_p10: peg_ratio,
        liquidity_2pct: 500_000_000_000,
        redemption_net: 10_000_000_000,
        supply: 10_000_000_000_000,
        supply_change_bps: 0,
        issuer_actions: IssuerActions {
            clawbacks: 1,
            clawback_amount: 5_000_000_000,
            auth_revocations: 1,
            flag_changes: 0,
        },
        endpoint: EndpointStatus::Up,
        inputs_hash: BytesN::from_array(env, &[7u8; 32]),
        poster: Address::generate(env),
    }
}

/// Registers RiskOracle (constructed atomically against a mock Staking)
/// and adds one enabled asset.
fn setup(env: &Env) -> (RiskOracleClient<'_>, Address) {
    env.mock_all_auths();
    let staking_id = env.register(MockStaking, ());
    let governor = Address::generate(env);
    let registry = Address::generate(env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking_id.clone()),
    );
    let client = RiskOracleClient::new(env, &contract_id);

    let asset = Address::generate(env);
    let issuer = Address::generate(env);
    client.add_asset(&asset_config(env, &asset, &issuer));

    (client, asset)
}

/// Fills the ring to `RING_SLOTS` consecutive epochs, 1 posting per call,
/// so the final posting measured below writes into a buffer that already
/// holds a full set of slots: the steady state every posting after startup
/// actually pays. Each posting uses a supply equal to the previous
/// epoch's, so the Section 11.3 supply-change consistency check (which
/// reads the previous epoch's ring slot) never rejects the fill. Returns
/// the next unused epoch.
fn warm_full_ring(env: &Env, client: &RiskOracleClient, asset: &Address) -> u64 {
    let keeper = Address::generate(env);
    for epoch in 0..RING_SLOTS as u64 {
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        client.post_signals(&keeper, asset, &signal_set(env, epoch, 9_900_000));
    }
    RING_SLOTS as u64
}

fn print_resources(label: &str, estimate: &CostEstimate) {
    let resources = estimate.resources();
    let fee = estimate.fee();
    println!("--- {label} ---");
    println!("  instructions:        {}", resources.instructions);
    println!("  mem_bytes:           {}", resources.mem_bytes);
    println!("  disk_read_entries:   {}", resources.disk_read_entries);
    println!("  memory_read_entries: {}", resources.memory_read_entries);
    println!("  disk_read_bytes:     {}", resources.disk_read_bytes);
    println!("  write_entries:       {}", resources.write_entries);
    println!("  write_bytes:         {}", resources.write_bytes);
    println!("  fee.total (stroops): {}", fee.total);
}

#[test]
fn budget_post_signals_against_a_full_ring() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let next_epoch = warm_full_ring(&env, &client, &asset);

    // The measured call: ring is full, this posting evicts and replaces
    // exactly one slot, the steady state cost after the buffer has warmed.
    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    let keeper = Address::generate(&env);
    client.post_signals(&keeper, &asset, &signal_set(&env, next_epoch, 9_850_000));

    let estimate = env.cost_estimate();
    print_resources("post_signals, full 240 slot ring", &estimate);

    let resources = estimate.resources();
    assert!(
        resources.write_bytes < 65_536,
        "a single ring entry must fit under contract_data_entry_size_bytes (65,536); got {}",
        resources.write_bytes
    );
}

/// The real Tier 1 style read: `score(asset)`, which reads the 168 newest
/// ring slots via `storage::get_window` and computes all six Section 6.1
/// components, applies hysteresis and persists the result. This replaced
/// an earlier `#[cfg(test)]`-only placeholder method that approximated
/// the same aggregates without being a real, shipped function; now that
/// `score` exists for real, the budget test measures it directly instead.
#[test]
fn budget_score_against_a_full_ring() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    warm_full_ring(&env, &client, &asset);

    let result = client.score(&asset);
    println!("computed: {result:?}");

    let estimate = env.cost_estimate();
    print_resources("score (Tier 1 style read), full 240 slot ring", &estimate);
}

/// Breaks the headline fee down by component. `fee()` uses a hardcoded
/// mainnet fee snapshot from 2026-07-10 (see its doc comment in
/// soroban-sdk), which may drift from the live network; this test exists
/// to show WHERE the fee goes, not to pin an exact total. See the PR
/// description for the quickstart-network simulated figure, which is the
/// number to trust over this one.
#[test]
fn budget_post_signals_fee_breakdown() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let next_epoch = warm_full_ring(&env, &client, &asset);

    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    let keeper = Address::generate(&env);
    client.post_signals(&keeper, &asset, &signal_set(&env, next_epoch, 9_850_000));

    let fee = env.cost_estimate().fee();
    println!("--- post_signals fee breakdown (stroops) ---");
    println!("  total:                 {}", fee.total);
    println!("  instructions:          {}", fee.instructions);
    println!("  disk_read_entries:     {}", fee.disk_read_entries);
    println!("  write_entries:         {}", fee.write_entries);
    println!("  disk_read_bytes:       {}", fee.disk_read_bytes);
    println!("  write_bytes:           {}", fee.write_bytes);
    println!("  contract_events:       {}", fee.contract_events);
    println!("  persistent_entry_rent: {}", fee.persistent_entry_rent);
    println!("  temporary_entry_rent:  {}", fee.temporary_entry_rent);

    // temporary_entry_rent dominates the total: see
    // budget_post_signals_rent_detail for why (the require_auth() nonce
    // entry's TTL bump, a temporary entry, not the ring or signals write).
}

/// Shows that the dominant fee component above is a `require_auth()` nonce
/// entry's rent, a temporary entry unrelated to the ring buffer or Signals
/// writes (both persistent). `persistent_entry_rent_bumps` stays at 1
/// because `Ring(asset)` is one entry rewritten every posting, not a new
/// entry each time.
#[test]
fn budget_post_signals_rent_detail() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let next_epoch = warm_full_ring(&env, &client, &asset);

    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    let keeper = Address::generate(&env);
    client.post_signals(&keeper, &asset, &signal_set(&env, next_epoch, 9_850_000));

    let resources = env.cost_estimate().resources();
    println!("--- post_signals rent detail ---");
    println!(
        "  persistent_rent_ledger_bytes: {}",
        resources.persistent_rent_ledger_bytes
    );
    println!(
        "  persistent_entry_rent_bumps:  {}",
        resources.persistent_entry_rent_bumps
    );
    println!(
        "  temporary_rent_ledger_bytes:  {}",
        resources.temporary_rent_ledger_bytes
    );
    println!(
        "  temporary_entry_rent_bumps:   {}",
        resources.temporary_entry_rent_bumps
    );
}

/// Review item C6 (re-review): the worst case for `try_advance_finality`'s
/// backward scan combines a FULL 240 slot ring (the realistic storage
/// read/write cost every posting already pays, see
/// `budget_post_signals_against_a_full_ring`) with a long run of
/// MISSING epochs between the newest Final epoch and the newest
/// posted one, forcing the scan to walk every epoch in its lookback
/// bound (`WINDOW_SECS / EPOCH_SECS`, 72) without finding anything,
/// rather than the common case (1-2 epochs back, given the 2 epoch
/// finality lag) terminating almost immediately.
///
/// Warms a full ring normally, then posts one more epoch, skips the
/// next 71 entirely (never posted), then posts the epoch 72 slots
/// later (the oldest epoch `check_epoch_window` still accepts relative
/// to that posting time) and measures THAT call, whose
/// `try_advance_finality` sweep must walk the full 71 epoch gap
/// before finding the epoch just before it Final.
#[test]
fn budget_finality_backward_scan_across_a_full_backfill_window() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let next_epoch = warm_full_ring(&env, &client, &asset);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    client.post_signals(&keeper, &asset, &signal_set(&env, next_epoch, 9_900_000));
    // The next 71 epochs intentionally never posted.
    let gap_start = next_epoch + 1;
    let resume_epoch = gap_start + 71;

    env.ledger().set_timestamp((resume_epoch + 1) * 3_600);
    client.post_signals(&keeper, &asset, &signal_set(&env, resume_epoch, 9_900_000));

    // Capture the estimate for THIS post_signals call immediately,
    // before any further invocation (including the sanity check read
    // below) can fold its own cost into cost_estimate()'s result.
    let estimate = env.cost_estimate();
    print_resources(
        "post_signals, full ring, try_advance_finality scanning a 71 epoch missing gap",
        &estimate,
    );

    // Sanity check, AFTER capturing the estimate above: resume_epoch
    // itself cannot be Final yet (posted this instant, pending_until
    // is in the future). next_epoch's own pending_until is also 2
    // epochs ahead of its own post time (SIGNAL_DISPUTE_SECS), so the
    // newest epoch actually Final by the time resume_epoch posts is
    // next_epoch - 2 (the same finality lag every other test in this
    // crate accounts for), reached only by walking back across nearly
    // the entire gap, not a trivial same-epoch or 1-step result.
    assert_eq!(
        client.score(&asset).epoch,
        next_epoch - 2,
        "the measured call's backward scan must walk nearly the full 71 epoch gap, \
         not stop early; a different result here means this test is not actually \
         measuring the scenario it claims to"
    );

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}) even at \
         this scan's worst case; got {}",
        resources.instructions
    );
}

/// The sub-epoch path's own exposure to the SAME `try_advance_
/// finality` sweep the hourly path's own
/// `budget_finality_backward_scan_across_a_full_backfill_window`
/// (above) measures: `post_sub_signals` calls the identical
/// `try_advance_finality(&env, &config, &asset,
/// FINALITY_LOOKBACK_EPOCHS)` inline, over the SAME `Ring(asset)`
/// (the hourly ring, never `Sub(asset)`, since finality is always an
/// hourly concept), so a 71 epoch missing gap costs the sub-epoch
/// path exactly as much as it costs the hourly one. No separate
/// bound is needed (the sweep is already capped at
/// `FINALITY_LOOKBACK_EPOCHS`, shared by both paths), but this proves
/// the sub-epoch path's own call site is not secretly worse (e.g. an
/// extra per-hour cost stacking on top of the shared scan).
#[test]
fn budget_post_sub_signals_finality_scan_across_a_full_backfill_window() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let next_epoch = warm_full_ring(&env, &client, &asset);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    client.post_signals(&keeper, &asset, &signal_set(&env, next_epoch, 9_900_000));
    // The next 71 hours intentionally never posted at all (neither
    // path), leaving Ring(asset)'s own gap for the sweep to walk.
    let gap_start = next_epoch + 1;
    let resume_hour = gap_start + 71;

    env.ledger().set_timestamp(resume_hour * 3_600 + 300);
    client.post_sub_signals(
        &keeper,
        &asset,
        &resume_hour,
        &0u32,
        &sub_signal_set(&env, resume_hour, 9_900_000),
    );

    let estimate = env.cost_estimate();
    print_resources(
        "post_sub_signals, full ring, try_advance_finality scanning a 71 epoch missing gap",
        &estimate,
    );

    assert_eq!(
        client.score(&asset).epoch,
        next_epoch - 2,
        "the measured call's backward scan must walk nearly the full 71 epoch gap, \
         not stop early; a different result here means this test is not actually \
         measuring the scenario it claims to"
    );

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}) even at \
         this scan's worst case; got {}",
        resources.instructions
    );
}

/// Issue #11 fix (feat/treasury): the worst case for
/// `reward_posters_for_newly_final_epochs` is a FULL backfill window
/// (`FINALITY_LOOKBACK_EPOCHS`, 73 at the defaults) becoming Final in
/// one call, posted by SEVERAL different keepers rotating through
/// the window, so the grouping step's `Map` holds as many distinct
/// posters as the scenario allows and the single measured call must
/// both scan the whole window AND issue one `reward_keeper` cross
/// contract call per distinct poster found in it, not just walk past
/// a long missing gap the way
/// `budget_finality_backward_scan_across_a_full_backfill_window`
/// measures.
///
/// This version uses `MockStaking`, whose own `reward_keeper` is a
/// cheap storage write with no further cross-contract call: it
/// UNDERSTATES the real cost, since production's `Staking.reward_keeper`
/// itself calls `Treasury.accrue_reward`, a second hop this mock never
/// makes. `budget_reward_keeper_grouping_with_real_staking_and_treasury`
/// in `staking`'s own integration test module measures the real,
/// 3-contract version of this same scenario; both numbers are reported
/// together in the PR description. This mock version stays, as a
/// cheap, RiskOracle-only lower bound that isolates the finality scan
/// and grouping cost from the reward-payout cost the other test adds.
///
/// Builds this by posting `FINALITY_LOOKBACK_EPOCHS` consecutive
/// epochs in quick succession (each one still Pending, none yet
/// observed Final: every post lands well inside the previous
/// posts' own `SIGNAL_DISPUTE_SECS` windows), rotating through 5
/// keepers round robin, then waiting out the dispute window and
/// making ONE further call whose finality sweep observes the entire
/// run as newly Final at once.
#[test]
fn budget_reward_keeper_grouping_across_a_full_backfill_window_several_keepers_mock_staking() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    const KEEPER_COUNT: usize = 5;
    let keepers: std::vec::Vec<Address> =
        (0..KEEPER_COUNT).map(|_| Address::generate(&env)).collect();

    // `lookback - 1` epochs, posted in a backfill (every post at the
    // SAME fixed `now`, each posting an older epoch than the
    // contract's own notion of "current"), so NONE of them can
    // become Final during this loop: pending_until is always
    // `now + SIGNAL_DISPUTE_SECS`, strictly in the future relative to
    // this same, unmoving `now`, regardless of how many of these
    // backfill posts happen first. This is the key difference from
    // posting them one real epoch apart (which, tried first, found a
    // genuine timing coincidence: EPOCH_SECS * 2 lands exactly on
    // SIGNAL_DISPUTE_SECS at the defaults, so epochs kept crossing
    // into Final mid-loop instead of staying Pending for this test's
    // own final, single sweep).
    let lookback = crate::FINALITY_LOOKBACK_EPOCHS as u64;
    let backfill_count = lookback - 1;
    let now = backfill_count * 3_600 + 3_600;
    env.ledger().set_timestamp(now);
    for epoch in 0..backfill_count {
        let keeper = &keepers[epoch as usize % KEEPER_COUNT];
        client.post_signals(keeper, &asset, &signal_set(&env, epoch, 9_900_000));
    }

    // One more post, far enough past the window's own dispute delay
    // that the entire backfilled run above crosses into Final during
    // THIS call's own finality sweep. The test harness's own default
    // CPU/memory budget (100M instructions, 40MB) is NOT reset between
    // calls the way a real network resets it per transaction: it is
    // one running total across every call this whole test makes, so
    // the 72 setup posts above already spent most of it before this
    // measured call even starts (confirmed: the measured call alone,
    // ~8.6M instructions, could never exceed 100M on its own). Lift
    // the budget here, right before the measured call, so the setup
    // loop's own already-spent cost cannot cut into the headroom this
    // test needs to measure the call that actually matters against
    // the real network ceiling.
    env.cost_estimate().budget().reset_unlimited();
    let final_epoch = backfill_count;
    env.ledger()
        .set_timestamp((final_epoch + 1) * 3_600 + crate::SIGNAL_DISPUTE_SECS);
    client.post_signals(
        &keepers[0],
        &asset,
        &signal_set(&env, final_epoch, 9_900_000),
    );

    let estimate = env.cost_estimate();
    print_resources(
        "post_signals, full backfill window (73 epochs) crossing into Final in one call, \
         5 keepers, MockStaking (understates the real cost; see the real-contracts version \
         in staking's own integration test module)",
        &estimate,
    );

    // Sanity check this test actually measures what it claims: all 5
    // keepers must have been rewarded by this one call (one
    // reward_keeper call per distinct poster, grouped), not 0 and not
    // partially.
    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);
    for keeper in &keepers {
        assert!(
            staking_client.reward_keeper_epochs(keeper) > 0,
            "every one of the 5 rotating keepers must have been rewarded by this call"
        );
    }
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "reward_keeper")),
        KEEPER_COUNT as u32,
        "exactly one reward_keeper call per distinct poster, not one per epoch"
    );

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}) even with \
         every keeper in the window rewarded in one call; got {}",
        resources.instructions
    );
    assert!(
        (resources.write_bytes as u64) < TX_MAX_WRITE_BYTES / 2,
        "must stay under 50% of tx_max_write_bytes ({TX_MAX_WRITE_BYTES}); got {}",
        resources.write_bytes
    );
    assert!(
        (resources.disk_read_entries as u64) < TX_MAX_READ_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_disk_read_entries ({TX_MAX_READ_LEDGER_ENTRIES}); got {}",
        resources.disk_read_entries
    );
    assert!(
        (resources.write_entries as u64) < TX_MAX_WRITE_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_write_ledger_entries ({TX_MAX_WRITE_LEDGER_ENTRIES}); got {}",
        resources.write_entries
    );
}

// -- Section 5.9 (v1.5): sub-epoch budgets --

fn sub_signal_set(env: &Env, hour: u64, peg_ratio: i128) -> SignalSet {
    SignalSet {
        epoch: hour,
        posted_at: 0,
        peg_ratio,
        peg_ratio_p10: peg_ratio,
        liquidity_2pct: 500_000_000_000,
        redemption_net: 10_000_000_000,
        supply: 10_000_000_000_000,
        supply_change_bps: 0,
        issuer_actions: IssuerActions {
            clawbacks: 1,
            clawback_amount: 5_000_000_000,
            auth_revocations: 1,
            flag_changes: 0,
        },
        endpoint: EndpointStatus::Up,
        inputs_hash: BytesN::from_array(env, &[7u8; 32]),
        poster: Address::generate(env),
    }
}

/// A single sub-epoch post at the 300s default, against a FRESH
/// `Sub(asset)` ring (the realistic steady state: unlike `Ring(asset)`,
/// `Sub(asset)` only ever holds at most 5 hours of history, so there
/// is no separate "cold" vs. "warm" ring size distinction to measure
/// the way `budget_post_signals_against_a_full_ring` needs one for the
/// 240 slot hourly ring).
#[test]
fn budget_post_sub_signals_at_the_300s_default() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let keeper = Address::generate(&env);
    let hour = 0u64;

    env.ledger().set_timestamp(300);
    client.post_sub_signals(
        &keeper,
        &asset,
        &hour,
        &0u32,
        &sub_signal_set(&env, hour, 9_900_000),
    );

    let estimate = env.cost_estimate();
    print_resources("post_sub_signals, 300s default, sub 0 of hour 0", &estimate);

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}); got {}",
        resources.instructions
    );
    assert!(
        resources.write_bytes < 65_536,
        "a single Sub(asset) entry must fit under contract_data_entry_size_bytes (65,536); got {}",
        resources.write_bytes
    );
}

/// The steady state of the most frequent call: a normal 5 minute post
/// in the middle of an hour, after the asset has been posting for a
/// while. Every sub-epoch of hour 0 and the first seven of hour 1 are
/// posted first, each at its own close, so the instance, `Sub(asset)`,
/// `SubRingNewest` and the hour's `HourPostedVia` already exist; the
/// measured post (hour 1, sub 7) is neither the asset's first post nor a
/// finality-scan catch-up, only what a keeper does every 300 seconds.
#[test]
fn budget_post_sub_signals_steady_state_mid_hour() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let keeper = Address::generate(&env);

    let post = |hour: u64, sub: u32| {
        env.ledger()
            .set_timestamp(hour * 3_600 + (sub as u64 + 1) * 300);
        client.post_sub_signals(
            &keeper,
            &asset,
            &hour,
            &sub,
            &sub_signal_set(&env, hour, 9_900_000),
        );
    };
    for sub in 0..12u32 {
        post(0, sub);
    }
    for sub in 0..7u32 {
        post(1, sub);
    }
    post(1, 7);

    let estimate = env.cost_estimate();
    print_resources(
        "post_sub_signals, steady state, hour 1 sub 7 after 19 earlier posts",
        &estimate,
    );
    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}); got {}",
        resources.instructions
    );
}

/// `resolve_sub_signal_dispute` overturning sub 0 of a full 12
/// sub-epoch hour near its 6 day ruling deadline: the case where the
/// overturn also re-extends the other 11 `SubSignals` entries, since
/// by then each is below the history re-extend threshold.
#[test]
fn budget_resolve_sub_signal_dispute_overturn_in_a_full_hour() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(crate::mocks::MockGovernor, ());
    let client = RiskOracleClient::new(
        &env,
        &env.register(
            RiskOracle,
            (governor.clone(), Address::generate(&env), staking),
        ),
    );
    let asset = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &Address::generate(&env)));
    let committee = Address::generate(&env);
    // Time and ledger sequence move together (5 seconds per ledger), so
    // TTLs age the way they would live. MockGovernor's committee is
    // temporary storage and is set again after aging (test setup only).
    let at = |t: u64| {
        env.ledger().set_timestamp(t);
        env.ledger().set_sequence_number((t / 5) as u32);
        crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    };

    let hour = 497_000u64;
    let keeper = Address::generate(&env);
    for sub in 0..12u32 {
        at(hour * 3_600 + (sub as u64 + 1) * 300);
        client.post_sub_signals(
            &keeper,
            &asset,
            &hour,
            &sub,
            &sub_signal_set(&env, hour, 9_900_000),
        );
    }
    let disputed_at = (hour + 1) * 3_600;
    at(disputed_at);
    client.dispute_sub_signals(
        &Address::generate(&env),
        &asset,
        &hour,
        &0u32,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    at(disputed_at + 518_400 - 60);
    client.resolve_sub_signal_dispute(
        &asset,
        &hour,
        &0u32,
        &false,
        &BytesN::from_array(&env, &[2u8; 32]),
    );

    let estimate = env.cost_estimate();
    print_resources(
        "resolve_sub_signal_dispute, overturn near the ruling deadline, full 12 sub-epoch hour",
        &estimate,
    );
    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}); got {}",
        resources.instructions
    );
}

/// The worst case for `build_hour` at the 300s default: 12 sub-epochs
/// in the hour, with the LAST one arriving as a genuine backfill (its
/// own post happening well after its own close, inside
/// `sub_backfill_secs`), so the measured call must read every one of
/// the 12 sub-epochs' own `SubSignals` entries (for
/// `hash_sub_epoch_inputs`), compute the full S4 roll-up, write
/// `Signals`/`Ring`, and run `reward_sub_epoch_keepers` across however
/// many distinct posters contributed.
///
/// The backfilled post itself cannot be the measured call: a freshly
/// posted sub-epoch is only Pending, never immediately Final (its OWN
/// `SIGNAL_DISPUTE_SECS` dispute window has not closed yet), so
/// `try_build_hour`'s own coverage scan would still find it NotReady
/// right after posting. The measured call is instead the explicit,
/// permissionless `build_hour` trigger, made once every sub-epoch
/// (including the backfilled one) has cleared its own window.
#[test]
fn budget_build_hour_worst_case_300s_12_subs_with_a_backfill() {
    let env = Env::default();
    let (client, asset) = setup(&env);
    let keeper = Address::generate(&env);
    let hour = 0u64;

    // Sub-epochs 0..11 post promptly, each right at its own close.
    for sub in 0..11u32 {
        let sub_start = sub as u64 * 300;
        env.ledger().set_timestamp(sub_start + 300);
        client.post_sub_signals(
            &keeper,
            &asset,
            &hour,
            &sub,
            &sub_signal_set(&env, hour, 9_900_000 + sub as i128 * 1_000),
        );
    }
    // Sub-epoch 11 posts as a genuine backfill: well after its own
    // close (hour*3,600 + 11*300 + 300 = 3,600), still inside
    // sub_backfill_secs (7,200s).
    env.ledger().set_timestamp(10_000);
    client.post_sub_signals(
        &keeper,
        &asset,
        &hour,
        &11u32,
        &sub_signal_set(&env, hour, 9_900_000 + 11_000),
    );

    // Past every one of the 12 sub-epochs' own pending_until,
    // including sub 11's (the latest to post, so the latest to
    // close): now every one reads effectively Final, not just
    // Pending-but-posted.
    env.ledger()
        .set_timestamp(10_000 + crate::SIGNAL_DISPUTE_SECS + 1);
    client.build_hour(&asset, &hour);

    let estimate = env.cost_estimate();
    print_resources(
        "build_hour, full 12 sub-epoch hour at the 300s default, one sub-epoch backfilled",
        &estimate,
    );

    assert!(
        client.is_final(&asset, &hour),
        "the measured build_hour call must have actually built the hour Final, not left \
         it pending; a different result here means this test is not measuring the \
         scenario it claims to"
    );

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}) even for \
         build_hour's own worst case (12 SubSignals reads, the full S4 roll-up, and the \
         reward_sub_epoch_keepers grouping call) running inline with the triggering post; \
         got {}",
        resources.instructions
    );
    assert!(
        (resources.write_bytes as u64) < TX_MAX_WRITE_BYTES / 2,
        "must stay under 50% of tx_max_write_bytes ({TX_MAX_WRITE_BYTES}); got {}",
        resources.write_bytes
    );
    assert!(
        (resources.disk_read_entries as u64) < TX_MAX_READ_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_disk_read_entries ({TX_MAX_READ_LEDGER_ENTRIES}); got {}",
        resources.disk_read_entries
    );
    assert!(
        (resources.write_entries as u64) < TX_MAX_WRITE_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_write_ledger_entries ({TX_MAX_WRITE_LEDGER_ENTRIES}); got {}",
        resources.write_entries
    );
}

/// #92 (docs/design/held-repost-disputes.md): a full 12 sub-epoch hour
/// whose sub 0 is overturned near its 6-day deadline and reposted past
/// the ring span, so the repost lives only in `HeldHour`. Measures the
/// repost's dispute, then its resolution: a keeper-wins ruling, or the
/// timeout at the last ledger of its `SubDispute`. Both resolutions
/// build the hour.
fn budget_held_repost_flow(timeout: bool) {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(crate::mocks::MockGovernor, ());
    let client = RiskOracleClient::new(
        &env,
        &env.register(
            RiskOracle,
            (governor.clone(), Address::generate(&env), staking),
        ),
    );
    let asset = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &Address::generate(&env)));
    let committee = Address::generate(&env);
    let at = |t: u64| {
        env.ledger().set_timestamp(t);
        env.ledger().set_sequence_number((t / 5) as u32);
        crate::mocks::MockGovernorClient::new(&env, &governor).set_committee(&committee);
    };
    let hash = |b: u8| BytesN::from_array(&env, &[b; 32]);

    let hour = 497_000u64;
    let keeper = Address::generate(&env);
    for sub in 0..12u32 {
        at(hour * 3_600 + (sub as u64 + 1) * 300);
        let peg = if sub == 0 { 1 } else { 9_900_000 };
        client.post_sub_signals(
            &keeper,
            &asset,
            &hour,
            &sub,
            &sub_signal_set(&env, hour, peg),
        );
    }
    let disputer = Address::generate(&env);
    let d1 = (hour + 1) * 3_600;
    at(d1);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &hash(1));
    let o1 = d1 + crate::SIGNAL_DISPUTE_RULING_SECS - 60;
    at(o1);
    client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &false, &hash(2));
    let repost = o1 + 7_200 - 60;
    at(repost);
    client.post_sub_signals(
        &keeper,
        &asset,
        &hour,
        &0u32,
        &sub_signal_set(&env, hour, 9_900_000),
    );

    let d2 = repost + 60;
    at(d2);
    client.dispute_sub_signals(&disputer, &asset, &hour, &0u32, &hash(3));
    if !timeout {
        print_resources(
            "dispute_sub_signals, a held repost, full 12 sub-epoch hour",
            &env.cost_estimate(),
        );
    }

    if timeout {
        at(d2 + (crate::SUB_DISPUTE_TTL_LEDGERS as u64) * 5 - 5);
        client.resolve_sub_dispute_timeout(&asset, &hour, &0u32);
        print_resources(
            "resolve_sub_dispute_timeout, a held repost at the last ledger of its SubDispute (builds)",
            &env.cost_estimate(),
        );
    } else {
        at(d2 + crate::SIGNAL_DISPUTE_RULING_SECS - 60);
        client.resolve_sub_signal_dispute(&asset, &hour, &0u32, &true, &hash(4));
        print_resources(
            "resolve_sub_signal_dispute, keeper wins on a held repost (builds)",
            &env.cost_estimate(),
        );
    }
    assert!(client.is_final(&asset, &hour));
    let resources = env.cost_estimate().resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}); got {}",
        resources.instructions
    );
}

#[test]
fn budget_held_repost_dispute_and_keeper_wins() {
    budget_held_repost_flow(false);
}

#[test]
fn budget_held_repost_late_timeout() {
    budget_held_repost_flow(true);
}
