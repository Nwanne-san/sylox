//! MarketFactory test suite. technical-doc.md Section 9.7, 11.1, 12.4;
//! `docs/design/markets.md`.
//!
//! `open_series` deploys against `DummySeries` below, a minimal
//! native contract with `Series`' own constructor signature
//! (`terms, factory, oracle, registry, treasury`) and nothing else,
//! so this suite can test `open_series`'s real deployment step and
//! everything that depends on a genuinely deployed address
//! (`reserve_cover`, `release_cover`, `has_live_cover`) without
//! reaching across crates for the real `Series` - that full
//! multi-contract scenario (and the real `Series`' own regression
//! test for the no-buyer-series bug below) lives in
//! `series::test::integration` instead, alongside `Series`' own
//! other real-contract tests.

extern crate std;

use soroban_sdk::{
    contract, contractimpl, testutils::Address as _, Address, BytesN, Env, IntoVal, Map, String,
    Vec,
};
use sylox_types::{AssetConfig, EventKind, Reference, SeriesTerms};

use crate::{Error, MarketFactory, MarketFactoryClient};

#[contract]
struct DummySeries;

#[contractimpl]
impl DummySeries {
    pub fn __constructor(
        _env: Env,
        _terms: SeriesTerms,
        _factory: Address,
        _oracle: Address,
        _registry: Address,
        _treasury: Address,
    ) {
    }
}

#[contract]
struct MockRiskOracle;

#[contractimpl]
impl MockRiskOracle {
    pub fn set_asset_config(env: Env, asset: Address, config: AssetConfig) {
        env.storage()
            .temporary()
            .set(&OracleKey::Config(asset), &config);
    }

    pub fn asset_config(env: Env, asset: Address) -> Option<AssetConfig> {
        env.storage().temporary().get(&OracleKey::Config(asset))
    }

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

    pub fn set_median_liquidity(env: Env, asset: Address, amount: i128) {
        env.storage()
            .temporary()
            .set(&OracleKey::Liquidity(asset), &amount);
    }

    pub fn median_liquidity(env: Env, asset: Address) -> i128 {
        env.storage()
            .temporary()
            .get(&OracleKey::Liquidity(asset))
            .unwrap_or(0)
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum OracleKey {
    Config(Address),
    Stale(Address),
    Liquidity(Address),
}

#[contract]
struct MockEventRegistry;

#[contractimpl]
impl MockEventRegistry {
    pub fn set_declared(env: Env, asset: Address, declared: bool) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Declared(asset), &declared);
    }

    pub fn has_declared(env: Env, asset: Address) -> bool {
        env.storage()
            .temporary()
            .get(&RegistryKey::Declared(asset))
            .unwrap_or(false)
    }

    pub fn set_current_version(env: Env, asset: Address, kind: EventKind, version: u32) {
        env.storage()
            .temporary()
            .set(&RegistryKey::Version(asset, kind), &version);
    }

    pub fn current_version(env: Env, asset: Address, kind: EventKind) -> u32 {
        env.storage()
            .temporary()
            .get(&RegistryKey::Version(asset, kind))
            .unwrap_or(0)
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum RegistryKey {
    Declared(Address),
    Version(Address, EventKind),
}

struct Fixture<'a> {
    client: MarketFactoryClient<'a>,
    contract_id: Address,
    oracle: MockRiskOracleClient<'a>,
    registry: MockEventRegistryClient<'a>,
    /// A real Stellar Asset Contract, admin `usdc_issuer` - so
    /// `SeriesTerms.settlement = usdc` genuinely passes the
    /// settlement-issuer check against any covered asset whose own
    /// issuer differs from `usdc_issuer` (true for every freshly
    /// generated test issuer).
    usdc: Address,
}

fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();

    let governor = Address::generate(env);
    let oracle_id = env.register(MockRiskOracle, ());
    let oracle = MockRiskOracleClient::new(env, &oracle_id);
    let registry_id = env.register(MockEventRegistry, ());
    let registry = MockEventRegistryClient::new(env, &registry_id);
    let treasury = Address::generate(env);
    let usdc_issuer = Address::generate(env);
    let usdc = env
        .register_stellar_asset_contract_v2(usdc_issuer)
        .address();
    let series_wasm = env.upload(DummySeries);

    let contract_id = env.register(
        MarketFactory,
        (
            governor,
            oracle_id,
            registry_id,
            treasury,
            usdc.clone(),
            series_wasm,
        ),
    );
    let client = MarketFactoryClient::new(env, &contract_id);

    Fixture {
        client,
        contract_id,
        oracle,
        registry,
        usdc,
    }
}

fn enabled_asset_config(env: &Env, asset: &Address, issuer: &Address) -> AssetConfig {
    AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: String::from_str(env, "example.com"),
        amm_adapters: Vec::new(env),
        fx_adapter: None,
        min_liquidity: 0,
        issuer_flags: sylox_types::IssuerFlags::default(),
        enabled: true,
    }
}

/// A `SeriesTerms` that passes every check, including deployment
/// itself (against `DummySeries`). Callers break exactly one field to
/// test that specific rejection instead.
fn valid_terms(env: &Env, fx: &Fixture, asset: &Address) -> SeriesTerms {
    let mut def_versions = Map::new(env);
    def_versions.set(EventKind::Depeg, 1);
    fx.registry
        .set_current_version(asset, &EventKind::Depeg, &1);

    SeriesTerms {
        asset: asset.clone(),
        def_versions,
        settlement: fx.usdc.clone(),
        start: 1_000,
        expiry: 1_000 + 90 * 86_400,
        claim_window_secs: 30 * 86_400,
        cap: 1_000_000_000,
        max_cover_per_buyer: 100_000_000,
        require_holding: false,
        fee_bps: 750,
    }
}

/// Registers `asset` as a real SAC (admin `issuer`), sets its
/// `AssetConfig` and lists `issuer` as the one related seller - the
/// common setup every "reaches deployment" test needs.
fn listed_asset(env: &Env, fx: &Fixture) -> (Address, Address) {
    let issuer = Address::generate(env);
    let asset = env
        .register_stellar_asset_contract_v2(issuer.clone())
        .address();
    fx.oracle
        .set_asset_config(&asset, &enabled_asset_config(env, &asset, &issuer));
    fx.client
        .set_related_sellers(&asset, &Vec::from_array(env, [issuer.clone()]));
    (asset, issuer)
}

/// Issue #21: `initialize` is gone, replaced by `__constructor`, which
/// Soroban runs atomically at deploy time - there is no longer a
/// window where a contract address exists but is unconfigured, so
/// there is nothing left for an `AlreadyInitialized`/`NotInitialized`
/// pair of tests to prove on a freshly `register`-ed address (every
/// `register` call makes a brand new address, already constructed by
/// the time the call returns). The one honest thing left to
/// demonstrate is `register_at`'s own testutils-only escape hatch,
/// which re-runs a contract's constructor against an address that is
/// already registered - not reproducible on-chain, where an address's
/// executable and constructor are fixed at creation. Mirrors
/// `staking`'s own `cannot_initialize_a_second_time` test.
#[test]
fn cannot_initialize_a_second_time() {
    let env = Env::default();
    let fx = setup(&env);

    // Something the first constructor never wrote, added after the
    // fact through an ordinary call - a pure local read, since the
    // re-registration below replaces `oracle` and a cross-call into
    // it (e.g. via cover_cap) would now hit a nonexistent contract.
    let asset = Address::generate(&env);
    let list = Vec::from_array(&env, [Address::generate(&env)]);
    fx.client.set_related_sellers(&asset, &list);

    let new_governor = Address::generate(&env);
    let new_oracle = Address::generate(&env);
    let new_registry = Address::generate(&env);
    let new_treasury = Address::generate(&env);
    let new_usdc = Address::generate(&env);
    let new_series_wasm = env.upload(MarketFactory);
    env.register_at(
        &fx.contract_id,
        MarketFactory,
        (
            new_governor.clone(),
            new_oracle.clone(),
            new_registry.clone(),
            new_treasury.clone(),
            new_usdc.clone(),
            new_series_wasm,
        ),
    );

    // The config half: reading storage directly (crate-private, but
    // this test is a child module of the crate root) proves the
    // second constructor's values are what is actually stored now.
    let stored_config = env.as_contract(&fx.contract_id, || {
        crate::storage::get_config(&env).unwrap()
    });
    assert_eq!(stored_config.governor, new_governor);
    assert_eq!(stored_config.oracle, new_oracle);
    assert_eq!(stored_config.registry, new_registry);
    assert_eq!(stored_config.treasury, new_treasury);
    assert_eq!(stored_config.usdc, new_usdc);

    // The unrelated-storage half: the related-seller list set before
    // re-registration is untouched by it.
    assert_eq!(fx.client.related_sellers(&asset), Some(list));
}

#[test]
fn version_upgrade_and_migrate_follow_the_shared_pattern() {
    let env = Env::default();
    let fx = setup(&env);
    assert_eq!(fx.client.version(), 1);

    let new_wasm = env.upload(MarketFactory);
    fx.client.upgrade(&new_wasm);
    // Upgraded to its own current code, so storage (and CONTRACT_VERSION) is unchanged.
    assert_eq!(fx.client.version(), 1);

    fx.client.migrate();
    let result = fx.client.try_migrate();
    assert_eq!(result, Err(Ok(Error::AlreadyMigrated)));
}

/// `set_governor` works exactly once: the current governor hands off
/// to a new address, and a governor-gated call afterward still
/// succeeds (through `setup()`'s own auth mocking, which approves
/// every `require_auth()` regardless of address; the real proof the
/// new address is the one actually recorded is
/// `set_governor_old_governor_rejected_new_governor_accepted` below).
#[test]
fn set_governor_succeeds_once() {
    let env = Env::default();
    let fx = setup(&env);
    let new_governor = Address::generate(&env);

    fx.client.set_governor(&new_governor);

    let new_wasm = env.upload(MarketFactory);
    fx.client.upgrade(&new_wasm);
}

/// `set_governor` cannot run twice: `GovernorAlreadySet` on the
/// second call, confirmed via `try_set_governor` since this is a real
/// returned error, not a native auth trap. Same one-time-lock shape
/// as `migrate`'s own `AlreadyMigrated` check above.
#[test]
fn set_governor_fails_the_second_call() {
    let env = Env::default();
    let fx = setup(&env);

    fx.client.set_governor(&Address::generate(&env));
    let result = fx.client.try_set_governor(&Address::generate(&env));

    assert_eq!(result, Err(Ok(Error::GovernorAlreadySet)));
}

/// Non-governor cannot call `set_governor`: same native auth trap
/// pattern as `upgrade_without_governor_auth_traps` below.
#[test]
#[should_panic]
fn set_governor_without_governor_auth_traps() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks.
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let series_wasm = env.upload(DummySeries);
    let contract_id = env.register(
        MarketFactory,
        (governor, oracle, registry, treasury, usdc, series_wasm),
    );
    let client = MarketFactoryClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.set_governor(&Address::generate(&env));
}

/// After a successful handover, the OLD governor is rejected on a
/// governor-gated call and the NEW governor is accepted, proving
/// `set_governor` genuinely changed which address is authorized, not
/// just that the call itself succeeded once.
#[test]
fn set_governor_old_governor_rejected_new_governor_accepted() {
    let env = Env::default();
    let old_governor = Address::generate(&env);
    let new_governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let series_wasm = env.upload(DummySeries);
    let contract_id = env.register(
        MarketFactory,
        (
            old_governor.clone(),
            oracle,
            registry,
            treasury,
            usdc,
            series_wasm,
        ),
    );
    let client = MarketFactoryClient::new(&env, &contract_id);

    env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &old_governor,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_governor",
            args: (new_governor.clone(),).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_governor(&new_governor);

    let new_wasm = env.upload(MarketFactory);

    // The old governor is no longer the one recorded: a call it signs
    // for still traps natively, exactly like any other non-governor
    // caller.
    let old_governor_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &old_governor,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "upgrade",
                args: (new_wasm.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.upgrade(&new_wasm);
    }));
    assert!(
        old_governor_result.is_err(),
        "the old governor must be rejected once set_governor has handed off"
    );

    // The new governor, signing for itself, succeeds.
    env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &new_governor,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &contract_id,
            fn_name: "upgrade",
            args: (new_wasm.clone(),).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.upgrade(&new_wasm);
}

#[test]
#[should_panic]
fn upgrade_without_governor_auth_traps() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks,
    // matching staking's own upgrade_without_governor_auth_traps.
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let series_wasm = env.upload(DummySeries);
    let contract_id = env.register(
        MarketFactory,
        (governor, oracle, registry, treasury, usdc, series_wasm),
    );
    let client = MarketFactoryClient::new(&env, &contract_id);

    let new_wasm_hash = env.upload(MarketFactory);
    client.upgrade(&new_wasm_hash);
}

#[test]
#[should_panic]
fn open_series_without_governor_auth_traps() {
    let env = Env::default();
    let governor = Address::generate(&env);
    let oracle_id = env.register(MockRiskOracle, ());
    let oracle = MockRiskOracleClient::new(&env, &oracle_id);
    let registry_id = env.register(MockEventRegistry, ());
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let series_wasm = env.upload(DummySeries);
    let contract_id = env.register(
        MarketFactory,
        (
            governor,
            oracle_id,
            registry_id,
            treasury,
            usdc.clone(),
            series_wasm,
        ),
    );
    let client = MarketFactoryClient::new(&env, &contract_id);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    oracle.set_asset_config(&asset, &enabled_asset_config(&env, &asset, &issuer));
    let mut def_versions = Map::new(&env);
    def_versions.set(EventKind::Depeg, 1);
    let terms = SeriesTerms {
        asset,
        def_versions,
        settlement: usdc,
        start: 1_000,
        expiry: 1_000 + 90 * 86_400,
        claim_window_secs: 30 * 86_400,
        cap: 1_000_000_000,
        max_cover_per_buyer: 100_000_000,
        require_holding: false,
        fee_bps: 750,
    };
    client.open_series(&terms);
}

#[test]
fn open_series_rejects_an_unknown_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_a_disabled_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let mut config = enabled_asset_config(&env, &asset, &issuer);
    config.enabled = false;
    fx.oracle.set_asset_config(&asset, &config);
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_a_stale_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .set_asset_config(&asset, &enabled_asset_config(&env, &asset, &issuer));
    fx.oracle.set_stale(&asset, &true);
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_an_asset_whose_related_seller_list_was_never_set() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .set_asset_config(&asset, &enabled_asset_config(&env, &asset, &issuer));
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::RelatedSellerListNotSet)));
}

#[test]
fn open_series_rejects_an_asset_whose_readable_issuer_is_not_on_its_own_list() {
    let env = Env::default();
    let fx = setup(&env);
    let issuer = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(issuer.clone())
        .address();
    fx.oracle
        .set_asset_config(&asset, &enabled_asset_config(&env, &asset, &issuer));
    // A non-empty list that deliberately excludes the real issuer.
    fx.client
        .set_related_sellers(&asset, &Vec::from_array(&env, [Address::generate(&env)]));
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::IssuerNotListed)));
}

#[test]
fn open_series_succeeds_for_an_asset_whose_admin_cannot_be_read() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env); // not a real SAC: admin() unreadable
    let issuer = Address::generate(&env);
    fx.oracle
        .set_asset_config(&asset, &enabled_asset_config(&env, &asset, &issuer));
    // A non-empty list that would reject `issuer` were it checked -
    // proving the skip, not merely an empty-list coincidence.
    fx.client
        .set_related_sellers(&asset, &Vec::from_array(&env, [Address::generate(&env)]));
    let terms = valid_terms(&env, &fx, &asset);
    let series = fx.client.open_series(&terms);
    assert_eq!(
        fx.client.series_for(&asset),
        Vec::from_array(&env, [series])
    );
}

#[test]
fn open_series_succeeds_and_records_pins() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let terms = valid_terms(&env, &fx, &asset);

    let series = fx.client.open_series(&terms);

    assert_eq!(
        fx.client.series_for(&asset),
        Vec::from_array(&env, [series])
    );
    assert_eq!(fx.client.series_count(), 1);
    assert!(fx.client.has_live_cover(&asset, &EventKind::Depeg, &1));
    assert!(!fx.client.has_live_cover(&asset, &EventKind::Depeg, &2));
}

#[test]
fn open_series_rejects_an_asset_blocked_by_a_declared_event() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    fx.registry.set_declared(&asset, &true);
    let terms = valid_terms(&env, &fx, &asset);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::AssetBlocked)));
}

#[test]
fn open_series_rejects_empty_def_versions() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.def_versions = Map::new(&env);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::DefinitionNotCurrent)));
}

#[test]
fn open_series_rejects_a_pinned_version_that_is_not_canonical() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.def_versions.set(EventKind::Depeg, 2); // registry canonical is 1
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::DefinitionNotCurrent)));
}

#[test]
fn open_series_rejects_settlement_equal_to_the_covered_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.settlement = asset.clone();
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidSettlement)));
}

#[test]
fn open_series_rejects_settlement_sharing_the_covered_assets_issuer() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    // A second asset sharing the same issuer, used as settlement.
    terms.settlement = env.register_stellar_asset_contract_v2(issuer).address();
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidSettlement)));
}

#[test]
fn open_series_rejects_require_holding_against_an_asset_referenced_pair() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, issuer) = listed_asset(&env, &fx);
    let mut config = enabled_asset_config(&env, &asset, &issuer);
    config.reference = Reference::Asset(Address::generate(&env));
    fx.oracle.set_asset_config(&asset, &config);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.require_holding = true;
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_start_at_or_after_expiry() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.start = terms.expiry;
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_fee_bps_above_10000() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let mut terms = valid_terms(&env, &fx, &asset);
    terms.fee_bps = 10_001;
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::InvalidTerms)));
}

#[test]
fn open_series_rejects_a_fifth_live_series_on_the_same_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    let terms = valid_terms(&env, &fx, &asset);
    for _ in 0..4 {
        fx.client.open_series(&terms);
    }
    assert_eq!(fx.client.series_for(&asset).len(), 4);
    let result = fx.client.try_open_series(&terms);
    assert_eq!(result, Err(Ok(Error::TooManySeries)));
}

#[test]
#[should_panic]
fn set_series_wasm_without_governor_auth_traps() {
    let env = Env::default();
    let fx_governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();
    let series_wasm = env.upload(DummySeries);
    let contract_id = env.register(
        MarketFactory,
        (fx_governor, oracle, registry, treasury, usdc, series_wasm),
    );
    let client = MarketFactoryClient::new(&env, &contract_id);

    let new_hash = BytesN::from_array(&env, &[9u8; 32]);
    client.set_series_wasm(&new_hash);
}

#[test]
fn related_sellers_defaults_to_none_until_set_then_reads_back() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    assert_eq!(fx.client.related_sellers(&asset), None);
    let list = Vec::from_array(&env, [Address::generate(&env)]);
    fx.client.set_related_sellers(&asset, &list);
    assert_eq!(fx.client.related_sellers(&asset), Some(list));
}

#[test]
fn price_guard_defaults_to_none_until_set_then_reads_back_and_can_be_cleared() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    assert_eq!(fx.client.price_guard(&asset), None);
    let guard = Address::generate(&env);
    fx.client.set_price_guard(&asset, &Some(guard.clone()));
    assert_eq!(fx.client.price_guard(&asset), Some(guard));
    fx.client.set_price_guard(&asset, &None);
    assert_eq!(fx.client.price_guard(&asset), None);
}

#[test]
fn cover_cap_is_the_minimum_of_hard_cap_and_liquidity_ratio_term() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    fx.oracle.set_median_liquidity(&asset, &1_000_000_000);
    // Default ratio 0.25: liquidity term = 250,000,000.
    assert_eq!(fx.client.cover_cap(&asset), 250_000_000);

    fx.client.set_hard_cap(&asset, &100_000_000);
    assert_eq!(fx.client.cover_cap(&asset), 100_000_000);

    fx.client.set_hard_cap(&asset, &1_000_000_000_000);
    fx.client
        .set_liquidity_cover_ratio(&(sylox_types::SCALE / 2)); // 0.5
    assert_eq!(fx.client.cover_cap(&asset), 500_000_000);
}

#[test]
fn reserve_and_release_cover_track_open_cover_and_live_cover_count() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    fx.oracle.set_median_liquidity(&asset, &1_000_000_000); // cap 250,000,000
    let terms = valid_terms(&env, &fx, &asset);
    let series = fx.client.open_series(&terms);

    assert!(fx.client.has_live_cover(&asset, &EventKind::Depeg, &1));

    fx.client.reserve_cover(&series, &100_000_000);
    assert_eq!(fx.client.open_cover(&asset), 100_000_000);

    let result = fx.client.try_reserve_cover(&series, &200_000_000);
    assert_eq!(result, Err(Ok(Error::CoverCapExceeded)));

    fx.client.release_cover(&series, &100_000_000);
    assert_eq!(fx.client.open_cover(&asset), 0);
    assert!(fx.client.series_for(&asset).is_empty());
    assert!(!fx.client.has_live_cover(&asset, &EventKind::Depeg, &1));

    // Not idempotent by design (see release_cover's own doc comment):
    // a second call for the same series, now cleared from Deployed,
    // is indistinguishable from a stranger and traps the same way.
    let result = fx.client.try_release_cover(&series, &0);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn reserve_cover_rejects_a_series_this_factory_never_deployed() {
    let env = Env::default();
    let fx = setup(&env);
    let stranger = Address::generate(&env);
    let result = fx.client.try_reserve_cover(&stranger, &1);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn release_cover_rejects_a_series_this_factory_never_deployed() {
    let env = Env::default();
    let fx = setup(&env);
    let stranger = Address::generate(&env);
    let result = fx.client.try_release_cover(&stranger, &1);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

/// PR #67 review: `release_cover` must fail loudly, not clamp to 0,
/// when asked to release more than `OpenCover(asset)` currently holds.
/// That exact gap is what silently stranded the no-buyer series in
/// the live list before this fix (`AccountingUnderflow`, deviation 17
/// in the design note).
#[test]
fn release_cover_rejects_releasing_more_than_open_cover() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    fx.oracle.set_median_liquidity(&asset, &1_000_000_000); // cap 250,000,000
    let terms = valid_terms(&env, &fx, &asset);
    let series = fx.client.open_series(&terms);

    fx.client.reserve_cover(&series, &100_000_000);
    assert_eq!(fx.client.open_cover(&asset), 100_000_000);

    let result = fx.client.try_release_cover(&series, &200_000_000);
    assert_eq!(result, Err(Ok(Error::AccountingUnderflow)));

    // The whole call traps and rolls back: nothing written earlier in
    // this same invocation (there was nothing to write before the
    // check here, but the point holds generally) is left applied.
    assert_eq!(fx.client.open_cover(&asset), 100_000_000);
    assert_eq!(fx.client.series_for(&asset).len(), 1);
    assert!(fx.client.has_live_cover(&asset, &EventKind::Depeg, &1));
}

/// PR #67 review: the companion failure mode, a pinned `(kind,
/// version)`'s own `LiveCoverCount` already at 0 when `release_cover`
/// tries to decrement it - not reachable through the public API alone
/// (every `open_series`/`release_cover` pair keeps it consistent), so
/// forced directly via `crate::storage`, the same way this crate's
/// own private-module test access already works elsewhere in this
/// file (`crate::storage::get_config`).
#[test]
fn release_cover_rejects_a_pin_count_already_at_zero() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, _issuer) = listed_asset(&env, &fx);
    fx.oracle.set_median_liquidity(&asset, &1_000_000_000);
    let terms = valid_terms(&env, &fx, &asset);
    let series = fx.client.open_series(&terms);

    env.as_contract(&fx.contract_id, || {
        crate::storage::set_live_cover_count(&env, &asset, EventKind::Depeg, 1, 0);
    });

    let result = fx.client.try_release_cover(&series, &0);
    assert_eq!(result, Err(Ok(Error::AccountingUnderflow)));

    // Rolled back: still deployed, still on the live list - the
    // rejected call changed nothing (`LiveCoverCount` itself is left
    // deliberately corrupted by this test's own forced setup above,
    // not by anything `release_cover` did, so it is not re-asserted
    // here).
    assert_eq!(fx.client.series_for(&asset).len(), 1);
}
