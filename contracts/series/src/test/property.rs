//! Property tests required by the task brief: random sequences of
//! `Series` operations against the real `RiskOracle`, `EventRegistry`,
//! `Staking`, `Treasury` and `MarketFactory` (the same wiring
//! `test::integration` uses, duplicated here rather than shared,
//! matching `event_registry::budget_test`'s own documented reasoning
//! for the same choice), checking I1-I6, I9, I10, I11 and I13 after
//! every single operation, I24 and I26 each in their own dedicated
//! property below. Every mutating call goes through its `try_`
//! variant: a real contract error is a `Result::Err` here, never a
//! host trap, so nothing is ever silently skipped past.
//!
//! `docs/design/markets.md` Section 13's own invariant table:
//! I1  Total cover units <= sum of sellers' collateral.
//! I2  Per seller, cover_written <= collateral.
//! I3  USDC balance of a series == total_collateral + total_premium
//!     - total_paid - total_withdrawn (an exact identity here, not
//!     just `>=`: this build's own fee path leaves nothing stranded,
//!     see `balance_identity_is_zero`).
//! I4  Cover units minted only in Open, only against a real fill.
//! I5  Payouts only in Triggered, only for a covering event.
//! I6  Premiums not withdrawable before Triggered or Expired.
//! I9  Sum of open cover per asset <= asset cap, checked and reserved
//!     in one call.
//! I10 No role moves USDC out of a series except seller withdraw,
//!     holder claim, or the Treasury fee at purchase - folded into
//!     I3's own exact balance identity above: an unauthorized leak
//!     would show up there directly.
//! I11 Series terms never change after `open_series`.
//! I13 Every live series pins the current canonical definition
//!     version of each kind it covers - checked once, since
//!     `def_versions` is fixed at construction (I11).

extern crate std;
use std::vec;

use proptest::prelude::*;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{token, Address, BytesN, Env, Map, Vec};
use sylox_types::{
    AssetConfig, EndpointStatus, EventDefinition, EventKind, IssuerActions, IssuerFlags, Reference,
    SeriesState, SeriesTerms, SignalSet,
};

use crate::{Series, SeriesClient};

/// Case count for one of this file's three property tests, adjustable
/// without touching what CI itself runs: reads the named environment
/// variable if set and parseable as a `u32`, falls back to `default`
/// otherwise. CI never sets these, so CI always gets `default` (this
/// file's own prior case counts). PR #67 review: run locally at a
/// larger count and post the counts, run times and results on the PR.
fn cases_from_env(var: &str, default: u32) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

const EPOCH_SECS: u64 = 3_600;
const SUB_EPOCH_SECS: u64 = 300;
const KEEPER_BOND: i128 = 50_000_000_000;
const EPOCH_BASE: u64 = 497_000;

struct Fixture<'a> {
    oracle: risk_oracle::RiskOracleClient<'a>,
    registry: event_registry::EventRegistryClient<'a>,
    factory: market_factory::MarketFactoryClient<'a>,
    staking: staking::StakingClient<'a>,
    usdc: Address,
    usdc_client: token::TokenClient<'a>,
    usdc_admin_client: token::StellarAssetClient<'a>,
}

fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();

    let governor = Address::generate(env);
    let sac_admin = Address::generate(env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let usdc_client = token::TokenClient::new(env, &usdc);
    let usdc_admin_client = token::StellarAssetClient::new(env, &usdc);

    let registry_id = Address::generate(env);
    let oracle_id = Address::generate(env);
    let staking_id = Address::generate(env);
    let treasury_id = Address::generate(env);
    let factory_id = Address::generate(env);

    env.register_at(
        &treasury_id,
        treasury::Treasury,
        (governor.clone(), staking_id.clone(), usdc.clone()),
    );

    env.register_at(
        &staking_id,
        staking::Staking,
        (
            governor.clone(),
            oracle_id.clone(),
            registry_id.clone(),
            treasury_id.clone(),
            usdc.clone(),
        ),
    );
    let staking = staking::StakingClient::new(env, &staking_id);

    env.register_at(
        &oracle_id,
        risk_oracle::RiskOracle,
        (governor.clone(), registry_id.clone(), staking_id.clone()),
    );
    let oracle = risk_oracle::RiskOracleClient::new(env, &oracle_id);

    env.register_at(
        &registry_id,
        event_registry::EventRegistry,
        (
            governor.clone(),
            oracle_id.clone(),
            staking_id.clone(),
            factory_id.clone(),
            usdc.clone(),
        ),
    );
    let registry = event_registry::EventRegistryClient::new(env, &registry_id);

    let series_wasm = env.upload(Series);
    env.register_at(
        &factory_id,
        market_factory::MarketFactory,
        (
            governor,
            oracle_id,
            registry_id,
            treasury_id,
            usdc.clone(),
            series_wasm,
        ),
    );
    let factory = market_factory::MarketFactoryClient::new(env, &factory_id);

    Fixture {
        oracle,
        registry,
        factory,
        staking,
        usdc,
        usdc_client,
        usdc_admin_client,
    }
}

fn asset_config(env: &Env, asset: &Address, issuer: &Address) -> AssetConfig {
    AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: soroban_sdk::String::from_str(env, "example.com"),
        amm_adapters: Vec::new(env),
        fx_adapter: None,
        min_liquidity: 100_000_000_000,
        issuer_flags: IssuerFlags::default(),
        enabled: true,
    }
}

fn depeg_definition(asset: &Address) -> EventDefinition {
    EventDefinition {
        asset: asset.clone(),
        kind: EventKind::Depeg,
        version: 0,
        reference: Reference::Usd,
        depeg_threshold: 9_500_000,
        depeg_window_secs: 259_200,
        max_missing_epochs: 6,
        cure_threshold: 9_800_000,
        freeze_pct_bps: 0,
        auth_revocation_threshold: 0,
        mint_spike_bps: 0,
        halt_window_secs: 0,
        challenge_secs: 86_400,
        ruling_deadline_secs: 1_209_600,
    }
}

fn signal_set(env: &Env, keeper: &Address, epoch: u64, peg_ratio: i128) -> SignalSet {
    SignalSet {
        epoch,
        posted_at: 0,
        peg_ratio,
        peg_ratio_p10: peg_ratio,
        liquidity_2pct: 500_000_000_000,
        redemption_net: 0,
        supply: 10_000_000_000_000,
        supply_change_bps: 0,
        issuer_actions: IssuerActions::default(),
        endpoint: EndpointStatus::Unknown,
        inputs_hash: BytesN::from_array(env, &[7u8; 32]),
        poster: keeper.clone(),
    }
}

fn post_run(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    start_epoch: u64,
    count: u64,
    peg_ratio: i128,
) {
    for i in 0..count {
        let epoch = start_epoch + i;
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        oracle.post_signals(keeper, asset, &signal_set(env, keeper, epoch, peg_ratio));
    }
}

fn post_sub_signal(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    hour: u64,
    peg_ratio: i128,
) {
    let sub_start = hour * EPOCH_SECS;
    env.ledger().set_timestamp(sub_start + SUB_EPOCH_SECS);
    oracle.post_sub_signals(
        keeper,
        asset,
        &hour,
        &0u32,
        &signal_set(env, keeper, hour, peg_ratio),
    );
}

/// Opens one real series with a full 168-slot `RiskOracle` ring and a
/// fresh feed, so `buy_cover` has a genuine chance to succeed.
fn open_real_series(env: &Env) -> (Fixture<'_>, SeriesClient<'_>, Address, Address, SeriesTerms) {
    let fx = setup(env);

    let asset_issuer = Address::generate(env);
    let asset = env
        .register_stellar_asset_contract_v2(asset_issuer.clone())
        .address();
    fx.oracle
        .add_asset(&asset_config(env, &asset, &asset_issuer));
    fx.registry.register_definition(&depeg_definition(&asset));
    fx.factory
        .set_related_sellers(&asset, &Vec::from_array(env, [asset_issuer.clone()]));

    let keeper = Address::generate(env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin_client.mint(&keeper, &KEEPER_BOND);
    fx.staking.stake(&keeper, &KEEPER_BOND);

    post_run(env, &fx.oracle, &keeper, &asset, EPOCH_BASE, 168, 9_900_000);
    let series_start_epoch = EPOCH_BASE + 168;
    post_sub_signal(
        env,
        &fx.oracle,
        &keeper,
        &asset,
        series_start_epoch,
        9_900_000,
    );

    let now = env.ledger().timestamp();
    let mut def_versions = Map::new(env);
    def_versions.set(EventKind::Depeg, 1u32);
    let terms = SeriesTerms {
        asset: asset.clone(),
        def_versions,
        settlement: fx.usdc.clone(),
        start: now,
        expiry: now + 90 * 86_400,
        claim_window_secs: 30 * 86_400,
        cap: 1_000_000_000_000,
        max_cover_per_buyer: 1_000_000_000_000,
        require_holding: false,
        fee_bps: 750,
    };
    let series_address = fx.factory.open_series(&terms);
    let series = SeriesClient::new(env, &series_address);
    (fx, series, series_address, keeper, terms)
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Deposit(u8, i128),
    Quote(u8, u32, i128),
    CancelQuote(u8),
    Withdraw(u8, i128),
    BuyCover(u8, i128, u32),
    TransferPosition(u8, u8),
    Claim(u8, i128),
    ClaimFor(u8),
    Sync,
    AdvanceTime(u32),
    /// Runs the real depeg-to-Declared flow and `trigger()`s the
    /// series, at most once per case (guarded by the caller): this is
    /// how I4/I5's own Triggered-state paths get genuinely exercised,
    /// not just checked vacuously.
    DeclareAndTrigger,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u8..3, 0i128..2_000_000_000).prop_map(|(s, a)| Op::Deposit(s, a)),
        3 => (0u8..3, 0u32..2_000, 0i128..2_000_000_000).prop_map(|(s, r, a)| Op::Quote(s, r, a)),
        1 => (0u8..3).prop_map(Op::CancelQuote),
        2 => (0u8..3, 0i128..2_000_000_000).prop_map(|(s, a)| Op::Withdraw(s, a)),
        3 => (0u8..2, 0i128..1_500_000_000, 0u32..2_000).prop_map(|(b, a, r)| Op::BuyCover(b, a, r)),
        1 => (0u8..3, 0u8..3).prop_map(|(f, t)| Op::TransferPosition(f, t)),
        2 => (0u8..2, 0i128..1_500_000_000).prop_map(|(b, a)| Op::Claim(b, a)),
        1 => (0u8..2).prop_map(Op::ClaimFor),
        2 => Just(Op::Sync),
        2 => (0u32..200_000).prop_map(Op::AdvanceTime),
        1 => Just(Op::DeclareAndTrigger),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases_from_env("SERIES_PROPTEST_CASES_CORE", 20),
        .. ProptestConfig::default()
    })]

    #[test]
    fn i1_i2_i3_i4_i5_i6_i9_i10_i11_i13_never_break(ops in prop::collection::vec(op(), 1..40)) {
        let env = Env::default();
        let (fx, series, series_address, keeper, initial_terms) = open_real_series(&env);
        let asset = initial_terms.asset.clone();

        // I13: pinned at construction, checked once (I11 proves it
        // can never change afterward).
        prop_assert_eq!(
            fx.registry.current_version(&asset, &EventKind::Depeg),
            initial_terms.def_versions.get(EventKind::Depeg).unwrap()
        );

        let sellers: std::vec::Vec<Address> = (0..3).map(|_| Address::generate(&env)).collect();
        let buyers: std::vec::Vec<Address> = (0..2).map(|_| Address::generate(&env)).collect();
        for s in &sellers {
            fx.usdc_admin_client.mint(s, &10_000_000_000);
        }
        for b in &buyers {
            fx.usdc_admin_client.mint(b, &10_000_000_000);
        }

        let mut triggered_once = false;

        for operation in ops {
            match operation {
                Op::Deposit(idx, amount) => {
                    let seller = &sellers[idx as usize];
                    let _ = series.try_deposit(seller, &amount);
                }
                Op::Quote(idx, rate, available) => {
                    let seller = &sellers[idx as usize];
                    let _ = series.try_quote(seller, &rate, &available);
                }
                Op::CancelQuote(idx) => {
                    let seller = &sellers[idx as usize];
                    let _ = series.try_cancel_quote(seller);
                }
                Op::Withdraw(idx, amount) => {
                    let seller = &sellers[idx as usize];
                    // I6: in Open/Closed, a withdraw attempt (success
                    // or failure) must never move `premium_earned`.
                    let state_before = series.state();
                    let premium_before = series
                        .position(seller)
                        .map(|p| p.premium_earned)
                        .unwrap_or(0);
                    let _ = series.try_withdraw(seller, &amount);
                    if matches!(state_before, SeriesState::Open | SeriesState::Closed) {
                        let premium_after = series
                            .position(seller)
                            .map(|p| p.premium_earned)
                            .unwrap_or(0);
                        prop_assert_eq!(
                            premium_before, premium_after,
                            "I6 violated: premium_earned moved during an Open/Closed withdraw"
                        );
                    }
                }
                Op::BuyCover(idx, amount, max_rate) => {
                    let buyer = &buyers[idx as usize];
                    let state_before = series.state();
                    let balance_before = series.balance(buyer);
                    if let Ok(Ok((filled, _premium))) =
                        series.try_buy_cover(buyer, &amount, &max_rate, &series.premium_quote(&amount, &max_rate).1)
                    {
                        // I4: a real mint only ever happens in Open.
                        prop_assert_eq!(
                            state_before, SeriesState::Open,
                            "I4 violated: buy_cover minted outside Open"
                        );
                        prop_assert_eq!(series.balance(buyer), balance_before + filled);
                    }
                }
                Op::TransferPosition(from_idx, to_idx) => {
                    let from = &sellers[from_idx as usize];
                    let to = &sellers[to_idx as usize];
                    let _ = series.try_transfer_position(from, to);
                }
                Op::Claim(idx, amount) => {
                    let buyer = &buyers[idx as usize];
                    let state_before = series.state();
                    if let Ok(Ok(paid)) = series.try_claim(buyer, &amount) {
                        // I5: a real payout only ever happens in Triggered.
                        prop_assert_eq!(
                            state_before, SeriesState::Triggered,
                            "I5 violated: claim paid outside Triggered"
                        );
                        prop_assert!(paid >= 0);
                    }
                }
                Op::ClaimFor(idx) => {
                    let buyer = &buyers[idx as usize];
                    let state_before = series.state();
                    if let Ok(Ok(paid)) = series.try_claim_for(buyer) {
                        if paid > 0 {
                            prop_assert_eq!(
                                state_before, SeriesState::Triggered,
                                "I5 violated: claim_for paid outside Triggered"
                            );
                        }
                    }
                }
                Op::Sync => {
                    let _ = series.sync();
                }
                Op::AdvanceTime(secs) => {
                    let now = env.ledger().timestamp();
                    env.ledger().set_timestamp(now + secs as u64);
                }
                Op::DeclareAndTrigger => {
                    if !triggered_once && env.ledger().timestamp() <= initial_terms.expiry {
                        triggered_once = true;
                        let now_epoch = env.ledger().timestamp() / EPOCH_SECS;
                        let window_start_epoch = now_epoch + 1;
                        post_run(&env, &fx.oracle, &keeper, &asset, window_start_epoch, 168, 9_900_000);
                        post_run(&env, &fx.oracle, &keeper, &asset, window_start_epoch + 168, 72, 9_000_000);
                        let now = env.ledger().timestamp();
                        env.ledger().set_timestamp(now + 7_200 + 1);
                        let proposer = Address::generate(&env);
                        if let Ok(Ok(id)) =
                            fx.registry.try_propose_tier1(&proposer, &asset, &EventKind::Depeg, &1)
                        {
                            let after = now + 7_200 + 1 + 86_400 + 259_200 + 7_200 + 1;
                            env.ledger().set_timestamp(after);
                            let _ = fx.registry.try_finalize(&id);
                            let _ = series.try_trigger();
                        }
                    }
                }
            }

            // I11: terms never change after construction.
            prop_assert_eq!(series.terms(), initial_terms.clone());

            // I1/I2: cover units vs collateral, per seller and in total.
            let mut total_collateral = 0i128;
            for s in &sellers {
                if let Some(position) = series.position(s) {
                    prop_assert!(
                        position.cover_written <= position.collateral,
                        "I2 violated: seller cover_written {} > collateral {}",
                        position.cover_written,
                        position.collateral
                    );
                    total_collateral += position.collateral;
                }
            }
            let total_cover_units: i128 = buyers.iter().map(|b| series.balance(b)).sum();
            prop_assert!(
                total_cover_units <= total_collateral,
                "I1 violated: total cover units {} > sum of sellers' collateral {}",
                total_cover_units,
                total_collateral
            );

            // I3 (and I10, folded in): the series' own USDC balance is
            // an exact function of its own tracked totals.
            let totals = series.totals();
            let expected_balance =
                totals.total_collateral + totals.total_premium - totals.total_paid - totals.total_withdrawn;
            prop_assert_eq!(
                fx.usdc_client.balance(&series_address),
                expected_balance,
                "I3 violated"
            );

            // I9: open cover never exceeds the asset-wide cap.
            if let Ok(Ok(cap)) = fx.factory.try_cover_cap(&asset) {
                prop_assert!(
                    fx.factory.open_cover(&asset) <= cap,
                    "I9 violated: open cover exceeds cover_cap"
                );
            }
        }
    }
}

/// Permanent regression test (PR #67 review): the exact 29-op
/// sequence proptest's own shrinker reduced the one-time, otherwise
/// unreproduced `i1_i2_i3_i4_i5_i6_i9_i10_i11_i13_never_break` failure
/// to, hand-written with no proptest and no seed. Shrinking only ever
/// accepts a smaller candidate by re-running the test function
/// against it and observing it fail, so this exact sequence did fail,
/// more than once, inside the original process; this test asks
/// whether it still does against a fresh, deterministic `Env` (each
/// case already builds one from scratch, with no state shared across
/// cases - `open_real_series` and everything above it is the only
/// thing a case touches). Every invariant is checked after every
/// single op, by index, with both sides of each comparison in the
/// panic message, exactly like the property test above, just with
/// plain `assert!`/`assert_eq!` in place of `prop_assert!`/
/// `prop_assert_eq!` (unavailable outside a `proptest!` body) and a
/// concrete op list in place of a generated one.
#[test]
fn shrunk_regression_from_the_one_time_unreproduced_core_property_failure() {
    let env = Env::default();
    let (fx, series, series_address, keeper, initial_terms) = open_real_series(&env);
    let asset = initial_terms.asset.clone();

    assert_eq!(
        fx.registry.current_version(&asset, &EventKind::Depeg),
        initial_terms.def_versions.get(EventKind::Depeg).unwrap()
    );

    let sellers: std::vec::Vec<Address> = (0..3).map(|_| Address::generate(&env)).collect();
    let buyers: std::vec::Vec<Address> = (0..2).map(|_| Address::generate(&env)).collect();
    for s in &sellers {
        fx.usdc_admin_client.mint(s, &10_000_000_000);
    }
    for b in &buyers {
        fx.usdc_admin_client.mint(b, &10_000_000_000);
    }

    let mut triggered_once = false;

    // The exact shrunk sequence from
    // contracts/series/proptest-regressions/test/property.txt,
    // transcribed verbatim.
    let ops = std::vec![
        Op::TransferPosition(2, 1),
        Op::Deposit(1, 1308872368),
        Op::Sync,
        Op::BuyCover(1, 1364954525, 1339),
        Op::DeclareAndTrigger,
        Op::BuyCover(0, 182912212, 207),
        Op::ClaimFor(1),
        Op::Withdraw(2, 138548896),
        Op::BuyCover(1, 150522093, 832),
        Op::AdvanceTime(102219),
        Op::Claim(1, 717723194),
        Op::Claim(1, 37601408),
        Op::Withdraw(1, 131867369),
        Op::CancelQuote(0),
        Op::Sync,
        Op::Quote(0, 146, 1911822422),
        Op::Quote(1, 124, 969258052),
        Op::Claim(1, 863845665),
        Op::Claim(0, 1316375229),
        Op::Quote(1, 350, 346297970),
        Op::Claim(0, 705137441),
        Op::Sync,
        Op::AdvanceTime(35792),
        Op::DeclareAndTrigger,
        Op::Deposit(1, 939755146),
        Op::Sync,
        Op::Withdraw(2, 924356086),
        Op::ClaimFor(1),
        Op::BuyCover(1, 624502862, 1084),
        Op::Deposit(1, 1799942037),
    ];

    for (i, operation) in ops.into_iter().enumerate() {
        match operation {
            Op::Deposit(idx, amount) => {
                let seller = &sellers[idx as usize];
                let _ = series.try_deposit(seller, &amount);
            }
            Op::Quote(idx, rate, available) => {
                let seller = &sellers[idx as usize];
                let _ = series.try_quote(seller, &rate, &available);
            }
            Op::CancelQuote(idx) => {
                let seller = &sellers[idx as usize];
                let _ = series.try_cancel_quote(seller);
            }
            Op::Withdraw(idx, amount) => {
                let seller = &sellers[idx as usize];
                let state_before = series.state();
                let premium_before = series
                    .position(seller)
                    .map(|p| p.premium_earned)
                    .unwrap_or(0);
                let _ = series.try_withdraw(seller, &amount);
                if matches!(state_before, SeriesState::Open | SeriesState::Closed) {
                    let premium_after = series
                        .position(seller)
                        .map(|p| p.premium_earned)
                        .unwrap_or(0);
                    assert_eq!(
                        premium_before, premium_after,
                        "op {i} ({operation:?}): I6 violated: premium_earned moved from {premium_before} to {premium_after} during an Open/Closed withdraw"
                    );
                }
            }
            Op::BuyCover(idx, amount, max_rate) => {
                let buyer = &buyers[idx as usize];
                let state_before = series.state();
                let balance_before = series.balance(buyer);
                if let Ok(Ok((filled, _premium))) = series.try_buy_cover(
                    buyer,
                    &amount,
                    &max_rate,
                    &series.premium_quote(&amount, &max_rate).1,
                ) {
                    assert_eq!(
                        state_before,
                        SeriesState::Open,
                        "op {i} ({operation:?}): I4 violated: buy_cover minted outside Open, state was {state_before:?}"
                    );
                    assert_eq!(
                        series.balance(buyer),
                        balance_before + filled,
                        "op {i} ({operation:?}): buyer balance did not increase by the filled amount"
                    );
                }
            }
            Op::TransferPosition(from_idx, to_idx) => {
                let from = &sellers[from_idx as usize];
                let to = &sellers[to_idx as usize];
                let _ = series.try_transfer_position(from, to);
            }
            Op::Claim(idx, amount) => {
                let buyer = &buyers[idx as usize];
                let state_before = series.state();
                if let Ok(Ok(paid)) = series.try_claim(buyer, &amount) {
                    assert_eq!(
                        state_before,
                        SeriesState::Triggered,
                        "op {i} ({operation:?}): I5 violated: claim paid outside Triggered, state was {state_before:?}"
                    );
                    assert!(
                        paid >= 0,
                        "op {i} ({operation:?}): claim returned a negative amount: {paid}"
                    );
                }
            }
            Op::ClaimFor(idx) => {
                let buyer = &buyers[idx as usize];
                let state_before = series.state();
                if let Ok(Ok(paid)) = series.try_claim_for(buyer) {
                    if paid > 0 {
                        assert_eq!(
                            state_before,
                            SeriesState::Triggered,
                            "op {i} ({operation:?}): I5 violated: claim_for paid {paid} outside Triggered, state was {state_before:?}"
                        );
                    }
                }
            }
            Op::Sync => {
                let _ = series.sync();
            }
            Op::AdvanceTime(secs) => {
                let now = env.ledger().timestamp();
                env.ledger().set_timestamp(now + secs as u64);
            }
            Op::DeclareAndTrigger => {
                if !triggered_once && env.ledger().timestamp() <= initial_terms.expiry {
                    triggered_once = true;
                    let now_epoch = env.ledger().timestamp() / EPOCH_SECS;
                    let window_start_epoch = now_epoch + 1;
                    post_run(
                        &env,
                        &fx.oracle,
                        &keeper,
                        &asset,
                        window_start_epoch,
                        168,
                        9_900_000,
                    );
                    post_run(
                        &env,
                        &fx.oracle,
                        &keeper,
                        &asset,
                        window_start_epoch + 168,
                        72,
                        9_000_000,
                    );
                    let now = env.ledger().timestamp();
                    env.ledger().set_timestamp(now + 7_200 + 1);
                    let proposer = Address::generate(&env);
                    if let Ok(Ok(id)) =
                        fx.registry
                            .try_propose_tier1(&proposer, &asset, &EventKind::Depeg, &1)
                    {
                        let after = now + 7_200 + 1 + 86_400 + 259_200 + 7_200 + 1;
                        env.ledger().set_timestamp(after);
                        let _ = fx.registry.try_finalize(&id);
                        let _ = series.try_trigger();
                    }
                }
            }
        }

        assert_eq!(
            series.terms(),
            initial_terms,
            "op {i} ({operation:?}): I11 violated: terms changed after construction"
        );

        let mut total_collateral = 0i128;
        for s in &sellers {
            if let Some(position) = series.position(s) {
                assert!(
                    position.cover_written <= position.collateral,
                    "op {i} ({operation:?}): I2 violated: seller cover_written {} > collateral {}",
                    position.cover_written,
                    position.collateral
                );
                total_collateral += position.collateral;
            }
        }
        let total_cover_units: i128 = buyers.iter().map(|b| series.balance(b)).sum();
        assert!(
            total_cover_units <= total_collateral,
            "op {i} ({operation:?}): I1 violated: total cover units {} > sum of sellers' collateral {}",
            total_cover_units,
            total_collateral
        );

        let totals = series.totals();
        let expected_balance = totals.total_collateral + totals.total_premium
            - totals.total_paid
            - totals.total_withdrawn;
        let actual_balance = fx.usdc_client.balance(&series_address);
        assert_eq!(
            actual_balance, expected_balance,
            "op {i} ({operation:?}): I3 violated: series balance {actual_balance} != expected {expected_balance} (totals: {totals:?})"
        );

        if let Ok(Ok(cap)) = fx.factory.try_cover_cap(&asset) {
            let open_cover = fx.factory.open_cover(&asset);
            assert!(
                open_cover <= cap,
                "op {i} ({operation:?}): I9 violated: open cover {open_cover} > cover_cap {cap}"
            );
        }
    }
}

// -- I24: `sync()` is idempotent and order independent --

/// Runs the same fixed external-event sequence (deposit, quote, buy,
/// a real Declared event, time passing, a claim and a withdraw) once
/// with `sync()` called after every single step, and once with it
/// called only at the end, against two independently built but
/// deterministic fixtures (two fresh `Env::default()` instances issue
/// the same `Address::generate` sequence given the same call order,
/// so both runs land on identical addresses and contract ids).
/// Randomizes the seller/buyer amounts and rate within safe bounds -
/// this is the "same sequence of external events" I24 itself names,
/// not a random walk of arbitrary operations.
fn run_i24_scenario(
    seller_collateral: i128,
    buy_amount: i128,
    rate_bps: u32,
    sync_after_every_step: bool,
) -> (SeriesState, sylox_types::SeriesTotals, i128, i128) {
    let env = Env::default();
    let (fx, series, series_address, keeper, terms) = open_real_series(&env);
    let asset = terms.asset.clone();

    let seller = Address::generate(&env);
    let buyer = Address::generate(&env);
    fx.usdc_admin_client.mint(&seller, &seller_collateral);
    fx.usdc_admin_client.mint(&buyer, &buy_amount);

    let sync = |s: &SeriesClient| {
        if sync_after_every_step {
            s.sync();
        }
    };

    series.deposit(&seller, &seller_collateral);
    sync(&series);
    series.quote(&seller, &rate_bps, &seller_collateral);
    sync(&series);
    let (filled, _premium) = series.buy_cover(
        &buyer,
        &buy_amount,
        &rate_bps,
        &series.premium_quote(&buy_amount, &rate_bps).1,
    );
    sync(&series);

    let now_epoch = env.ledger().timestamp() / EPOCH_SECS;
    let window_start_epoch = now_epoch + 1;
    post_run(
        &env,
        &fx.oracle,
        &keeper,
        &asset,
        window_start_epoch,
        168,
        9_900_000,
    );
    sync(&series);
    post_run(
        &env,
        &fx.oracle,
        &keeper,
        &asset,
        window_start_epoch + 168,
        72,
        9_000_000,
    );
    sync(&series);

    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + 7_200 + 1);
    let proposer = Address::generate(&env);
    let event_id = fx
        .registry
        .propose_tier1(&proposer, &asset, &EventKind::Depeg, &1);
    sync(&series);

    let after = now + 7_200 + 1 + 86_400 + 259_200 + 7_200 + 1;
    env.ledger().set_timestamp(after);
    fx.registry.finalize(&event_id);
    sync(&series);

    let _ = series.trigger();
    sync(&series);

    series.claim(&buyer, &filled);
    sync(&series);

    let position = series.position(&seller).unwrap();
    let withdrawal = position.collateral - position.cover_written + position.premium_earned;
    series.withdraw(&seller, &withdrawal);

    // Always a final sync, in both variants, so both read the same
    // terminal state regardless of how many times it ran already.
    let final_state = series.sync();

    (
        final_state,
        series.totals(),
        fx.usdc_client.balance(&buyer),
        fx.usdc_client.balance(&series_address),
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases_from_env("SERIES_PROPTEST_CASES_I24", 6),
        .. ProptestConfig::default()
    })]

    #[test]
    fn i24_sync_is_idempotent_and_order_independent(
        seller_collateral in 10_000i128..100_000,
        buy_amount in 1_000i128..9_000,
        rate_bps in 100u32..2_000,
    ) {
        let seller_collateral = seller_collateral * 10_000_000;
        let buy_amount = buy_amount * 10_000_000;

        let synced_every_step =
            run_i24_scenario(seller_collateral, buy_amount, rate_bps, true);
        let synced_once_at_end =
            run_i24_scenario(seller_collateral, buy_amount, rate_bps, false);

        prop_assert_eq!(
            synced_every_step, synced_once_at_end,
            "I24 violated: final (state, totals, buyer balance, series balance) differ \
             depending on when sync() was called"
        );
    }
}

// -- I26: `LiveCoverCount` tracks the live list exactly --
//
// `MarketFactory`'s public API has no direct `LiveCoverCount(asset,
// kind, version)` getter (only the boolean `has_live_cover`), so this
// checks the two things that ARE observable and must move in
// lockstep with it (Section 10): the live series list's own length
// against a shadow count of "opened, not yet released" series, and
// `has_live_cover`'s own boolean against "shadow count > 0".

#[derive(Clone, Copy, Debug)]
enum LifecycleOp {
    Open,
    Expire(u8),
}

fn lifecycle_op() -> impl Strategy<Value = LifecycleOp> {
    prop_oneof![
        3 => Just(LifecycleOp::Open),
        2 => (0u8..4).prop_map(LifecycleOp::Expire),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases_from_env("SERIES_PROPTEST_CASES_I26", 20),
        .. ProptestConfig::default()
    })]

    #[test]
    fn i26_live_cover_count_matches_the_live_series_list(ops in prop::collection::vec(lifecycle_op(), 1..24)) {
        let env = Env::default();
        let fx = setup(&env);

        let asset_issuer = Address::generate(&env);
        let asset = env
            .register_stellar_asset_contract_v2(asset_issuer.clone())
            .address();
        fx.oracle.add_asset(&asset_config(&env, &asset, &asset_issuer));
        fx.registry.register_definition(&depeg_definition(&asset));
        fx.factory
            .set_related_sellers(&asset, &Vec::from_array(&env, [asset_issuer.clone()]));

        let keeper = Address::generate(&env);
        fx.staking.add_keeper(&keeper);
        fx.usdc_admin_client.mint(&keeper, &KEEPER_BOND);
        fx.staking.stake(&keeper, &KEEPER_BOND);

        let mut next_epoch = EPOCH_BASE;
        let mut live: std::vec::Vec<(SeriesClient, u64)> = std::vec::Vec::new();

        for operation in ops {
            match operation {
                LifecycleOp::Open => {
                    if live.len() < 4 {
                        env.ledger().set_timestamp((next_epoch + 1) * EPOCH_SECS);
                        fx.oracle.post_signals(
                            &keeper,
                            &asset,
                            &signal_set(&env, &keeper, next_epoch, 9_900_000),
                        );
                        next_epoch += 1;

                        let now = env.ledger().timestamp();
                        let mut def_versions = Map::new(&env);
                        def_versions.set(EventKind::Depeg, 1u32);
                        let terms = SeriesTerms {
                            asset: asset.clone(),
                            def_versions,
                            settlement: fx.usdc.clone(),
                            start: now,
                            expiry: now + 10_000,
                            claim_window_secs: 100,
                            cap: 1,
                            max_cover_per_buyer: 1,
                            require_holding: false,
                            fee_bps: 0,
                        };
                        if let Ok(Ok(series_address)) = fx.factory.try_open_series(&terms) {
                            let series = SeriesClient::new(&env, &series_address);
                            live.push((series, now + 10_000));
                        }
                    }
                }
                LifecycleOp::Expire(idx) => {
                    if !live.is_empty() {
                        let idx = idx as usize % live.len();
                        let (series, expiry) = live.remove(idx);
                        let target = expiry + 259_200 + 1;
                        let now = env.ledger().timestamp();
                        if target > now {
                            env.ledger().set_timestamp(target);
                        }
                        let state = series.sync();
                        prop_assert_eq!(state, SeriesState::Expired);
                    }
                }
            }

            prop_assert_eq!(
                fx.factory.series_for(&asset).len() as usize,
                live.len(),
                "I26 violated: MarketFactory's live series list disagrees with the shadow count"
            );
            prop_assert_eq!(
                fx.factory.has_live_cover(&asset, &EventKind::Depeg, &1),
                !live.is_empty(),
                "I26 violated: has_live_cover disagrees with whether any series is still live"
            );
        }
    }
}
