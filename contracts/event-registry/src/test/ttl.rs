//! Issue #28: every state-changing call extends the instance TTL, and
//! every persistent entry it writes is extended, with the shared
//! `sylox_types::ttl` constants. Each checked call runs after the
//! ledger has moved past every re-extend threshold, so it passes only
//! if the call itself extends what it touches
//! (`sylox_types::ttl_check`). Only `EventRegistry`'s own entries are
//! checked here; the `RiskOracle` calls that build history are setup.

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, BytesN, Env};
use sylox_types::ttl_check::{age_past_thresholds, assert_extended, entries};
use sylox_types::{EventKind, EventState, IssuerFlags};

use super::{
    asset_config, depeg_definition, post_failing_depeg_window, setup, signal_set, Fixture,
    EPOCH_SECS, SIGNAL_DISPUTE_SECS, WINDOW_SECS,
};
use crate::EventRegistry;

fn step(env: &Env, fx: &Fixture, label: &str, call: impl FnOnce()) -> usize {
    age_past_thresholds(env);
    let before = entries(env, &fx.contract_id);
    call();
    assert_extended(env, &fx.contract_id, &before, &[], label)
}

/// A registered asset with a canonical Depeg definition (checked) and a
/// failing window posted, ready for `propose_tier1`.
fn failing_asset(env: &Env, fx: &Fixture) -> Address {
    let asset = Address::generate(env);
    let issuer = Address::generate(env);
    fx.oracle
        .add_asset(&asset_config(env, &asset, &issuer, IssuerFlags::default()));
    let written = step(env, fx, "register_definition", || {
        fx.client
            .register_definition(&depeg_definition(env, &asset));
    });
    assert!(written >= 2, "Def and Canonical");
    post_failing_depeg_window(env, &fx.oracle, &fx.keeper, &asset);
    asset
}

fn propose(env: &Env, fx: &Fixture, asset: &Address) -> u64 {
    let mut id = 0;
    let written = step(env, fx, "propose_tier1", || {
        id = fx
            .client
            .propose_tier1(&Address::generate(env), asset, &EventKind::Depeg, &1);
    });
    assert!(written >= 2, "Event and Status at least");
    id
}

#[test]
fn constructor_leaves_the_instance_extended() {
    let env = Env::default();
    let before = entries(&env, &Address::generate(&env));
    let g = || Address::generate(&env);
    let id = env.register(EventRegistry, (g(), g(), g(), g(), g()));
    assert_extended(&env, &id, &before, &[], "__constructor");
}

#[test]
fn register_propose_and_finalize_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = failing_asset(&env, &fx);
    let id = propose(&env, &fx, &asset);
    let now = env.ledger().timestamp();
    env.ledger()
        .set_timestamp(now + 86_400 + WINDOW_SECS + SIGNAL_DISPUTE_SECS + 1);
    step(&env, &fx, "finalize", || fx.client.finalize(&id));
    assert_eq!(fx.client.event(&id).unwrap().state, EventState::Declared);
}

#[test]
fn checkpoint_cure_extends_what_it_touches() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = failing_asset(&env, &fx);
    let id = propose(&env, &fx, &asset);
    let record = fx.client.event(&id).unwrap();
    let first = record.proposed_at / EPOCH_SECS;
    for epoch in first..first + 3 {
        env.ledger().set_timestamp((epoch + 1) * EPOCH_SECS);
        fx.oracle.post_signals(
            &fx.keeper,
            &asset,
            &signal_set(&env, &fx.keeper, epoch, 9_900_000, 500_000_000_000),
        );
    }
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + SIGNAL_DISPUTE_SECS + 1);
    let written = step(&env, &fx, "checkpoint_cure", || {
        fx.client.checkpoint_cure(&id);
    });
    assert!(written >= 1, "CureProgress");
}

#[test]
fn challenge_and_rule_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = failing_asset(&env, &fx);
    let id = propose(&env, &fx, &asset);
    step(&env, &fx, "challenge", || {
        fx.client.challenge(
            &Address::generate(&env),
            &id,
            &BytesN::from_array(&env, &[1u8; 32]),
        )
    });
    step(&env, &fx, "rule", || {
        fx.client
            .rule(&id, &true, &BytesN::from_array(&env, &[2u8; 32]))
    });
    assert_eq!(fx.client.event(&id).unwrap().state, EventState::Declared);
}

#[test]
fn resolve_timeout_extends_what_it_touches() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = failing_asset(&env, &fx);
    let id = propose(&env, &fx, &asset);
    fx.client.challenge(
        &Address::generate(&env),
        &id,
        &BytesN::from_array(&env, &[1u8; 32]),
    );
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + 1_209_600 + 1);
    let written = step(&env, &fx, "resolve_timeout", || {
        fx.client.resolve_timeout(&id)
    });
    assert!(written >= 1, "CommitteeMisses and the event record");
}

#[test]
fn set_factory_upgrade_and_migrate_extend_the_instance() {
    let env = Env::default();
    let fx = setup(&env);
    step(&env, &fx, "set_factory", || {
        fx.client.set_factory(&Address::generate(&env))
    });
    step(&env, &fx, "upgrade", || {
        fx.client.upgrade(&env.upload(EventRegistry))
    });
    step(&env, &fx, "migrate", || fx.client.migrate());
}
