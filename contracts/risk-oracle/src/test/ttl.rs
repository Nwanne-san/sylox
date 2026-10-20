//! Issue #28: every state-changing call extends the instance TTL, and
//! every persistent entry it writes is extended, with the shared
//! `sylox_types::ttl` constants. Each checked call runs after the
//! ledger has moved past every re-extend threshold, so it passes only
//! if the call itself extends what it touches
//! (`sylox_types::ttl_check`).
//!
//! `Sub(asset)`, `SubDispute` and `HeldHour` keep their own Section 5.9
//! S3 lifetimes (sized to the dispute ruling window, extended on every
//! post and dispute) and are excluded from the shared-constant check.

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, BytesN, Env, Map, Vec};
use sylox_types::ttl_check::{age_past_thresholds, assert_extended, entries};
use sylox_types::IssuerActions;

use super::{asset_config, signal_set, time_at_epoch};
use crate::mocks::{MockGovernor, MockGovernorClient, MockStaking};
use crate::{RiskOracle, RiskOracleClient};

const OWN_POLICY: &[&str] = &["Sub", "SubDispute", "HeldHour"];

struct Fx<'a> {
    client: RiskOracleClient<'a>,
    governor: Address,
    committee: Address,
    asset: Address,
    issuer: Address,
    keeper: Address,
}

/// A real `MockGovernor` with a committee, so the committee-gated calls
/// can resolve it; `MockStaking` for keeper checks and probe
/// aggregates; the registry is a plain address (mocked auth).
fn setup(env: &Env) -> Fx<'_> {
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let committee = Address::generate(env);
    MockGovernorClient::new(env, &governor).set_committee(&committee);
    let id = env.register(
        RiskOracle,
        (governor.clone(), Address::generate(env), staking),
    );
    Fx {
        client: RiskOracleClient::new(env, &id),
        governor,
        committee,
        asset: Address::generate(env),
        issuer: Address::generate(env),
        keeper: Address::generate(env),
    }
}

fn step(env: &Env, fx: &Fx, label: &str, call: impl FnOnce()) -> usize {
    age_past_thresholds(env);
    // `MockGovernor` keeps its committee in temporary storage, which
    // the test host does expire; put it back (test setup, not under test).
    MockGovernorClient::new(env, &fx.governor).set_committee(&fx.committee);
    let before = entries(env, &fx.client.address);
    call();
    assert_extended(env, &fx.client.address, &before, OWN_POLICY, label)
}

fn post(env: &Env, fx: &Fx, epoch: u64) {
    env.ledger().set_timestamp(time_at_epoch(epoch));
    fx.client
        .post_signals(&fx.keeper, &fx.asset, &signal_set(env, epoch, 9_900_000));
}

fn hash(env: &Env, b: u8) -> BytesN<32> {
    BytesN::from_array(env, &[b; 32])
}

#[test]
fn constructor_leaves_the_instance_extended() {
    let env = Env::default();
    let before = entries(&env, &Address::generate(&env));
    let g = || Address::generate(&env);
    let id = env.register(RiskOracle, (g(), g(), g()));
    assert_extended(&env, &id, &before, OWN_POLICY, "__constructor");
}

#[test]
fn asset_formula_and_admin_calls_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    let cfg = asset_config(&env, &fx.asset, &fx.issuer);
    assert_eq!(
        step(&env, &fx, "add_asset", || fx.client.add_asset(&cfg)),
        1,
        "Asset(asset)"
    );
    let mut updated = cfg.clone();
    updated.min_liquidity = 7;
    step(&env, &fx, "update_asset", || {
        fx.client.update_asset(&fx.asset, &updated)
    });
    step(&env, &fx, "set_formula", || {
        fx.client.set_formula(
            &2,
            &Vec::from_array(&env, [3_500u32, 2_000, 1_500, 1_500, 1_000, 500]),
            &Map::new(&env),
        )
    });
    step(&env, &fx, "set_sub_epoch_secs", || {
        fx.client.set_sub_epoch_secs(&fx.asset, &600)
    });
    step(&env, &fx, "set_event_in_progress", || {
        fx.client.set_event_in_progress(&fx.asset, &true)
    });
    step(&env, &fx, "set_event_band", || {
        fx.client.set_event_band(&fx.asset)
    });
    step(&env, &fx, "clear_event_band", || {
        fx.client.clear_event_band(&fx.asset)
    });
    step(&env, &fx, "disable_asset", || {
        fx.client.disable_asset(&fx.asset)
    });
    step(&env, &fx, "upgrade", || {
        fx.client.upgrade(&env.upload(RiskOracle))
    });
    step(&env, &fx, "migrate", || fx.client.migrate());
}

#[test]
fn hourly_posting_disputes_and_sweeps_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    fx.client
        .add_asset(&asset_config(&env, &fx.asset, &fx.issuer));

    let written = step(&env, &fx, "post_signals (first)", || post(&env, &fx, 5));
    assert!(
        written >= 4,
        "Signals, Ring, RingNewest, FirstEpoch at least"
    );
    step(&env, &fx, "post_signals", || post(&env, &fx, 6));
    step(&env, &fx, "post_signals (backfill)", || post(&env, &fx, 4));

    let disputer = Address::generate(&env);
    step(&env, &fx, "dispute_signals", || {
        fx.client
            .dispute_signals(&disputer, &fx.asset, &5, &hash(&env, 9))
    });
    step(&env, &fx, "resolve_signal_dispute", || {
        fx.client
            .resolve_signal_dispute(&fx.asset, &5, &true, &hash(&env, 3))
    });

    fx.client
        .dispute_signals(&disputer, &fx.asset, &6, &hash(&env, 9));
    env.ledger()
        .set_timestamp(time_at_epoch(6) + crate::SIGNAL_DISPUTE_RULING_SECS + 1);
    step(&env, &fx, "resolve_signal_dispute_timeout", || {
        fx.client.resolve_signal_dispute_timeout(&fx.asset, &6)
    });

    post(&env, &fx, 7);
    env.ledger()
        .set_timestamp(time_at_epoch(7) + crate::SIGNAL_DISPUTE_SECS + 1);
    step(&env, &fx, "finalize_endpoint", || {
        fx.client.finalize_endpoint(&fx.asset, &7)
    });
    env.ledger().set_timestamp(time_at_epoch(7) + 30 * 86_400);
    step(&env, &fx, "check_stale", || {
        fx.client.check_stale(&fx.asset);
    });
    let _ = IssuerActions::default();
}

#[test]
fn sub_epoch_posting_disputes_and_build_hour_extend_what_they_touch() {
    let env = Env::default();
    let fx = setup(&env);
    fx.client
        .add_asset(&asset_config(&env, &fx.asset, &fx.issuer));
    let hour = 497_000u64;
    let per_hour = 12u32;

    for sub in 0..per_hour {
        env.ledger()
            .set_timestamp(hour * 3_600 + (sub as u64 + 1) * 300);
        let mut s = signal_set(&env, hour, 10_000_000);
        s.supply_change_bps = 0;
        step(&env, &fx, "post_sub_signals", || {
            fx.client
                .post_sub_signals(&fx.keeper, &fx.asset, &hour, &sub, &s)
        });
    }

    let disputer = Address::generate(&env);
    step(&env, &fx, "dispute_sub_signals", || {
        fx.client
            .dispute_sub_signals(&disputer, &fx.asset, &hour, &0u32, &hash(&env, 4))
    });
    step(&env, &fx, "resolve_sub_signal_dispute", || {
        fx.client
            .resolve_sub_signal_dispute(&fx.asset, &hour, &0u32, &true, &hash(&env, 5))
    });

    fx.client
        .dispute_sub_signals(&disputer, &fx.asset, &hour, &1u32, &hash(&env, 4));
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + crate::SIGNAL_DISPUTE_RULING_SECS + 1);
    step(&env, &fx, "resolve_sub_dispute_timeout", || {
        fx.client
            .resolve_sub_dispute_timeout(&fx.asset, &hour, &1u32)
    });

    step(&env, &fx, "build_hour", || {
        fx.client.build_hour(&fx.asset, &hour)
    });
    assert!(fx.client.is_final(&fx.asset, &hour));
}

/// A disputed `Signals(asset, epoch)` written with the history policy
/// (10 days) is still live when its ruling deadline arrives: the 2 hour
/// dispute window plus the 6 day `SIGNAL_DISPUTE_RULING_SECS`, measured
/// in ledgers at `SECONDS_PER_LEDGER`. At the network minimum (about 7
/// days) that margin would be under a day.
#[test]
fn a_disputed_history_entry_is_still_live_at_its_ruling_deadline() {
    use soroban_sdk::testutils::storage::Persistent as _;
    use sylox_types::ttl::HISTORY_TTL_EXTEND_TO;

    let env = Env::default();
    let fx = setup(&env);
    fx.client
        .add_asset(&asset_config(&env, &fx.asset, &fx.issuer));
    post(&env, &fx, 5);
    let key = crate::storage::DataKey::Signals(fx.asset.clone(), 5);
    let ttl = || -> u32 {
        env.as_contract(&fx.client.address, || {
            env.storage().persistent().get_ttl(&key)
        })
    };
    assert_eq!(
        ttl(),
        HISTORY_TTL_EXTEND_TO,
        "history keys get 10 days, not 365"
    );

    fx.client
        .dispute_signals(&Address::generate(&env), &fx.asset, &5, &hash(&env, 9));
    let deadline_secs = crate::SIGNAL_DISPUTE_SECS + crate::SIGNAL_DISPUTE_RULING_SECS;
    let deadline_ledgers = (deadline_secs / crate::SECONDS_PER_LEDGER) as u32;
    let seq = env.ledger().sequence();
    env.ledger().set_sequence_number(seq + deadline_ledgers);
    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS + 1);

    assert!(
        ttl() > 0,
        "Signals must still be live at the ruling deadline"
    );
    assert!(
        ttl() >= HISTORY_TTL_EXTEND_TO - deadline_ledgers,
        "nothing else shortened it"
    );
    // MockGovernor's committee is temporary storage (test setup only).
    MockGovernorClient::new(&env, &fx.governor).set_committee(&fx.committee);
    fx.client.resolve_signal_dispute_timeout(&fx.asset, &5);
    assert!(fx.client.is_final(&fx.asset, &5));
}
