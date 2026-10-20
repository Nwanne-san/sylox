//! EventRegistry test suite. technical-doc.md Section 8 (all), 12.2,
//! 13, 14, 15, 21; design note `docs/design/event-registry.md`.

extern crate std;

mod integration;
mod property;
mod sub_epochs;
mod ttl;

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{contract, contractimpl, Address, BytesN, Env, IntoVal, Symbol};
use sylox_types::{
    AssetConfig, AssetEventStatus, CoverGate, EndpointStatus, EventDefinition, EventKind,
    EventState, IssuerFlags, Reference,
};

use crate::{Error, EventRegistry, EventRegistryClient};

// -- Minimal mocks for RiskOracle and Staking, for unit tests that
// don't need the real cross-contract behavior; the integration test
// module uses the real RiskOracle, Staking and Treasury throughout. --

/// Minimal mock `Staking`, only what `RiskOracle.post_signals` and
/// `EventRegistry`'s own bond calls need for a unit test. Every
/// keeper is active; bonds are recorded (so a test can assert a lock
/// happened) but move no real funds, matching `risk-oracle`'s own
/// `MockStaking` convention (not reused directly: that one is a
/// private `#[cfg(test)]` item inside `risk-oracle`'s own crate, not
/// reachable from here). The integration test module uses the real
/// `Staking` and `Treasury` instead.
#[contract]
struct MockStaking;

#[contractimpl]
impl MockStaking {
    pub fn is_active_keeper(_env: Env, _keeper: Address) -> bool {
        true
    }

    /// Overrides `aggregate`'s return for one `(asset, epoch)`, for
    /// the handful of tests that need a specific endpoint status
    /// (e.g. `RecentEndpointOutage`). Every other `(asset, epoch)`
    /// keeps the existing hardcoded `Up` default, so no existing test
    /// is affected by this method merely existing.
    pub fn set_aggregate(env: Env, asset: Address, epoch: u64, status: EndpointStatus) {
        env.storage()
            .temporary()
            .set(&StakingKey::Aggregate(asset, epoch), &status);
    }

    pub fn aggregate(env: Env, asset: Address, epoch: u64) -> EndpointStatus {
        env.storage()
            .temporary()
            .get(&StakingKey::Aggregate(asset, epoch))
            .unwrap_or(EndpointStatus::Up)
    }

    pub fn settle_probes(_env: Env, _asset: Address, _epoch: u64) {}

    pub fn reward_keeper(_env: Env, _keeper: Address, _epochs: u32) -> i128 {
        0
    }

    /// Section 5.9 S6 (v1.5): `RiskOracle.try_build_hour`'s own reward
    /// call for a built hour's distinct posters. A no-op return, same
    /// as `reward_keeper`: no event-registry test asserts anything
    /// about keeper reward amounts, only that a built hour's other
    /// effects (ring state, cover_gate) land correctly.
    pub fn reward_keeper_sub_epochs(
        _env: Env,
        _keeper: Address,
        _sub_epoch_count: u32,
        _sub_epoch_secs: u64,
    ) -> i128 {
        0
    }

    pub fn lock_bond(
        env: Env,
        key: sylox_types::BondKey,
        owner: Address,
        amount: i128,
        subject: Option<Address>,
    ) {
        let _ = subject;
        env.storage()
            .temporary()
            .set(&StakingKey::Bond(key), &(owner, amount));
    }

    pub fn release_bond(env: Env, key: sylox_types::BondKey) {
        env.storage().temporary().remove(&StakingKey::Bond(key));
    }

    pub fn forfeit_bond(env: Env, key: sylox_types::BondKey, winner: Option<Address>) {
        let _ = winner;
        env.storage().temporary().remove(&StakingKey::Bond(key));
    }

    pub fn bond(env: Env, key: sylox_types::BondKey) -> Option<(Address, i128)> {
        env.storage().temporary().get(&StakingKey::Bond(key))
    }

    /// `RiskOracle.resolve_signal_dispute`'s own disputer-wins path
    /// (finding F4's tests exercise it, via a real `RiskOracle`) calls
    /// this to slash the overturned keeper's own stake. A no-op here
    /// is enough: no event-registry test asserts anything about a
    /// keeper's staked balance.
    pub fn slash(
        _env: Env,
        _who: Address,
        _amount: i128,
        _winner: Option<Address>,
        _reason: BytesN<32>,
    ) {
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum StakingKey {
    Bond(sylox_types::BondKey),
    Aggregate(Address, u64),
}

/// markets design note Section 10: `version_has_live_cover` now calls
/// the real `MarketFactory.has_live_cover`, so `setup`'s own
/// `factory` address has to be a real contract, not a bare generated
/// address - every unit test here defaults to "no live series pins
/// anything" (matching this stand-in's own pre-markets behavior)
/// unless a test explicitly says otherwise via `set_has_live_cover`.
#[contract]
struct MockMarketFactory;

#[contractimpl]
impl MockMarketFactory {
    pub fn set_has_live_cover(
        env: Env,
        asset: Address,
        kind: EventKind,
        version: u32,
        value: bool,
    ) {
        env.storage()
            .temporary()
            .set(&FactoryKey::LiveCover(asset, kind, version), &value);
    }
    pub fn has_live_cover(env: Env, asset: Address, kind: EventKind, version: u32) -> bool {
        env.storage()
            .temporary()
            .get(&FactoryKey::LiveCover(asset, kind, version))
            .unwrap_or(false)
    }
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
enum FactoryKey {
    LiveCover(Address, EventKind, u32),
}

#[contract]
struct MockGovernor;

#[contractimpl]
impl MockGovernor {
    pub fn set_committee(env: Env, committee: Address) {
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "committee"), &committee);
    }

    pub fn committee(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "committee"))
            .unwrap()
    }
}

struct Fixture<'a> {
    client: EventRegistryClient<'a>,
    #[allow(dead_code)]
    contract_id: Address,
    governor_contract: Address,
    committee: Address,
    oracle: risk_oracle::RiskOracleClient<'a>,
    #[allow(dead_code)]
    oracle_id: Address,
    staking: Address,
    staking_client: MockStakingClient<'a>,
    factory: Address,
    #[allow(dead_code)]
    factory_client: MockMarketFactoryClient<'a>,
    usdc: Address,
    keeper: Address,
}

fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();

    let governor_contract = env.register(MockGovernor, ());
    let governor_client = MockGovernorClient::new(env, &governor_contract);
    let committee = Address::generate(env);
    governor_client.set_committee(&committee);

    let staking = env.register(MockStaking, ());
    let staking_client = MockStakingClient::new(env, &staking);

    let factory = env.register(MockMarketFactory, ());
    let factory_client = MockMarketFactoryClient::new(env, &factory);
    let usdc = Address::generate(env);

    // `EventRegistry`'s constructor needs `oracle_id` and
    // `RiskOracle`'s needs `registry_id`, so neither can be
    // registered (with its real constructor) before the other
    // exists. `Address::generate` hands out both addresses up front -
    // no on-chain equivalent is needed here, since these are just
    // test addresses - breaking the cycle, matching `staking::test`'s
    // own `setup` convention for the same problem.
    let registry_id = Address::generate(env);
    let oracle_id = Address::generate(env);

    env.register_at(
        &oracle_id,
        risk_oracle::RiskOracle,
        (
            governor_contract.clone(),
            registry_id.clone(),
            staking.clone(),
        ),
    );
    let oracle = risk_oracle::RiskOracleClient::new(env, &oracle_id);

    env.register_at(
        &registry_id,
        EventRegistry,
        (
            governor_contract.clone(),
            oracle_id.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(env, &registry_id);

    let keeper = Address::generate(env);

    Fixture {
        client,
        contract_id: registry_id,
        governor_contract,
        committee,
        oracle,
        oracle_id,
        staking,
        staking_client,
        factory,
        factory_client,
        usdc,
        keeper,
    }
}

fn asset_config(env: &Env, asset: &Address, issuer: &Address, flags: IssuerFlags) -> AssetConfig {
    AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: soroban_sdk::String::from_str(env, "example.com"),
        amm_adapters: soroban_sdk::Vec::new(env),
        fx_adapter: None,
        min_liquidity: 100_000_000_000,
        issuer_flags: flags,
        enabled: true,
    }
}

fn depeg_definition(_env: &Env, asset: &Address) -> EventDefinition {
    EventDefinition {
        asset: asset.clone(),
        kind: EventKind::Depeg,
        version: 0, // ignored on input
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

fn issuer_freeze_definition(_env: &Env, asset: &Address) -> EventDefinition {
    EventDefinition {
        asset: asset.clone(),
        kind: EventKind::IssuerFreeze,
        version: 0,
        reference: Reference::Usd,
        depeg_threshold: 0,
        depeg_window_secs: 0,
        max_missing_epochs: 0,
        cure_threshold: 0,
        freeze_pct_bps: 500,
        auth_revocation_threshold: 3,
        mint_spike_bps: 0,
        halt_window_secs: 0,
        challenge_secs: 86_400,
        ruling_deadline_secs: 1_209_600,
    }
}

fn signal_set(
    env: &Env,
    keeper: &Address,
    epoch: u64,
    peg_ratio: i128,
    liquidity: i128,
) -> sylox_types::SignalSet {
    sylox_types::SignalSet {
        epoch,
        posted_at: 0,
        peg_ratio,
        peg_ratio_p10: peg_ratio,
        liquidity_2pct: liquidity,
        redemption_net: 0,
        supply: 10_000_000_000_000,
        supply_change_bps: 0,
        issuer_actions: sylox_types::IssuerActions::default(),
        endpoint: EndpointStatus::Unknown,
        inputs_hash: BytesN::from_array(env, &[7u8; 32]),
        poster: keeper.clone(),
    }
}

/// Posts epochs `[start_epoch, start_epoch + count)`, one `EPOCH_SECS`
/// apart (the steady-posting convention every other test in this
/// workspace uses), each with the given `peg_ratio`/`liquidity`.
/// Leaves the ledger timestamp at the last epoch's own close; callers
/// needing every epoch effectively Final must advance the clock by at
/// least `SIGNAL_DISPUTE_SECS` more themselves.
#[allow(clippy::too_many_arguments)]
fn post_run(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    start_epoch: u64,
    count: u64,
    peg_ratio: i128,
    liquidity: i128,
) {
    for i in 0..count {
        let epoch = start_epoch + i;
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        oracle.post_signals(
            keeper,
            asset,
            &signal_set(env, keeper, epoch, peg_ratio, liquidity),
        );
    }
}

/// Builds exactly the data a Depeg `propose_tier1` call needs to
/// pass: 168 healthy baseline epochs (epochs `0..168`, liquidity well
/// above `min_liquidity`), then 72 failing epochs (`168..240`, peg
/// ratio below `depeg_threshold`), then advances the clock past the
/// last epoch's own `pending_until` so every epoch is effectively
/// Final. Returns the epoch range `[168, 240)` is anchored to, i.e.
/// the epoch `propose_tier1`'s own window ends at (239).
fn post_failing_depeg_window(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
) {
    post_run(
        env,
        oracle,
        keeper,
        asset,
        0,
        168,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        env,
        oracle,
        keeper,
        asset,
        168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);
}

/// The same 168 epoch healthy baseline, but a Depeg window
/// (`168..240`) that PASSES (peg ratio at or above threshold), for
/// tests that need a registered, checkable Depeg definition without
/// an event actually triggering.
fn post_healthy_depeg_window(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
) {
    post_run(
        env,
        oracle,
        keeper,
        asset,
        0,
        240,
        9_900_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);
}

const EPOCH_SECS: u64 = 3_600;
const SIGNAL_DISPUTE_SECS: u64 = 7_200;
#[allow(dead_code)]
const WINDOW_SECS: u64 = 259_200;
#[allow(dead_code)]
const SIGNAL_DISPUTE_RULING_SECS: u64 = 518_400;

// -- register_definition --

#[test]
fn register_definition_succeeds_for_depeg_and_assigns_version_1() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let version = fx
        .client
        .register_definition(&depeg_definition(&env, &asset));
    assert_eq!(version, 1);
    assert_eq!(fx.client.current_version(&asset, &EventKind::Depeg), 1);
    let stored = fx.client.definition(&asset, &EventKind::Depeg, &1).unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.depeg_threshold, 9_500_000);
}

#[test]
fn register_definition_requires_governor_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    assert_eq!(
        env.auths()[0].0,
        fx.governor_contract,
        "register_definition must check the governor's own auth"
    );
}

#[test]
fn register_definition_rejects_an_unknown_asset() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let result = fx
        .client
        .try_register_definition(&depeg_definition(&env, &asset));
    assert_eq!(result, Err(Ok(Error::InvalidDefinition)));
}

#[test]
fn register_definition_rejects_a_reference_mismatch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let mut def = depeg_definition(&env, &asset);
    def.reference = sylox_types::Reference::Fiat(
        Symbol::new(&env, "ARS"),
        sylox_types::FxRateSource::Official,
    );
    let result = fx.client.try_register_definition(&def);
    assert_eq!(result, Err(Ok(Error::InvalidDefinition)));
}

#[test]
fn register_definition_rejects_a_non_zero_parameter_unused_by_the_kind() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let mut def = depeg_definition(&env, &asset);
    def.freeze_pct_bps = 100;
    let result = fx.client.try_register_definition(&def);
    assert_eq!(result, Err(Ok(Error::InvalidDefinition)));
}

#[test]
fn register_definition_rejects_a_window_plus_baseline_too_large_for_the_ring() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let mut def = depeg_definition(&env, &asset);
    // 72 window epochs + 168 baseline epochs = 240 = RING_SLOTS already;
    // one more epoch of window tips it over.
    def.depeg_window_secs = (72 + 1) * 3_600;
    let result = fx.client.try_register_definition(&def);
    assert_eq!(result, Err(Ok(Error::InvalidDefinition)));
}

/// PR #15 review, finding F3: `challenge_secs` must be a whole number
/// of epochs, and within `MAX_CURE_EPOCHS`, or the cure window can be
/// empty (0 or sub-hour) or too large for the progress bitmap (73h).
#[test]
fn register_definition_rejects_a_challenge_secs_not_aligned_to_a_whole_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    for bad in [1_800u64, 5_400, 73 * 3_600] {
        let mut def = depeg_definition(&env, &asset);
        def.challenge_secs = bad;
        let result = fx.client.try_register_definition(&def);
        assert_eq!(
            result,
            Err(Ok(Error::InvalidDefinition)),
            "challenge_secs {bad} must be rejected"
        );
    }
}

/// PR #15 review, finding F3: a proposal whose cure window covers
/// exactly one epoch still evaluates that epoch, rather than falling
/// through a zero-iteration loop into an automatic Cured.
#[test]
fn finalize_evaluates_exactly_one_cure_epoch_when_challenge_secs_is_one_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    let mut def = depeg_definition(&env, &asset);
    def.challenge_secs = EPOCH_SECS;
    fx.client.register_definition(&def);
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    // Exactly one cure epoch: [proposed_at, proposed_at + EPOCH_SECS).
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + EPOCH_SECS) / EPOCH_SECS;
    assert_eq!(last_cure_epoch - first_cure_epoch, 1);

    // Leave it permanently missing: never post it, advance past its
    // own backfill window.
    let epoch_close = (first_cure_epoch + 1) * EPOCH_SECS;
    env.ledger()
        .set_timestamp(epoch_close + WINDOW_SECS + SIGNAL_DISPUTE_SECS + 1);

    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Declared,
        "the single cure epoch must actually be evaluated, not skipped"
    );
}

#[test]
fn register_definition_rejects_issuer_freeze_when_neither_flag_is_set() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let result = fx
        .client
        .try_register_definition(&issuer_freeze_definition(&env, &asset));
    assert_eq!(result, Err(Ok(Error::FreezeImpossible)));
}

#[test]
fn register_definition_accepts_issuer_freeze_when_a_flag_is_set() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
    ));

    let version = fx
        .client
        .register_definition(&issuer_freeze_definition(&env, &asset));
    assert_eq!(version, 1);
}

#[test]
fn register_definition_rejects_a_second_version_while_an_event_is_in_progress() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    let keeper = Address::generate(&env);
    post_failing_depeg_window(&env, &fx.oracle, &keeper, &asset);
    fx.client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    let result = fx
        .client
        .try_register_definition(&depeg_definition(&env, &asset));
    assert_eq!(result, Err(Ok(Error::EventInProgress)));
}

/// markets design note Section 10: `version_has_live_cover` now
/// really calls `MarketFactory.has_live_cover` - this proves the
/// wiring itself works, not just that it was replaced without
/// breaking anything else.
#[test]
fn register_definition_rejects_definition_in_use_when_a_live_series_pins_it() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    fx.factory_client
        .set_has_live_cover(&asset, &EventKind::Depeg, &1, &true);

    let result = fx
        .client
        .try_register_definition(&depeg_definition(&env, &asset));
    assert_eq!(result, Err(Ok(Error::DefinitionInUse)));

    // Once the factory reports no live series pinning it, superseding works again.
    fx.factory_client
        .set_has_live_cover(&asset, &EventKind::Depeg, &1, &false);
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    assert_eq!(fx.client.current_version(&asset, &EventKind::Depeg), 2);
}

// -- propose_tier1: Depeg --

#[test]
fn propose_tier1_depeg_succeeds_on_a_failing_window() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);

    let caller = Address::generate(&env);
    let id = fx
        .client
        .propose_tier1(&caller, &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.state, EventState::Proposed);
    assert_eq!(record.kind, EventKind::Depeg);
    assert_eq!(record.def_version, 1);
    assert_eq!(record.bond, 0, "Tier 1 posts no proposer bond");
    assert_eq!(
        record.window_start,
        168 * 3_600,
        "window_start is the start of the window's own first epoch (168)"
    );
    assert!(matches!(
        fx.client.event_status(&asset, &EventKind::Depeg),
        AssetEventStatus::InProgress(found_id) if found_id == id
    ));
}

/// PR #15 review, finding F5: `propose_tier1` must authenticate
/// `caller`, the address stored as `proposer` and published in
/// `EventProposed` — a public record an indexer will display.
#[test]
fn propose_tier1_requires_callers_own_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);

    let caller = Address::generate(&env);
    fx.client
        .propose_tier1(&caller, &asset, &EventKind::Depeg, &1);
    assert_eq!(
        env.auths()[0].0,
        caller,
        "propose_tier1 must check caller's own auth, not anyone else's"
    );
}

#[test]
fn propose_tier1_depeg_rejects_a_passing_window() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_healthy_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::Tier1CheckFailed)));
}

#[test]
fn propose_tier1_depeg_tolerates_missing_epochs_up_to_max_missing_epochs() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // 168 healthy baseline, then only 66 of the 72 window epochs
    // posted (6 missing, exactly `max_missing_epochs`), all failing.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        168,
        9_900_000,
        500_000_000_000,
    );
    for i in 0..72u64 {
        let epoch = 168 + i;
        if i < 6 {
            continue; // leave the first 6 window epochs Empty
        }
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_000_000, 500_000_000_000),
        );
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert!(fx.client.event(&id).is_some());
}

#[test]
fn propose_tier1_depeg_rejects_more_than_max_missing_epochs() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        168,
        9_900_000,
        500_000_000_000,
    );
    for i in 0..72u64 {
        let epoch = 168 + i;
        if i < 7 {
            continue; // 7 missing, one more than max_missing_epochs
        }
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_000_000, 500_000_000_000),
        );
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::Tier1CheckFailed)));
}

#[test]
fn propose_tier1_depeg_rejects_a_liquidity_baseline_below_min_liquidity() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    // min_liquidity is 100_000_000_000 (see asset_config); post a
    // baseline just under it.
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        168,
        9_900_000,
        50_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::Tier1CheckFailed)));
}

/// PR #25 review: `check_depeg`'s own baseline guard
/// (`window_start_epoch < BASELINE_EPOCHS`) compared an absolute
/// epoch number to 168. On a real network that absolute number is
/// always far larger than 168, so the guard never actually required
/// the LIQUIDITY BASELINE (as opposed to the Depeg window itself,
/// which has its own, separate, self-correcting missing-epoch tally)
/// to fall entirely within this asset's own real history. A
/// short-lived asset could pass a 7-day liquidity baseline check
/// from a handful of real epochs plus a run of permanently-missing
/// ones that the median silently excludes rather than counts
/// against.
#[test]
fn propose_tier1_depeg_fails_closed_with_a_liquidity_baseline_shorter_than_real_history() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // Only 24 real baseline epochs (not the full 168), immediately
    // followed by a genuinely failing 72-epoch Depeg window, all at a
    // realistic epoch base.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE,
        24,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE + 24,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(
        result,
        Err(Ok(Error::Tier1CheckFailed)),
        "a 24 epoch baseline at a realistic epoch base must fail closed, \
         not pass as if it were a full 168 epoch (7 day) baseline"
    );
}

/// The success-case companion to the test above, at the same
/// realistic epoch base: a genuine, full 168-epoch baseline followed
/// by a failing 72-epoch window must still let `propose_tier1`
/// through. Proves the fix (measuring against `first_epoch`) doesn't
/// reject a real, complete history, only a short one.
#[test]
fn propose_tier1_depeg_succeeds_with_a_full_168_epoch_baseline_at_a_realistic_epoch() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE,
        168,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE + 168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert!(fx.client.event(&id).is_some());
}

/// Regression: `check_depeg`'s own `window_start_epoch` subtraction
/// must not underflow when the asset has posted far fewer epochs
/// than the 72 the window needs; it must fail closed, not panic or
/// wrap into a garbage epoch number.
#[test]
fn propose_tier1_depeg_fails_closed_with_not_enough_history_for_one_window() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // Only 5 epochs ever posted, nowhere near the 72 the window needs.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        5,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::Tier1CheckFailed)));
}

#[test]
fn propose_tier1_rejects_an_unregistered_version() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::UnknownDefinition)));
}

#[test]
fn propose_tier1_rejects_a_non_canonical_version_with_no_live_series() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    // Supersede version 1 with version 2 (no event in progress, no
    // live series to object either way in this build).
    post_healthy_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    assert_eq!(fx.client.current_version(&asset, &EventKind::Depeg), 2);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(
        result,
        Err(Ok(Error::VersionNotCovered)),
        "review item D3: a superseded, non-canonical version is not proposable \
         with no MarketFactory to confirm a live series still pins it"
    );
}

#[test]
fn propose_tier1_at_the_canonical_version_still_works_after_a_new_version_is_registered() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_healthy_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        240,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &2);
    assert_eq!(fx.client.event(&id).unwrap().def_version, 2);
}

// -- propose_tier1: IssuerFreeze --

fn post_issuer_actions(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    clawback_amount: i128,
    auth_revocations: u32,
) {
    post_issuer_actions_from(
        env,
        oracle,
        keeper,
        asset,
        0,
        clawback_amount,
        auth_revocations,
    );
}

/// PR #25 review: same as `post_issuer_actions`, but starting at
/// `start_epoch` instead of always 0, so a test can exercise a
/// realistic, unix-time-derived epoch range, where
/// `check_issuer_freeze`'s own absolute-epoch-vs-window-length guard
/// (fixed by this same review) would otherwise never have been
/// exercised against real history requirements.
#[allow(clippy::too_many_arguments)]
fn post_issuer_actions_from(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    start_epoch: u64,
    clawback_amount: i128,
    auth_revocations: u32,
) {
    for i in 0..168u64 {
        let epoch = start_epoch + i;
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        let mut set = signal_set(env, keeper, epoch, 10_000_000, 500_000_000_000);
        if i == 100 {
            set.issuer_actions = sylox_types::IssuerActions {
                clawbacks: 1,
                clawback_amount,
                auth_revocations,
                flag_changes: 0,
            };
        }
        oracle.post_signals(keeper, asset, &set);
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);
}

#[test]
fn propose_tier1_issuer_freeze_succeeds_on_a_clawback_above_threshold() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: false,
            clawback_enabled: true,
        },
    ));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &asset));
    // freeze_pct_bps = 500 (5%); supply = 10_000_000_000_000; 5% is
    // 500_000_000_000.
    post_issuer_actions(&env, &fx.oracle, &fx.keeper, &asset, 600_000_000_000, 0);

    let id = fx.client.propose_tier1(
        &Address::generate(&env),
        &asset,
        &EventKind::IssuerFreeze,
        &1,
    );
    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.window_start, 100 * 3_600);
}

#[test]
fn propose_tier1_issuer_freeze_rejects_below_both_thresholds() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: false,
            clawback_enabled: true,
        },
    ));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &asset));
    post_issuer_actions(&env, &fx.oracle, &fx.keeper, &asset, 1_000_000, 1);

    let result = fx.client.try_propose_tier1(
        &Address::generate(&env),
        &asset,
        &EventKind::IssuerFreeze,
        &1,
    );
    assert_eq!(result, Err(Ok(Error::Tier1CheckFailed)));
}

#[test]
fn propose_tier1_issuer_freeze_succeeds_on_revocations_above_threshold() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
    ));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &asset));
    // auth_revocation_threshold = 3; post 4.
    post_issuer_actions(&env, &fx.oracle, &fx.keeper, &asset, 0, 4);

    let id = fx.client.propose_tier1(
        &Address::generate(&env),
        &asset,
        &EventKind::IssuerFreeze,
        &1,
    );
    assert!(fx.client.event(&id).is_some());
}

/// PR #27 review (round 2): `check_issuer_freeze` deliberately has no
/// minimum-history requirement, unlike `check_depeg`'s liquidity
/// baseline. A sparse window can only undercount
/// `clawback_sum`/`revocation_sum`, never inflate them, so there is no
/// false-trigger risk to gate against; "not enough history to rely on
/// this asset" is a cover-sale concern (MarketFactory's job), not a
/// payout-trigger one. A newly tracked asset (far fewer than 168
/// epochs of history, at a realistic epoch base) with a genuine
/// revocation spike above `auth_revocation_threshold` must still be
/// able to trigger IssuerFreeze.
#[test]
fn propose_tier1_issuer_freeze_succeeds_with_fewer_than_168_epochs_of_history() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
    ));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &asset));

    // Only 24 real epochs of history, with a genuine revocation spike
    // (4 > auth_revocation_threshold of 3).
    for i in 0..24u64 {
        let epoch = REALISTIC_EPOCH_BASE + i;
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        let mut set = signal_set(&env, &fx.keeper, epoch, 10_000_000, 500_000_000_000);
        if i == 10 {
            set.issuer_actions = sylox_types::IssuerActions {
                clawbacks: 1,
                clawback_amount: 0,
                auth_revocations: 4,
                flag_changes: 0,
            };
        }
        fx.oracle.post_signals(&fx.keeper, &asset, &set);
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let id = fx.client.propose_tier1(
        &Address::generate(&env),
        &asset,
        &EventKind::IssuerFreeze,
        &1,
    );
    assert!(
        fx.client.event(&id).is_some(),
        "a genuine revocation spike must trigger IssuerFreeze even with \
         only 24 epochs of real history; history is a cover-sale gate, \
         not a payout-trigger gate"
    );
}

/// PR #27 review (round 2): `first_epoch` alone proves CALENDAR time
/// has elapsed since the asset's first post, not that the baseline
/// window `check_depeg` reads is actually populated. Post one
/// baseline epoch at a realistic base, go dark for 168+ epochs, then
/// resume with a failing 72-epoch Depeg window immediately. The
/// `first_epoch`-based guard (`window_start_epoch < first_epoch +
/// 168`) is satisfied by calendar time alone, but the baseline range
/// `[baseline_start, window_start_epoch)` no longer contains that one
/// real epoch, except right at its edge.
#[test]
fn propose_tier1_depeg_fails_closed_with_an_empty_baseline_after_a_168_epoch_gap() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // One real baseline epoch at the realistic base (becomes
    // first_epoch), then total silence until the Depeg window itself
    // starts 168 + 72 epochs later, so window_start_epoch sits at
    // exactly first_epoch + 168 + 72 - and the baseline range
    // [window_start_epoch - 168, window_start_epoch) contains none of
    // it (the single real epoch, at REALISTIC_EPOCH_BASE, is 72
    // epochs before baseline_start).
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE,
        1,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE + 1 + 168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(
        result,
        Err(Ok(Error::Tier1CheckFailed)),
        "a baseline range containing zero real epochs (the one real \
         epoch this asset ever posted having already rotated out of \
         [baseline_start, window_start_epoch)) must fail closed; \
         first_epoch alone (calendar time) is not sufficient"
    );
}

/// Sharper variant of the above: the baseline range contains exactly
/// ONE real epoch (not zero), so `liquidity_values.is_empty()` does
/// NOT catch it, yet 167 of the 168 baseline slots are still missing.
/// Exercises the new `MIN_BASELINE_FINAL_EPOCHS` minimum-count gate
/// (1 is far below the 96 required).
#[test]
fn propose_tier1_depeg_fails_closed_with_a_single_real_epoch_baseline_after_a_168_epoch_gap() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // One real baseline epoch at the realistic base (first_epoch),
    // then silence until exactly 168 epochs before the Depeg window
    // starts, so that one real epoch is the OLDEST epoch inside
    // [baseline_start, window_start_epoch) rather than outside it.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE,
        1,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        REALISTIC_EPOCH_BASE + 168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(
        result,
        Err(Ok(Error::Tier1CheckFailed)),
        "a baseline backed by a single real epoch out of 168 must \
         fail closed; liquidity_values.is_empty() alone only catches \
         the all-missing extreme, not a near-empty one"
    );
}

/// Exact boundary of `MIN_BASELINE_FINAL_EPOCHS` (96 = 168 - 72, one
/// full backfill window's worth of tolerated gap): a baseline with
/// exactly 96 real epochs (the other 72 missing in the middle of the
/// baseline, matching the system's own designed backfill tolerance)
/// must pass. The asset's very first post is at epoch 0, so
/// `first_epoch` = 0 and the calendar guard
/// (`window_start_epoch >= first_epoch + 168`) is satisfied the same
/// way every other realistic-epoch-base test in this file satisfies
/// it, isolating this test to the minimum-count gate alone.
#[test]
fn propose_tier1_depeg_succeeds_with_exactly_the_minimum_baseline_final_epochs() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // Epoch 0 only (sets first_epoch), then a 72 epoch gap, then 95
    // more real epochs (1 + 95 = 96 real baseline epochs total), then
    // a genuinely failing 72 epoch window.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        1,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        73,
        95,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert!(
        fx.client.event(&id).is_some(),
        "exactly 96 real baseline epochs (168 - 72, one full backfill \
         window of gap) must be enough to pass"
    );
}

#[test]
fn propose_tier1_depeg_fails_closed_with_one_fewer_than_the_minimum_baseline_final_epochs() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));

    // Epoch 0 only (sets first_epoch), then a 73 epoch gap, then 94
    // more real epochs (1 + 94 = 95 real baseline epochs total), one
    // fewer than the minimum.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        1,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        74,
        94,
        9_900_000,
        500_000_000_000,
    );
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        168,
        72,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(
        result,
        Err(Ok(Error::Tier1CheckFailed)),
        "95 real baseline epochs, one fewer than MIN_BASELINE_FINAL_EPOCHS \
         (96), must fail closed"
    );
}

#[test]
fn challenge_finalize_rule_and_resolve_timeout_reject_an_unknown_event_id() {
    let env = Env::default();
    let fx = setup(&env);
    let bogus_id = 999u64;

    assert_eq!(
        fx.client.try_challenge(
            &Address::generate(&env),
            &bogus_id,
            &BytesN::from_array(&env, &[1u8; 32])
        ),
        Err(Ok(Error::UnknownEvent))
    );
    assert_eq!(
        fx.client.try_finalize(&bogus_id),
        Err(Ok(Error::UnknownEvent))
    );
    assert_eq!(
        fx.client
            .try_rule(&bogus_id, &true, &BytesN::from_array(&env, &[2u8; 32])),
        Err(Ok(Error::UnknownEvent))
    );
    assert_eq!(
        fx.client.try_resolve_timeout(&bogus_id),
        Err(Ok(Error::UnknownEvent))
    );
}

// -- challenge --

#[test]
fn challenge_locks_a_bond_and_escalates_in_the_same_call() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    let challenger = Address::generate(&env);
    fx.client
        .challenge(&challenger, &id, &BytesN::from_array(&env, &[1u8; 32]));

    let record = fx.client.event(&id).unwrap();
    assert_eq!(
        record.state,
        EventState::Escalated,
        "escalates in the same call; Challenged is never an observed storage value"
    );
    assert!(record.escalated_at.is_some());
    assert!(fx
        .staking_client
        .bond(&sylox_types::BondKey::EventChallenge(id))
        .is_some());
}

#[test]
fn challenge_rejects_after_the_challenge_window_closes() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + 86_400 + 1);
    let result = fx.client.try_challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    assert_eq!(result, Err(Ok(Error::ChallengeWindowClosed)));
}

#[test]
fn challenge_rejects_a_second_challenge() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );

    let result = fx.client.try_challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[2u8; 32]),
    );
    assert_eq!(result, Err(Ok(Error::WrongState)));
}

// -- finalize: unchallenged, and Depeg cure --

#[test]
fn finalize_rejects_before_the_challenge_window_elapses() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    let result = fx.client.try_finalize(&id);
    assert_eq!(result, Err(Ok(Error::ChallengeWindowOpen)));
}

#[test]
fn finalize_declares_an_unchallenged_event_after_the_window() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    // The cure window's own epochs (closing in [proposed_at,
    // proposed_at + challenge_secs)) are never posted; advance past
    // their own backfill window too, so they resolve to permanently
    // missing rather than merely not-ready, letting finalize decide.
    let now = env.ledger().timestamp();
    env.ledger()
        .set_timestamp(now + 86_400 + WINDOW_SECS + SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize(&id);

    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.state, EventState::Declared);
    assert!(record.declared_at.is_some());
    assert!(matches!(
        fx.client.event_status(&asset, &EventKind::Depeg),
        AssetEventStatus::Declared(found_id, 1, _, _) if found_id == id
    ));
    assert!(fx.client.has_declared(&asset));
    assert!(
        !fx.client.in_progress(&asset),
        "Declared leaves the active count, the oracle flag must clear"
    );
}

/// Review items D1/R1/R2/R3: the cure window is the challenge window;
/// every one of its epochs must be Final and `>= cure_threshold`.
#[test]
fn finalize_cures_when_every_epoch_in_the_challenge_window_recovers() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    // Post the cure window's own epochs (closing inside
    // [proposed_at, proposed_at + challenge_secs)) at or above
    // cure_threshold (9_800_000).
    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    for epoch in first_cure_epoch..last_cure_epoch {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    let now = record.proposed_at + challenge_secs;
    env.ledger()
        .set_timestamp(now.max(env.ledger().timestamp()) + SIGNAL_DISPUTE_SECS + 1);

    fx.client.finalize(&id);
    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.state, EventState::Cured);
}

/// Review item R1: withholding attack. One permanently missing epoch
/// in the cure window, every other epoch above `cure_threshold`,
/// must still route to Declared, never Cured.
#[test]
fn finalize_never_cures_with_one_permanently_missing_cure_window_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    for epoch in first_cure_epoch..last_cure_epoch {
        if epoch == first_cure_epoch {
            continue; // withheld: left permanently missing
        }
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    // Past WINDOW_SECS from the withheld epoch's own close: it is now
    // permanently missing, not merely not-ready.
    let withheld_close = (first_cure_epoch + 1) * EPOCH_SECS;
    env.ledger()
        .set_timestamp(withheld_close + WINDOW_SECS + SIGNAL_DISPUTE_SECS + 1);

    fx.client.finalize(&id);
    let record = fx.client.event(&id).unwrap();
    assert_eq!(
        record.state,
        EventState::Declared,
        "review item R1: a permanently missing cure-window epoch must never cure, \
         even when every other epoch recovered"
    );
}

/// Review item R2: a late-posted, still-Pending epoch in the cure
/// window is not ready, never missing.
#[test]
fn finalize_returns_data_not_final_for_a_late_posted_still_pending_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    let last_epoch = last_cure_epoch - 1;
    for epoch in first_cure_epoch..last_cure_epoch {
        if epoch == last_epoch {
            continue; // posted separately below, deliberately late
        }
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    // Post the LAST cure-window epoch at the very last legal moment
    // (just before its own backfill window closes), so it is still
    // genuinely Pending, not yet Final, when challenge_secs elapses.
    let late_post_time = (last_epoch + 1) * EPOCH_SECS + WINDOW_SECS;
    env.ledger().set_timestamp(late_post_time);
    fx.oracle.post_signals(
        &fx.keeper,
        &asset,
        &signal_set(&env, &fx.keeper, last_epoch, 9_900_000, 500_000_000_000),
    );

    env.ledger()
        .set_timestamp((record.proposed_at + challenge_secs).max(late_post_time + 1));
    let result = fx.client.try_finalize(&id);
    assert_eq!(
        result,
        Err(Ok(Error::DataNotFinal)),
        "review item R2: a genuinely posted, still-Pending epoch must never be \
         classified as permanently missing merely because the clock has moved \
         past its own close plus the backfill window"
    );

    // Once pending_until passes, finalize decides.
    env.ledger()
        .set_timestamp(late_post_time + SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize(&id);
    assert_eq!(fx.client.event(&id).unwrap().state, EventState::Cured);
}

/// Review item R3: a Disputed epoch in the cure window is not ready
/// until the dispute resolves or times out (ADR-010).
#[test]
fn finalize_returns_data_not_final_while_a_cure_window_epoch_is_disputed() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    let disputed_epoch = last_cure_epoch - 1;
    for epoch in first_cure_epoch..last_cure_epoch {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    // Dispute the last cure-window epoch while it is still Pending
    // (before its own pending_until), so it never becomes Final on
    // schedule.
    fx.oracle.dispute_signals(
        &Address::generate(&env),
        &asset,
        &disputed_epoch,
        &BytesN::from_array(&env, &[2u8; 32]),
    );

    env.ledger()
        .set_timestamp((record.proposed_at + challenge_secs).max(env.ledger().timestamp() + 1));
    assert_eq!(
        fx.client.try_finalize(&id),
        Err(Ok(Error::DataNotFinal)),
        "review item R3: a Disputed epoch is not ready until resolved or timed out"
    );

    // Time out the dispute (ADR-010's own silence-reads-as-keeper-wins
    // default); the epoch becomes Final and finalize can now decide.
    let now = env.ledger().timestamp();
    env.ledger()
        .set_timestamp(now + SIGNAL_DISPUTE_RULING_SECS + 1);
    fx.oracle
        .resolve_signal_dispute_timeout(&asset, &disputed_epoch);
    env.ledger().set_timestamp(env.ledger().timestamp() + 1);

    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Cured,
        "once the dispute times out the epoch is Final and above cure_threshold"
    );
}

/// PR #15 review, finding F4: the cure window is fully recovered and
/// Final. Separately, the newest epoch overall (posted after the
/// cure window, by a keeper who keeps posting) is disputed and
/// overturned, leaving its own ring POSITION `Empty`. `finalize`,
/// called right after the overturn, must still read the cure window
/// correctly and decide Cured, not misread every cure-window epoch
/// as missing because the newest ring position's own stored epoch
/// reset to 0.
#[test]
fn finalize_cures_correctly_even_when_the_newest_epoch_was_just_overturned() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    for epoch in first_cure_epoch..last_cure_epoch {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    // One more epoch, posted after the whole cure window is already
    // Final, then overturned: its own ring position is now Empty.
    // Never rewind the clock: advance to whichever is later, the
    // extra epoch's own close or where the clock already is.
    let extra_epoch = last_cure_epoch;
    let extra_close = (extra_epoch + 1) * EPOCH_SECS;
    env.ledger()
        .set_timestamp(extra_close.max(env.ledger().timestamp()));
    fx.oracle.post_signals(
        &fx.keeper,
        &asset,
        &signal_set(&env, &fx.keeper, extra_epoch, 9_900_000, 500_000_000_000),
    );
    let disputer = Address::generate(&env);
    fx.oracle.dispute_signals(
        &disputer,
        &asset,
        &extra_epoch,
        &BytesN::from_array(&env, &[4u8; 32]),
    );
    fx.oracle.resolve_signal_dispute(
        &asset,
        &extra_epoch,
        &false, // disputer wins: overturned, resets to Empty
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    assert!(fx
        .oracle
        .ring(&asset)
        .iter()
        .all(|slot| slot.epoch != extra_epoch));

    env.ledger()
        .set_timestamp((record.proposed_at + challenge_secs).max(env.ledger().timestamp() + 1));
    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Cured,
        "finding F4: an overturned newest epoch must not make finalize \
         misread every cure-window epoch as missing"
    );
}

/// PR #15 review, finding F4: the matching `cover_gate` case. With
/// the newest epoch overturned, `cover_gate` must still read the
/// real cure/depeg history correctly rather than reporting Clear
/// because `newest_epoch` regressed to `Some(0)`.
#[test]
fn cover_gate_reports_recent_depeg_even_when_the_newest_epoch_was_just_overturned() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);

    // One more epoch, posted after the failing window, then
    // overturned.
    let extra_epoch = 240u64;
    env.ledger().set_timestamp((extra_epoch + 1) * EPOCH_SECS);
    fx.oracle.post_signals(
        &fx.keeper,
        &asset,
        &signal_set(&env, &fx.keeper, extra_epoch, 9_900_000, 500_000_000_000),
    );
    let disputer = Address::generate(&env);
    fx.oracle.dispute_signals(
        &disputer,
        &asset,
        &extra_epoch,
        &BytesN::from_array(&env, &[4u8; 32]),
    );
    fx.oracle.resolve_signal_dispute(
        &asset,
        &extra_epoch,
        &false,
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    assert_eq!(fx.client.cover_gate(&asset), CoverGate::RecentDepeg);
}

// -- checkpoint_cure (PR #15 review, finding F2) --

/// Posts every epoch from `start_epoch` through `through_epoch`
/// (inclusive) continuously, `EPOCH_SECS` apart, each above
/// `cure_threshold`. Used to simulate keepers who never stop posting,
/// the scenario finding F2 is about: real keepers post every hour
/// regardless of whether anyone has called `finalize` yet.
fn post_continuous_healthy_run(
    env: &Env,
    oracle: &risk_oracle::RiskOracleClient,
    keeper: &Address,
    asset: &Address,
    start_epoch: u64,
    through_epoch: u64,
) {
    post_run(
        env,
        oracle,
        keeper,
        asset,
        start_epoch,
        through_epoch - start_epoch + 1,
        9_900_000,
        500_000_000_000,
    );
}

/// PR #15 review, finding F2: without checkpointing, a cured depeg
/// becomes Declared if `finalize` is called late enough that the
/// cure window's own epochs have rotated out of `RiskOracle`'s ring
/// (keepers post continuously; nothing stops them just because no one
/// has called `finalize` yet). Run against the code BEFORE this fix,
/// this scenario produces Declared; `checkpoint_cure`, called once
/// while the cure-window epochs are still live, must make `finalize`
/// still produce Cured even after the ring has long since rotated
/// past them.
#[test]
fn checkpoint_cure_makes_a_late_finalize_still_cure() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    let last_epoch_in_window = last_cure_epoch - 1;

    // Post every cure-window epoch except the very last one up front,
    // above cure_threshold.
    for epoch in first_cure_epoch..last_epoch_in_window {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    // The last cure-window epoch is posted late, disputed, and the
    // dispute times out (R3's own worst-case path), still above
    // cure_threshold.
    let late_post_time = (last_epoch_in_window + 1) * EPOCH_SECS + WINDOW_SECS;
    env.ledger().set_timestamp(late_post_time);
    fx.oracle.post_signals(
        &fx.keeper,
        &asset,
        &signal_set(
            &env,
            &fx.keeper,
            last_epoch_in_window,
            9_900_000,
            500_000_000_000,
        ),
    );
    let disputer = Address::generate(&env);
    fx.oracle.dispute_signals(
        &disputer,
        &asset,
        &last_epoch_in_window,
        &BytesN::from_array(&env, &[3u8; 32]),
    );
    env.ledger()
        .set_timestamp(late_post_time + SIGNAL_DISPUTE_RULING_SECS + 1);
    fx.oracle
        .resolve_signal_dispute_timeout(&asset, &last_epoch_in_window);

    // Checkpoint now, while every cure-window epoch is still live in
    // the ring (newest epoch so far is last_epoch_in_window, well
    // under RING_SLOTS positions old relative to itself).
    let progress = fx.client.checkpoint_cure(&id);
    assert!(!progress.any_missing);
    assert!(!progress.any_below_threshold);

    // Keepers keep posting long after, until the entire cure window
    // has rotated out of the ring (RING_SLOTS = 240 positions).
    let keep_posting_from = last_epoch_in_window + 1;
    let keep_posting_through = first_cure_epoch + 240 + 5;
    post_continuous_healthy_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        keep_posting_from,
        keep_posting_through,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Cured,
        "finding F2: a checkpointed cure must survive the cure window rotating \
         out of the ring, even though finalize is called long after"
    );
}

/// PR #15 review, finding F2: without ever calling `checkpoint_cure`,
/// a genuinely cured depeg becomes Declared once `finalize` is called
/// after the cure window has rotated out of the ring. Pins down the
/// documented "seller's responsibility" rule (design note deviation
/// list): nothing records cure-window progress unless something
/// calls `checkpoint_cure` while that data is still live.
#[test]
fn finalize_without_a_checkpoint_declares_once_the_cure_window_has_rotated_out() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    for epoch in first_cure_epoch..last_cure_epoch {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    // No checkpoint_cure call. Keepers keep posting until the cure
    // window has fully rotated out of the ring.
    let keep_posting_from = last_cure_epoch;
    let keep_posting_through = first_cure_epoch + 240 + 5;
    post_continuous_healthy_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        keep_posting_from,
        keep_posting_through,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Declared,
        "without a checkpoint, a rotated-out cure window reads as missing, \
         which is Declared under the strict cure rule (R1)"
    );
}

/// PR #15 review, finding F2: a recorded failure declares immediately,
/// even while a later, unrecorded epoch is still NotReady — one
/// failure already rules out a cure.
#[test]
fn checkpoint_cure_recorded_failure_short_circuits_an_unrecorded_not_ready_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    let last_epoch_in_window = last_cure_epoch - 1;

    // The first cure epoch posts BELOW cure_threshold: a recorded
    // failure. Every other epoch except the very last is posted
    // healthy; the last epoch is left unposted (still NotReady,
    // inside its own backfill window).
    for epoch in first_cure_epoch..last_epoch_in_window {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        let peg_ratio = if epoch == first_cure_epoch {
            9_000_000 // below cure_threshold (9_800_000)
        } else {
            9_900_000
        };
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, peg_ratio, 500_000_000_000),
        );
    }
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + SIGNAL_DISPUTE_SECS + 1);

    // The last cure epoch is still unposted and inside its own
    // backfill window: NotReady, not missing.
    let progress = fx.client.checkpoint_cure(&id);
    assert!(progress.any_below_threshold);
    assert!(!progress.any_missing);

    // finalize must declare right away: the already-recorded failure
    // settles the outcome, regardless of the still-NotReady epoch.
    env.ledger()
        .set_timestamp((record.proposed_at + challenge_secs).max(env.ledger().timestamp() + 1));
    fx.client.finalize(&id);
    assert_eq!(
        fx.client.event(&id).unwrap().state,
        EventState::Declared,
        "a recorded failure must declare immediately, not wait on a NotReady epoch"
    );
}

/// PR #15 review, finding F2: calling `checkpoint_cure` twice in a row
/// is a no-op the second time; the bitmap and flags are unchanged and
/// nothing is counted twice.
#[test]
fn checkpoint_cure_is_idempotent() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    let challenge_secs = 86_400u64;
    let first_cure_epoch = record.proposed_at / EPOCH_SECS;
    let last_cure_epoch = (record.proposed_at + challenge_secs) / EPOCH_SECS;
    for epoch in first_cure_epoch..last_cure_epoch {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + SIGNAL_DISPUTE_SECS + 1);

    let first = fx.client.checkpoint_cure(&id);
    let second = fx.client.checkpoint_cure(&id);
    assert_eq!(first, second);
}

/// PR #15 review, finding F2: `checkpoint_cure` is only valid for a
/// Proposed Depeg event; IssuerFreeze has no cure path at all.
#[test]
fn checkpoint_cure_rejects_issuer_freeze_and_a_non_proposed_event() {
    let env = Env::default();
    let fx = setup(&env);

    let freeze_asset = Address::generate(&env);
    let freeze_issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &freeze_asset,
        &freeze_issuer,
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
    ));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &freeze_asset));
    post_issuer_actions(&env, &fx.oracle, &fx.keeper, &freeze_asset, 0, 4);
    let freeze_id = fx.client.propose_tier1(
        &Address::generate(&env),
        &freeze_asset,
        &EventKind::IssuerFreeze,
        &1,
    );
    assert_eq!(
        fx.client.try_checkpoint_cure(&freeze_id),
        Err(Ok(Error::WrongState))
    );

    let depeg_asset = Address::generate(&env);
    let depeg_issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &depeg_asset,
        &depeg_issuer,
        IssuerFlags::default(),
    ));
    fx.client
        .register_definition(&depeg_definition(&env, &depeg_asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &depeg_asset);
    let depeg_id = fx.client.propose_tier1(
        &Address::generate(&env),
        &depeg_asset,
        &EventKind::Depeg,
        &1,
    );
    fx.client.challenge(
        &Address::generate(&env),
        &depeg_id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    assert_eq!(
        fx.client.try_checkpoint_cure(&depeg_id),
        Err(Ok(Error::WrongState))
    );
}

// -- rule --

#[test]
fn rule_declare_true_forfeits_the_challenger_bond_entirely() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let challenger = Address::generate(&env);
    fx.client
        .challenge(&challenger, &id, &BytesN::from_array(&env, &[1u8; 32]));

    fx.client
        .rule(&id, &true, &BytesN::from_array(&env, &[9u8; 32]));
    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.state, EventState::Declared);
    assert!(
        fx.staking_client
            .bond(&sylox_types::BondKey::EventChallenge(id))
            .is_none(),
        "forfeit_bond clears the bond record"
    );
}

#[test]
fn rule_declare_false_releases_the_challenger_bond_in_full() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let challenger = Address::generate(&env);
    fx.client
        .challenge(&challenger, &id, &BytesN::from_array(&env, &[1u8; 32]));

    fx.client
        .rule(&id, &false, &BytesN::from_array(&env, &[9u8; 32]));
    let record = fx.client.event(&id).unwrap();
    assert_eq!(record.state, EventState::Rejected);
    assert!(fx
        .staking_client
        .bond(&sylox_types::BondKey::EventChallenge(id))
        .is_none());
    assert!(!fx.client.in_progress(&asset));
}

/// Review item E4: two kinds live on the same asset (Depeg,
/// IssuerFreeze), each its own event sharing one `ActiveCount`.
/// Rejecting one must not clear the oracle's `event_in_progress` flag
/// while the other is still live.
#[test]
fn two_kinds_live_rejecting_one_leaves_event_in_progress_true() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(
        &env,
        &asset,
        &issuer,
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
    ));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    fx.client
        .register_definition(&issuer_freeze_definition(&env, &asset));

    // One combined 240-epoch run: a healthy baseline (0..168), then a
    // failing Depeg window (168..240) with an auth-revocation spike
    // at epoch 200 (inside IssuerFreeze's own last-168-epoch window
    // too), so both kinds trigger off the same posted data without
    // double-posting any epoch.
    for epoch in 0u64..240 {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        let peg_ratio = if epoch < 168 { 9_900_000 } else { 9_000_000 };
        let mut set = signal_set(&env, &fx.keeper, epoch, peg_ratio, 500_000_000_000);
        if epoch == 200 {
            set.issuer_actions = sylox_types::IssuerActions {
                clawbacks: 1,
                clawback_amount: 0,
                auth_revocations: 4,
                flag_changes: 0,
            };
        }
        fx.oracle.post_signals(&fx.keeper, &asset, &set);
    }
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let depeg_id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let freeze_id = fx.client.propose_tier1(
        &Address::generate(&env),
        &asset,
        &EventKind::IssuerFreeze,
        &1,
    );

    assert_eq!(fx.client.active_event_count(&asset), 2);
    assert!(fx.client.in_progress(&asset));
    assert!(fx.oracle.event_in_progress(&asset));

    // rule() requires Escalated; challenge the IssuerFreeze event
    // first, then have the committee reject it.
    fx.client.challenge(
        &Address::generate(&env),
        &freeze_id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    fx.client
        .rule(&freeze_id, &false, &BytesN::from_array(&env, &[9u8; 32]));

    assert_eq!(fx.client.active_event_count(&asset), 1);
    assert!(
        fx.client.in_progress(&asset),
        "the Depeg event is still live"
    );
    assert!(
        fx.oracle.event_in_progress(&asset),
        "event_in_progress must stay true while any kind is still live"
    );

    fx.client.challenge(
        &Address::generate(&env),
        &depeg_id,
        &BytesN::from_array(&env, &[2u8; 32]),
    );
    fx.client
        .rule(&depeg_id, &false, &BytesN::from_array(&env, &[9u8; 32]));
    assert!(!fx.client.in_progress(&asset));
    assert!(!fx.oracle.event_in_progress(&asset));
}

#[test]
fn rule_rejects_after_the_ruling_deadline() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );

    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + 1_209_600 + 1);
    let result = fx
        .client
        .try_rule(&id, &true, &BytesN::from_array(&env, &[9u8; 32]));
    assert_eq!(result, Err(Ok(Error::RulingDeadlinePassed)));
}

#[test]
fn rule_requires_committee_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );

    fx.client
        .rule(&id, &true, &BytesN::from_array(&env, &[9u8; 32]));
    assert_eq!(env.auths()[0].0, fx.committee);
}

// -- resolve_timeout --

#[test]
fn resolve_timeout_declares_tier1_and_refunds_the_bond_in_full() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );

    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + 1_209_600 + 1);
    fx.client.resolve_timeout(&id);

    let record = fx.client.event(&id).unwrap();
    assert_eq!(
        record.state,
        EventState::Declared,
        "ADR-002: escalated Tier 1 defaults to Declared on timeout"
    );
    assert!(
        fx.staking_client
            .bond(&sylox_types::BondKey::EventChallenge(id))
            .is_none(),
        "the bond is refunded (removed from the mock's own bookkeeping), not forfeited"
    );
    assert_eq!(fx.client.committee_misses(&fx.committee), 1);
}

#[test]
fn resolve_timeout_rejects_before_the_deadline() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );

    let result = fx.client.try_resolve_timeout(&id);
    assert_eq!(result, Err(Ok(Error::RulingDeadlineNotReached)));
}

// -- re-proposal after Cured/Rejected (review item D4) --

#[test]
fn re_proposal_on_unchanged_data_is_rejected() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    fx.client
        .rule(&id, &false, &BytesN::from_array(&env, &[9u8; 32]));

    // Immediately re-propose: no new epoch has become Final, so the
    // freshly computed window is identical.
    let result =
        fx.client
            .try_propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_eq!(result, Err(Ok(Error::EventInProgress)));
}

#[test]
fn re_proposal_once_window_start_moves_past_left_at_is_accepted() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    fx.client
        .rule(&id, &false, &BytesN::from_array(&env, &[9u8; 32]));

    // Post well more than 72 more failing epochs: the new 72 epoch
    // window's own start (which only advances epoch-for-epoch as new
    // epochs post) must move strictly past `left_at`'s own
    // WALL-CLOCK time, not just past the old window's last epoch
    // number, and `left_at` sits partway into the gap between epoch
    // 240's close and epoch 241's, not exactly on an epoch boundary.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        240,
        80,
        9_000_000,
        500_000_000_000,
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + SIGNAL_DISPUTE_SECS + 1);

    let new_id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    assert_ne!(new_id, id);
}

// -- reads --

#[test]
fn covers_checks_all_four_conditions() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    let id = fx
        .client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);
    let record = fx.client.event(&id).unwrap();

    assert!(fx
        .client
        .covers(&id, &1, &0, &(record.window_start + 259_200)));
    assert!(
        !fx.client
            .covers(&id, &2, &0, &(record.window_start + 259_200)),
        "wrong def_version"
    );
    assert!(
        !fx.client.covers(
            &id,
            &1,
            &(record.window_start + 1),
            &(record.window_start + 259_200)
        ),
        "window_start before series start"
    );
    assert!(
        !fx.client.covers(&id, &1, &0, &(record.window_start - 1)),
        "window_start after series expiry"
    );
}

#[test]
fn cover_gate_reports_event_in_progress_first() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    post_failing_depeg_window(&env, &fx.oracle, &fx.keeper, &asset);
    fx.client
        .propose_tier1(&Address::generate(&env), &asset, &EventKind::Depeg, &1);

    assert_eq!(fx.client.cover_gate(&asset), CoverGate::EventInProgress);
}

#[test]
fn cover_gate_reports_recent_depeg() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    // No definition registered: the Section 23 default window/threshold applies.
    post_run(
        &env,
        &fx.oracle,
        &fx.keeper,
        &asset,
        0,
        1,
        9_000_000,
        500_000_000_000,
    );
    assert_eq!(fx.client.cover_gate(&asset), CoverGate::RecentDepeg);
}

#[test]
fn cover_gate_clear_with_no_activity() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    assert_eq!(fx.client.cover_gate(&asset), CoverGate::Clear);
}

// -- Issue #12: upgrade / migrate / version --

#[test]
fn version_reads_contract_version_constant() {
    let env = Env::default();
    let fx = setup(&env);
    assert_eq!(fx.client.version(), crate::CONTRACT_VERSION);
}

/// Non-governor cannot upgrade: a native auth trap, demonstrated
/// directly with no auth mocking at all (a plain `Address::generate`
/// as governor here, not `setup`'s own `MockGovernor` contract - the
/// point is only that `require_auth` genuinely checks, same as every
/// other no-mock test in this workspace).
#[test]
#[should_panic]
fn upgrade_without_governor_auth_traps() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks.
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let staking = Address::generate(&env);
    let factory = Address::generate(&env);
    let usdc = Address::generate(&env);
    let contract_id = env.register(
        EventRegistry,
        (
            governor.clone(),
            oracle.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(&env, &contract_id);

    let new_wasm_hash = env.upload(EventRegistry);
    // No mock_all_auths(): the governor never actually signs this call.
    client.upgrade(&new_wasm_hash);
}

/// Upgrading to new code (a fresh wasm hash, uploaded via `env.upload`
/// per soroban-sdk's own documented pattern for exercising
/// `update_current_contract` in a unit test without a compiled .wasm
/// fixture) keeps all existing storage: a registered event definition
/// still reads back correctly through the same client and address
/// after it.
#[test]
fn upgrade_keeps_existing_storage() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));
    fx.client
        .register_definition(&depeg_definition(&env, &asset));
    let version_before = fx.client.current_version(&asset, &EventKind::Depeg);

    let new_wasm_hash = env.upload(EventRegistry);
    fx.client.upgrade(&new_wasm_hash);

    assert_eq!(
        fx.client.current_version(&asset, &EventKind::Depeg),
        version_before,
        "current_version(asset, kind) must read the same after an upgrade with no storage-shape change"
    );
    assert_eq!(
        fx.client.version(),
        crate::CONTRACT_VERSION,
        "the re-uploaded code is the same crate, so its own version() constant is unchanged"
    );
}

/// `migrate()` always targets this code's own `CONTRACT_VERSION` (no
/// caller-chosen argument - see `migrate`'s own doc comment for why),
/// and can't run twice.
#[test]
fn migrate_cannot_run_twice() {
    let env = Env::default();
    let fx = setup(&env);
    fx.client.migrate();
    let result = fx.client.try_migrate();
    assert_eq!(result, Err(Ok(Error::AlreadyMigrated)));
}

/// Non-governor cannot migrate: same native auth trap pattern as
/// `upgrade_without_governor_auth_traps`.
#[test]
#[should_panic]
fn migrate_without_governor_auth_traps() {
    let env = Env::default();
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let staking = Address::generate(&env);
    let factory = Address::generate(&env);
    let usdc = Address::generate(&env);
    let contract_id = env.register(
        EventRegistry,
        (
            governor.clone(),
            oracle.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.migrate();
}

/// markets design note Section 10, deviation 14: the real
/// `MarketFactory` address lands once, then is permanently locked.
/// (The end-to-end proof that a reachable factory address actually
/// gets queried is `register_definition_rejects_definition_in_use_
/// when_a_live_series_pins_it`, against `setup()`'s own factory; this
/// test is specifically about the one-time lock.)
#[test]
fn set_factory_succeeds_once_then_locks() {
    let env = Env::default();
    let fx = setup(&env);

    fx.client.set_factory(&Address::generate(&env));

    let result = fx.client.try_set_factory(&Address::generate(&env));
    assert_eq!(result, Err(Ok(Error::FactoryAlreadySet)));
}

#[test]
#[should_panic]
fn set_factory_without_governor_auth_traps() {
    let env = Env::default();
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let staking = Address::generate(&env);
    let factory = Address::generate(&env);
    let usdc = Address::generate(&env);
    let contract_id = env.register(
        EventRegistry,
        (
            governor.clone(),
            oracle.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.set_factory(&Address::generate(&env));
}

/// `set_governor` works exactly once: the current governor hands off
/// to a new address, and a governor-gated call afterward still
/// succeeds (through `setup()`'s own `mock_all_auths()`, which
/// approves every `require_auth()` regardless of address; the real
/// proof the new address is the one actually recorded is
/// `set_governor_old_governor_rejected_new_governor_accepted` below).
#[test]
fn set_governor_succeeds_once() {
    let env = Env::default();
    let fx = setup(&env);
    let new_governor = Address::generate(&env);

    fx.client.set_governor(&new_governor);

    fx.client.set_factory(&Address::generate(&env));
}

/// `set_governor` cannot run twice: `GovernorAlreadySet` on the
/// second call, confirmed via `try_set_governor` since this is a real
/// returned error, not a native auth trap. Same one-time-lock shape
/// as `set_factory_succeeds_once_then_locks` above.
#[test]
fn set_governor_fails_the_second_call() {
    let env = Env::default();
    let fx = setup(&env);

    fx.client.set_governor(&Address::generate(&env));
    let result = fx.client.try_set_governor(&Address::generate(&env));

    assert_eq!(result, Err(Ok(Error::GovernorAlreadySet)));
}

/// Non-governor cannot call `set_governor`: same native auth trap
/// pattern as `set_factory_without_governor_auth_traps`.
#[test]
#[should_panic]
fn set_governor_without_governor_auth_traps() {
    let env = Env::default();
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let staking = Address::generate(&env);
    let factory = Address::generate(&env);
    let usdc = Address::generate(&env);
    let contract_id = env.register(
        EventRegistry,
        (
            governor.clone(),
            oracle.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(&env, &contract_id);

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
    let staking = Address::generate(&env);
    let factory = Address::generate(&env);
    let usdc = Address::generate(&env);
    let contract_id = env.register(
        EventRegistry,
        (
            old_governor.clone(),
            oracle.clone(),
            staking.clone(),
            factory.clone(),
            usdc.clone(),
        ),
    );
    let client = EventRegistryClient::new(&env, &contract_id);

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

    let new_factory = Address::generate(&env);

    // The old governor is no longer the one recorded: a call it signs
    // for still traps natively, exactly like any other non-governor
    // caller.
    let old_governor_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &old_governor,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_factory",
                args: (new_factory.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_factory(&new_factory);
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
            fn_name: "set_factory",
            args: (new_factory.clone(),).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_factory(&new_factory);
}

/// There is no longer a separate `initialize` call to invoke twice
/// against one address - `register`'s own constructor runs exactly
/// once, atomically, when the contract is deployed, and no Soroban
/// deployment path can target an address that already has code at
/// it. The closest thing to "initialize a second time" that still
/// exists is `Env::register_at`, a testutils-only helper whose own
/// doc comment says plainly that it is not reproducible on-chain:
/// "Registering a contract that is already registered replaces it...
/// the new contract's constructor will be called again during
/// re-registration. That behavior only exists for this test utility
/// and is not reproducible on-chain, where contract Wasm updates
/// don't cause constructor to be called."
///
/// It is NOT a clean redeploy either, which is what makes it
/// dangerous as a test shortcut and exactly why it has no on-chain
/// equivalent: re-running the constructor only overwrites *instance*
/// storage (`storage::Config`, written by `__constructor`), while
/// *persistent* storage (every registered `EventDefinition`, written
/// by `register_definition`) is untouched - confirmed against
/// `soroban-env-host`'s own
/// `register_test_contract_with_constructor` (`host/lifecycle.rs`),
/// which calls `store_contract_instance` and then the constructor,
/// with no step that clears persistent or temporary entries for the
/// address. A real chain has no path that produces this mixed state:
/// a freshly deployed address starts with nothing at all, and an
/// existing address can never run a constructor a second time. This
/// test proves both halves: the config really does change (the new
/// governor's oracle is wired in, not the old one), and the old
/// data really does survive (the definition registered under the
/// first construction is still there).
#[test]
fn cannot_initialize_a_second_time() {
    let env = Env::default();
    let fx = setup(&env);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &issuer, IssuerFlags::default()));

    let version = fx
        .client
        .register_definition(&depeg_definition(&env, &asset));
    assert_eq!(version, 1);
    assert_eq!(fx.client.current_version(&asset, &EventKind::Depeg), 1);

    // A second, real RiskOracle that has never heard of `asset` -
    // standing in for "a different deployment's own dependencies",
    // the same role `fx.oracle_id` plays for the first construction.
    let other_registry_stand_in = Address::generate(&env);
    let other_staking_stand_in = Address::generate(&env);
    let other_oracle_id = env.register(
        risk_oracle::RiskOracle,
        (
            fx.governor_contract.clone(),
            other_registry_stand_in,
            other_staking_stand_in,
        ),
    );

    // Re-run `__constructor` at the SAME address, via the testutils-
    // only `register_at` (see doc comment above) - the only way this
    // SDK can simulate a second construction at all - wiring in the
    // new oracle instead of `fx.oracle_id`.
    env.register_at(
        &fx.contract_id,
        EventRegistry,
        (
            fx.governor_contract.clone(),
            other_oracle_id,
            fx.staking.clone(),
            fx.factory.clone(),
            fx.usdc.clone(),
        ),
    );

    // The config really changed: `register_definition` now consults
    // the NEW oracle, which has no `asset_config` for `asset` at all,
    // so even re-registering the exact same definition fails. If the
    // re-run had been a no-op, or had somehow kept talking to the
    // first oracle, this would still succeed.
    let result = fx
        .client
        .try_register_definition(&depeg_definition(&env, &asset));
    assert_eq!(result, Err(Ok(Error::InvalidDefinition)));

    // The old data was NOT wiped: the definition registered under
    // the first construction, before the re-run, still reads back
    // exactly as it did before - proving this is a mutation of a
    // live instance's config, not a fresh start.
    assert_eq!(
        fx.client.current_version(&asset, &EventKind::Depeg),
        version,
        "persistent storage must survive register_at re-running the \
         constructor; only instance storage (the Config it writes) \
         is touched"
    );
}
