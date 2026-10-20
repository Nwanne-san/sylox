//! Issue #28: every state-changing call extends the instance TTL, and
//! every persistent entry it writes is extended, with the shared
//! `sylox_types::ttl` constants. Each call below runs after the ledger
//! has moved past every re-extend threshold, so it passes only if the
//! call itself extends what it touches (`sylox_types::ttl_check`).

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};
use sylox_types::ttl_check::{age_past_thresholds, assert_extended, entries};
use sylox_types::TreasuryBucket;

use super::{deposit_into, setup, Fixture};
use crate::{Treasury, TreasuryClient};

/// Ages the ledger, runs `call`, and checks every entry it wrote.
fn step(env: &Env, fx: &Fixture, label: &str, call: impl FnOnce()) -> usize {
    age_past_thresholds(env);
    let before = entries(env, &fx.contract_id);
    call();
    assert_extended(env, &fx.contract_id, &before, &[], label)
}

#[test]
fn constructor_leaves_the_instance_extended() {
    let env = Env::default();
    let before = entries(&env, &Address::generate(&env));
    let id = env.register(
        Treasury,
        (
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ),
    );
    assert_extended(&env, &id, &before, &[], "__constructor");
}

#[test]
fn every_state_changing_call_extends_what_it_touches() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);

    step(&env, &fx, "deposit", || {
        deposit_into(&env, &fx, TreasuryBucket::ReporterRewards, 1_000);
    });
    step(&env, &fx, "allocate", || {
        fx.client.allocate(
            &TreasuryBucket::ReporterRewards,
            &TreasuryBucket::KeeperRewards,
            &100,
        );
    });
    let written = step(&env, &fx, "accrue_reward", || {
        fx.client
            .accrue_reward(&who, &TreasuryBucket::ReporterRewards, &50);
    });
    assert_eq!(written, 1, "Accrued(who)");
    let written = step(&env, &fx, "accrue_reward again", || {
        fx.client
            .accrue_reward(&who, &TreasuryBucket::ReporterRewards, &25);
    });
    assert_eq!(written, 1, "an existing Accrued(who) is re-extended");
    step(&env, &fx, "claim_reward", || {
        fx.client.claim_reward(&who);
    });
    step(&env, &fx, "spend", || {
        fx.client.spend(
            &TreasuryBucket::KeeperRewards,
            &Address::generate(&env),
            &10,
        );
    });
    step(&env, &fx, "upgrade", || {
        fx.client.upgrade(&env.upload(Treasury));
    });
    step(&env, &fx, "migrate", || {
        fx.client.migrate();
    });
    let _ = TreasuryClient::new(&env, &fx.contract_id);
}
