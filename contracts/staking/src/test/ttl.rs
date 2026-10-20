//! Issue #28: every state-changing call extends the instance TTL, and
//! every persistent entry it writes is extended, with the shared
//! `sylox_types::ttl` constants. Each call below runs after the ledger
//! has moved past every re-extend threshold, so it passes only if the
//! call itself extends what it touches (`sylox_types::ttl_check`).
//! Temporary entries (probes, submitter indexes, settled markers) keep
//! their own `PROBE_TTL_LEDGERS` lifetime and are not checked here.

extern crate std;

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, BytesN, Env};
use sylox_types::ttl_check::{age_past_thresholds, assert_extended, entries};
use sylox_types::{BondKey, EndpointStatus, TreasuryBucket};

use super::{fund, fund_treasury_bucket, probe, region, settlement_opens, setup, Fixture};
use crate::{params, Staking};

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
    let g = || Address::generate(&env);
    let id = env.register(Staking, (g(), g(), g(), g(), g()));
    assert_extended(&env, &id, &before, &[], "__constructor");
}

#[test]
fn membership_stake_and_exit_calls_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    let reporter = Address::generate(&env);

    assert_eq!(
        step(&env, &fx, "add_keeper", || fx.client.add_keeper(&keeper)),
        1
    );
    fund(&fx, &keeper, params::KEEPER_BOND);
    assert_eq!(
        step(&env, &fx, "stake keeper", || fx
            .client
            .stake(&keeper, &params::KEEPER_BOND)),
        1
    );
    assert_eq!(
        step(&env, &fx, "add_reporter", || {
            fx.client.add_reporter(&reporter, &region(&env, "eu"))
        }),
        1
    );
    fund(&fx, &reporter, params::REPORTER_STAKE);
    step(&env, &fx, "stake reporter", || {
        fx.client.stake(&reporter, &params::REPORTER_STAKE)
    });
    step(&env, &fx, "unstake_request", || {
        fx.client
            .unstake_request(&reporter, &params::REPORTER_STAKE)
    });
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + params::UNSTAKE_COOLDOWN_SECS);
    step(&env, &fx, "unstake", || {
        fx.client.unstake(&reporter);
    });
    step(&env, &fx, "remove_reporter", || {
        fx.client.remove_reporter(&reporter)
    });
    step(&env, &fx, "remove_keeper", || {
        fx.client.remove_keeper(&keeper)
    });
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + params::KEEPER_EXIT_DELAY_SECS);
    step(&env, &fx, "withdraw_keeper_bond", || {
        fx.client.withdraw_keeper_bond(&keeper);
    });
    step(&env, &fx, "upgrade", || {
        fx.client.upgrade(&env.upload(Staking))
    });
    step(&env, &fx, "migrate", || fx.client.migrate());
}

#[test]
fn probe_bond_slash_and_reward_calls_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::ReporterRewards,
        params::REPORTER_REWARD_PER_EPOCH,
    );
    fund_treasury_bucket(&env, &fx, TreasuryBucket::KeeperRewards, 1_000_000_000);

    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    fund(&fx, &keeper, params::KEEPER_BOND);
    fx.client.stake(&keeper, &params::KEEPER_BOND);

    // Three reporters, one probe each for epoch 0.
    env.ledger().set_timestamp(params::EPOCH_SECS);
    let mut reporters = std::vec::Vec::new();
    for name in ["eu", "us", "af"] {
        let r = Address::generate(&env);
        fx.client.add_reporter(&r, &region(&env, name));
        fund(&fx, &r, params::REPORTER_STAKE);
        fx.client.stake(&r, &params::REPORTER_STAKE);
        reporters.push(r);
    }
    // Probes are temporary entries with their own PROBE_TTL_LEDGERS
    // lifetime, shorter than the aging `step` uses, so the probes and
    // the settle run without aging in between: `settle_probes` then
    // does real work. The ledger is aged once before the first probe.
    // The instance checks after the later probes and after settle hold
    // but cannot prove those calls extended it themselves, since the
    // first probe already did.
    age_past_thresholds(&env);
    for r in &reporters {
        let before = entries(&env, &fx.contract_id);
        fx.client
            .submit_probe(r, &probe(&env, &asset, 0, EndpointStatus::Up));
        assert_extended(&env, &fx.contract_id, &before, &[], "submit_probe");
    }
    env.ledger().set_timestamp(settlement_opens(0));
    let before = entries(&env, &fx.contract_id);
    fx.client.settle_probes(&asset, &0);
    assert_extended(&env, &fx.contract_id, &before, &[], "settle_probes");
    for r in &reporters {
        assert_eq!(
            fx.treasury_client.accrued(r),
            params::REPORTER_REWARD_PER_EPOCH / 3,
            "settle_probes did real work"
        );
    }

    let disputer = Address::generate(&env);
    fund(&fx, &disputer, 2 * params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset.clone(), 7);
    let written = step(&env, &fx, "lock_bond", || {
        fx.client
            .lock_bond(&key, &disputer, &params::KEEPER_BOND, &Some(keeper.clone()))
    });
    assert!(written >= 1, "Bond(key)");
    step(&env, &fx, "forfeit_bond", || {
        fx.client.forfeit_bond(&key, &Some(keeper.clone()))
    });
    let key2 = BondKey::SignalDispute(asset.clone(), 8);
    fx.client.lock_bond(
        &key2,
        &disputer,
        &params::KEEPER_BOND,
        &Some(keeper.clone()),
    );
    step(&env, &fx, "release_bond", || fx.client.release_bond(&key2));
    step(&env, &fx, "claim", || {
        fx.client.claim(&disputer);
    });
    step(&env, &fx, "slash", || {
        fx.client.slash(
            &keeper,
            &1_000_000,
            &Some(disputer.clone()),
            &BytesN::from_array(&env, &[0u8; 32]),
        )
    });
    step(&env, &fx, "reward_keeper", || {
        fx.client.reward_keeper(&keeper, &1);
    });
    step(&env, &fx, "reward_keeper_sub_epochs", || {
        fx.client.reward_keeper_sub_epochs(&keeper, &12, &300);
    });
}
