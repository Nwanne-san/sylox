//! Resource spike for `buy_cover` against the worst case the task
//! brief names explicitly: a full 168-slot `RiskOracle` ring (so
//! `reserve_cover`'s own `cover_cap` -> `median_liquidity` call reads
//! real history, not an empty one) and all 64 quotes
//! (`series::MAX_QUOTES`) walked in one call. Numbers reported
//! against the shared network limits (`sylox_types::network_limits`),
//! at 50% of each, matching `risk_oracle`/`event_registry`'s own
//! `budget_test.rs` convention. Self-contained, duplicating the real
//! multi-contract fixture `test::integration` also builds, rather
//! than reaching into it (a sibling module cannot see it anyway, and
//! `test.rs`'s own fixture is mock-based, not real contracts).
//!
//! Fills `docs/design/markets.md` Section 11's own "gap, see below"
//! rows (Instructions, Memory) with the real numbers this measures.

extern crate std;

use soroban_sdk::testutils::{cost_estimate::CostEstimate, Address as _, Ledger as _};
use soroban_sdk::{token, Address, BytesN, Env, Map};
use std::println;
use sylox_types::network_limits::{
    TX_MAX_FOOTPRINT_ENTRIES, TX_MAX_INSTRUCTIONS, TX_MAX_READ_LEDGER_ENTRIES, TX_MAX_WRITE_BYTES,
    TX_MAX_WRITE_LEDGER_ENTRIES, TX_MEMORY_LIMIT_BYTES,
};
use sylox_types::{
    AssetConfig, EndpointStatus, EventDefinition, EventKind, IssuerActions, IssuerFlags, Reference,
    SeriesTerms, SignalSet,
};

use crate::{Series, SeriesClient};

const EPOCH_SECS: u64 = 3_600;
const SUB_EPOCH_SECS: u64 = 300;
/// Mirrors `Staking`'s own private `params::KEEPER_BOND`, not
/// exported; see `test::integration`'s identical comment.
const KEEPER_BOND: i128 = 50_000_000_000;

struct Fixture<'a> {
    oracle: risk_oracle::RiskOracleClient<'a>,
    registry: event_registry::EventRegistryClient<'a>,
    factory: market_factory::MarketFactoryClient<'a>,
    staking: staking::StakingClient<'a>,
    usdc: Address,
    usdc_admin_client: token::StellarAssetClient<'a>,
}

fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();

    let governor = Address::generate(env);
    let sac_admin = Address::generate(env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
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
        usdc_admin_client,
    }
}

fn asset_config(env: &Env, asset: &Address, issuer: &Address) -> AssetConfig {
    AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: soroban_sdk::String::from_str(env, "example.com"),
        amm_adapters: soroban_sdk::Vec::new(env),
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

/// Builds a full 168 slot ring of healthy history ending exactly at
/// `end_epoch - 1`, then posts one sub-epoch at `end_epoch` so
/// `check_feed_not_behind` (M1) passes, leaving "now" at
/// `end_epoch * EPOCH_SECS + SUB_EPOCH_SECS`.
fn build_full_ring_and_fresh_feed(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    end_epoch: u64,
) {
    for i in 0..168u64 {
        let epoch = end_epoch - 168 + i;
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        oracle.post_signals(keeper, asset, &signal_set(env, keeper, epoch, 9_900_000));
    }
    let sub_start = end_epoch * EPOCH_SECS;
    env.ledger().set_timestamp(sub_start + SUB_EPOCH_SECS);
    oracle.post_sub_signals(
        keeper,
        asset,
        &end_epoch,
        &0u32,
        &signal_set(env, keeper, end_epoch, 9_900_000),
    );
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
    println!("  footprint_entries:   {}", footprint_entries(estimate));
    println!("  fee.total (stroops): {}", fee.total);
}

/// See `event_registry::budget_test`'s identical helper for why this,
/// not a single `CostEstimate` field, is the number
/// `tx_max_footprint_entries` is actually checked against.
fn footprint_entries(estimate: &CostEstimate) -> u64 {
    let resources = estimate.resources();
    resources.disk_read_entries as u64
        + resources.memory_read_entries as u64
        + resources.write_entries as u64
}

fn assert_under_half(estimate: &CostEstimate, label: &str) {
    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "{label}: must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}); got {}",
        resources.instructions
    );
    assert!(
        (resources.mem_bytes as u64) < TX_MEMORY_LIMIT_BYTES / 2,
        "{label}: must stay under 50% of tx_memory_limit ({TX_MEMORY_LIMIT_BYTES}); got {}",
        resources.mem_bytes
    );
    assert!(
        (resources.write_bytes as u64) < TX_MAX_WRITE_BYTES / 2,
        "{label}: must stay under 50% of tx_max_write_bytes ({TX_MAX_WRITE_BYTES}); got {}",
        resources.write_bytes
    );
    assert!(
        (resources.disk_read_entries as u64) < TX_MAX_READ_LEDGER_ENTRIES as u64 / 2,
        "{label}: must stay under 50% of tx_max_disk_read_entries ({TX_MAX_READ_LEDGER_ENTRIES}); got {}",
        resources.disk_read_entries
    );
    assert!(
        (resources.write_entries as u64) < TX_MAX_WRITE_LEDGER_ENTRIES as u64 / 2,
        "{label}: must stay under 50% of tx_max_write_ledger_entries ({TX_MAX_WRITE_LEDGER_ENTRIES}); got {}",
        resources.write_entries
    );
    let footprint = footprint_entries(estimate);
    assert!(
        footprint < TX_MAX_FOOTPRINT_ENTRIES as u64 / 2,
        "{label}: must stay under 50% of tx_max_footprint_entries ({TX_MAX_FOOTPRINT_ENTRIES}); got {footprint}"
    );
}

/// Opens a real series against a real `RiskOracle` ring, 168 slots
/// full, and real `EventRegistry`/`MarketFactory`/`Staking`/`Treasury`
/// wiring throughout - everything `buy_cover`'s own worst case
/// actually calls into.
fn setup_series_with_full_ring(env: &Env) -> (Fixture<'_>, SeriesClient<'_>, Address) {
    let fx = setup(env);

    let asset_issuer = Address::generate(env);
    let asset = env
        .register_stellar_asset_contract_v2(asset_issuer.clone())
        .address();
    fx.oracle
        .add_asset(&asset_config(env, &asset, &asset_issuer));
    fx.registry.register_definition(&depeg_definition(&asset));
    fx.factory.set_related_sellers(
        &asset,
        &soroban_sdk::Vec::from_array(env, [asset_issuer.clone()]),
    );

    let keeper = Address::generate(env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin_client.mint(&keeper, &KEEPER_BOND);
    fx.staking.stake(&keeper, &KEEPER_BOND);

    const EPOCH_BASE: u64 = 497_000;
    build_full_ring_and_fresh_feed(env, &fx.oracle, &keeper, &asset, EPOCH_BASE + 168);

    let now = env.ledger().timestamp();
    let mut def_versions = Map::new(env);
    def_versions.set(EventKind::Depeg, 1u32);
    let terms = SeriesTerms {
        asset,
        def_versions,
        settlement: fx.usdc.clone(),
        start: now,
        expiry: now + 90 * 86_400,
        claim_window_secs: 30 * 86_400,
        cap: 10_000_000_000_000,
        max_cover_per_buyer: 10_000_000_000_000,
        require_holding: false,
        fee_bps: 750,
    };
    let series_address = fx.factory.open_series(&terms);
    let series = SeriesClient::new(env, &series_address);
    (fx, series, series_address)
}

#[test]
fn budget_buy_cover_full_168_slot_ring_and_64_quotes_walked() {
    let env = Env::default();
    let (fx, series, _series_address) = setup_series_with_full_ring(&env);

    // 64 sellers (`series::MAX_QUOTES`), each depositing the minimum
    // and quoting all of it, so one `buy_cover` call for their
    // combined total walks and fully fills every single one - the
    // worst case `walk_quotes`' own loop can ever face.
    const MIN_DEPOSIT: i128 = 1_000_000_000;
    const QUOTE_COUNT: u32 = 64;
    for i in 0..QUOTE_COUNT {
        let seller = Address::generate(&env);
        fx.usdc_admin_client.mint(&seller, &MIN_DEPOSIT);
        series.deposit(&seller, &MIN_DEPOSIT);
        series.quote(&seller, &(100 + i), &MIN_DEPOSIT);
    }
    assert_eq!(series.quotes().len(), QUOTE_COUNT);

    let buyer = Address::generate(&env);
    let buy_amount = MIN_DEPOSIT * QUOTE_COUNT as i128;
    fx.usdc_admin_client.mint(&buyer, &buy_amount);

    // The 168-slot ring build plus 64 deposit/quote calls above
    // already spent real budget in this same `Env`; reset it so only
    // the one call actually being measured counts, matching
    // `event_registry::budget_test`'s own convention.
    // Signed above the live premium so the refund transfer runs too:
    // the worst case for `buy_cover`'s own cost.
    let max_premium = series.premium_quote(&buy_amount, &(100 + QUOTE_COUNT)).1 + 1_000;
    env.cost_estimate().budget().reset_unlimited();
    let (filled, _premium) =
        series.buy_cover(&buyer, &buy_amount, &(100 + QUOTE_COUNT), &max_premium);
    assert_eq!(filled, buy_amount, "all 64 quotes must fully fill");

    let estimate = env.cost_estimate();
    print_resources(
        "buy_cover, full 168-slot RiskOracle ring, all 64 quotes walked and filled",
        &estimate,
    );
    assert_under_half(&estimate, "buy_cover (168-slot ring, 64 quotes)");
}

/// A lighter companion measurement: the same 168-slot ring, but only
/// one quote to walk - shows how much of `buy_cover`'s own cost is
/// the oracle/registry/factory cross-calls (fixed) versus the quote
/// walk itself (scales with quote count).
#[test]
fn budget_buy_cover_full_168_slot_ring_single_quote() {
    let env = Env::default();
    let (fx, series, _series_address) = setup_series_with_full_ring(&env);

    const MIN_DEPOSIT: i128 = 1_000_000_000;
    let seller = Address::generate(&env);
    fx.usdc_admin_client.mint(&seller, &MIN_DEPOSIT);
    series.deposit(&seller, &MIN_DEPOSIT);
    series.quote(&seller, &500u32, &MIN_DEPOSIT);

    let buyer = Address::generate(&env);
    fx.usdc_admin_client.mint(&buyer, &MIN_DEPOSIT);
    let max_premium = series.premium_quote(&MIN_DEPOSIT, &500u32).1 + 1_000;
    env.cost_estimate().budget().reset_unlimited();
    let (filled, _premium) = series.buy_cover(&buyer, &MIN_DEPOSIT, &500u32, &max_premium);
    assert_eq!(filled, MIN_DEPOSIT);

    let estimate = env.cost_estimate();
    print_resources(
        "buy_cover, full 168-slot RiskOracle ring, a single quote walked",
        &estimate,
    );
    assert_under_half(&estimate, "buy_cover (168-slot ring, 1 quote)");
}
