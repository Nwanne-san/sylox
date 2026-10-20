//! Series test suite. technical-doc.md Section 9 (all), 10 (all), 11;
//! `docs/design/markets.md`.
//!
//! Mocks for `RiskOracle`, `EventRegistry` and `MarketFactory`, for
//! unit tests that don't need the real cross-contract behavior. The
//! real day-88 scenario, against every real contract, lives in
//! `test::integration` below.

extern crate std;

mod integration;
mod max_premium;
mod property;

use soroban_sdk::{
    contract, contractimpl, testutils::Address as _, testutils::Ledger as _, Address, Env, Map, Vec,
};
use sylox_types::{
    AssetEventStatus, Band, CoverGate, EventKind, SeriesState, SeriesTerms, SignalSet, SlotState,
    SubEpoch,
};

use crate::{Error, Series, SeriesClient};

const DEFAULT_SUB_EPOCH_SECS: u64 = 300;

#[contract]
struct MockRiskOracle;

#[contractimpl]
impl MockRiskOracle {
    pub fn set_stale(env: Env, asset: Address, stale: bool) {
        env.storage()
            .temporary()
            .set(&OracleKey::Stale(asset), &stale);
    }
    pub fn is_stale(env: Env, asset: Address) -> bool {
        env.storage()
            .temporary()
            .get(&OracleKey::Stale(asset))
            .unwrap_or(false)
    }

    pub fn set_band(env: Env, asset: Address, band: Band) {
        env.storage()
            .temporary()
            .set(&OracleKey::Band(asset), &band);
    }
    pub fn band(env: Env, asset: Address) -> Band {
        env.storage()
            .temporary()
            .get(&OracleKey::Band(asset))
            .unwrap_or(Band::Normal)
    }

    /// Overrides the default "the last sub-epoch that just closed"
    /// computation below, for `FeedBehind`-specific tests.
    pub fn set_live_override(
        env: Env,
        asset: Address,
        value: Option<(SubEpoch, SignalSet, SlotState)>,
    ) {
        match value {
            Some(v) => env.storage().temporary().set(&OracleKey::Live(asset), &v),
            None => env
                .storage()
                .temporary()
                .set(&OracleKey::LiveNone(asset), &true),
        }
    }
    pub fn live(env: Env, asset: Address) -> Option<(SubEpoch, SignalSet, SlotState)> {
        if env
            .storage()
            .temporary()
            .get(&OracleKey::LiveNone(asset.clone()))
            .unwrap_or(false)
        {
            return None;
        }
        if let Some(v) = env
            .storage()
            .temporary()
            .get(&OracleKey::Live(asset.clone()))
        {
            return Some(v);
        }
        // Default: the sub-epoch that just closed, exactly at the
        // current ledger time's own grid boundary - passes M1 with no
        // per-test setup needed.
        let now = env.ledger().timestamp();
        let close = (now / DEFAULT_SUB_EPOCH_SECS) * DEFAULT_SUB_EPOCH_SECS;
        if close == 0 {
            return None;
        }
        let sub_start = close - DEFAULT_SUB_EPOCH_SECS;
        let hour = sub_start / 3_600;
        let sub = ((sub_start % 3_600) / DEFAULT_SUB_EPOCH_SECS) as u32;
        Some((
            SubEpoch { hour, sub },
            default_signal_set(&env),
            SlotState::Pending,
        ))
    }

    pub fn set_latest(env: Env, asset: Address, signals: Option<SignalSet>) {
        match signals {
            Some(s) => env.storage().temporary().set(&OracleKey::Latest(asset), &s),
            None => env.storage().temporary().remove(&OracleKey::Latest(asset)),
        }
    }
    pub fn latest(env: Env, asset: Address) -> Option<SignalSet> {
        env.storage()
            .temporary()
            .get(&OracleKey::Latest(asset))
            .or(Some(default_signal_set(&env)))
    }

    pub fn set_reference_rate(env: Env, asset: Address, rate: i128) {
        env.storage()
            .temporary()
            .set(&OracleKey::Rate(asset), &rate);
    }
    pub fn reference_rate(env: Env, asset: Address) -> i128 {
        env.storage()
            .temporary()
            .get(&OracleKey::Rate(asset))
            .unwrap_or(sylox_types::SCALE)
    }

    pub fn sub_epoch_secs_at(env: Env, _asset: Address, _hour: u64) -> u64 {
        let _ = env;
        DEFAULT_SUB_EPOCH_SECS
    }
}

fn default_signal_set(env: &Env) -> SignalSet {
    SignalSet {
        epoch: 0,
        posted_at: env.ledger().timestamp(),
        peg_ratio: sylox_types::SCALE,
        peg_ratio_p10: sylox_types::SCALE,
        liquidity_2pct: 0,
        redemption_net: 0,
        supply: 0,
        supply_change_bps: 0,
        issuer_actions: sylox_types::IssuerActions::default(),
        endpoint: sylox_types::EndpointStatus::Up,
        inputs_hash: soroban_sdk::BytesN::from_array(env, &[0u8; 32]),
        // `Address::generate` is test-harness-only and cannot run
        // inside a contract function's own body (this helper is
        // called from inside `MockRiskOracle.live`/`latest`); any
        // fixed address is fine, since no test asserts on `poster`.
        poster: env.current_contract_address(),
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum OracleKey {
    Stale(Address),
    Band(Address),
    Live(Address),
    LiveNone(Address),
    Latest(Address),
    Rate(Address),
}

#[contract]
struct MockEventRegistry;

#[contractimpl]
impl MockEventRegistry {
    pub fn set_cover_gate(env: Env, asset: Address, gate: CoverGate) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Gate(asset), &gate);
    }
    pub fn cover_gate(env: Env, asset: Address) -> CoverGate {
        env.storage()
            .temporary()
            .get(&RegistryKey::Gate(asset))
            .unwrap_or(CoverGate::Clear)
    }

    pub fn set_event_status(env: Env, asset: Address, kind: EventKind, status: AssetEventStatus) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Status(asset, kind), &status);
    }
    pub fn event_status(env: Env, asset: Address, kind: EventKind) -> AssetEventStatus {
        env.storage()
            .temporary()
            .get(&RegistryKey::Status(asset, kind))
            .unwrap_or(AssetEventStatus::None)
    }

    pub fn set_covers(env: Env, event_id: u64, value: bool) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Covers(event_id), &value);
    }
    pub fn covers(env: Env, event_id: u64, _def_version: u32, _start: u64, _expiry: u64) -> bool {
        env.storage()
            .temporary()
            .get(&RegistryKey::Covers(event_id))
            .unwrap_or(true)
    }

    pub fn set_definition(
        env: Env,
        asset: Address,
        kind: EventKind,
        version: u32,
        def: sylox_types::EventDefinition,
    ) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Def(asset, kind, version), &def);
    }
    pub fn definition(
        env: Env,
        asset: Address,
        kind: EventKind,
        version: u32,
    ) -> Option<sylox_types::EventDefinition> {
        env.storage()
            .temporary()
            .get(&RegistryKey::Def(asset, kind, version))
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum RegistryKey {
    Gate(Address),
    Status(Address, EventKind),
    Covers(u64),
    Def(Address, EventKind, u32),
}

/// `sylox_types::treasury_deposit::deposit_treasury_share`'s own
/// target: just enough of `Treasury.deposit`'s shape for the
/// pre-authorized nested USDC transfer to land somewhere real.
#[contract]
struct MockTreasury;

#[contractimpl]
impl MockTreasury {
    pub fn deposit(env: Env, from: Address, bucket: sylox_types::TreasuryBucket, amount: i128) {
        let total: i128 = env
            .storage()
            .temporary()
            .get(&soroban_sdk::symbol_short!("TOTAL"))
            .unwrap_or(0);
        env.storage()
            .temporary()
            .set(&soroban_sdk::symbol_short!("TOTAL"), &(total + amount));
        let _ = (from, bucket);
    }
}

#[contract]
struct MockMarketFactory;

#[contractimpl]
impl MockMarketFactory {
    pub fn reserve_cover(env: Env, series: Address, amount: i128) {
        let total: i128 = env
            .storage()
            .temporary()
            .get(&FactoryKey::Reserved(series.clone()))
            .unwrap_or(0);
        env.storage()
            .temporary()
            .set(&FactoryKey::Reserved(series), &(total + amount));
    }
    pub fn release_cover(env: Env, series: Address, amount: i128) {
        let total: i128 = env
            .storage()
            .temporary()
            .get(&FactoryKey::Reserved(series.clone()))
            .unwrap_or(0);
        env.storage()
            .temporary()
            .set(&FactoryKey::Reserved(series), &(total - amount));
    }
    pub fn reserved(env: Env, series: Address) -> i128 {
        env.storage()
            .temporary()
            .get(&FactoryKey::Reserved(series))
            .unwrap_or(0)
    }

    pub fn set_price_guard(env: Env, asset: Address, guard: Option<Address>) {
        match guard {
            Some(g) => env.storage().temporary().set(&FactoryKey::Guard(asset), &g),
            None => env.storage().temporary().remove(&FactoryKey::Guard(asset)),
        }
    }
    pub fn price_guard(env: Env, asset: Address) -> Option<Address> {
        env.storage().temporary().get(&FactoryKey::Guard(asset))
    }

    pub fn set_related_sellers(env: Env, asset: Address, list: Option<Vec<Address>>) {
        match list {
            Some(l) => env
                .storage()
                .temporary()
                .set(&FactoryKey::RelatedSellers(asset), &l),
            None => env
                .storage()
                .temporary()
                .remove(&FactoryKey::RelatedSellers(asset)),
        }
    }
    pub fn related_sellers(env: Env, asset: Address) -> Option<Vec<Address>> {
        env.storage()
            .temporary()
            .get(&FactoryKey::RelatedSellers(asset))
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum FactoryKey {
    Reserved(Address),
    Guard(Address),
    RelatedSellers(Address),
}

struct Fixture<'a> {
    client: SeriesClient<'a>,
    asset: Address,
    settlement: Address,
    oracle: MockRiskOracleClient<'a>,
    registry: MockEventRegistryClient<'a>,
    factory: MockMarketFactoryClient<'a>,
    contract_id: Address,
}

/// `start`, 90 days to `expiry`, default terms otherwise. `now` is set
/// to `start` by the caller.
fn default_terms(env: &Env, asset: &Address, settlement: &Address, start: u64) -> SeriesTerms {
    let mut def_versions = Map::new(env);
    def_versions.set(EventKind::Depeg, 1);
    SeriesTerms {
        asset: asset.clone(),
        def_versions,
        settlement: settlement.clone(),
        start,
        expiry: start + 90 * 86_400,
        claim_window_secs: 30 * 86_400,
        cap: 1_000_000_000_000,
        max_cover_per_buyer: 900_000_000_000,
        require_holding: false,
        fee_bps: 750,
    }
}

fn setup(env: &Env) -> Fixture<'_> {
    setup_with_treasury(env, |env, _settlement| env.register(MockTreasury, ()))
}

/// `setup`, with the `Treasury` built by `make_treasury` from the
/// settlement token's address.
fn setup_with_treasury(
    env: &Env,
    make_treasury: impl FnOnce(&Env, &Address) -> Address,
) -> Fixture<'_> {
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);

    let oracle_id = env.register(MockRiskOracle, ());
    let oracle = MockRiskOracleClient::new(env, &oracle_id);
    let registry_id = env.register(MockEventRegistry, ());
    let registry = MockEventRegistryClient::new(env, &registry_id);
    let factory_id = env.register(MockMarketFactory, ());
    let factory = MockMarketFactoryClient::new(env, &factory_id);

    let asset_issuer = Address::generate(env);
    let asset = env
        .register_stellar_asset_contract_v2(asset_issuer)
        .address();
    let settlement_issuer = Address::generate(env);
    let settlement = env
        .register_stellar_asset_contract_v2(settlement_issuer)
        .address();
    let treasury = make_treasury(env, &settlement);

    factory.set_related_sellers(&asset, &Some(Vec::new(env)));

    let terms = default_terms(env, &asset, &settlement, env.ledger().timestamp());
    let contract_id = env.register(
        Series,
        (terms, factory_id, oracle_id, registry_id, treasury),
    );
    let client = SeriesClient::new(env, &contract_id);

    Fixture {
        client,
        asset,
        settlement,
        oracle,
        registry,
        factory,
        contract_id,
    }
}

fn mint_settlement(env: &Env, fx: &Fixture, who: &Address, amount: i128) {
    soroban_sdk::token::StellarAssetClient::new(env, &fx.settlement).mint(who, &amount);
}

#[test]
fn deposit_succeeds_and_tracks_collateral() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);

    fx.client.deposit(&seller, &5_000_000_000);
    let position = fx.client.position(&seller).unwrap();
    assert_eq!(position.collateral, 5_000_000_000);
    assert_eq!(fx.client.totals().total_collateral, 5_000_000_000);
}

#[test]
fn deposit_rejects_below_minimum() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 1_000_000_000);
    let result = fx.client.try_deposit(&seller, &999_999_999);
    assert_eq!(result, Err(Ok(Error::BelowMinDeposit)));
}

#[test]
fn deposit_rejects_a_related_seller() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    fx.factory
        .set_related_sellers(&fx.asset, &Some(Vec::from_array(&env, [seller.clone()])));
    let result = fx.client.try_deposit(&seller, &5_000_000_000);
    assert_eq!(result, Err(Ok(Error::RelatedSeller)));
}

#[test]
fn quote_orders_by_rate_then_sequence_and_rejects_over_free_collateral() {
    let env = Env::default();
    let fx = setup(&env);
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    mint_settlement(&env, &fx, &a, 200_000_000_000);
    mint_settlement(&env, &fx, &b, 200_000_000_000);
    fx.client.deposit(&a, &100_000_000_000);
    fx.client.deposit(&b, &50_000_000_000);

    let result = fx.client.try_quote(&a, &400, &200_000_000_000);
    assert_eq!(result, Err(Ok(Error::QuoteExceedsFree)));

    fx.client.quote(&a, &400, &100_000_000_000);
    fx.client.quote(&b, &300, &50_000_000_000);

    let quotes = fx.client.quotes();
    assert_eq!(quotes.len(), 2);
    assert_eq!(quotes.get(0).unwrap().seller, b); // cheaper first
    assert_eq!(quotes.get(1).unwrap().seller, a);
}

#[test]
fn requoting_cancels_the_old_quote_and_goes_to_the_back_of_its_new_tier() {
    let env = Env::default();
    let fx = setup(&env);
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    mint_settlement(&env, &fx, &a, 200_000_000_000);
    mint_settlement(&env, &fx, &b, 200_000_000_000);
    fx.client.deposit(&a, &100_000_000_000);
    fx.client.deposit(&b, &100_000_000_000);

    fx.client.quote(&a, &300, &50_000_000_000);
    fx.client.quote(&b, &300, &50_000_000_000);
    // a re-quotes at the SAME rate: must land after b, not before.
    fx.client.quote(&a, &300, &60_000_000_000);

    let quotes = fx.client.quotes();
    assert_eq!(quotes.len(), 2);
    assert_eq!(quotes.get(0).unwrap().seller, b);
    assert_eq!(quotes.get(1).unwrap().seller, a);
    assert_eq!(quotes.get(1).unwrap().available, 60_000_000_000);
}

#[test]
fn cancel_quote_removes_it() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    fx.client.deposit(&seller, &5_000_000_000);
    fx.client.quote(&seller, &300, &5_000_000_000);
    assert_eq!(fx.client.quotes().len(), 1);
    fx.client.cancel_quote(&seller);
    assert_eq!(fx.client.quotes().len(), 0);
}

/// Section 10.5's own worked example, reproduced to the base unit.
#[test]
fn buy_cover_matches_the_section_10_5_worked_example() {
    let env = Env::default();
    let fx = setup(&env);
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    let buyer = Address::generate(&env);
    // 7 decimals throughout: 1 USDC = 10,000,000 base units.
    let usdc = 10_000_000i128;
    mint_settlement(&env, &fx, &a, 100_000 * usdc);
    mint_settlement(&env, &fx, &b, 50_000 * usdc);
    mint_settlement(&env, &fx, &buyer, 1_000 * usdc);

    fx.client.deposit(&a, &(100_000 * usdc));
    fx.client.deposit(&b, &(50_000 * usdc));
    fx.client.quote(&a, &400, &(100_000 * usdc));
    fx.client.quote(&b, &300, &(50_000 * usdc));

    // 90 days left: start == now, expiry == start + 90d, so
    // time_remaining == 90d == 7,776,000s exactly.
    let (filled, premium) = fx.client.buy_cover(
        &buyer,
        &(80_000 * usdc),
        &500,
        &fx.client.premium_quote(&(80_000 * usdc), &500).1,
    );
    assert_eq!(filled, 80_000 * usdc);

    // B (cheaper, 300bps) fills first and fully (50,000 of its own
    // 50,000 available); the remaining 30,000 comes from A (400bps).
    let premium_b = ceil_div((50_000 * usdc) * 300 * 7_776_000, 10_000 * 31_536_000);
    let premium_a = ceil_div((30_000 * usdc) * 400 * 7_776_000, 10_000 * 31_536_000);
    assert_eq!(premium, premium_a + premium_b);

    let pos_a = fx.client.position(&a).unwrap();
    let pos_b = fx.client.position(&b).unwrap();
    assert_eq!(pos_b.cover_written, 50_000 * usdc);
    assert_eq!(pos_a.cover_written, 30_000 * usdc);

    let fee_a = ceil_div(premium_a * 750, 10_000);
    let fee_b = ceil_div(premium_b * 750, 10_000);
    assert_eq!(pos_a.premium_earned, premium_a - fee_a);
    assert_eq!(pos_b.premium_earned, premium_b - fee_b);

    assert_eq!(fx.client.balance(&buyer), 80_000 * usdc);
    assert_eq!(
        fx.client.totals().total_premium,
        (premium_a - fee_a) + (premium_b - fee_b)
    );
}

fn ceil_div(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

#[test]
fn buy_cover_rejects_when_asset_is_stale() {
    let env = Env::default();
    let fx = setup(&env);
    fx.oracle.set_stale(&fx.asset, &true);
    let buyer = Address::generate(&env);
    mint_settlement(&env, &fx, &buyer, 1_000_000_000);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::AssetStale)));
}

#[test]
fn buy_cover_rejects_when_band_is_distress() {
    let env = Env::default();
    let fx = setup(&env);
    fx.oracle.set_band(&fx.asset, &Band::Distress);
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::AssetDistressed)));
}

#[test]
fn buy_cover_rejects_on_event_in_progress() {
    let env = Env::default();
    let fx = setup(&env);
    fx.registry
        .set_cover_gate(&fx.asset, &CoverGate::EventInProgress);
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::AssetDistressed)));
}

#[test]
fn buy_cover_rejects_on_recent_depeg() {
    let env = Env::default();
    let fx = setup(&env);
    fx.registry
        .set_cover_gate(&fx.asset, &CoverGate::RecentDepeg);
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::RecentFailureSignals)));
}

#[test]
fn buy_cover_rejects_feed_behind() {
    let env = Env::default();
    let fx = setup(&env);
    fx.oracle.set_live_override(&fx.asset, &None);
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::FeedBehind)));
}

#[test]
fn buy_cover_rejects_fast_signal_pause() {
    let env = Env::default();
    let fx = setup(&env);
    let guard_admin = Address::generate(&env);
    let guard_id = env.register(price_guard_mock::PriceGuardMock, (guard_admin,));
    price_guard_mock::PriceGuardMockClient::new(&env, &guard_id)
        .set_status(&fx.asset, &sylox_types::GuardStatus::Paused);
    fx.factory.set_price_guard(&fx.asset, &Some(guard_id));
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::FastSignalPause)));
}

#[test]
fn buy_cover_rejects_no_fill_when_no_quote_meets_the_rate() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    fx.client.deposit(&seller, &5_000_000_000);
    fx.client.quote(&seller, &800, &5_000_000_000);
    let buyer = Address::generate(&env);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_000_000,
        &500,
        &fx.client.premium_quote(&1_000_000, &500).1,
    );
    assert_eq!(result, Err(Ok(Error::NoFill)));
}

#[test]
fn buy_cover_rejects_over_series_cap() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 2_000_000_000_000);
    fx.client.deposit(&seller, &2_000_000_000_000);
    fx.client.quote(&seller, &100, &2_000_000_000_000);
    let buyer = Address::generate(&env);
    mint_settlement(&env, &fx, &buyer, 100_000_000_000);
    let result = fx.client.try_buy_cover(
        &buyer,
        &1_100_000_000_000,
        &1_000,
        &fx.client.premium_quote(&1_100_000_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::SeriesCapExceeded)));
}

#[test]
fn buy_cover_rejects_over_buyer_cap() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 2_000_000_000_000);
    fx.client.deposit(&seller, &2_000_000_000_000);
    fx.client.quote(&seller, &100, &2_000_000_000_000);
    let buyer = Address::generate(&env);
    mint_settlement(&env, &fx, &buyer, 1_000_000_000_000);
    let result = fx.client.try_buy_cover(
        &buyer,
        &950_000_000_000,
        &1_000,
        &fx.client.premium_quote(&950_000_000_000, &1_000).1,
    );
    assert_eq!(result, Err(Ok(Error::BuyerCapExceeded)));
}

#[test]
fn trigger_and_claim_after_a_declared_event() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    let buyer = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    mint_settlement(&env, &fx, &buyer, 10_000_000_000);
    fx.client.deposit(&seller, &10_000_000_000);
    fx.client.quote(&seller, &300, &10_000_000_000);
    let (filled, _) = fx.client.buy_cover(
        &buyer,
        &5_000_000_000,
        &1_000,
        &fx.client.premium_quote(&5_000_000_000, &1_000).1,
    );
    assert!(filled > 0);

    fx.registry.set_event_status(
        &fx.asset,
        &EventKind::Depeg,
        &AssetEventStatus::Declared(42, 1, fx.client.terms().start, 0),
    );

    let event_id = fx.client.trigger();
    assert_eq!(event_id, 42);
    assert_eq!(fx.client.state(), SeriesState::Triggered);
    assert_eq!(fx.factory.reserved(&fx.contract_id), 0);

    let claimed = fx.client.claim(&buyer, &filled);
    assert_eq!(claimed, filled);
    assert_eq!(fx.client.balance(&buyer), 0);
}

#[test]
fn claim_for_rejects_before_the_claim_window_and_succeeds_after() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    let buyer = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    mint_settlement(&env, &fx, &buyer, 10_000_000_000);
    fx.client.deposit(&seller, &10_000_000_000);
    fx.client.quote(&seller, &300, &10_000_000_000);
    let (filled, _) = fx.client.buy_cover(
        &buyer,
        &5_000_000_000,
        &1_000,
        &fx.client.premium_quote(&5_000_000_000, &1_000).1,
    );

    fx.registry.set_event_status(
        &fx.asset,
        &EventKind::Depeg,
        &AssetEventStatus::Declared(7, 1, fx.client.terms().start, 0),
    );
    fx.client.trigger();

    let result = fx.client.try_claim_for(&buyer);
    assert_eq!(result, Err(Ok(Error::ClaimWindowOpen)));

    let claim_window = fx.client.terms().claim_window_secs;
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + claim_window + 1);

    let claimed = fx.client.claim_for(&buyer);
    assert_eq!(claimed, filled);
}

#[test]
fn state_machine_walks_open_closed_pending_expired_then_finalized() {
    let env = Env::default();
    let fx = setup(&env);
    let terms = fx.client.terms();
    assert_eq!(fx.client.state(), SeriesState::Open);

    // A real Depeg window (72h), so Pending's own acceptance period
    // is observable before Expired, rather than collapsing to zero.
    fx.registry.set_definition(
        &fx.asset,
        &EventKind::Depeg,
        &1,
        &depeg_definition(&env, &fx.asset),
    );

    env.ledger().set_timestamp(terms.expiry - 3 * 86_400 + 1);
    assert_eq!(fx.client.sync(), SeriesState::Closed);

    env.ledger().set_timestamp(terms.expiry);
    assert_eq!(fx.client.sync(), SeriesState::Pending);

    // No covering event ever shows up; after the Depeg acceptance
    // period (depeg_window_secs = 72h), the series expires.
    env.ledger().set_timestamp(terms.expiry + 72 * 3_600);
    assert_eq!(fx.client.sync(), SeriesState::Expired);

    // Finalized once the balance identity reaches zero (nobody had
    // anything deposited in this test, so it already is).
    assert_eq!(fx.client.sync(), SeriesState::Finalized);
}

fn depeg_definition(_env: &Env, asset: &Address) -> sylox_types::EventDefinition {
    sylox_types::EventDefinition {
        asset: asset.clone(),
        kind: EventKind::Depeg,
        version: 1,
        reference: sylox_types::Reference::Usd,
        depeg_threshold: 9_500_000,
        depeg_window_secs: 72 * 3_600,
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

#[test]
fn withdraw_in_open_excludes_quoted_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    fx.client.deposit(&seller, &10_000_000_000);
    fx.client.quote(&seller, &300, &4_000_000_000);

    let result = fx.client.try_withdraw(&seller, &7_000_000_000);
    assert_eq!(result, Err(Ok(Error::WithdrawTooLarge)));

    let withdrawn = fx.client.withdraw(&seller, &6_000_000_000);
    assert_eq!(withdrawn, 6_000_000_000);
}

#[test]
fn transfer_position_moves_collateral_and_quote_rejects_if_recipient_has_one() {
    let env = Env::default();
    let fx = setup(&env);
    let from = Address::generate(&env);
    let to = Address::generate(&env);
    mint_settlement(&env, &fx, &from, 10_000_000_000);
    mint_settlement(&env, &fx, &to, 10_000_000_000);
    fx.client.deposit(&from, &10_000_000_000);
    fx.client.quote(&from, &300, &5_000_000_000);

    fx.client.transfer_position(&from, &to);
    assert!(fx.client.position(&from).is_none());
    assert_eq!(fx.client.position(&to).unwrap().collateral, 10_000_000_000);
    assert_eq!(fx.client.quotes().get(0).unwrap().seller, to);

    mint_settlement(&env, &fx, &from, MIN_DEPOSIT_FOR_TEST);
    fx.client.deposit(&from, &MIN_DEPOSIT_FOR_TEST);
    let result = fx.client.try_transfer_position(&to, &from);
    assert_eq!(result, Err(Ok(Error::RecipientHasPosition)));
}

const MIN_DEPOSIT_FOR_TEST: i128 = 1_000_000_000;

#[test]
fn sep41_transfer_approve_and_burn() {
    let env = Env::default();
    let fx = setup(&env);
    let seller = Address::generate(&env);
    let buyer = Address::generate(&env);
    let other = Address::generate(&env);
    mint_settlement(&env, &fx, &seller, 10_000_000_000);
    mint_settlement(&env, &fx, &buyer, 10_000_000_000);
    fx.client.deposit(&seller, &10_000_000_000);
    fx.client.quote(&seller, &300, &10_000_000_000);
    let (filled, _) = fx.client.buy_cover(
        &buyer,
        &1_000_000_000,
        &1_000,
        &fx.client.premium_quote(&1_000_000_000, &1_000).1,
    );

    fx.client.transfer(&buyer, &other, &(filled / 2));
    assert_eq!(fx.client.balance(&other), filled / 2);
    assert_eq!(fx.client.balance(&buyer), filled - filled / 2);

    fx.client
        .approve(&buyer, &other, &100, &(env.ledger().sequence() + 100));
    assert_eq!(fx.client.allowance(&buyer, &other), 100);
    fx.client.transfer_from(&other, &buyer, &other, &100);
    assert_eq!(fx.client.allowance(&buyer, &other), 0);

    assert_eq!(fx.client.decimals(), 7);
}
