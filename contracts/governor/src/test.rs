//! Governor tests. docs/design/governor.md Section 13.
//!
//! The fixture below wires the real `Governor` to the five real targets
//! (`RiskOracle`, `EventRegistry`, `Staking`, `Treasury`,
//! `MarketFactory`, the last deploying the real `Series` Wasm), each
//! constructed with the Governor's own address as its `governor`, the
//! way the handover in the design note's Section 10 leaves them. Every
//! address is generated up front, the same technique
//! `event_registry::test::integration` uses for the constructor cycle.
//!
//! This file: the Governor's own rules (lifecycle, errors, cancel,
//! pause, params, self-migration, TTL). `test/integration.rs`: every
//! dispatch row against the real targets, upgrades, G1, the issue #22
//! committee paths. `test/property.rs`: G2, G3, G9 under random
//! sequences. `budget_test.rs`: `execute(OpenSeries)` and
//! `execute(Upgrade)` against the network limits.

extern crate std;

use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{symbol_short, token, vec, Address, BytesN, Env, IntoVal, Map, Symbol, Vec};
use sylox_types::ttl::{INSTANCE_TTL_EXTEND_TO, PERSISTENT_TTL_EXTEND_TO};
use sylox_types::{Action, ActionState, PauseScope, TreasuryBucket};

use crate::storage::{DataKey, MIGRATION_VERSION_KEY};
use crate::{
    timelock, Error, Governor, GovernorClient, Targets, CONTRACT_VERSION, GRACE_SECS,
    LIQUIDITY_COVER_RATIO, MAX_PAUSE_SECS,
};

pub(crate) mod integration;
mod property;

pub(crate) const DAY: u64 = 86_400;
/// Not 0, so nothing here accidentally only exercises the small-number
/// edge of a timestamp check.
pub(crate) const START: u64 = 1_000_000;

pub(crate) struct Fixture<'a> {
    pub gov: GovernorClient<'a>,
    /// 2 of 3, the testnet shape (D26).
    pub signers: [Address; 3],
    pub committee: Address,
    pub guardian: Address,
    pub oracle: risk_oracle::RiskOracleClient<'a>,
    pub registry: event_registry::EventRegistryClient<'a>,
    pub staking: staking::StakingClient<'a>,
    pub treasury: treasury::TreasuryClient<'a>,
    pub factory: market_factory::MarketFactoryClient<'a>,
    pub usdc: Address,
    pub usdc_client: token::TokenClient<'a>,
    pub usdc_admin: token::StellarAssetClient<'a>,
}

pub(crate) fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let sac_admin = Address::generate(env);
    let usdc = env.register_stellar_asset_contract_v2(sac_admin).address();

    let gov_id = Address::generate(env);
    let oracle_id = Address::generate(env);
    let registry_id = Address::generate(env);
    let staking_id = Address::generate(env);
    let treasury_id = Address::generate(env);
    let factory_id = Address::generate(env);

    let signers = [
        Address::generate(env),
        Address::generate(env),
        Address::generate(env),
    ];
    let committee = Address::generate(env);
    let guardian = Address::generate(env);
    env.register_at(
        &gov_id,
        Governor,
        (
            Vec::from_array(env, signers.clone()),
            2u32,
            committee.clone(),
            guardian.clone(),
            Targets {
                oracle: oracle_id.clone(),
                registry: registry_id.clone(),
                staking: staking_id.clone(),
                treasury: treasury_id.clone(),
                factory: factory_id.clone(),
            },
        ),
    );

    env.register_at(
        &treasury_id,
        treasury::Treasury,
        (gov_id.clone(), staking_id.clone(), usdc.clone()),
    );
    env.register_at(
        &staking_id,
        staking::Staking,
        (
            gov_id.clone(),
            oracle_id.clone(),
            registry_id.clone(),
            treasury_id.clone(),
            usdc.clone(),
        ),
    );
    env.register_at(
        &oracle_id,
        risk_oracle::RiskOracle,
        (gov_id.clone(), registry_id.clone(), staking_id.clone()),
    );
    env.register_at(
        &registry_id,
        event_registry::EventRegistry,
        (
            gov_id.clone(),
            oracle_id.clone(),
            staking_id.clone(),
            factory_id.clone(),
            usdc.clone(),
        ),
    );
    let series_wasm = env.upload(series::Series);
    env.register_at(
        &factory_id,
        market_factory::MarketFactory,
        (
            gov_id.clone(),
            oracle_id.clone(),
            registry_id.clone(),
            treasury_id.clone(),
            usdc.clone(),
            series_wasm,
        ),
    );

    Fixture {
        gov: GovernorClient::new(env, &gov_id),
        signers,
        committee,
        guardian,
        oracle: risk_oracle::RiskOracleClient::new(env, &oracle_id),
        registry: event_registry::EventRegistryClient::new(env, &registry_id),
        staking: staking::StakingClient::new(env, &staking_id),
        treasury: treasury::TreasuryClient::new(env, &treasury_id),
        factory: market_factory::MarketFactoryClient::new(env, &factory_id),
        usdc: usdc.clone(),
        usdc_client: token::TokenClient::new(env, &usdc),
        usdc_admin: token::StellarAssetClient::new(env, &usdc),
    }
}

pub(crate) fn now(env: &Env) -> u64 {
    env.ledger().timestamp()
}

pub(crate) fn set_time(env: &Env, ts: u64) {
    env.ledger().set_timestamp(ts);
}

/// Queued by signer 0, approved by signers 0 and 1 (the threshold).
pub(crate) fn queue_and_approve(fx: &Fixture, action: &Action) -> u64 {
    let id = fx.gov.queue(&fx.signers[0], action);
    fx.gov.approve(&fx.signers[0], &id);
    fx.gov.approve(&fx.signers[1], &id);
    id
}

/// Queues, approves, moves the clock to exactly `eta` and executes.
pub(crate) fn pass(env: &Env, fx: &Fixture, action: &Action) -> u64 {
    let id = queue_and_approve(fx, action);
    set_time(env, fx.gov.action(&id).unwrap().eta);
    fx.gov.execute(&id);
    id
}

fn stored_action(env: &Env, fx: &Fixture, id: u64) -> sylox_types::QueuedAction {
    env.as_contract(&fx.gov.address, || {
        env.storage()
            .persistent()
            .get(&DataKey::Action(id))
            .unwrap()
    })
}

fn targets(env: &Env) -> Targets {
    Targets {
        oracle: Address::generate(env),
        registry: Address::generate(env),
        staking: Address::generate(env),
        treasury: Address::generate(env),
        factory: Address::generate(env),
    }
}

// -- constructor (D1, D2, G9) --

#[test]
fn version_is_the_contract_version() {
    let env = Env::default();
    let fx = setup(&env);
    assert_eq!(fx.gov.version(), CONTRACT_VERSION);
}

#[test]
#[should_panic(expected = "Error(Contract, #607)")]
fn constructor_rejects_an_empty_signer_set() {
    let env = Env::default();
    let c = Address::generate(&env);
    env.register(
        Governor,
        (Vec::<Address>::new(&env), 1u32, c.clone(), c, targets(&env)),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #607)")]
fn constructor_rejects_a_duplicate_signer() {
    let env = Env::default();
    let s = Address::generate(&env);
    let c = Address::generate(&env);
    env.register(
        Governor,
        (vec![&env, s.clone(), s], 1u32, c.clone(), c, targets(&env)),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #607)")]
fn constructor_rejects_a_zero_threshold() {
    let env = Env::default();
    let c = Address::generate(&env);
    env.register(
        Governor,
        (
            vec![&env, Address::generate(&env)],
            0u32,
            c.clone(),
            c,
            targets(&env),
        ),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #607)")]
fn constructor_rejects_a_threshold_above_the_signer_count() {
    let env = Env::default();
    let c = Address::generate(&env);
    env.register(
        Governor,
        (
            vec![&env, Address::generate(&env)],
            2u32,
            c.clone(),
            c,
            targets(&env),
        ),
    );
}

// -- queue (Section 4.2, 4.4) --

#[test]
fn queue_needs_the_proposer_auth_and_a_current_signer() {
    let env = Env::default();
    let fx = setup(&env);
    let action = Action::SetCommittee(Address::generate(&env));

    let id = fx.gov.queue(&fx.signers[2], &action);
    assert_eq!(env.auths()[0].0, fx.signers[2]);
    assert_eq!(id, 0);

    let outsider = Address::generate(&env);
    assert_eq!(
        fx.gov.try_queue(&outsider, &action),
        Err(Ok(Error::NotSigner))
    );
}

#[test]
fn queue_is_not_an_approval_and_ids_count_up() {
    let env = Env::default();
    let fx = setup(&env);
    let action = Action::SetCommittee(Address::generate(&env));
    let first = fx.gov.queue(&fx.signers[0], &action);
    let second = fx.gov.queue(&fx.signers[0], &action);
    assert_eq!((first, second), (0, 1), "duplicates are allowed (D18)");
    let queued = fx.gov.action(&first).unwrap();
    assert_eq!(queued.state, ActionState::Queued);
    assert!(queued.approvals.is_empty());
    assert_eq!(queued.proposer, fx.signers[0]);
}

/// G4: `eta - queue time` is the Section 4.3 timelock for every row,
/// and `expires_at = eta + GRACE_SECS`.
#[test]
fn every_action_gets_its_section_4_3_timelock() {
    let env = Env::default();
    let fx = setup(&env);
    let a = Address::generate(&env);
    let hash = BytesN::from_array(&env, &[3u8; 32]);
    let cfg = integration::asset_config(&env, &a, &a);
    let terms = sylox_types::SeriesTerms {
        asset: a.clone(),
        def_versions: Map::new(&env),
        settlement: fx.usdc.clone(),
        start: 0,
        expiry: 1,
        claim_window_secs: 1,
        cap: 1,
        max_cover_per_buyer: 1,
        require_holding: false,
        fee_bps: 750,
    };
    let rows: std::vec::Vec<(Action, u64)> = std::vec![
        (Action::OpenSeries(terms), 2),
        (Action::AddAsset(cfg.clone()), 2),
        (Action::AddKeeper(a.clone()), 2),
        (Action::AddReporter(a.clone(), symbol_short!("eu")), 2),
        (Action::DisableAsset(a.clone()), 2),
        (Action::RemoveKeeper(a.clone()), 2),
        (Action::RemoveReporter(a.clone()), 2),
        (
            Action::SetRelatedSellers(a.clone(), vec![&env, a.clone()]),
            2
        ),
        (Action::UpdateAsset(a.clone(), cfg), 7),
        (
            Action::SetParam(Symbol::new(&env, LIQUIDITY_COVER_RATIO), 1),
            7
        ),
        (Action::SetFormula(1, Vec::new(&env), Map::new(&env)), 7),
        (
            Action::RegisterDefinition(integration::depeg_definition(&a)),
            7
        ),
        (
            Action::TreasuryAllocate(TreasuryBucket::Fees, TreasuryBucket::Slashed, 1),
            7
        ),
        (Action::SetSubEpochSecs(a.clone(), 300), 7),
        (Action::SetHardCap(a.clone(), 1), 7),
        (Action::SetPriceGuard(a.clone(), None), 7),
        (
            Action::Upgrade(fx.staking.address.clone(), hash.clone()),
            14
        ),
        (Action::SetSeriesWasm(hash), 14),
        (Action::SetSigners(vec![&env, a.clone()], 1), 14),
        (Action::SetCommittee(a.clone()), 14),
        (
            Action::TreasurySpend(TreasuryBucket::Fees, a.clone(), 1),
            14
        ),
        (Action::SetGuardian(a.clone()), 14),
        (Action::SetFactory(a), 14),
        (Action::Unpause(PauseScope::NewCover), 0),
        (Action::Pause(PauseScope::Signals), 0),
    ];
    for (action, days) in rows {
        assert_eq!(timelock(&action), days * DAY);
        let id = fx.gov.queue(&fx.signers[0], &action);
        let queued = fx.gov.action(&id).unwrap();
        assert_eq!(queued.eta - START, days * DAY, "{action:?}");
        assert_eq!(queued.expires_at, queued.eta + GRACE_SECS);
    }
}

#[test]
fn queue_rejects_what_can_never_execute() {
    let env = Env::default();
    let fx = setup(&env);
    let s = &fx.signers[0];
    let a = Address::generate(&env);
    let invalid = [
        Action::SetSigners(Vec::new(&env), 1),
        Action::SetSigners(vec![&env, a.clone(), a.clone()], 1),
        Action::SetSigners(vec![&env, a.clone()], 0),
        Action::SetSigners(vec![&env, a.clone()], 2),
    ];
    for action in invalid {
        assert_eq!(fx.gov.try_queue(s, &action), Err(Ok(Error::InvalidSigners)));
    }
    assert_eq!(
        fx.gov
            .try_queue(s, &Action::SetParam(symbol_short!("fee_bps"), 750)),
        Err(Ok(Error::InvalidAction)),
        "D11: liquidity_cover_ratio is the only v1 SetParam key"
    );
    assert_eq!(
        fx.gov.try_queue(
            s,
            &Action::Upgrade(
                Address::generate(&env),
                BytesN::from_array(&env, &[1u8; 32])
            )
        ),
        Err(Ok(Error::InvalidAction)),
        "only the five targets and the Governor itself can be upgraded"
    );
    // The Governor itself is a valid upgrade target.
    fx.gov.queue(
        s,
        &Action::Upgrade(fx.gov.address.clone(), BytesN::from_array(&env, &[1u8; 32])),
    );
}

// -- approve (Section 4.2, D15, D17) --

#[test]
fn approvals_reach_the_threshold_and_move_to_approved() {
    let env = Env::default();
    let fx = setup(&env);
    let id = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );

    fx.gov.approve(&fx.signers[1], &id);
    assert_eq!(env.auths()[0].0, fx.signers[1]);
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Queued);
    assert_eq!(
        fx.gov.try_approve(&fx.signers[1], &id),
        Err(Ok(Error::AlreadyApproved))
    );

    fx.gov.approve(&fx.signers[2], &id);
    let queued = fx.gov.action(&id).unwrap();
    assert_eq!(queued.state, ActionState::Approved);
    assert_eq!(queued.approvals.len(), 2);

    // Approved actions still take approvals (D15) and stay Approved.
    fx.gov.approve(&fx.signers[0], &id);
    let queued = fx.gov.action(&id).unwrap();
    assert_eq!(queued.state, ActionState::Approved);
    assert_eq!(queued.approvals.len(), 3);
    assert_eq!(
        fx.gov.try_approve(&Address::generate(&env), &id),
        Err(Ok(Error::NotSigner))
    );
    assert_eq!(
        fx.gov.try_approve(&fx.signers[0], &99),
        Err(Ok(Error::UnknownAction))
    );
}

#[test]
fn approving_after_eta_is_allowed_and_never_executes_by_itself() {
    let env = Env::default();
    let fx = setup(&env);
    let committee = Address::generate(&env);
    let id = fx
        .gov
        .queue(&fx.signers[0], &Action::SetCommittee(committee.clone()));
    let queued = fx.gov.action(&id).unwrap();
    set_time(&env, queued.eta + DAY);
    fx.gov.approve(&fx.signers[0], &id);
    fx.gov.approve(&fx.signers[1], &id);
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Approved);
    assert_eq!(fx.gov.committee(), fx.committee, "not executed yet (D17)");
    fx.gov.execute(&id);
    assert_eq!(fx.gov.committee(), committee);
}

#[test]
fn approve_after_expiry_is_rejected() {
    let env = Env::default();
    let fx = setup(&env);
    let id = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    set_time(&env, fx.gov.action(&id).unwrap().expires_at + 1);
    assert_eq!(
        fx.gov.try_approve(&fx.signers[0], &id),
        Err(Ok(Error::ActionExpired))
    );
}

// -- execute (Section 4.2, G2, G3, D25) --

#[test]
fn execute_checks_threshold_timelock_and_expiry_and_runs_once() {
    let env = Env::default();
    let fx = setup(&env);
    let committee = Address::generate(&env);
    let id = fx
        .gov
        .queue(&fx.signers[0], &Action::SetCommittee(committee.clone()));
    let queued = fx.gov.action(&id).unwrap();

    set_time(&env, queued.eta);
    assert_eq!(fx.gov.try_execute(&id), Err(Ok(Error::ThresholdNotMet)));

    fx.gov.approve(&fx.signers[0], &id);
    fx.gov.approve(&fx.signers[1], &id);
    set_time(&env, queued.eta - 1);
    assert_eq!(fx.gov.try_execute(&id), Err(Ok(Error::TimelockActive)));

    set_time(&env, queued.eta);
    // Anyone may execute (D25): no auth is required or recorded.
    fx.gov.execute(&id);
    assert!(env.auths().is_empty());
    assert_eq!(fx.gov.committee(), committee);
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Executed);
    assert_eq!(fx.gov.try_execute(&id), Err(Ok(Error::WrongState)));
    assert_eq!(fx.gov.try_execute(&42), Err(Ok(Error::UnknownAction)));
}

#[test]
fn execute_after_expires_at_is_rejected_and_reads_expired_with_no_write() {
    let env = Env::default();
    let fx = setup(&env);
    let id = queue_and_approve(&fx, &Action::SetCommittee(Address::generate(&env)));
    let queued = fx.gov.action(&id).unwrap();

    set_time(&env, queued.expires_at);
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Approved);

    set_time(&env, queued.expires_at + 1);
    assert_eq!(fx.gov.try_execute(&id), Err(Ok(Error::ActionExpired)));
    assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Expired);
    assert_eq!(
        stored_action(&env, &fx, id).state,
        ActionState::Approved,
        "Expired is reported by the read, never written"
    );
}

/// D15: only CURRENT signers' approvals count, against the threshold
/// at execution time; pending actions are not cancelled.
#[test]
fn a_removed_signers_approval_stops_counting() {
    let env = Env::default();
    let fx = setup(&env);
    let committee = Address::generate(&env);
    let pending = fx
        .gov
        .queue(&fx.signers[0], &Action::SetCommittee(committee.clone()));
    fx.gov.approve(&fx.signers[1], &pending);
    fx.gov.approve(&fx.signers[2], &pending);
    assert_eq!(
        fx.gov.action(&pending).unwrap().state,
        ActionState::Approved
    );

    // Signer 2 is replaced by a new signer; threshold stays 2.
    let newcomer = Address::generate(&env);
    pass(
        &env,
        &fx,
        &Action::SetSigners(
            vec![&env, fx.signers[0].clone(), fx.signers[1].clone(), newcomer],
            2,
        ),
    );
    assert_eq!(
        fx.gov.action(&pending).unwrap().state,
        ActionState::Approved,
        "not cancelled"
    );
    assert_eq!(
        fx.gov.try_execute(&pending),
        Err(Ok(Error::ThresholdNotMet))
    );
}

/// D15: an Approved action that a `SetSigners` dropped below the
/// threshold recovers when a new signer approves it.
#[test]
fn an_approved_action_below_threshold_after_a_signer_change_recovers() {
    let env = Env::default();
    let fx = setup(&env);
    let committee = Address::generate(&env);
    let pending = queue_and_approve(&fx, &Action::SetCommittee(committee.clone()));
    assert_eq!(
        fx.gov.action(&pending).unwrap().state,
        ActionState::Approved
    );

    // Signer 1, one of its two approvers, is replaced.
    let newcomer = Address::generate(&env);
    pass(
        &env,
        &fx,
        &Action::SetSigners(
            vec![
                &env,
                fx.signers[0].clone(),
                newcomer.clone(),
                fx.signers[2].clone(),
            ],
            2,
        ),
    );
    assert_eq!(
        fx.gov.try_execute(&pending),
        Err(Ok(Error::ThresholdNotMet))
    );

    fx.gov.approve(&newcomer, &pending);
    assert_eq!(
        fx.gov.action(&pending).unwrap().state,
        ActionState::Approved
    );
    fx.gov.execute(&pending);
    assert_eq!(fx.gov.committee(), committee);
    assert_eq!(
        fx.gov.action(&pending).unwrap().state,
        ActionState::Executed
    );
    assert_eq!(
        fx.gov.try_approve(&fx.signers[2], &pending),
        Err(Ok(Error::WrongState)),
        "no approvals once Executed"
    );
}

#[test]
fn a_raised_threshold_applies_to_actions_already_approved() {
    let env = Env::default();
    let fx = setup(&env);
    let pending = queue_and_approve(&fx, &Action::SetCommittee(Address::generate(&env)));
    pass(
        &env,
        &fx,
        &Action::SetSigners(Vec::from_array(&env, fx.signers.clone()), 3),
    );
    assert_eq!(
        fx.gov.try_execute(&pending),
        Err(Ok(Error::ThresholdNotMet))
    );
}

#[test]
fn duplicate_actions_each_need_the_threshold() {
    let env = Env::default();
    let fx = setup(&env);
    let action = Action::Pause(PauseScope::Deposits);
    let first = queue_and_approve(&fx, &action);
    let second = fx.gov.queue(&fx.signers[0], &action);
    fx.gov.approve(&fx.signers[0], &second);
    fx.gov.execute(&first);
    assert_eq!(fx.gov.try_execute(&second), Err(Ok(Error::ThresholdNotMet)));
}

// -- cancel (Section 5, D6) --

#[test]
fn cancel_by_threshold_in_queued_and_approved() {
    let env = Env::default();
    let fx = setup(&env);
    let both = vec![&env, fx.signers[1].clone(), fx.signers[2].clone()];

    let queued = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    fx.gov.cancel(&both, &queued);
    let auths: std::vec::Vec<Address> = env.auths().into_iter().map(|(a, _)| a).collect();
    assert!(auths.contains(&fx.signers[1]) && auths.contains(&fx.signers[2]));
    assert_eq!(
        fx.gov.action(&queued).unwrap().state,
        ActionState::Cancelled
    );

    let approved = queue_and_approve(&fx, &Action::SetCommittee(Address::generate(&env)));
    fx.gov.cancel(&both, &approved);
    assert_eq!(
        fx.gov.action(&approved).unwrap().state,
        ActionState::Cancelled
    );

    assert_eq!(fx.gov.try_execute(&approved), Err(Ok(Error::WrongState)));
    assert_eq!(
        fx.gov.try_approve(&fx.signers[2], &queued),
        Err(Ok(Error::WrongState))
    );
    assert_eq!(
        fx.gov.try_cancel(&both, &queued),
        Err(Ok(Error::WrongState))
    );
}

#[test]
fn cancel_counts_distinct_current_signers_only() {
    let env = Env::default();
    let fx = setup(&env);
    let id = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    assert_eq!(
        fx.gov.try_cancel(
            &vec![&env, fx.signers[1].clone(), fx.signers[1].clone()],
            &id
        ),
        Err(Ok(Error::ThresholdNotMet)),
        "a signer listed twice counts once"
    );
    assert_eq!(
        fx.gov.try_cancel(
            &vec![&env, fx.signers[1].clone(), Address::generate(&env)],
            &id
        ),
        Err(Ok(Error::NotSigner))
    );
    assert_eq!(
        fx.gov.try_cancel(&vec![&env, fx.signers[1].clone()], &id),
        Err(Ok(Error::ThresholdNotMet)),
        "one signer who is not the proposer is below the threshold"
    );
}

#[test]
fn the_proposer_alone_can_cancel_while_queued_but_not_once_approved() {
    let env = Env::default();
    let fx = setup(&env);
    let proposer = vec![&env, fx.signers[0].clone()];

    let queued = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    fx.gov.approve(&fx.signers[1], &queued);
    fx.gov.cancel(&proposer, &queued);
    assert_eq!(env.auths()[0].0, fx.signers[0]);
    assert_eq!(
        fx.gov.action(&queued).unwrap().state,
        ActionState::Cancelled
    );

    let approved = queue_and_approve(&fx, &Action::SetCommittee(Address::generate(&env)));
    assert_eq!(
        fx.gov.try_cancel(&proposer, &approved),
        Err(Ok(Error::WrongState))
    );
}

/// D6: the proposer-alone route needs the proposer to still be a
/// current signer.
#[test]
fn a_removed_proposer_cannot_cancel_alone() {
    let env = Env::default();
    let fx = setup(&env);
    let queued = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    pass(
        &env,
        &fx,
        &Action::SetSigners(vec![&env, fx.signers[1].clone(), fx.signers[2].clone()], 2),
    );
    assert_eq!(fx.gov.action(&queued).unwrap().state, ActionState::Queued);
    assert_eq!(
        fx.gov
            .try_cancel(&vec![&env, fx.signers[0].clone()], &queued),
        Err(Ok(Error::NotSigner))
    );
    // The current signers still can.
    fx.gov.cancel(
        &vec![&env, fx.signers[1].clone(), fx.signers[2].clone()],
        &queued,
    );
    assert_eq!(
        fx.gov.action(&queued).unwrap().state,
        ActionState::Cancelled
    );
}

#[test]
fn cancel_after_expiry_is_rejected() {
    let env = Env::default();
    let fx = setup(&env);
    let id = fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    set_time(&env, fx.gov.action(&id).unwrap().expires_at + 1);
    assert_eq!(
        fx.gov.try_cancel(&vec![&env, fx.signers[0].clone()], &id),
        Err(Ok(Error::ActionExpired))
    );
}

// -- pause (Section 8.1, D7, D8, G5, G6) --

#[test]
fn the_guardian_pauses_and_a_pause_lapses_by_itself() {
    let env = Env::default();
    let fx = setup(&env);
    assert!(!fx.gov.paused(&PauseScope::NewCover));

    fx.gov.pause(&PauseScope::NewCover);
    assert_eq!(env.auths()[0].0, fx.guardian);
    assert_eq!(
        env.events()
            .all()
            .filter_by_contract(&fx.gov.address)
            .events()
            .len(),
        1,
        "paused"
    );
    assert!(fx.gov.paused(&PauseScope::NewCover));
    assert!(
        !fx.gov.paused(&PauseScope::Deposits),
        "scopes are independent"
    );

    set_time(&env, START + MAX_PAUSE_SECS - 1);
    assert!(fx.gov.paused(&PauseScope::NewCover));
    set_time(&env, START + MAX_PAUSE_SECS);
    assert!(
        !fx.gov.paused(&PauseScope::NewCover),
        "G6: lapses with no write"
    );

    // Once lapsed, the guardian can pause again.
    fx.gov.pause(&PauseScope::NewCover);
    assert!(fx.gov.paused(&PauseScope::NewCover));
}

#[test]
fn the_guardian_cannot_extend_an_active_pause() {
    let env = Env::default();
    let fx = setup(&env);
    fx.gov.pause(&PauseScope::Signals);
    set_time(&env, START + DAY);
    assert_eq!(
        fx.gov.try_pause(&PauseScope::Signals),
        Err(Ok(Error::AlreadyPaused))
    );
    set_time(&env, START + MAX_PAUSE_SECS);
    assert!(
        !fx.gov.paused(&PauseScope::Signals),
        "the lapse did not move"
    );
}

#[test]
fn action_pause_renews_or_starts_a_pause_and_unpause_needs_the_threshold() {
    let env = Env::default();
    let fx = setup(&env);
    fx.gov.pause(&PauseScope::NewSeries);

    // Renewal: 0 day timelock, full threshold, lapse reset to now + max.
    set_time(&env, START + 10 * DAY);
    let renew = fx
        .gov
        .queue(&fx.signers[0], &Action::Pause(PauseScope::NewSeries));
    fx.gov.approve(&fx.signers[0], &renew);
    assert_eq!(fx.gov.try_execute(&renew), Err(Ok(Error::ThresholdNotMet)));
    fx.gov.approve(&fx.signers[1], &renew);
    fx.gov.execute(&renew);
    set_time(&env, START + 10 * DAY + MAX_PAUSE_SECS - 1);
    assert!(fx.gov.paused(&PauseScope::NewSeries), "lapse was reset");

    // A fresh pause of a scope that is not paused (D8).
    pass(&env, &fx, &Action::Pause(PauseScope::Deposits));
    assert!(fx.gov.paused(&PauseScope::Deposits));

    // Unpause: no waiting time, full threshold.
    let unpause = fx
        .gov
        .queue(&fx.signers[0], &Action::Unpause(PauseScope::Deposits));
    fx.gov.approve(&fx.signers[0], &unpause);
    assert_eq!(
        fx.gov.try_execute(&unpause),
        Err(Ok(Error::ThresholdNotMet))
    );
    fx.gov.approve(&fx.signers[2], &unpause);
    fx.gov.execute(&unpause);
    assert!(!fx.gov.paused(&PauseScope::Deposits));
    assert!(
        fx.gov.paused(&PauseScope::NewSeries),
        "other scopes unaffected"
    );
}

#[test]
fn set_guardian_changes_who_can_pause() {
    let env = Env::default();
    let fx = setup(&env);
    let next = Address::generate(&env);
    pass(&env, &fx, &Action::SetGuardian(next.clone()));
    fx.gov.pause(&PauseScope::NewCover);
    assert_eq!(env.auths()[0].0, next);
}

#[test]
fn pause_by_anyone_but_the_guardian_traps() {
    let env = Env::default();
    let fx = setup(&env);
    let signer = fx.signers[0].clone();
    env.mock_auths(&[MockAuth {
        address: &signer,
        invoke: &MockAuthInvoke {
            contract: &fx.gov.address,
            fn_name: "pause",
            args: (PauseScope::NewCover,).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    assert!(fx.gov.try_pause(&PauseScope::NewCover).is_err());
}

// -- reads: committee, param (Section 7, 9, D11) --

#[test]
fn committee_reads_the_stored_address_and_set_committee_changes_it() {
    let env = Env::default();
    let fx = setup(&env);
    assert_eq!(fx.gov.committee(), fx.committee);
    let next = Address::generate(&env);
    pass(&env, &fx, &Action::SetCommittee(next.clone()));
    assert_eq!(fx.gov.committee(), next);
}

#[test]
fn param_is_none_until_set_param_runs() {
    let env = Env::default();
    let fx = setup(&env);
    let key = Symbol::new(&env, LIQUIDITY_COVER_RATIO);
    assert_eq!(fx.gov.param(&key), None);
    pass(&env, &fx, &Action::SetParam(key.clone(), 5_000_000));
    assert_eq!(fx.gov.param(&key), Some(5_000_000));
}

// -- own migration (Section 6.3, D14) --

#[test]
fn own_migration_runs_once_on_the_first_state_changing_call() {
    let env = Env::default();
    let fx = setup(&env);
    let read = || -> u32 {
        env.as_contract(&fx.gov.address, || {
            env.storage()
                .instance()
                .get(&MIGRATION_VERSION_KEY)
                .unwrap()
        })
    };
    assert_eq!(read(), CONTRACT_VERSION, "fresh storage needs no migration");

    // As if the data were left by an older version of this code.
    env.as_contract(&fx.gov.address, || {
        env.storage()
            .instance()
            .set(&MIGRATION_VERSION_KEY, &(CONTRACT_VERSION - 1));
    });
    fx.gov.committee();
    assert_eq!(read(), CONTRACT_VERSION - 1, "a read does not migrate");
    fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    assert_eq!(read(), CONTRACT_VERSION);
}

// -- TTL (Section 3.2, issue #28, D20) --

fn instance_ttl(env: &Env, fx: &Fixture) -> u32 {
    env.as_contract(&fx.gov.address, || env.storage().instance().get_ttl())
}

fn action_ttl(env: &Env, fx: &Fixture, id: u64) -> u32 {
    env.as_contract(&fx.gov.address, || {
        env.storage().persistent().get_ttl(&DataKey::Action(id))
    })
}

fn age_ledgers(env: &Env, by: u32) {
    let seq = env.ledger().sequence();
    env.ledger().set_sequence_number(seq + by);
}

#[test]
fn state_changing_calls_and_target_reads_extend_the_instance_ttl() {
    let env = Env::default();
    let fx = setup(&env);
    fx.gov.queue(
        &fx.signers[0],
        &Action::SetCommittee(Address::generate(&env)),
    );
    assert_eq!(instance_ttl(&env, &fx), INSTANCE_TTL_EXTEND_TO);

    age_ledgers(&env, 200_000);
    fx.gov.committee();
    assert_eq!(instance_ttl(&env, &fx), INSTANCE_TTL_EXTEND_TO);

    age_ledgers(&env, 200_000);
    fx.gov.paused(&PauseScope::NewCover);
    assert_eq!(instance_ttl(&env, &fx), INSTANCE_TTL_EXTEND_TO);

    age_ledgers(&env, 200_000);
    fx.gov.pause(&PauseScope::NewCover);
    assert_eq!(instance_ttl(&env, &fx), INSTANCE_TTL_EXTEND_TO);
}

#[test]
fn every_action_write_extends_that_actions_ttl() {
    let env = Env::default();
    let fx = setup(&env);
    let id = fx
        .gov
        .queue(&fx.signers[0], &Action::Pause(PauseScope::NewCover));
    assert_eq!(action_ttl(&env, &fx, id), PERSISTENT_TTL_EXTEND_TO);

    age_ledgers(&env, 2_000_000);
    fx.gov.approve(&fx.signers[0], &id);
    assert_eq!(action_ttl(&env, &fx, id), PERSISTENT_TTL_EXTEND_TO);

    age_ledgers(&env, 2_000_000);
    fx.gov.approve(&fx.signers[1], &id);
    age_ledgers(&env, 2_000_000);
    fx.gov.execute(&id);
    assert_eq!(action_ttl(&env, &fx, id), PERSISTENT_TTL_EXTEND_TO);
}
