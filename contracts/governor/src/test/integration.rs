//! docs/design/governor.md Section 13, integration: every Section 4.3
//! dispatch row executes against the real targets (rejected at
//! `eta - 1`, accepted at `eta`), upgrades and their migration (D13),
//! the Governor's own self-upgrade (D14), G1 (no governor-gated call
//! reaches a target except through `execute`), and the issue #22
//! committee paths, including D16's miss attribution after a rotation.
//!
//! Setup steps that are not themselves under test (registering an
//! asset to post signals against, staking a keeper) call the targets
//! directly under `mock_all_auths`, which also mocks the Governor
//! contract's own authorization; every assertion about what the
//! Governor does goes through `queue`/`approve`/`execute`.

extern crate std;

use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{
    contract, contractimpl, symbol_short, vec, Address, BytesN, Env, IntoVal, Map, Symbol, Val, Vec,
};
use sylox_types::{
    Action, ActionState, AssetConfig, EndpointStatus, EventDefinition, EventKind, EventState,
    IssuerActions, IssuerFlags, Reference, SeriesTerms, SignalSet, TreasuryBucket,
};

use super::{now, queue_and_approve, set_time, setup, Fixture, DAY, START};
use crate::{Error, Governor, LIQUIDITY_COVER_RATIO};

pub(crate) const EPOCH_SECS: u64 = 3_600;
/// Mirrors `Staking`'s private `params::KEEPER_BOND` (not exported),
/// as every other cross-crate integration test here does.
pub(crate) const KEEPER_BOND: i128 = 50_000_000_000;
/// `RiskOracle`'s private `signal_dispute_bond()` and
/// `SIGNAL_DISPUTE_RULING_SECS`.
const SIGNAL_DISPUTE_BOND: i128 = 10_000_000_000;
const SIGNAL_DISPUTE_RULING_SECS: u64 = 518_400;
const CHALLENGE_BOND: i128 = 10_000_000_000;

pub(crate) fn asset_config(env: &Env, asset: &Address, issuer: &Address) -> AssetConfig {
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

pub(crate) fn depeg_definition(asset: &Address) -> EventDefinition {
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

/// Posts epochs `start..start + count`, each at its own close.
pub(crate) fn post_run(
    env: &Env,
    fx: &Fixture,
    keeper: &Address,
    asset: &Address,
    start: u64,
    count: u64,
    peg_ratio: i128,
) {
    for epoch in start..start + count {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle
            .post_signals(keeper, asset, &signal_set(env, keeper, epoch, peg_ratio));
    }
}

/// A registered, enabled asset with its canonical Depeg definition, its
/// related seller list, and a bonded keeper ready to post. Direct,
/// mocked calls: none of this is under test here.
pub(crate) fn listed_asset(env: &Env, fx: &Fixture) -> (Address, Address) {
    let issuer = Address::generate(env);
    let asset = env
        .register_stellar_asset_contract_v2(issuer.clone())
        .address();
    fx.oracle.add_asset(&asset_config(env, &asset, &issuer));
    fx.registry.register_definition(&depeg_definition(&asset));
    fx.factory
        .set_related_sellers(&asset, &vec![env, issuer.clone()]);
    let keeper = Address::generate(env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin.mint(&keeper, &KEEPER_BOND);
    fx.staking.stake(&keeper, &KEEPER_BOND);
    (asset, keeper)
}

pub(crate) fn series_terms(env: &Env, fx: &Fixture, asset: &Address, start: u64) -> SeriesTerms {
    let mut def_versions = Map::new(env);
    def_versions.set(EventKind::Depeg, 1u32);
    SeriesTerms {
        asset: asset.clone(),
        def_versions,
        settlement: fx.usdc.clone(),
        start,
        expiry: start + 90 * DAY,
        claim_window_secs: 30 * DAY,
        cap: 10_000_000_000_000,
        max_cover_per_buyer: 10_000_000_000_000,
        require_holding: false,
        fee_bps: 750,
    }
}

/// Queues and approves, checks `execute` is rejected at `eta - 1`
/// (when the timelock is not zero), then executes at exactly `eta`.
fn pass_checked(env: &Env, fx: &Fixture, action: &Action) -> u64 {
    let id = queue_and_approve(fx, action);
    let eta = fx.gov.action(&id).unwrap().eta;
    if eta > now(env) {
        set_time(env, eta - 1);
        assert_eq!(
            fx.gov.try_execute(&id),
            Err(Ok(Error::TimelockActive)),
            "{action:?} at eta - 1"
        );
    }
    set_time(env, eta);
    fx.gov.execute(&id);
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Executed);
    id
}

fn migrated_to(env: &Env, contract: &Address) -> u32 {
    env.as_contract(contract, || {
        env.storage()
            .instance()
            .get(&symbol_short!("MIGRATE"))
            .unwrap_or(0)
    })
}

// -- every dispatch row (Section 4.3) --

#[test]
fn risk_oracle_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let issuer = Address::generate(&env);
    let asset = Address::generate(&env);

    pass_checked(
        &env,
        &fx,
        &Action::AddAsset(asset_config(&env, &asset, &issuer)),
    );
    assert!(fx.oracle.asset_config(&asset).unwrap().enabled);

    let mut updated = asset_config(&env, &asset, &issuer);
    updated.min_liquidity = 7;
    pass_checked(&env, &fx, &Action::UpdateAsset(asset.clone(), updated));
    assert_eq!(fx.oracle.asset_config(&asset).unwrap().min_liquidity, 7);

    pass_checked(&env, &fx, &Action::SetSubEpochSecs(asset.clone(), 600));
    assert_eq!(
        fx.oracle
            .sub_epoch_config(&asset)
            .unwrap()
            .pending_sub_epoch_secs,
        Some(600)
    );

    // A target error reverts the whole execute and leaves the action
    // Approved (Section 4.2): weights must sum to 10,000.
    let bad = queue_and_approve(
        &fx,
        &Action::SetFormula(2, vec![&env, 1u32], Map::new(&env)),
    );
    set_time(&env, fx.gov.action(&bad).unwrap().eta);
    assert!(fx.gov.try_execute(&bad).is_err());
    assert_eq!(fx.gov.action(&bad).unwrap().state, ActionState::Approved);
    pass_checked(
        &env,
        &fx,
        &Action::SetFormula(
            2,
            vec![&env, 3_500u32, 2_000, 1_500, 1_500, 1_000, 500],
            Map::new(&env),
        ),
    );

    pass_checked(&env, &fx, &Action::DisableAsset(asset.clone()));
    assert!(!fx.oracle.asset_config(&asset).unwrap().enabled);
}

#[test]
fn registry_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let issuer = Address::generate(&env);
    let asset = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(&env, &asset, &issuer));

    pass_checked(
        &env,
        &fx,
        &Action::RegisterDefinition(depeg_definition(&asset)),
    );
    assert_eq!(fx.registry.current_version(&asset, &EventKind::Depeg), 1);

    pass_checked(&env, &fx, &Action::SetFactory(fx.factory.address.clone()));
    // `set_factory` locks after its first call: a second one reaches
    // the registry and is rejected there.
    let again = queue_and_approve(&fx, &Action::SetFactory(Address::generate(&env)));
    set_time(&env, fx.gov.action(&again).unwrap().eta);
    assert!(fx.gov.try_execute(&again).is_err());
    assert_eq!(fx.gov.action(&again).unwrap().state, ActionState::Approved);
}

#[test]
fn staking_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    let reporter = Address::generate(&env);

    pass_checked(&env, &fx, &Action::AddKeeper(keeper.clone()));
    assert!(fx.staking.keeper(&keeper).is_some());
    pass_checked(
        &env,
        &fx,
        &Action::AddReporter(reporter.clone(), symbol_short!("eu")),
    );
    assert_eq!(
        fx.staking.reporter(&reporter).unwrap().region,
        symbol_short!("eu")
    );

    pass_checked(&env, &fx, &Action::RemoveKeeper(keeper.clone()));
    assert!(fx.staking.keeper(&keeper).unwrap().removed_at.is_some());
    pass_checked(&env, &fx, &Action::RemoveReporter(reporter.clone()));
    assert!(fx.staking.reporter(&reporter).unwrap().removed_at.is_some());
}

#[test]
fn treasury_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let depositor = Address::generate(&env);
    fx.usdc_admin.mint(&depositor, &1_000);
    fx.treasury
        .deposit(&depositor, &TreasuryBucket::Fees, &1_000);

    pass_checked(
        &env,
        &fx,
        &Action::TreasuryAllocate(TreasuryBucket::Fees, TreasuryBucket::KeeperRewards, 400),
    );
    assert_eq!(fx.treasury.balance(&TreasuryBucket::Fees), 600);
    assert_eq!(fx.treasury.balance(&TreasuryBucket::KeeperRewards), 400);

    let to = Address::generate(&env);
    pass_checked(
        &env,
        &fx,
        &Action::TreasurySpend(TreasuryBucket::Fees, to.clone(), 100),
    );
    assert_eq!(fx.usdc_client.balance(&to), 100);
    assert_eq!(fx.treasury.balance(&TreasuryBucket::Fees), 500);
    // G8: the Governor itself never holds funds.
    assert_eq!(fx.usdc_client.balance(&fx.gov.address), 0);
}

#[test]
fn factory_list_and_guard_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let list = vec![&env, Address::generate(&env), Address::generate(&env)];

    pass_checked(
        &env,
        &fx,
        &Action::SetRelatedSellers(asset.clone(), list.clone()),
    );
    assert_eq!(fx.factory.related_sellers(&asset), Some(list));

    let guard = Address::generate(&env);
    pass_checked(
        &env,
        &fx,
        &Action::SetPriceGuard(asset.clone(), Some(guard.clone())),
    );
    assert_eq!(fx.factory.price_guard(&asset), Some(guard));
    pass_checked(&env, &fx, &Action::SetPriceGuard(asset.clone(), None));
    assert_eq!(fx.factory.price_guard(&asset), None);
}

#[test]
fn governor_rows_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let committee = Address::generate(&env);
    pass_checked(&env, &fx, &Action::SetCommittee(committee.clone()));
    assert_eq!(fx.gov.committee(), committee);

    let guardian = Address::generate(&env);
    pass_checked(&env, &fx, &Action::SetGuardian(guardian.clone()));
    fx.gov.pause(&sylox_types::PauseScope::NewCover);
    assert_eq!(env.auths()[0].0, guardian);

    pass_checked(
        &env,
        &fx,
        &Action::Unpause(sylox_types::PauseScope::NewCover),
    );
    assert!(!fx.gov.paused(&sylox_types::PauseScope::NewCover));
    pass_checked(&env, &fx, &Action::Pause(sylox_types::PauseScope::Signals));
    assert!(fx.gov.paused(&sylox_types::PauseScope::Signals));

    let newcomer = Address::generate(&env);
    pass_checked(
        &env,
        &fx,
        &Action::SetSigners(vec![&env, fx.signers[0].clone(), newcomer.clone()], 2),
    );
    // The old signer 1 can no longer queue; the newcomer can.
    assert_eq!(
        fx.gov
            .try_queue(&fx.signers[1], &Action::SetCommittee(committee.clone())),
        Err(Ok(Error::NotSigner))
    );
    fx.gov.queue(&newcomer, &Action::SetCommittee(committee));
}

/// `SetParam` (D11), `SetHardCap` and `OpenSeries` against a real
/// asset with 168 epochs of history, so `cover_cap` and `open_series`
/// read real data. `SetParam` and `SetHardCap` are queued first (7
/// days); `OpenSeries` 2 days before they all come due; the keeper
/// keeps posting through the wait so the asset never goes stale.
#[test]
fn market_settings_and_open_series_execute_through_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let base = START / EPOCH_SECS + 1;
    set_time(&env, base * EPOCH_SECS);

    let key = Symbol::new(&env, LIQUIDITY_COVER_RATIO);
    let set_ratio = queue_and_approve(&fx, &Action::SetParam(key.clone(), 5_000_000));
    let set_cap = queue_and_approve(&fx, &Action::SetHardCap(asset.clone(), 777));

    post_run(&env, &fx, &keeper, &asset, base, 120, 9_900_000);
    let series_start = (base + 168) * EPOCH_SECS;
    let open = queue_and_approve(
        &fx,
        &Action::OpenSeries(series_terms(&env, &fx, &asset, series_start)),
    );
    post_run(&env, &fx, &keeper, &asset, base + 120, 48, 9_900_000);
    assert_eq!(now(&env), series_start);
    assert_eq!(fx.gov.action(&set_ratio).unwrap().eta, series_start);

    let default_cap = fx.factory.cover_cap(&asset);
    assert!(default_cap > 0);
    fx.gov.execute(&set_ratio);
    assert_eq!(fx.gov.param(&key), Some(5_000_000));
    assert_eq!(
        fx.factory.cover_cap(&asset),
        default_cap * 2,
        "liquidity_cover_ratio 0.25 -> 0.5"
    );
    fx.gov.execute(&set_cap);
    assert_eq!(fx.factory.cover_cap(&asset), 777);

    assert!(fx.factory.series_for(&asset).is_empty());
    fx.gov.execute(&open);
    assert_eq!(fx.factory.series_for(&asset).len(), 1);
}

/// Stands in for `Series` so `SetSeriesWasm` is observable: the next
/// `open_series` deploys this, and `marker()` proves it.
#[contract]
pub struct MarkerSeries;

#[contractimpl]
impl MarkerSeries {
    pub fn __constructor(
        _env: Env,
        _terms: SeriesTerms,
        _factory: Address,
        _oracle: Address,
        _registry: Address,
        _treasury: Address,
    ) {
    }

    pub fn marker(_env: Env) -> u32 {
        42
    }
}

#[test]
fn set_series_wasm_changes_what_open_series_deploys() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let base = START / EPOCH_SECS + 1;
    set_time(&env, base * EPOCH_SECS);

    let marker_wasm = env.upload(MarkerSeries);
    let set_wasm = queue_and_approve(&fx, &Action::SetSeriesWasm(marker_wasm));
    post_run(&env, &fx, &keeper, &asset, base, 12 * 24, 9_900_000);
    let series_start = (base + 14 * 24) * EPOCH_SECS;
    let open = queue_and_approve(
        &fx,
        &Action::OpenSeries(series_terms(&env, &fx, &asset, series_start)),
    );
    post_run(
        &env,
        &fx,
        &keeper,
        &asset,
        base + 12 * 24,
        2 * 24,
        9_900_000,
    );
    assert_eq!(now(&env), series_start);

    fx.gov.execute(&set_wasm);
    fx.gov.execute(&open);
    let deployed = fx.factory.series_for(&asset).get(0).unwrap();
    assert_eq!(MarkerSeriesClient::new(&env, &deployed).marker(), 42);
}

// -- upgrades (Section 6, D13, D14) --

#[test]
fn each_target_upgrades_after_14_days_and_migrates_with_the_new_code() {
    let env = Env::default();
    let fx = setup(&env);
    let targets: [(Address, BytesN<32>); 5] = [
        (
            fx.oracle.address.clone(),
            env.upload(risk_oracle::RiskOracle),
        ),
        (
            fx.registry.address.clone(),
            env.upload(event_registry::EventRegistry),
        ),
        (fx.staking.address.clone(), env.upload(staking::Staking)),
        (fx.treasury.address.clone(), env.upload(treasury::Treasury)),
        (
            fx.factory.address.clone(),
            env.upload(market_factory::MarketFactory),
        ),
    ];
    for (target, hash) in targets {
        assert_eq!(migrated_to(&env, &target), 0);
        pass_checked(&env, &fx, &Action::Upgrade(target.clone(), hash.clone()));
        let version: u32 = env.invoke_contract(&target, &symbol_short!("version"), Vec::new(&env));
        assert_eq!(
            migrated_to(&env, &target),
            version,
            "migrate ran in the same execute, with the new code's version"
        );

        // The same version again: the target's own AlreadyMigrated is
        // accepted, the upgrade still lands.
        pass_checked(&env, &fx, &Action::Upgrade(target.clone(), hash));
        assert_eq!(migrated_to(&env, &target), version);
    }
}

/// Section 6.1: Staking and Treasury hold funds; their upgrade waits 14
/// days and keeps I14 and I15.
#[test]
fn staking_and_treasury_upgrades_keep_their_funds() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin.mint(&keeper, &KEEPER_BOND);
    fx.staking.stake(&keeper, &KEEPER_BOND);
    let depositor = Address::generate(&env);
    fx.usdc_admin.mint(&depositor, &5_000);
    fx.treasury
        .deposit(&depositor, &TreasuryBucket::Fees, &5_000);

    pass_checked(
        &env,
        &fx,
        &Action::Upgrade(fx.staking.address.clone(), env.upload(staking::Staking)),
    );
    pass_checked(
        &env,
        &fx,
        &Action::Upgrade(fx.treasury.address.clone(), env.upload(treasury::Treasury)),
    );

    // I14: Staking's balance is exactly the keeper's bond.
    assert_eq!(fx.usdc_client.balance(&fx.staking.address), KEEPER_BOND);
    assert_eq!(fx.staking.keeper(&keeper).unwrap().bond, KEEPER_BOND);
    // I15: Treasury's balance covers its buckets.
    assert_eq!(fx.treasury.balance(&TreasuryBucket::Fees), 5_000);
    assert!(fx.usdc_client.balance(&fx.treasury.address) >= 5_000);
}

#[test]
fn the_governor_upgrades_itself_and_keeps_working() {
    let env = Env::default();
    let fx = setup(&env);
    let pending = fx.gov.queue(
        &fx.signers[0],
        &Action::Pause(sylox_types::PauseScope::Deposits),
    );

    let hash = env.upload(Governor);
    let id = queue_and_approve(&fx, &Action::Upgrade(fx.gov.address.clone(), hash));
    let eta = fx.gov.action(&id).unwrap().eta;
    set_time(&env, eta - 1);
    assert_eq!(fx.gov.try_execute(&id), Err(Ok(Error::TimelockActive)));
    set_time(&env, eta);
    fx.gov.execute(&id);
    // `upgraded` (the shared sylox_types event) plus `action_executed`;
    // any other execute emits only the latter.
    let upgraded = env
        .events()
        .all()
        .filter_by_contract(&fx.gov.address)
        .events()
        .len()
        == 2;
    assert!(upgraded, "Upgraded emitted by the Governor itself");
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Executed);

    // Stored data decodes under the new code, and it still executes.
    assert_eq!(fx.gov.committee(), fx.committee);
    assert_eq!(fx.gov.action(&pending).unwrap().state, ActionState::Queued);
    let committee = Address::generate(&env);
    pass_checked(&env, &fx, &Action::SetCommittee(committee.clone()));
    assert_eq!(fx.gov.committee(), committee);
}

// -- G1: no governor-gated call reaches a target except through execute --

fn direct_call_traps(env: &Env, who: &Address, contract: &Address, fn_name: &str, args: Vec<Val>) {
    env.mock_auths(&[MockAuth {
        address: who,
        invoke: &MockAuthInvoke {
            contract,
            fn_name,
            args: args.clone(),
            sub_invokes: &[],
        },
    }]);
    let result = env.try_invoke_contract::<Val, soroban_sdk::Error>(
        contract,
        &Symbol::new(env, fn_name),
        args,
    );
    assert!(
        result.is_err(),
        "{fn_name} on a target must not accept a direct call"
    );
}

#[test]
fn signers_guardian_and_committee_cannot_call_a_target_directly() {
    let env = Env::default();
    let fx = setup(&env);
    let x = Address::generate(&env);
    for who in [
        fx.signers[0].clone(),
        fx.guardian.clone(),
        fx.committee.clone(),
    ] {
        direct_call_traps(
            &env,
            &who,
            &fx.staking.address,
            "add_keeper",
            vec![&env, x.into_val(&env)],
        );
        direct_call_traps(
            &env,
            &who,
            &fx.treasury.address,
            "spend",
            vec![
                &env,
                TreasuryBucket::Fees.into_val(&env),
                x.into_val(&env),
                1i128.into_val(&env),
            ],
        );
        direct_call_traps(
            &env,
            &who,
            &fx.oracle.address,
            "disable_asset",
            vec![&env, x.into_val(&env)],
        );
        direct_call_traps(
            &env,
            &who,
            &fx.factory.address,
            "set_hard_cap",
            vec![&env, x.into_val(&env), 1i128.into_val(&env)],
        );
        direct_call_traps(
            &env,
            &who,
            &fx.registry.address,
            "set_factory",
            vec![&env, x.into_val(&env)],
        );
        direct_call_traps(
            &env,
            &who,
            &fx.staking.address,
            "upgrade",
            vec![&env, BytesN::from_array(&env, &[1u8; 32]).into_val(&env)],
        );
    }
}

// -- the issue #22 committee paths (Section 7, D16) --

#[test]
fn signal_dispute_resolution_reads_the_committee_from_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let epoch = START / EPOCH_SECS + 1;
    post_run(&env, &fx, &keeper, &asset, epoch, 2, 9_900_000);

    let disputer = Address::generate(&env);
    fx.usdc_admin.mint(&disputer, &(2 * SIGNAL_DISPUTE_BOND));
    let alt = BytesN::from_array(&env, &[9u8; 32]);
    fx.oracle.dispute_signals(&disputer, &asset, &epoch, &alt);
    fx.oracle
        .resolve_signal_dispute(&asset, &epoch, &true, &alt);
    assert_eq!(env.auths()[0].0, fx.committee, "the Governor's committee");
    assert!(fx.oracle.is_final(&asset, &epoch));

    // The timeout path also reads committee() to record the miss.
    fx.oracle
        .dispute_signals(&disputer, &asset, &(epoch + 1), &alt);
    set_time(&env, now(&env) + SIGNAL_DISPUTE_RULING_SECS + 1);
    fx.oracle
        .resolve_signal_dispute_timeout(&asset, &(epoch + 1));
    assert!(fx.oracle.is_final(&asset, &(epoch + 1)));
}

/// A Depeg event on `asset`, challenged and so Escalated. Returns the
/// event id.
fn escalated_event(env: &Env, fx: &Fixture, asset: &Address, keeper: &Address, base: u64) -> u64 {
    post_run(env, fx, keeper, asset, base, 168, 9_900_000);
    post_run(env, fx, keeper, asset, base + 168, 72, 9_000_000);
    set_time(env, now(env) + 7_200 + 1);
    let id = fx
        .registry
        .propose_tier1(&Address::generate(env), asset, &EventKind::Depeg, &1);
    let challenger = Address::generate(env);
    fx.usdc_admin.mint(&challenger, &CHALLENGE_BOND);
    fx.registry
        .challenge(&challenger, &id, &BytesN::from_array(env, &[1u8; 32]));
    assert_eq!(fx.registry.event(&id).unwrap().state, EventState::Escalated);
    id
}

#[test]
fn rule_reads_the_committee_from_the_governor() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let id = escalated_event(&env, &fx, &asset, &keeper, START / EPOCH_SECS + 1);

    fx.registry
        .rule(&id, &true, &BytesN::from_array(&env, &[2u8; 32]));
    assert_eq!(env.auths()[0].0, fx.committee);
    assert_eq!(fx.registry.event(&id).unwrap().state, EventState::Declared);
}

/// D16: a ruling deadline that passes after `SetCommittee` executes is
/// recorded as a miss by the NEW committee.
#[test]
fn a_timeout_after_a_rotation_records_the_miss_against_the_new_committee() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let id = escalated_event(&env, &fx, &asset, &keeper, START / EPOCH_SECS + 1);
    let deadline = fx.registry.ruling_deadline(&id).unwrap();

    let next = Address::generate(&env);
    let rotate = queue_and_approve(&fx, &Action::SetCommittee(next.clone()));
    set_time(&env, deadline.max(fx.gov.action(&rotate).unwrap().eta));
    fx.gov.execute(&rotate);

    fx.registry.resolve_timeout(&id);
    assert_eq!(fx.registry.committee_misses(&next), 1);
    assert_eq!(fx.registry.committee_misses(&fx.committee), 0);
    assert_eq!(
        fx.registry.event(&id).unwrap().state,
        EventState::Declared,
        "ADR-002: an escalated Tier 1 Depeg defaults to Declared"
    );
}
