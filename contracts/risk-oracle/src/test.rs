extern crate std;

mod golden_vectors;
mod property;
mod release_wasm;
mod sub_epochs;
mod ttl;

use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    Address, BytesN, Env, Event as _, FromVal, IntoVal,
};
use sylox_types::{
    Band, EndpointStatus, FxRateSource, IssuerActions, Reference, SignalSet, SlotState,
};

use crate::mocks::{MockFxAdapter, MockGovernor, MockPriceAdapter, MockStaking};
use crate::storage::RING_SLOTS;
use crate::{Error, RiskOracle, RiskOracleClient};

struct Fixture<'a> {
    client: RiskOracleClient<'a>,
    staking: Address,
    governor: Address,
    registry: Address,
}

fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(env);
    let registry = Address::generate(env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(env, &contract_id);
    Fixture {
        client,
        staking,
        governor,
        registry,
    }
}

fn asset_config(env: &Env, asset: &Address, issuer: &Address) -> sylox_types::AssetConfig {
    sylox_types::AssetConfig {
        asset: asset.clone(),
        issuer: issuer.clone(),
        reference: Reference::Usd,
        home_domain: soroban_sdk::String::from_str(env, "example.com"),
        amm_adapters: soroban_sdk::Vec::new(env),
        fx_adapter: None,
        min_liquidity: 100_000_000_000,
        issuer_flags: sylox_types::IssuerFlags::default(),
        enabled: true,
    }
}

// -- Timing helpers (re-review item C7): never hand-compute a
// timestamp or epoch boundary in a test; derive it from the same
// constants the contract itself uses. --

/// The ledger timestamp this crate's convention posts epoch `n` at:
/// its own close time, matching `check_epoch_window`'s `epoch_close`.
fn time_at_epoch(epoch: u64) -> u64 {
    (epoch + 1) * crate::EPOCH_SECS
}

/// The epoch number whose close time is `timestamp`, i.e. the inverse
/// of `time_at_epoch`. Panics if `timestamp` does not fall exactly on
/// an epoch boundary (a test bug, not a valid input).
#[allow(dead_code)]
fn epoch_at_time(timestamp: u64) -> u64 {
    assert_eq!(timestamp % crate::EPOCH_SECS, 0, "not an epoch boundary");
    timestamp / crate::EPOCH_SECS - 1
}

/// The first ledger timestamp at which a score computed from
/// `last_epoch` reads stale, i.e. the smallest `now` for which
/// `is_stale_epoch`'s own condition (`now / EPOCH_SECS -
/// last_epoch > SCORE_STALE_AFTER_EPOCHS`) first holds. Every real
/// call site below exercises `check_stale`/`asset_stale`, the
/// `score()`/`check_stale` family judged against
/// `SCORE_STALE_AFTER_EPOCHS`, never `is_stale`'s own
/// `STALE_AFTER_EPOCHS` (see `is_stale_false_right_after_posting_
/// then_true_after_stale_after_epochs`, which hardcodes its own
/// timestamps against `STALE_AFTER_EPOCHS` instead, deliberately not
/// sharing this helper).
fn first_stale_time(last_epoch: u64) -> u64 {
    (last_epoch + crate::SCORE_STALE_AFTER_EPOCHS + 1) * crate::EPOCH_SECS
}

/// The epoch `SIGNAL_DISPUTE_SECS` after `epoch`'s own close, i.e. how
/// many whole epochs the 2 epoch finality lag (review item C5) spans.
#[allow(dead_code)]
fn finality_lag_epochs() -> u64 {
    crate::SIGNAL_DISPUTE_SECS / crate::EPOCH_SECS
}

/// The backfill window, in epochs: the largest gap `check_epoch_window`
/// still accepts a late posting across.
fn backfill_window_epochs() -> u64 {
    crate::WINDOW_SECS / crate::EPOCH_SECS
}

fn signal_set(env: &Env, epoch: u64, peg_ratio: i128) -> SignalSet {
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
        endpoint: EndpointStatus::Up,
        inputs_hash: BytesN::from_array(env, &[1u8; 32]),
        poster: Address::generate(env),
    }
}

fn setup_with_asset(env: &Env) -> (Fixture<'_>, Address) {
    let fx = setup(env);
    let asset = Address::generate(env);
    let issuer = Address::generate(env);
    fx.client.add_asset(&asset_config(env, &asset, &issuer));
    (fx, asset)
}

fn post(env: &Env, client: &RiskOracleClient, asset: &Address, epoch: u64, peg_ratio: i128) {
    env.ledger().set_timestamp((epoch + 1) * 3_600);
    let keeper = Address::generate(env);
    client.post_signals(&keeper, asset, &signal_set(env, epoch, peg_ratio));
}

/// Posts `epoch - 1`'s supply equal to `epoch`'s, so the Section 11.3
/// supply-change consistency check never rejects a sequential fill.
fn post_sequential(
    env: &Env,
    client: &RiskOracleClient,
    asset: &Address,
    epoch: u64,
    peg_ratio: i128,
) {
    post(env, client, asset, epoch, peg_ratio);
}

/// Finds `epoch`'s slot in `ring(asset)`'s output by epoch identity, not
/// by array position: `ring()` returns slots ordered oldest-first by
/// recency (`storage::get_ring`'s doc comment), which is not the same as
/// `epoch % RING_SLOTS`.
fn find_slot_by_epoch(
    ring: &soroban_sdk::Vec<sylox_types::RingSlot>,
    epoch: u64,
) -> sylox_types::RingSlot {
    ring.iter()
        .find(|slot| slot.epoch == epoch)
        .unwrap_or_else(|| std::panic!("epoch {epoch} not found in ring()"))
}

// -- add_asset / update_asset / disable_asset --

#[test]
fn add_asset_requires_governor_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let cfg = asset_config(&env, &asset, &issuer);

    fx.client.add_asset(&cfg);
    assert_eq!(
        env.auths(),
        [(
            fx.governor.clone(),
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    fx.client.address.clone(),
                    soroban_sdk::symbol_short!("add_asset"),
                    (cfg.clone(),).into_val(&env),
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
    assert_eq!(fx.client.asset_config(&asset), Some(cfg));
}

#[test]
fn add_asset_rejects_an_asset_that_already_exists() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let issuer = Address::generate(&env);
    let result = fx
        .client
        .try_add_asset(&asset_config(&env, &asset, &issuer));
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn update_asset_rejects_a_change_to_reference() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let issuer = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.reference = Reference::Fiat(soroban_sdk::symbol_short!("EUR"), FxRateSource::Official);
    let result = fx.client.try_update_asset(&asset, &cfg);
    assert_eq!(result, Err(Ok(Error::ReferenceImmutable)));
}

#[test]
fn update_asset_allows_a_change_to_min_liquidity() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let issuer = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.min_liquidity = 999;
    fx.client.update_asset(&asset, &cfg);
    assert_eq!(fx.client.asset_config(&asset).unwrap().min_liquidity, 999);
}

#[test]
fn disable_asset_blocks_further_posting() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fx.client.disable_asset(&asset);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let result = fx
        .client
        .try_post_signals(&keeper, &asset, &signal_set(&env, 5, 9_900_000));
    assert_eq!(result, Err(Ok(Error::UnknownAsset)));
}

// -- post_signals: auth, keeper eligibility, epoch window --

#[test]
fn post_signals_requires_keeper_auth() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let s = signal_set(&env, 5, 9_900_000);
    fx.client.post_signals(&keeper, &asset, &s);

    assert_eq!(
        env.auths(),
        [(
            keeper.clone(),
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    fx.client.address.clone(),
                    soroban_sdk::Symbol::new(&env, "post_signals"),
                    (keeper.clone(), asset.clone(), s.clone()).into_val(&env),
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
}

#[test]
fn post_signals_rejects_an_inactive_keeper() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    staking_client.set_active(&keeper, &false);

    let result = fx
        .client
        .try_post_signals(&keeper, &asset, &signal_set(&env, 5, 9_900_000));
    assert_eq!(result, Err(Ok(Error::KeeperNotActive)));
}

#[test]
fn post_signals_writes_signals_and_ring_slot() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let s = signal_set(&env, 5, 9_900_000);
    fx.client.post_signals(&keeper, &asset, &s);

    let stored = fx.client.signals(&asset, &5).expect("signals stored");
    assert_eq!(stored.peg_ratio, 9_900_000);
    assert_eq!(stored.poster, keeper);
    // No Staking.aggregate was set for this (asset, epoch), so the mock
    // returns Unknown; a keeper-supplied Up must not survive either way.
    assert_eq!(stored.endpoint, EndpointStatus::Unknown);

    let ring = fx.client.ring(&asset);
    assert_eq!(ring.len(), RING_SLOTS);
}

#[test]
fn post_signals_sources_endpoint_only_from_staking_aggregate() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    staking_client.set_aggregate(&asset, &5, &EndpointStatus::Degraded);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.endpoint = EndpointStatus::Up; // keeper tries to claim Up
    fx.client.post_signals(&keeper, &asset, &s);

    let stored = fx.client.signals(&asset, &5).unwrap();
    assert_eq!(stored.endpoint, EndpointStatus::Degraded);
}

#[test]
fn post_signals_rejects_unknown_asset() {
    let env = Env::default();
    let fx = setup(&env);
    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let asset = Address::generate(&env);
    let s = signal_set(&env, 5, 9_900_000);
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::UnknownAsset)));
}

#[test]
fn post_signals_rejects_an_epoch_not_yet_closed() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // At timestamp 3_600 the current epoch is 1; epoch 1 has not closed yet.
    env.ledger().set_timestamp(3_600);
    let keeper = Address::generate(&env);
    let s = signal_set(&env, 1, 9_900_000);
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::WrongEpoch)));
}

#[test]
fn post_signals_rejects_an_epoch_older_than_window_secs() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // window_secs = 259_200 (72h) = 72 epochs at the default epoch_secs.
    env.ledger().set_timestamp(3_600 + 259_200 + 3_600);
    let keeper = Address::generate(&env);
    let s = signal_set(&env, 0, 9_900_000);
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::WrongEpoch)));
}

#[test]
fn post_signals_rejects_a_duplicate_posting_for_the_same_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    fx.client
        .post_signals(&keeper, &asset, &signal_set(&env, 5, 9_900_000));
    let result = fx
        .client
        .try_post_signals(&keeper, &asset, &signal_set(&env, 5, 9_800_000));
    assert_eq!(result, Err(Ok(Error::EpochAlreadyPosted)));
}

// -- post_signals: Section 11.3 sanity bounds, in full --

#[test]
fn post_signals_rejects_peg_ratio_outside_sanity_bounds() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.peg_ratio = 2 * sylox_types::SCALE + 1;
    s.peg_ratio_p10 = s.peg_ratio; // keep p10 <= peg_ratio so only this bound fires
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_rejects_peg_ratio_p10_above_peg_ratio() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.peg_ratio_p10 = s.peg_ratio + 1;
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_accepts_peg_ratio_p10_equal_to_peg_ratio() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.peg_ratio_p10 = s.peg_ratio;
    fx.client.post_signals(&keeper, &asset, &s);
    assert!(fx.client.signals(&asset, &5).is_some());
}

#[test]
fn post_signals_rejects_negative_liquidity() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.liquidity_2pct = -1;
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_rejects_liquidity_above_the_upper_bound() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.liquidity_2pct = i128::MAX / sylox_types::SCALE + 1;
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_rejects_negative_supply() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.supply = -1;
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_rejects_supply_above_the_upper_bound() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    env.ledger().set_timestamp(10 * 3_600);
    let keeper = Address::generate(&env);
    let mut s = signal_set(&env, 5, 9_900_000);
    s.supply = i128::MAX / sylox_types::SCALE + 1;
    let result = fx.client.try_post_signals(&keeper, &asset, &s);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_rejects_a_supply_change_bps_inconsistent_with_the_previous_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp(6 * 3_600);
    let mut first = signal_set(&env, 5, 9_900_000);
    first.supply = 10_000_000_000_000;
    fx.client.post_signals(&keeper, &asset, &first);

    env.ledger().set_timestamp(7 * 3_600);
    let mut second = signal_set(&env, 6, 9_900_000);
    second.supply = 11_000_000_000_000; // true change is 1,000 bps (10%)
    second.supply_change_bps = 0; // posted value is wrong
    let result = fx.client.try_post_signals(&keeper, &asset, &second);
    assert_eq!(result, Err(Ok(Error::SanityBoundFailed)));
}

#[test]
fn post_signals_accepts_a_supply_change_bps_consistent_with_the_previous_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp(6 * 3_600);
    let mut first = signal_set(&env, 5, 9_900_000);
    first.supply = 10_000_000_000_000;
    fx.client.post_signals(&keeper, &asset, &first);

    env.ledger().set_timestamp(7 * 3_600);
    let mut second = signal_set(&env, 6, 9_900_000);
    second.supply = 11_000_000_000_000;
    second.supply_change_bps = 1_000; // correct: 10% = 1,000 bps
    fx.client.post_signals(&keeper, &asset, &second);
    assert!(fx.client.signals(&asset, &6).is_some());
}

#[test]
fn post_signals_accepts_any_supply_change_bps_for_the_first_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp(3_600);
    let mut s = signal_set(&env, 0, 9_900_000);
    s.supply_change_bps = 999_999; // nothing to compare epoch 0 against
    fx.client.post_signals(&keeper, &asset, &s);
    assert!(fx.client.signals(&asset, &0).is_some());
}

// -- ring buffer: epoch identity, never array position (required change #3) --

#[test]
fn ring_wraps_at_ring_slots_and_identifies_slots_by_epoch_not_position() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    let epoch_a: u64 = 1;
    let epoch_b: u64 = epoch_a + RING_SLOTS as u64; // same position as epoch_a

    post(&env, &fx.client, &asset, epoch_a, 9_900_000);
    assert_eq!(
        fx.client.signals(&asset, &epoch_a).unwrap().peg_ratio,
        9_900_000
    );

    env.ledger().set_timestamp((epoch_b + 1) * 3_600);
    fx.client
        .post_signals(&keeper, &asset, &signal_set(&env, epoch_b, 9_500_000));

    // get_ring, read by epoch identity via get(0) is wrong by construction
    // now; instead confirm the SIGNALS record for epoch_a is untouched
    // (position based overwrite would not affect this) while the ring
    // position that both epochs share now resolves, by epoch lookup, only
    // to epoch_b.
    assert_eq!(
        fx.client.signals(&asset, &epoch_a).unwrap().peg_ratio,
        9_900_000,
        "epoch_a's own Signals record must be untouched by epoch_b's post"
    );
}

/// Wraps the ring more than twice and proves that a window read aligned
/// to a set of known epochs returns exactly those epochs' data, not
/// whatever happens to sit at the matching array positions. This is the
/// test the review explicitly asked for: "wrap the ring at least twice
/// and prove windows and aggregates read the right epochs."
#[test]
fn window_reads_the_right_epochs_after_wrapping_twice() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    // Post 2*RING_SLOTS + 10 consecutive epochs (more than two full wraps),
    // each with a peg_ratio equal to its own epoch number (offset so it is
    // never zero), so a window read can check "did I get epoch N's data"
    // unambiguously.
    let total_epochs = 2 * RING_SLOTS as u64 + 10;
    for epoch in 0..total_epochs {
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, 1_000_000 + epoch as i128);
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let newest = total_epochs - 1;
    let contract_id = fx.client.address.clone();
    env.as_contract(&contract_id, || {
        let window = crate::storage::get_window(&env, &asset, newest - 9, 10);
        for (i, slot) in window.iter().enumerate() {
            let expected_epoch = newest - 9 + i as u64;
            let slot = slot.expect("recent epoch must not be missing");
            assert_eq!(slot.epoch, expected_epoch);
            assert_eq!(slot.peg_ratio, 1_000_000 + expected_epoch as i128);
        }

        // The ring has wrapped more than twice; positions now hold only
        // the newest epoch congruent to them. A window straddling an old,
        // long overwritten epoch range must report those epochs as
        // missing, never return a wrong epoch's data for them.
        let stale_window = crate::storage::get_window(&env, &asset, 0, 10);
        for slot in stale_window.iter() {
            assert!(
                slot.is_none(),
                "an epoch whose position has been overwritten many times over must read as missing, not as some other epoch's data"
            );
        }
    });
}

/// Required change #4: a write must never overwrite a slot that holds a
/// newer epoch than the incoming one.
#[test]
fn write_ring_slot_refuses_to_overwrite_a_newer_epoch() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(
        RiskOracle,
        (
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ),
    );
    let asset = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let newer_epoch = RING_SLOTS as u64 + 5; // same position as epoch 5
        let older_epoch = 5u64;

        let signals = signal_set(&env, newer_epoch, 9_900_000);
        let wrote_newer =
            crate::storage::write_ring_slot(&env, &asset, newer_epoch, &signals, 100, None);
        assert!(wrote_newer);

        let stale_signals = signal_set(&env, older_epoch, 9_000_000);
        let wrote_older =
            crate::storage::write_ring_slot(&env, &asset, older_epoch, &stale_signals, 100, None);
        assert!(
            !wrote_older,
            "writing an older epoch over a position that holds a newer one must be refused"
        );

        // The position must still hold newer_epoch's data, untouched.
        let slot = crate::storage::get_slot(&env, &asset, newer_epoch).unwrap();
        assert_eq!(slot.peg_ratio, 9_900_000);
        assert!(crate::storage::get_slot(&env, &asset, older_epoch).is_none());
    });
}

// -- ring buffer header (required change #5) --

#[test]
#[should_panic(expected = "ring buffer layout mismatch")]
fn get_ring_packed_rejects_a_header_that_does_not_match() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(
        RiskOracle,
        (
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ),
    );
    let asset = Address::generate(&env);

    env.as_contract(&contract_id, || {
        // Write a well formed ring first so a Ring(asset) entry exists...
        let signals = signal_set(&env, 0, 9_900_000);
        crate::storage::write_ring_slot(&env, &asset, 0, &signals, 100, None);

        // ...then corrupt its header directly and confirm the next read
        // refuses to interpret the body under the wrong layout.
        crate::storage::corrupt_header_for_test(&env, &asset);
        let _ = crate::storage::get_ring(&env, &asset);
    });
}

// -- score, band, hysteresis, staleness --

/// Posts `epochs` consecutive epochs with a constant `peg_ratio` and
/// `liquidity`, and sets the mock Staking aggregate to `Up` for every one
/// of them, so a "healthy signals" test is not accidentally scored
/// against `EndpointStatus::Unknown` (the mock's default when no
/// aggregate was ever set, which `post_signals` would otherwise source
/// for real per Section 7.4).
///
/// Posts 2 extra epochs beyond `epochs` (review item C5: a slot is only
/// EFFECTIVELY final once `pending_until` passes, `signal_dispute_secs`
/// after it was posted, which at the default parameters is 2 epochs;
/// posting in strict order means epoch N only becomes final once epoch
/// N+2 posts) so that after this call, `epochs` consecutive epochs
/// starting at 0 are all effectively final, not just posted. Then
/// finalize_endpoint sweeps any last-epoch finality forward explicitly,
/// so the caller does not depend on a further post_signals call to
/// notice it (review items C1, C5's lazy finalization: this is the test
/// side exercising it rather than waiting on it).
fn fill_ring_with_constant_signal(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
    epochs: u64,
    peg_ratio: i128,
    liquidity: i128,
) {
    fill_ring_with_constant_signal_from(
        env, client, staking, asset, 0, epochs, peg_ratio, liquidity,
    );
}

/// PR #25 review: same as `fill_ring_with_constant_signal`, but
/// starting from `start_epoch` instead of always 0, so a test can
/// exercise a realistic, unix-time-derived epoch range (hundreds of
/// thousands) instead of only the epoch-0 case every existing test
/// used, which hid the absolute-epoch-vs-window-length bug this
/// review found: a comparison like `newest_final + 1 <
/// AGGREGATE_SLOTS_7D` is trivially true near epoch 0 for the right
/// reason (too little history) but never true on a real network
/// (epoch numbers themselves are always far larger than any window
/// length), for the wrong reason.
#[allow(clippy::too_many_arguments)]
fn fill_ring_with_constant_signal_from(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
    start_epoch: u64,
    epochs: u64,
    peg_ratio: i128,
    liquidity: i128,
) {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    let total = epochs + 2;
    for i in 0..total {
        let epoch = start_epoch + i;
        staking_client.set_aggregate(asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(env, epoch, peg_ratio);
        s.liquidity_2pct = liquidity;
        s.supply_change_bps = 0;
        client.post_signals(&keeper, asset, &s);
    }
    client.finalize_endpoint(asset, &(start_epoch + epochs - 1));
}

#[test]
fn score_is_stale_with_fewer_than_168_epochs_posted() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        10,
        10_000_000,
        500_000_000_000,
    );
    let score = fx.client.score(&asset);
    assert!(score.stale);
}

/// PR #25 review: `recompute_score`'s own history guard
/// (`newest_final + 1 < AGGREGATE_SLOTS_7D`) compared an absolute
/// epoch number to 168. On a real network epoch numbers are
/// unix-time-derived, around 497,000 as of this fix, always far
/// larger than 168, so that guard never actually fired; a new asset
/// got scored from however many epochs it had posted instead of
/// reading stale until it had a genuine 7 days (168 epochs) of its
/// own history (review decision D2). Every test elsewhere in this
/// suite starts at epoch 0, which hid this: near epoch 0, "newest
/// epoch < 168" and "this asset has fewer than 168 epochs of
/// history" happen to coincide, so the bug is invisible there. This
/// test starts at a realistic epoch instead, where the two diverge.
#[test]
fn score_stays_stale_with_no_stored_score_at_a_realistic_epoch_until_168_real_epochs_exist() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Post one continuous run of 170 epochs (168 real + 2 so the
    // 168th becomes effectively final via post_signals' own
    // on-backfill finality sweep, matching
    // fill_ring_with_constant_signal's own convention), checking the
    // score after the 24th and after the 168th.
    for i in 0..170u64 {
        let epoch = REALISTIC_EPOCH_BASE + i;
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);

        if i == 23 {
            // Just posted the 24th epoch (i is 0-indexed); its own
            // finality is still 2 epochs away, but score() must
            // already read stale with nothing stored regardless of
            // finality, since 24 real epochs is nowhere near 168.
            let score = fx.client.score(&asset);
            assert!(
                score.stale,
                "24 epochs at a realistic epoch base must still read stale"
            );
            env.as_contract(&contract_id, || {
                assert!(
                    crate::storage::get_score(&env, &asset).is_none(),
                    "recompute_score must never have written a score from \
                     only 24 epochs of real history, regardless of how \
                     large the absolute epoch number is"
                );
            });
        }
    }

    let score = fx.client.score(&asset);
    assert!(
        !score.stale,
        "168 epochs of real history at a realistic epoch base must produce a score"
    );
    env.as_contract(&contract_id, || {
        assert!(
            crate::storage::get_score(&env, &asset).is_some(),
            "recompute_score must have written a score once 168 real epochs exist"
        );
    });
}

/// PR #27 review (round 2): `FirstEpoch` only proves CALENDAR time has
/// elapsed since the asset's first post; it says nothing about whether
/// the 168-epoch window `aggregate_from_ring` actually reads is
/// populated. Post epoch F, go dark for 168+ epochs (7+ real days of
/// silence), then post exactly one more epoch and let it finalize. The
/// `FirstEpoch`-based guard (`newest_final + 1 < first_epoch + 168`)
/// is satisfied by calendar time alone and does not block this, but
/// the window `aggregate_from_ring` reads (`[newest_final+1-168,
/// newest_final]`) no longer contains epoch F at all, so at most one
/// of its 168 slots holds real data. This must not produce a live
/// score from a 167/168-empty window. Exercises the new
/// `MIN_FINAL_EPOCHS_7D` minimum-count gate in `aggregate_from_ring`.
#[test]
fn score_must_not_resume_from_a_single_epoch_after_a_168_epoch_gap() {
    const REALISTIC_EPOCH_BASE: u64 = 497_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    let first_epoch = REALISTIC_EPOCH_BASE;
    staking_client.set_aggregate(&asset, &first_epoch, &EndpointStatus::Up);
    env.ledger().set_timestamp((first_epoch + 1) * 3_600);
    let mut s0 = signal_set(&env, first_epoch, 10_000_000);
    s0.liquidity_2pct = 500_000_000_000;
    s0.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s0);

    // Go dark for 170 epochs (> 168, i.e. > 7 real days), then post
    // exactly one more epoch, promptly (no backdating, no WrongEpoch
    // risk): this clears the FirstEpoch calendar guard
    // (first_epoch + 170 + 1 > first_epoch + 168) while leaving the
    // 168-epoch window almost entirely empty.
    let resumed_epoch = first_epoch + 170;
    staking_client.set_aggregate(&asset, &resumed_epoch, &EndpointStatus::Up);
    env.ledger().set_timestamp((resumed_epoch + 1) * 3_600);
    let mut s1 = signal_set(&env, resumed_epoch, 10_000_000);
    s1.liquidity_2pct = 500_000_000_000;
    s1.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s1);

    // Let resumed_epoch finalize (SIGNAL_DISPUTE_SECS = 2 epochs) by
    // posting one more epoch past it; post_signals' own finality
    // sweep then calls recompute_score(resumed_epoch).
    let trailing_epoch = resumed_epoch + 2;
    staking_client.set_aggregate(&asset, &trailing_epoch, &EndpointStatus::Up);
    env.ledger().set_timestamp((trailing_epoch + 1) * 3_600);
    let mut s2 = signal_set(&env, trailing_epoch, 10_000_000);
    s2.liquidity_2pct = 500_000_000_000;
    s2.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s2);

    let score = fx.client.score(&asset);
    assert!(
        score.stale,
        "a 168-epoch window with only 1 real data point must not \
         produce a live score; FirstEpoch alone (calendar time) is \
         not sufficient, a minimum-count-of-real-epochs check is \
         also required"
    );
    env.as_contract(&contract_id, || {
        assert!(
            crate::storage::get_score(&env, &asset).is_none(),
            "recompute_score must never have written a score from a \
             window with only 1 real data point out of 168"
        );
    });
}

/// Exact boundary of `MIN_FINAL_EPOCHS_7D` (96 = 168 - 72, one full
/// backfill window's worth of tolerated gap): a window with exactly
/// 96 real epochs (the other 72 missing in the middle, matching the
/// system's own designed backfill tolerance) must produce a real
/// score. The asset's very first post is at epoch 0, so `first_epoch`
/// = 0 and the calendar guard is satisfied the same way as every
/// other realistic-epoch-base test, isolating this test to the
/// minimum-count gate alone.
#[test]
fn score_is_written_with_exactly_the_minimum_final_epochs_in_the_window() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Epoch 0 only (sets first_epoch), then a 72 epoch gap, then 95
    // more real epochs (1 + 95 = 96 real epochs in [0, 167]), then 2
    // trailing epochs so epoch 167 itself finalizes.
    let post = |epoch: u64| {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    };
    post(0);
    for epoch in 73..170u64 {
        post(epoch);
    }

    let score = fx.client.score(&asset);
    assert!(
        !score.stale,
        "exactly 96 real epochs (168 - 72, one full backfill window \
         of gap) must be enough to produce a score"
    );
    env.as_contract(&contract_id, || {
        assert!(
            crate::storage::get_score(&env, &asset).is_some(),
            "recompute_score must have written a score from exactly \
             96 real epochs in the window"
        );
    });
}

#[test]
fn score_stays_stale_with_one_fewer_than_the_minimum_final_epochs_in_the_window() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Epoch 0 only (sets first_epoch), then a 73 epoch gap, then 94
    // more real epochs (1 + 94 = 95 real epochs in [0, 167]), one
    // fewer than the minimum, then 2 trailing epochs so epoch 167
    // itself finalizes.
    let post = |epoch: u64| {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    };
    post(0);
    for epoch in 74..170u64 {
        post(epoch);
    }

    let score = fx.client.score(&asset);
    assert!(
        score.stale,
        "95 real epochs, one fewer than MIN_FINAL_EPOCHS_7D (96), \
         must not produce a score"
    );
    env.as_contract(&contract_id, || {
        assert!(
            crate::storage::get_score(&env, &asset).is_none(),
            "recompute_score must never have written a score from \
             only 95 real epochs in the window"
        );
    });
}

#[test]
fn score_is_normal_band_with_healthy_signals() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    // peg_ratio_p10 == SCALE (no deviation), good liquidity, no issuer
    // actions, no redemption pressure, no supply shock, endpoint Up: every
    // component should be 0.
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        sylox_types::SCALE,
        100_000_000_000, // >= min_liquidity, so component L is 0 too
    );
    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(score.band, Band::Normal);
    assert_eq!(score.score, 0);
}

/// Review item C3: a single epoch's bad `peg_ratio` inside the 72 epoch
/// Depeg window must not move the band, because component P is the 10th
/// percentile of the window, computed onchain, not a single value. One
/// severely depegged epoch among 72 healthy ones sorts to the bottom of
/// the window and falls below the `len / 10 = 7` index `percentile_10`
/// reads, so it never reaches component P at all.
#[test]
fn percentile_p10_ignores_a_single_epoch_wick() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        // The Depeg window is the newest 72 of the 168 posted (epochs
        // 96..168); epoch 96, the oldest epoch inside it, carries the
        // single wick. Epochs 168 and 169 are flush epochs (review item
        // C5: a slot is only final 2 epochs after it posts).
        if epoch == 96 {
            s.peg_ratio = 1_000_000;
            s.peg_ratio_p10 = 1_000_000; // severely depegged, one epoch only
        }
        fx.client.post_signals(&keeper, &asset, &s);
    }
    fx.client.finalize_endpoint(&asset, &167);

    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(
        score.band,
        Band::Normal,
        "a single wick inside the Depeg window must not move the band through component P"
    );
    assert_eq!(score.score, 0);
}

/// The mirror case: enough bad epochs inside the Depeg window (more than
/// 10% of it) DO reach the percentile and move component P, confirming
/// the wick test above is not passing merely because component P is
/// broken rather than because the wick is correctly excluded.
#[test]
fn percentile_p10_reflects_a_sustained_depeg_in_the_window() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        // 20 of the newest 72 epochs (96..168) are depegged: well over
        // the 10th percentile index (7), so it must land on a depegged
        // value this time. Epochs 168 and 169 are flush epochs (review
        // item C5).
        if (96..116).contains(&epoch) {
            s.peg_ratio = 1_000_000;
            s.peg_ratio_p10 = 1_000_000;
        }
        fx.client.post_signals(&keeper, &asset, &s);
    }
    fx.client.finalize_endpoint(&asset, &167);

    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_ne!(
        score.band,
        Band::Normal,
        "a sustained depeg covering more than the 10th percentile must move the band"
    );
}

/// Posts `epochs` epochs (starting at `start_epoch`) that max out every
/// score component, so the weighted sum (weights sum to 10,000 bps = 100
/// points) reaches Distress (>=75) with room to spare: P=100 (peg_ratio_p10
/// at 0.5, 50% deviation, over d_max=0.10), E=100 (endpoint Down), L=100
/// (liquidity 0 against min_liquidity), I=100 (large clawback and many
/// auth revocations), R=100 (large redemption outflow relative to
/// supply). P+E+L+I+R alone already weighs 3500+2000+1000+1500+1500 =
/// 9500 of the 10,000 bps total.
///
/// Posts 2 extra distressed epochs beyond `epochs` (review item C5: a
/// slot is only effectively final `signal_dispute_secs` after it posts,
/// 2 epochs at the default parameters, and posting in strict order
/// means epoch N only becomes final once epoch N+2 posts), then sweeps
/// finality forward with `finalize_endpoint` so the caller does not need
/// a further `post_signals` call to observe it. Returns `start_epoch +
/// epochs + 2`, the next entirely FRESH epoch (one that was never
/// posted, distressed or otherwise) a caller can continue from.
fn fill_ring_distressed(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
    start_epoch: u64,
    epochs: u64,
) -> u64 {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    let total = epochs + 2;
    for i in 0..total {
        let epoch = start_epoch + i;
        staking_client.set_aggregate(asset, &epoch, &EndpointStatus::Down);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(env, epoch, 5_000_000);
        s.liquidity_2pct = 0;
        s.supply_change_bps = 0;
        // redemption_net_24h sums 24 epochs of this value; well over the
        // r_max = 0.10 threshold on its own, so R clamps to 100.
        s.redemption_net = s.supply / 100; // 1% of supply redeemed this epoch
        s.issuer_actions = IssuerActions {
            clawbacks: 1,
            // clawback_amount_7d sums 168 epochs; well over c_max = 0.01
            // on its own, so I clamps to 100 from this term alone.
            clawback_amount: s.supply / 5_000,
            auth_revocations: 1, // auth_revocations_7d sums to 168, over k_max = 20
            flag_changes: 0,
        };
        client.post_signals(&keeper, asset, &s);
    }
    client.finalize_endpoint(asset, &(start_epoch + epochs - 1));
    start_epoch + total
}

#[test]
fn score_is_distress_band_with_a_depegged_price() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(score.band, Band::Distress);
}

/// Posts `count` healthy epochs starting at `start_epoch` (no flush
/// epochs added; the caller decides how many of them actually become
/// final by how many more it posts afterward, or by calling
/// `finalize_endpoint` itself).
fn post_healthy_epochs(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
    start_epoch: u64,
    count: u64,
) {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    for i in 0..count {
        let epoch = start_epoch + i;
        staking_client.set_aggregate(asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        client.post_signals(&keeper, asset, &s);
    }
}

#[test]
fn score_hysteresis_requires_consecutive_epochs_to_move_down() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // Reach Distress first.
    let next_epoch = fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    let distressed = fx.client.score(&asset);
    assert_eq!(distressed.band, Band::Distress);

    // Post exactly 1 healthy epoch worth of new data (plus the 2 more
    // postings needed for ANY epoch to become final at all, review item
    // C5), so exactly 1 new final epoch is observed: band_down_epochs
    // (3) has not elapsed, so the band must not move yet.
    post_healthy_epochs(&env, &fx.client, &fx.staking, &asset, next_epoch, 3);
    let after_one = fx.client.score(&asset);
    assert_eq!(
        after_one.band,
        Band::Distress,
        "band_down_epochs (3) has not elapsed yet: only 1 new final epoch has been observed"
    );
}

#[test]
fn score_moves_down_after_band_down_epochs_consecutive_qualifying_epochs() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let next_epoch = fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    assert_eq!(fx.client.score(&asset).band, Band::Distress);

    // 3 final healthy epochs needs 3 + 2 postings (review item C5's
    // finality lag), then one more call to let the last of them settle.
    post_healthy_epochs(&env, &fx.client, &fx.staking, &asset, next_epoch, 5);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    let flush_epoch = next_epoch + 5;
    staking_client.set_aggregate(&asset, &flush_epoch, &EndpointStatus::Up);
    env.ledger().set_timestamp((flush_epoch + 1) * 3_600);
    let mut flush = signal_set(&env, flush_epoch, sylox_types::SCALE);
    flush.liquidity_2pct = 100_000_000_000;
    flush.supply_change_bps = 0;
    fx.client
        .post_signals(&Address::generate(&env), &asset, &flush);

    let result = fx.client.score(&asset);
    assert_ne!(
        result.band,
        Band::Distress,
        "3 consecutive final qualifying epochs must move the band down"
    );
}

#[test]
fn set_event_band_forces_event_and_is_sticky_until_cleared() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        sylox_types::SCALE,
        100_000_000_000,
    );
    assert_eq!(fx.client.score(&asset).band, Band::Normal);

    fx.client.set_event_band(&asset);
    assert_eq!(fx.client.band(&asset), Band::Event);

    // Posting more healthy signals must not move it out of Event.
    // fill_ring_with_constant_signal already posted epochs 0..170 (168
    // plus 2 flush epochs, review item C5), so the next fresh epoch is
    // 170.
    let keeper = Address::generate(&env);
    env.ledger().set_timestamp(171 * 3_600);
    fx.client.post_signals(&keeper, &asset, &{
        let mut s = signal_set(&env, 170, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        s
    });
    assert_eq!(fx.client.band(&asset), Band::Event);

    fx.client.clear_event_band(&asset);
    assert_eq!(fx.client.band(&asset), Band::Normal);
}

#[test]
fn set_event_band_requires_registry_auth() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fx.client.set_event_band(&asset);
    assert_eq!(
        env.auths(),
        [(
            fx.registry.clone(),
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    fx.client.address.clone(),
                    soroban_sdk::Symbol::new(&env, "set_event_band"),
                    (asset.clone(),).into_val(&env),
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
}

#[test]
fn is_stale_true_before_any_posting() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    assert!(fx.client.is_stale(&asset));
}

#[test]
fn is_stale_false_right_after_posting_then_true_after_stale_after_epochs() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);
    env.ledger().set_timestamp(6 * 3_600);
    assert!(!fx.client.is_stale(&asset));

    // stale_after_epochs = 3: posting epoch 5, now at epoch 5 + 4 = 9 must
    // already be stale (more than 3 epochs since the last posting).
    env.ledger().set_timestamp(10 * 3_600);
    assert!(fx.client.is_stale(&asset));
}

// -- median_liquidity --

#[test]
fn median_liquidity_is_zero_before_168_epochs() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(&env, &fx.client, &fx.staking, &asset, 10, 9_900_000, 42);
    assert_eq!(fx.client.median_liquidity(&asset), 0);
}

#[test]
fn median_liquidity_over_168_constant_slots() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(&env, &fx.client, &fx.staking, &asset, 168, 9_900_000, 777);
    assert_eq!(fx.client.median_liquidity(&asset), 777);
}

// -- reference_rate --

#[test]
fn reference_rate_usd_is_scale() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    assert_eq!(fx.client.reference_rate(&asset), sylox_types::SCALE);
}

#[test]
fn reference_rate_fiat_reads_the_fx_adapter() {
    let env = Env::default();
    let fx = setup(&env);
    let fx_adapter_id = env.register(MockFxAdapter, ());
    let fx_adapter_client = crate::mocks::MockFxAdapterClient::new(&env, &fx_adapter_id);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.reference = Reference::Fiat(soroban_sdk::symbol_short!("ARS"), FxRateSource::Official);
    cfg.fx_adapter = Some(fx_adapter_id.clone());
    fx.client.add_asset(&cfg);

    env.ledger().set_timestamp(1_000);
    fx_adapter_client.set_rate(
        &soroban_sdk::symbol_short!("ARS"),
        &FxRateSource::Official,
        &123_456,
        &1_000,
    );
    assert_eq!(fx.client.reference_rate(&asset), 123_456);
}

#[test]
fn add_asset_rejects_reference_asset() {
    // Review decision D3: Reference::Asset is rejected in v1, so it can
    // never actually reach storage, and reference_rate's own Asset branch
    // (ReferenceRateUnavailable) is unreachable through the public API;
    // kept in reference_rate only as a defensive fallback.
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let other_asset = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.reference = Reference::Asset(other_asset);

    let result = fx.client.try_add_asset(&cfg);
    assert_eq!(result, Err(Ok(Error::ReferenceNotSupported)));
}

#[test]
fn update_asset_rejects_reference_asset() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let issuer = Address::generate(&env);
    let other_asset = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.reference = Reference::Asset(other_asset);

    let result = fx.client.try_update_asset(&asset, &cfg);
    assert_eq!(result, Err(Ok(Error::ReferenceNotSupported)));
}

// -- reward_keeper on the finality scan (issue #11 fix, feat/treasury) --

#[test]
fn one_keeper_is_rewarded_exactly_once_per_newly_final_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);

    env.ledger().set_timestamp(time_at_epoch(0));
    fx.client
        .post_signals(&keeper, &asset, &signal_set(&env, 0, 9_900_000));

    // Finality for epoch 0 is still pending (SIGNAL_DISPUTE_SECS has
    // not elapsed); the scan that already ran inside post_signals
    // found nothing new to reward yet.
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper), 0);

    // Advance past the dispute window and touch the asset again
    // (finalize_endpoint is the permissionless sweep trigger): epoch
    // 0 is now observed Final for the first time, and the keeper who
    // posted it is rewarded for exactly 1 epoch.
    env.ledger()
        .set_timestamp(time_at_epoch(0) + crate::SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize_endpoint(&asset, &0);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper), 1);

    // A second sweep (any later state-changing call) must not reward
    // the same already-announced epoch again: the mock records only
    // the most recent call's own epochs argument, so if reward_keeper
    // were called again for this keeper with a nonzero count, this
    // would still read 1, not 2; call_count being unchanged below is
    // the stronger, unambiguous check that reward_keeper was not
    // invoked a second time at all.
    let calls_before = staking_client.call_count(&soroban_sdk::Symbol::new(&env, "reward_keeper"));
    fx.client.finalize_endpoint(&asset, &0);
    let calls_after = staking_client.call_count(&soroban_sdk::Symbol::new(&env, "reward_keeper"));
    assert_eq!(calls_before, calls_after);
}

#[test]
fn two_keepers_posting_different_epochs_are_each_rewarded_their_own_count_in_one_sweep() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper_a = Address::generate(&env);
    let keeper_b = Address::generate(&env);

    // keeper_a posts epochs 0 and 1; keeper_b posts epoch 2, all
    // close enough together that none of the three individually
    // crosses into Final (SIGNAL_DISPUTE_SECS past its own post
    // time) until the single later finalize_endpoint call below,
    // which observes all three as newly Final in one sweep. Posting
    // epoch 2 only 2 * EPOCH_SECS after epoch 0 (not at its exact
    // pending_until boundary) keeps this test from depending on
    // whichever post happens to land exactly when an earlier
    // epoch's own window closes.
    env.ledger().set_timestamp(time_at_epoch(0));
    fx.client
        .post_signals(&keeper_a, &asset, &signal_set(&env, 0, 9_900_000));
    env.ledger().set_timestamp(time_at_epoch(1));
    fx.client
        .post_signals(&keeper_a, &asset, &signal_set(&env, 1, 9_900_000));
    env.ledger().set_timestamp(time_at_epoch(2));
    fx.client
        .post_signals(&keeper_b, &asset, &signal_set(&env, 2, 9_900_000));

    let calls_before_sweep =
        staking_client_for(&env, &fx).call_count(&soroban_sdk::Symbol::new(&env, "reward_keeper"));

    env.ledger()
        .set_timestamp(time_at_epoch(2) + crate::SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize_endpoint(&asset, &2);

    let staking_client = staking_client_for(&env, &fx);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper_a), 2);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper_b), 1);
    // Exactly one reward_keeper call per distinct poster in this one
    // sweep (2 posters), never one call per epoch (which would have
    // been 3): checked as a delta across just this sweep, since
    // earlier posts may themselves have already triggered a reward
    // if an epoch happened to cross into Final mid-sequence.
    let calls_after_sweep =
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "reward_keeper"));
    assert_eq!(calls_after_sweep - calls_before_sweep, 2);
}

fn staking_client_for<'a>(env: &'a Env, fx: &Fixture<'a>) -> crate::mocks::MockStakingClient<'a> {
    crate::mocks::MockStakingClient::new(env, &fx.staking)
}

#[test]
fn an_overturned_epoch_is_never_rewarded() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    let keeper = Address::generate(&env);
    env.ledger().set_timestamp(time_at_epoch(0));
    client.post_signals(&keeper, &asset, &signal_set(&env, 0, 9_900_000));

    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &0, &BytesN::from_array(&env, &[9u8; 32]));
    // Disputer wins: the slot is Overturned, never Final.
    client.resolve_signal_dispute(&asset, &0, &false, &BytesN::from_array(&env, &[0u8; 32]));

    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper), 0);

    // Even well past the point epoch 0 would have become Final had it
    // stood, no later sweep ever rewards the keeper for it: the slot
    // is Overturned, not Final, so it can never appear in a finality
    // scan's newly_announced set.
    env.ledger()
        .set_timestamp(time_at_epoch(0) + crate::SIGNAL_DISPUTE_SECS + 10_000);
    client.finalize_endpoint(&asset, &0);
    assert_eq!(staking_client.reward_keeper_epochs(&keeper), 0);
}

// -- dispute_signals / resolve_signal_dispute --

#[test]
fn dispute_signals_locks_a_bond_and_marks_the_slot_disputed() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);

    let disputer = Address::generate(&env);
    let alt_hash = BytesN::from_array(&env, &[9u8; 32]);
    fx.client.dispute_signals(&disputer, &asset, &5, &alt_hash);

    let slot = find_slot_by_epoch(&fx.client.ring(&asset), 5);
    assert_eq!(slot.state, SlotState::Disputed);

    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "lock_bond")),
        1
    );
}

#[test]
fn dispute_signals_rejects_a_second_dispute_on_the_same_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);

    let disputer = Address::generate(&env);
    let alt_hash = BytesN::from_array(&env, &[9u8; 32]);
    fx.client.dispute_signals(&disputer, &asset, &5, &alt_hash);

    let result = fx
        .client
        .try_dispute_signals(&disputer, &asset, &5, &alt_hash);
    assert_eq!(result, Err(Ok(Error::DisputeWindowClosed)));
}

#[test]
fn dispute_signals_rejects_disputing_after_the_window_closes() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);

    // signal_dispute_secs = 7_200 (2h); jump well past it.
    env.ledger().set_timestamp(6 * 3_600 + 3 * 3_600);
    let disputer = Address::generate(&env);
    let alt_hash = BytesN::from_array(&env, &[9u8; 32]);
    let result = fx
        .client
        .try_dispute_signals(&disputer, &asset, &5, &alt_hash);
    assert_eq!(result, Err(Ok(Error::DisputeWindowClosed)));
}

#[test]
fn resolve_signal_dispute_requires_committee_auth_from_governor() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);
    let disputer = Address::generate(&env);
    fx.client
        .dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));

    // RiskOracle was initialized with fx.governor as a plain Address, not
    // a contract; resolve_signal_dispute calls Governor.committee() on it,
    // which has no committee() function, so this must fail rather than
    // silently succeed under mock_all_auths. This test documents that
    // gap: see "Known gaps" in the PR description (RiskOracle needs a
    // real or mock Governor at the governor address to exercise this
    // function end to end).
    let result = fx.client.try_resolve_signal_dispute(
        &asset,
        &5,
        &true,
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    assert!(result.is_err());
}

#[test]
fn resolve_signal_dispute_keeper_wins_finalizes_the_slot() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));
    post(&env, &client, &asset, 5, 9_900_000);

    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));

    client.resolve_signal_dispute(&asset, &5, &true, &BytesN::from_array(&env, &[0u8; 32]));

    let slot = find_slot_by_epoch(&client.ring(&asset), 5);
    assert_eq!(slot.state, SlotState::Final);
}

#[test]
fn resolve_signal_dispute_disputer_wins_reopens_the_epoch() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));
    post(&env, &client, &asset, 5, 9_900_000);

    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));
    client.resolve_signal_dispute(&asset, &5, &false, &BytesN::from_array(&env, &[0u8; 32]));

    // An overturned slot resets to Empty: epoch 5 no longer appears in
    // ring()'s output as itself (an Empty slot carries no real epoch
    // identity), and get_slot(asset, 5) must report it missing.
    assert!(
        client.ring(&asset).iter().all(|slot| slot.epoch != 5),
        "an overturned epoch must not still read as epoch 5 anywhere in the ring"
    );
    assert!(client
        .try_dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[1u8; 32]))
        .is_err());
}

// -- resolve_signal_dispute_timeout (ADR-010, issue #4 fix) --

/// Shared setup for the timeout tests: a real `MockGovernor` (not just
/// a plain `Address`, which `resolve_signal_dispute_requires_committee_auth_from_governor`
/// shows has no `committee()` to call), one asset, one posted and
/// disputed epoch.
#[allow(clippy::type_complexity)]
fn setup_disputed_epoch(
    env: &Env,
) -> (
    RiskOracleClient<'_>,
    Address,
    Address,
    Address,
    Address,
    Address,
) {
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(env, &contract_id);

    let governor_client = crate::mocks::MockGovernorClient::new(env, &governor);
    let committee = Address::generate(env);
    governor_client.set_committee(&committee);

    let asset = Address::generate(env);
    let issuer = Address::generate(env);
    client.add_asset(&asset_config(env, &asset, &issuer));
    post(env, &client, &asset, 5, 9_900_000);

    let disputer = Address::generate(env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(env, &[9u8; 32]));

    (client, asset, staking, committee, disputer, contract_id)
}

#[test]
fn resolve_signal_dispute_timeout_rejects_before_the_ruling_deadline() {
    let env = Env::default();
    let (client, asset, _staking, _committee, _disputer, _contract_id) = setup_disputed_epoch(&env);

    // Still inside signal_dispute_ruling_secs (7d) from the dispute.
    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS - 1);
    let result = client.try_resolve_signal_dispute_timeout(&asset, &5);
    assert_eq!(result, Err(Ok(Error::RulingDeadlineNotReached)));
}

#[test]
fn resolve_signal_dispute_timeout_succeeds_exactly_at_the_deadline() {
    let env = Env::default();
    let (client, asset, _staking, _committee, _disputer, _contract_id) = setup_disputed_epoch(&env);

    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    client.resolve_signal_dispute_timeout(&asset, &5);

    let slot = find_slot_by_epoch(&client.ring(&asset), 5);
    assert_eq!(slot.state, SlotState::Final);
}

#[test]
fn resolve_signal_dispute_timeout_releases_the_bond_never_slashes() {
    let env = Env::default();
    let (client, asset, staking, _committee, _disputer, _contract_id) = setup_disputed_epoch(&env);

    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    client.resolve_signal_dispute_timeout(&asset, &5);

    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "release_bond")),
        1
    );
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "forfeit_bond")),
        0
    );
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "slash")),
        0
    );
}

#[test]
fn resolve_signal_dispute_timeout_records_a_committee_miss() {
    let env = Env::default();
    let (client, asset, _staking, committee, _disputer, contract_id) = setup_disputed_epoch(&env);

    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    client.resolve_signal_dispute_timeout(&asset, &5);

    let misses = env.as_contract(&contract_id, || {
        crate::storage::get_committee_misses(&env, &committee)
    });
    assert_eq!(misses, 1);
}

#[test]
fn resolve_signal_dispute_timeout_is_permissionless() {
    let env = Env::default();
    let (client, asset, _staking, _committee, _disputer, _contract_id) = setup_disputed_epoch(&env);

    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    // No require_auth set up for any particular caller beyond
    // mock_all_auths (which setup_disputed_epoch already enabled);
    // the call succeeding with no committee or governor auth proves
    // this function checks no caller identity at all.
    let result = client.try_resolve_signal_dispute_timeout(&asset, &5);
    assert!(result.is_ok());
}

#[test]
fn resolve_signal_dispute_timeout_rejects_an_epoch_with_no_open_dispute() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);

    let result = fx.client.try_resolve_signal_dispute_timeout(&asset, &5);
    assert_eq!(result, Err(Ok(Error::DisputeWindowClosed)));
}

#[test]
fn resolve_signal_dispute_timeout_cannot_run_twice() {
    let env = Env::default();
    let (client, asset, _staking, _committee, _disputer, _contract_id) = setup_disputed_epoch(&env);

    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    client.resolve_signal_dispute_timeout(&asset, &5);

    // clear_dispute already ran; a second call finds no open dispute.
    let result = client.try_resolve_signal_dispute_timeout(&asset, &5);
    assert_eq!(result, Err(Ok(Error::DisputeWindowClosed)));
}

#[test]
fn resolve_signal_dispute_timeout_does_not_block_a_committee_ruling_that_arrives_first() {
    let env = Env::default();
    let (client, asset, _staking, committee, disputer, contract_id) = setup_disputed_epoch(&env);

    // The committee rules well before the deadline.
    env.ledger().set_timestamp(time_at_epoch(5) + 3_600);
    client.resolve_signal_dispute(&asset, &5, &true, &BytesN::from_array(&env, &[0u8; 32]));

    // The dispute is already cleared; a late timeout call has nothing
    // left to act on, and the committee's on-time ruling is not
    // counted as a miss.
    env.ledger()
        .set_timestamp(time_at_epoch(5) + crate::SIGNAL_DISPUTE_RULING_SECS);
    let result = client.try_resolve_signal_dispute_timeout(&asset, &5);
    assert_eq!(result, Err(Ok(Error::DisputeWindowClosed)));
    let misses = env.as_contract(&contract_id, || {
        crate::storage::get_committee_misses(&env, &committee)
    });
    assert_eq!(misses, 0);
    let _ = disputer;
}

// -- finalize_endpoint --

#[test]
fn finalize_endpoint_books_a_late_aggregate() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post(&env, &fx.client, &asset, 5, 9_900_000);

    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    staking_client.set_aggregate(&asset, &5, &EndpointStatus::Down);

    env.ledger().set_timestamp(7 * 3_600);
    fx.client.finalize_endpoint(&asset, &5);

    assert_eq!(
        fx.client.signals(&asset, &5).unwrap().endpoint,
        EndpointStatus::Down
    );
    assert_eq!(
        staking_client.call_count(&soroban_sdk::Symbol::new(&env, "settle_probes")),
        1
    );
}

#[test]
fn finalize_endpoint_rejects_an_epoch_not_yet_closed() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    env.ledger().set_timestamp(3_600);
    let result = fx.client.try_finalize_endpoint(&asset, &1);
    assert_eq!(result, Err(Ok(Error::WrongEpoch)));
}

// -- set_formula --

#[test]
fn set_formula_rejects_weights_not_summing_to_10000() {
    let env = Env::default();
    let fx = setup(&env);
    let mut weights = soroban_sdk::Vec::new(&env);
    for w in [1_000u32, 1_000, 1_000, 1_000, 1_000, 1_000] {
        weights.push_back(w);
    }
    let result = fx
        .client
        .try_set_formula(&1, &weights, &soroban_sdk::Map::new(&env));
    assert_eq!(result, Err(Ok(Error::WeightsInvalid)));
}

#[test]
fn set_formula_accepts_weights_summing_to_10000() {
    let env = Env::default();
    let fx = setup(&env);
    let mut weights = soroban_sdk::Vec::new(&env);
    for w in [2_000u32, 2_000, 2_000, 2_000, 1_000, 1_000] {
        weights.push_back(w);
    }
    fx.client
        .set_formula(&2, &weights, &soroban_sdk::Map::new(&env));
}

// -- assets / latest --

#[test]
fn assets_lists_every_added_asset_once() {
    let env = Env::default();
    let fx = setup(&env);
    let a1 = Address::generate(&env);
    let a2 = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.client.add_asset(&asset_config(&env, &a1, &issuer));
    fx.client.add_asset(&asset_config(&env, &a2, &issuer));
    let assets = fx.client.assets();
    assert_eq!(assets.len(), 2);
    assert!(assets.contains(&a1));
    assert!(assets.contains(&a2));
}

#[test]
fn latest_returns_the_newest_posted_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    post_sequential(&env, &fx.client, &asset, 5, 9_900_000);
    post_sequential(&env, &fx.client, &asset, 6, 9_850_000);
    assert_eq!(fx.client.latest(&asset).unwrap().epoch, 6);
}

// -- AMM cross check --

#[test]
fn post_signals_rejects_a_peg_ratio_too_far_from_a_price_adapter() {
    let env = Env::default();
    let fx = setup(&env);
    let adapter_id = env.register(MockPriceAdapter, ());
    let adapter_client = crate::mocks::MockPriceAdapterClient::new(&env, &adapter_id);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.amm_adapters.push_back(adapter_id.clone());
    fx.client.add_asset(&cfg);

    env.ledger().set_timestamp(3_600);
    adapter_client.set_price(&asset, &10_000_000, &500_000_000_000, &0);

    env.ledger().set_timestamp(3_600 * 2);
    let keeper = Address::generate(&env);
    // 9,000,000 vs adapter's 10,000,000 is a 1,000 bps gap, over the 300
    // bps default tolerance.
    let result = fx
        .client
        .try_post_signals(&keeper, &asset, &signal_set(&env, 1, 9_000_000));
    assert_eq!(result, Err(Ok(Error::AmmCrossCheckFailed)));
}

#[test]
fn post_signals_skips_the_cross_check_when_adapter_liquidity_is_too_low() {
    let env = Env::default();
    let fx = setup(&env);
    let adapter_id = env.register(MockPriceAdapter, ());
    let adapter_client = crate::mocks::MockPriceAdapterClient::new(&env, &adapter_id);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let mut cfg = asset_config(&env, &asset, &issuer);
    cfg.amm_adapters.push_back(adapter_id.clone());
    fx.client.add_asset(&cfg);

    env.ledger().set_timestamp(3_600);
    // Below cfg.min_liquidity (100_000_000_000): the cross check must skip.
    adapter_client.set_price(&asset, &10_000_000, &1, &0);

    env.ledger().set_timestamp(3_600 * 2);
    let keeper = Address::generate(&env);
    fx.client
        .post_signals(&keeper, &asset, &signal_set(&env, 1, 9_000_000));
    assert!(fx.client.signals(&asset, &1).is_some());
}

// -- C1: score() / band() are pure reads --

#[test]
fn score_is_a_pure_read_calling_it_repeatedly_changes_nothing() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        sylox_types::SCALE,
        100_000_000_000,
    );

    let first = fx.client.score(&asset);
    for _ in 0..10 {
        let repeat = fx.client.score(&asset);
        assert_eq!(
            repeat, first,
            "score() must return the same RiskScore every time with no state changing call in between"
        );
    }
    // band() goes through the same pure read path.
    for _ in 0..10 {
        assert_eq!(fx.client.band(&asset), first.band);
    }
}

#[test]
fn score_down_streak_advances_at_most_once_per_new_final_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let next_epoch = fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    assert_eq!(fx.client.score(&asset).band, Band::Distress);

    // Post exactly 1 new healthy epoch (plus the 2 extra needed for it
    // to become final, review item C5), then call score() many times
    // with no further state changing call. The down streak must not
    // advance on any of these repeated reads: only a genuinely new
    // final epoch (from a further post_signals or finalize_endpoint
    // call) is allowed to advance it.
    post_healthy_epochs(&env, &fx.client, &fx.staking, &asset, next_epoch, 3);
    let after_first_new_final = fx.client.score(&asset);
    for _ in 0..5 {
        let repeat = fx.client.score(&asset);
        assert_eq!(repeat, after_first_new_final);
    }
    assert_eq!(
        after_first_new_final.band,
        Band::Distress,
        "only 1 new final epoch observed; band_down_epochs (3) has not elapsed"
    );
}

// -- D1: set_event_in_progress forces at least Distress --

#[test]
fn set_event_in_progress_forces_at_least_distress_and_applies_immediately() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        sylox_types::SCALE,
        100_000_000_000,
    );
    assert_eq!(fx.client.score(&asset).band, Band::Normal);

    // No new final epoch has arrived; the override must still apply
    // immediately, because score() applies it as a read-time floor on
    // every call rather than needing a recompute to notice the flag
    // changed.
    fx.client.set_event_in_progress(&asset, &true);
    assert_eq!(fx.client.band(&asset), Band::Distress);

    fx.client.set_event_in_progress(&asset, &false);
    assert_eq!(
        fx.client.band(&asset),
        Band::Normal,
        "clearing the override must let the raw score show through again"
    );
}

#[test]
fn set_event_in_progress_does_not_persist_into_the_stored_score() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_with_constant_signal(
        &env,
        &fx.client,
        &fx.staking,
        &asset,
        168,
        sylox_types::SCALE,
        100_000_000_000,
    );
    assert_eq!(fx.client.score(&asset).band, Band::Normal);

    // The override is applied only at read time (score()); the
    // underlying RiskScore this contract actually persists must stay
    // the plain hysteresis band throughout, confirmed here by reading
    // storage directly rather than through the overridden score() read.
    fx.client.set_event_in_progress(&asset, &true);
    assert_eq!(fx.client.band(&asset), Band::Distress);
    env.as_contract(&fx.client.address, || {
        let stored = crate::storage::get_score(&env, &asset).unwrap();
        assert_eq!(
            stored.band,
            Band::Normal,
            "the override must never be baked into the persisted RiskScore"
        );
    });
}

#[test]
fn set_event_in_progress_does_not_downgrade_a_worse_band() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    assert_eq!(fx.client.score(&asset).band, Band::Distress);

    // Already at or above Distress: the override is a floor, not a
    // ceiling, so it must not move an Event-worthy or already-Distress
    // score down to exactly Distress.
    fx.client.set_event_in_progress(&asset, &true);
    assert_eq!(fx.client.band(&asset), Band::Distress);
}

#[test]
fn set_event_in_progress_does_not_touch_the_hysteresis_down_streak() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let next_epoch = fill_ring_distressed(&env, &fx.client, &fx.staking, &asset, 0, 168);
    assert_eq!(fx.client.score(&asset).band, Band::Distress);

    // One new qualifying final epoch arrives from post_healthy_epochs
    // (the usual recompute_score path; see
    // score_down_streak_advances_at_most_once_per_new_final_epoch for
    // why this particular call only ever contributes one qualifying
    // epoch here). Setting and clearing the override around it must not
    // add to, or otherwise disturb, that streak: only a genuinely new
    // final epoch is allowed to advance it, and the override never
    // recomputes at all.
    post_healthy_epochs(&env, &fx.client, &fx.staking, &asset, next_epoch, 3);
    let before = fx.client.score(&asset);
    fx.client.set_event_in_progress(&asset, &true);
    fx.client.set_event_in_progress(&asset, &false);
    let after = fx.client.score(&asset);
    assert_eq!(
        after, before,
        "the override must not change the stored RiskScore at all"
    );
    assert_eq!(
        after.band,
        Band::Distress,
        "only 1 of band_down_epochs (3) distinct newer epochs has elapsed"
    );
}

#[test]
fn set_event_in_progress_requires_registry_auth() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    fx.client.set_event_in_progress(&asset, &true);
    assert_eq!(
        env.auths(),
        [(
            fx.registry.clone(),
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    fx.client.address.clone(),
                    soroban_sdk::Symbol::new(&env, "set_event_in_progress"),
                    (asset.clone(), true).into_val(&env),
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
}

#[test]
fn event_in_progress_reads_the_flag_directly_with_no_auth() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    assert!(!fx.client.event_in_progress(&asset));

    fx.client.set_event_in_progress(&asset, &true);
    assert!(fx.client.event_in_progress(&asset));

    fx.client.set_event_in_progress(&asset, &false);
    assert!(!fx.client.event_in_progress(&asset));
}

/// PR #15 review, finding F4: `newest_epoch` must keep reporting the
/// real newest epoch ever written even when that epoch's own ring
/// POSITION is currently `Empty` (overturned, not yet reposted) —
/// unlike reading the position's own stored `.epoch`, which resets to
/// 0 on an empty slot.
#[test]
fn newest_epoch_survives_an_overturn_of_the_newest_slot() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    governor_client.set_committee(&Address::generate(&env));

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));
    post(&env, &client, &asset, 5, 9_900_000);
    assert_eq!(client.newest_epoch(&asset), Some(5));

    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));
    client.resolve_signal_dispute(&asset, &5, &false, &BytesN::from_array(&env, &[0u8; 32]));

    // Epoch 5's own ring position is now Empty, but it is still the
    // newest epoch anyone ever posted.
    assert!(client.ring(&asset).iter().all(|slot| slot.epoch != 5));
    assert_eq!(
        client.newest_epoch(&asset),
        Some(5),
        "an overturned newest slot must not make newest_epoch regress to None or 0"
    );
}

// -- Required error coverage: Unauthorized, MathOverflow --

/// `Error::Unauthorized` (3) is never returned by any production call
/// site (confirmed: no `Error::Unauthorized` construction anywhere
/// outside `error.rs`'s own definition). Every authorization check in
/// this contract (`add_asset`, `update_asset`, `set_formula`,
/// `post_signals`, `dispute_signals`, `resolve_signal_dispute`,
/// `set_event_band`, `clear_event_band`, `set_event_in_progress`) goes
/// through Soroban's native `Address::require_auth()`, which traps the
/// host call directly rather than returning a `Result::Err` this
/// contract could wrap — so the caller never gets a `RiskOracle::Error`
/// back at all on an auth failure, it gets a host error. This test
/// demonstrates that trap directly (no `mock_all_auths`, so the
/// registry's signature is genuinely missing) rather than asserting a
/// code path that does not exist.
#[test]
#[should_panic]
fn missing_auth_traps_natively_rather_than_returning_unauthorized() {
    let env = Env::default();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let asset = Address::generate(&env);

    // No mock_all_auths(): the registry never actually signs this call.
    client.set_event_in_progress(&asset, &true);
}

/// `Error::MathOverflow` (5), from `aggregate_from_ring`'s checked sum
/// over the 7 day window: `clawback_amount` has no sanity bound in
/// `check_sanity_bounds` (only `peg_ratio`, `peg_ratio_p10`,
/// `liquidity_2pct` and `supply` are bounded there), so a keeper
/// posting a large `clawback_amount` on enough epochs makes the
/// `checked_add` sum over `AGGREGATE_SLOTS_7D` (168) epochs overflow
/// `i128`. `recompute_score`'s `Result` propagates through
/// `post_signals` via `?`, so the overflow surfaces as the 168th post
/// itself failing (the one that first triggers a score computation),
/// not as a separate call.
#[test]
fn post_signals_reports_math_overflow_when_the_7d_clawback_sum_cannot_fit_i128() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Posting in strict order only makes an epoch effectively final once
    // 2 further epochs have posted (pending_until = post_time +
    // SIGNAL_DISPUTE_SECS, review item C5), so epoch 167 (the one that
    // first completes the 168-epoch window) only becomes the
    // newest-final epoch, and triggers recompute_score, once epoch 169
    // posts.
    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        // Large enough that summing just 2 of these 168 postings already
        // exceeds i128::MAX; well within check_sanity_bounds, which does
        // not bound this field at all.
        s.issuer_actions = IssuerActions {
            clawbacks: 1,
            clawback_amount: i128::MAX / 2 + 1,
            auth_revocations: 0,
            flag_changes: 0,
        };
        let result = fx.client.try_post_signals(&keeper, &asset, &s);
        if epoch == 169 {
            assert_eq!(
                result,
                Err(Ok(Error::MathOverflow)),
                "epoch 167 becomes newest-final here, completing the 7 day window and \
                 triggering the overflowing clawback_amount_7d sum"
            );
            return;
        }
        let _ = result.unwrap();
    }
    unreachable!("loop always returns at epoch 169");
}

// -- Section 6.3 override: forced to at least Warning if P = 100 or E = 100 --

#[test]
fn score_is_forced_to_at_least_warning_when_endpoint_is_down() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Every component other than E stays at 0 (same healthy baseline as
    // score_is_normal_band_with_healthy_signals); only the latest final
    // epoch's endpoint is Down. With w_E = 0.20, an unforced weighted
    // score would be 100 * 2000 / 10000 = 20 (Band::Normal), well under
    // Warning's 50..=74 range: the override, not the raw weighted sum,
    // is what moves this to Warning.
    for epoch in 0..170u64 {
        let status = if epoch == 167 {
            EndpointStatus::Down
        } else {
            EndpointStatus::Up
        };
        staking_client.set_aggregate(&asset, &epoch, &status);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(
        score.score, 20,
        "the raw weighted score itself is unaffected by the override"
    );
    assert_eq!(
        score.band,
        Band::Warning,
        "E = 100 must force at least Warning even though the weighted score alone is Normal"
    );
}

#[test]
fn score_is_forced_to_at_least_warning_when_p_saturates_at_100() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // d_max defaults to 0.10 (SCALE / 10); a peg_ratio deviating by more
    // than that from SCALE clamps component P at exactly 100. Every
    // other component stays at 0, so the unforced weighted score would
    // be 100 * 3500 / 10000 = 35 (Band::Watch), under Warning's range.
    let depegged_ratio = sylox_types::SCALE / 2; // 50% off peg, far past d_max
    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s = signal_set(&env, epoch, depegged_ratio);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(
        score.score, 35,
        "the raw weighted score itself is unaffected by the override"
    );
    assert_eq!(
        score.band,
        Band::Warning,
        "P = 100 must force at least Warning even though the weighted score alone is Watch"
    );
}

// -- Required test: backfill equivalence --

/// One signal per epoch, varying slightly by epoch so a position or
/// ordering bug in the ring would actually change the aggregates,
/// unlike a constant signal.
fn varying_signal_set(env: &Env, epoch: u64) -> SignalSet {
    // peg_ratio oscillates a small amount around SCALE, never enough to
    // move component P's band on its own; the point is per-epoch
    // variation, not a depeg scenario.
    let wobble = (epoch % 7) as i128 * 1_000;
    let mut s = signal_set(env, epoch, sylox_types::SCALE + wobble);
    s.liquidity_2pct = 100_000_000_000 + (epoch % 5) as i128 * 1_000_000;
    s.supply_change_bps = 0;
    s
}

/// Posts `epochs` (plus 2 flush epochs, review item C5) worth of
/// `varying_signal_set`, in strict chronological order, with no gaps.
fn post_all_in_order(env: &Env, client: &RiskOracleClient, staking: &Address, asset: &Address) {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    for epoch in 0..170u64 {
        staking_client.set_aggregate(asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        client.post_signals(&keeper, asset, &varying_signal_set(env, epoch));
    }
    client.finalize_endpoint(asset, &167);
}

/// Posts the same 170 epochs as `post_all_in_order`, but skips epochs
/// 50..56 during the first pass (simulating a 6 epoch keeper outage),
/// continues posting epochs 56..76 on schedule, then backfills the
/// skipped 50..56 (still well inside `WINDOW_SECS`'s 72 epoch lookback
/// from epoch 50's own close), before continuing on through 170. Every
/// epoch ends up posted with the exact same `SignalSet` content as
/// `post_all_in_order`, just in a different order.
fn post_with_an_outage_then_backfill(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
) {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    for epoch in &[&(0..50u64), &(56..76u64)] {
        for e in (*epoch).clone() {
            staking_client.set_aggregate(asset, &e, &EndpointStatus::Up);
            env.ledger().set_timestamp((e + 1) * 3_600);
            client.post_signals(&keeper, asset, &varying_signal_set(env, e));
        }
    }
    // Backfill the outage. The clock does not move backward: these
    // posts land at whatever "now" already is (end of the 56..76 pass),
    // well inside WINDOW_SECS for epochs this recent.
    for e in 50..56u64 {
        staking_client.set_aggregate(asset, &e, &EndpointStatus::Up);
        client.post_signals(&keeper, asset, &varying_signal_set(env, e));
    }
    for e in 76..170u64 {
        staking_client.set_aggregate(asset, &e, &EndpointStatus::Up);
        env.ledger().set_timestamp((e + 1) * 3_600);
        client.post_signals(&keeper, asset, &varying_signal_set(env, e));
    }
    client.finalize_endpoint(asset, &167);
}

#[test]
fn backfill_after_an_outage_matches_no_outage() {
    let env_a = Env::default();
    let (fx_a, asset_a) = setup_with_asset(&env_a);
    post_all_in_order(&env_a, &fx_a.client, &fx_a.staking, &asset_a);

    let env_b = Env::default();
    let (fx_b, asset_b) = setup_with_asset(&env_b);
    post_with_an_outage_then_backfill(&env_b, &fx_b.client, &fx_b.staking, &asset_b);

    let ring_a = fx_a.client.ring(&asset_a);
    let ring_b = fx_b.client.ring(&asset_b);
    assert_eq!(
        ring_a.len(),
        ring_b.len(),
        "both rings must hold the same number of written slots"
    );
    for epoch in 0..170u64 {
        let slot_a = find_slot_by_epoch(&ring_a, epoch);
        let slot_b = find_slot_by_epoch(&ring_b, epoch);
        assert_eq!(
            slot_a.peg_ratio, slot_b.peg_ratio,
            "epoch {epoch}: peg_ratio must match regardless of posting order"
        );
        assert_eq!(
            slot_a.liquidity_2pct, slot_b.liquidity_2pct,
            "epoch {epoch}: liquidity_2pct must match regardless of posting order"
        );
        assert_eq!(
            slot_a.state, slot_b.state,
            "epoch {epoch}: final state must match regardless of posting order"
        );
    }

    let score_a = fx_a.client.score(&asset_a);
    let score_b = fx_b.client.score(&asset_b);
    assert_eq!(
        score_a.score, score_b.score,
        "an outage followed by backfill must score identically to no outage at all"
    );
    assert_eq!(score_a.band, score_b.band);
    assert_eq!(score_a.epoch, score_b.epoch);
    assert!(!score_a.stale);
    assert!(!score_b.stale);
}

// -- Re-review item C6: finality must not stall on a gap --
//
// try_advance_finality (as of this round) walks a sequential cursor
// forward from the last known-Final epoch and stops at the first one
// that is not Final. A permanently missing epoch, an overturned epoch
// that is never reposted, or an unresolved dispute therefore freezes
// every later epoch's finality (and so the score) forever, even though
// every one of those later epochs individually closed and became
// Final long ago. These four tests are written to FAIL against that
// design, per the review's "write failing tests first" instruction;
// the fix follows in the next commit.

/// Posts a plain healthy epoch (no flush, caller controls finality
/// entirely) — used by the C6 tests to build a long posting history
/// with a deliberate gap, without fill_ring_with_constant_signal's
/// automatic flush/finalize_endpoint call masking the gap.
fn post_one_healthy_epoch(
    env: &Env,
    client: &RiskOracleClient,
    staking: &Address,
    asset: &Address,
    epoch: u64,
) {
    let keeper = Address::generate(env);
    let staking_client = crate::mocks::MockStakingClient::new(env, staking);
    staking_client.set_aggregate(asset, &epoch, &EndpointStatus::Up);
    env.ledger().set_timestamp((epoch + 1) * 3_600);
    let mut s = signal_set(env, epoch, sylox_types::SCALE);
    s.liquidity_2pct = 100_000_000_000;
    s.supply_change_bps = 0;
    client.post_signals(&keeper, asset, &s);
}

#[test]
fn finality_keeps_advancing_past_a_permanently_missing_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // Post 0..100 normally, reaching well past the 168 epoch warm-up
    // (so scoring is already active), skip epoch 100 forever, then
    // keep posting 101..230 (comfortably past epoch 100's own 72 epoch
    // backfill window, and still inside RING_SLOTS=240 so the gap is
    // not simply overwritten by wraparound).
    for epoch in 0..100u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    // epoch 100 intentionally never posted.
    for epoch in 101..230u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }

    let score = fx.client.score(&asset);
    assert!(
        score.epoch >= 220,
        "score must keep advancing to near the newest Final epoch even though \
         epoch 100 is permanently missing; got stuck at epoch {}",
        score.epoch
    );
    assert!(
        !score.stale,
        "a score this close to the newest epoch must not read stale"
    );
}

#[test]
fn finality_keeps_advancing_past_an_overturned_epoch_that_is_never_reposted() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    for epoch in 0..101u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
    }
    // Dispute and overturn epoch 100 (disputer wins), then never repost it.
    let disputer = Address::generate(&env);
    client.dispute_signals(
        &disputer,
        &asset,
        &100,
        &BytesN::from_array(&env, &[9u8; 32]),
    );
    client.resolve_signal_dispute(&asset, &100, &false, &BytesN::from_array(&env, &[0u8; 32]));

    for epoch in 101..230u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
    }

    let score = client.score(&asset);
    assert!(
        score.epoch >= 220,
        "score must keep advancing past an overturned-and-never-reposted epoch; \
         got stuck at epoch {}",
        score.epoch
    );
    assert!(!score.stale);
}

#[test]
fn finality_keeps_advancing_past_an_unresolved_dispute() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    for epoch in 0..101u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
    }
    // Dispute epoch 100 and never resolve it: it stays Disputed forever.
    let disputer = Address::generate(&env);
    client.dispute_signals(
        &disputer,
        &asset,
        &100,
        &BytesN::from_array(&env, &[9u8; 32]),
    );

    for epoch in 101..230u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
    }

    let score = client.score(&asset);
    assert!(
        score.epoch >= 220,
        "later epochs must still finalize and the score must keep advancing even \
         though epoch 100's dispute is never resolved; got stuck at epoch {}",
        score.epoch
    );
    assert!(!score.stale);
}

#[test]
fn a_frozen_score_must_report_stale_judged_against_its_own_epoch() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // Post exactly enough healthy epochs to produce one real score,
    // then go quiet (no further posting at all): the newest POSTED
    // epoch and the newest FINAL epoch are the same here, so this
    // establishes the baseline "stale eventually goes true" behavior
    // before the sharper assertion below.
    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let fresh = fx.client.score(&asset);
    assert!(!fresh.stale);
    let scored_epoch = fresh.epoch;

    env.ledger().set_timestamp((scored_epoch + 1 + 200) * 3_600);
    let frozen = fx.client.score(&asset);
    assert!(
        frozen.stale,
        "a score computed from an epoch this far in the past must report stale, \
         judged against the epoch it was actually computed from"
    );
}

#[test]
fn score_stays_fresh_behind_a_permanent_gap_once_finality_advances_independently() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // Reach a real score, then leave a permanent gap at epoch 100 and
    // keep posting for 200+ further epochs. Before review item C6's
    // fix, the sequential finality cursor would have stalled at the
    // gap, freezing score() at an epoch far enough in the past to
    // eventually read score().stale = true even while is_stale()
    // (which reads only the newest POSTED epoch) kept reporting
    // fresh, misleading a caller that trusted is_stale() alone. With
    // per-epoch independent finality, score() keeps advancing past
    // the gap, so BOTH reads agree the asset is current: there is no
    // more divergence to mislead anyone with, because there is no
    // more frozen score to mask.
    for epoch in 0..100u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    // epoch 100 intentionally never posted.
    for epoch in 101..300u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }

    assert!(!fx.client.is_stale(&asset));
    let score = fx.client.score(&asset);
    assert!(
        !score.stale,
        "score() must keep advancing past the permanent gap at epoch 100, so its \
         staleness judgment agrees with is_stale() instead of lagging behind it"
    );
    assert!(score.epoch >= 290, "got stuck at epoch {}", score.epoch);
}

// -- Re-review item C6, step 3: signals_final emission tests --
//
// `env.events().all()` only returns events from the LAST contract
// invocation (soroban-sdk's own doc comment on `Events::all`), so
// every test below checks immediately after each individual call and
// accumulates its own running tally, rather than querying once at the
// end of a long posting loop.

/// Counts how many `signals_final` events matching `(asset, target_epoch)`
/// appear in the events emitted by the single most recent contract
/// invocation.
fn signals_final_count_for_asset(
    env: &Env,
    contract_id: &Address,
    asset: &Address,
    target_epoch: u64,
) -> usize {
    let expected = crate::events::SignalsFinal {
        asset: asset.clone(),
        epoch: target_epoch,
    }
    .to_xdr(env, contract_id);
    env.events()
        .all()
        .events()
        .iter()
        .filter(|e| *e == &expected)
        .count()
}

#[test]
fn each_epoch_emits_signals_final_exactly_once_across_many_calls() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();

    // 170 epochs posted in strict order (no gaps, no disputes): every
    // epoch eventually becomes final through the lazy backward scan
    // triggered by a LATER post. Tally, per post_signals call, how
    // many times each specific epoch's signals_final fires across the
    // whole run, and assert every epoch that ever becomes final gets
    // exactly one.
    let mut counts: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
        for candidate in 0..=epoch {
            let n = signals_final_count_for_asset(&env, &contract_id, &asset, candidate);
            if n > 0 {
                *counts.entry(candidate).or_insert(0) += n;
            }
        }
    }

    assert!(
        !counts.is_empty(),
        "at least some epochs must have become final over 170 posts"
    );
    for (epoch, count) in &counts {
        assert_eq!(
            *count, 1,
            "epoch {epoch} emitted signals_final {count} time(s); expected exactly 1"
        );
    }
}

#[test]
fn a_backfilled_epoch_emits_signals_final_exactly_once_when_first_observed_final() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Post 0..50 in order, skip epoch 50 (to be backfilled shortly),
    // continue 51..60: epochs newer than 50 become final first, while
    // 50 itself is still missing.
    for epoch in 0..50u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    for epoch in 51..60u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    assert_eq!(
        signals_final_count_for_asset(&env, &contract_id, &asset, 50),
        0,
        "epoch 50 cannot be final before it has even been posted"
    );

    // Backfill epoch 50 now, WITHOUT moving the clock backward: it
    // lands at whatever "now" already is (the end of the 51..60 pass),
    // matching post_with_an_outage_then_backfill's pattern elsewhere in
    // this file. epoch 50 cannot itself be the one that becomes final
    // on this same call (its own pending_until is in the future
    // relative to this post's "now"), so continue posting forward a
    // few more epochs for its finality lag to resolve.
    staking_client.set_aggregate(&asset, &50, &EndpointStatus::Up);
    let mut s = signal_set(&env, 50, sylox_types::SCALE);
    s.liquidity_2pct = 100_000_000_000;
    s.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s);
    let mut total = signals_final_count_for_asset(&env, &contract_id, &asset, 50);
    for epoch in 60..65u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
        total += signals_final_count_for_asset(&env, &contract_id, &asset, 50);
    }

    assert_eq!(
        total, 1,
        "the backfilled epoch 50 must emit signals_final exactly once total, got {total}"
    );
    assert!(
        fx.client.is_final(&asset, &50),
        "epoch 50 must actually have become final by the end of this run"
    );
}

#[test]
fn an_overturned_and_reposted_epoch_emits_signals_final_exactly_once() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    // Post, dispute, and overturn epoch 5 (disputer wins). It must
    // never emit signals_final while overturned (it is Empty, never
    // Final). Repost it, then post enough further epochs for its
    // finality lag to resolve, and confirm exactly one emission total,
    // from the repost's own eventual finality, never from the original
    // (overturned) posting.
    post_one_healthy_epoch(&env, &client, &staking, &asset, 5);
    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));
    client.resolve_signal_dispute(&asset, &5, &false, &BytesN::from_array(&env, &[0u8; 32]));
    assert_eq!(
        signals_final_count_for_asset(&env, &contract_id, &asset, 5),
        0,
        "an overturned epoch must never emit signals_final"
    );

    // Repost epoch 5 (ADR-005, review item C4): the clock does not
    // move backward; this repost lands at whatever "now" already is.
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);
    staking_client.set_aggregate(&asset, &5, &EndpointStatus::Up);
    let mut s = signal_set(&env, 5, sylox_types::SCALE);
    s.liquidity_2pct = 100_000_000_000;
    s.supply_change_bps = 0;
    client.post_signals(&keeper, &asset, &s);

    let mut total = signals_final_count_for_asset(&env, &contract_id, &asset, 5);
    for epoch in 6..15u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
        total += signals_final_count_for_asset(&env, &contract_id, &asset, 5);
    }

    assert_eq!(
        total, 1,
        "the reposted epoch 5 must emit signals_final exactly once total, got {total}"
    );
    assert!(client.is_final(&asset, &5));
}

#[test]
fn resolve_signal_dispute_keeper_wins_emits_signals_final_once_at_resolution_never_again() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    post_one_healthy_epoch(&env, &client, &staking, &asset, 5);
    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &5, &BytesN::from_array(&env, &[9u8; 32]));
    assert_eq!(
        signals_final_count_for_asset(&env, &contract_id, &asset, 5),
        0,
        "disputing must not itself emit signals_final"
    );

    // Review item C6 (re-review): keeper_wins resolves the slot to
    // Final immediately and decisively, set_slot_final/emission
    // happening right here, not waiting for the independent backward
    // scan or a clock check.
    client.resolve_signal_dispute(&asset, &5, &true, &BytesN::from_array(&env, &[0u8; 32]));
    let mut total = signals_final_count_for_asset(&env, &contract_id, &asset, 5);
    assert_eq!(
        total, 1,
        "resolve_signal_dispute with keeper_wins = true must emit signals_final exactly \
         once, at resolution itself"
    );

    // Keep posting further epochs: epoch 5 must never emit again.
    for epoch in 6..15u64 {
        post_one_healthy_epoch(&env, &client, &staking, &asset, epoch);
        total += signals_final_count_for_asset(&env, &contract_id, &asset, 5);
    }
    assert_eq!(
        total, 1,
        "epoch 5 must never emit signals_final again after its resolution-time emission"
    );
}

// -- Re-review item C7: check_stale, asset_stale redesign --

/// Counts how many `asset_stale` events for `asset` appear among the
/// events emitted by the single most recent contract invocation.
fn asset_stale_count(env: &Env, contract_id: &Address, asset: &Address, last_epoch: u64) -> usize {
    let expected = crate::events::AssetStale {
        asset: asset.clone(),
        last_epoch,
    }
    .to_xdr(env, contract_id);
    env.events()
        .all()
        .events()
        .iter()
        .filter(|e| *e == &expected)
        .count()
}

#[test]
fn check_stale_fires_once_when_keepers_stop_posting_then_nothing_on_a_repeat_call() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();

    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let last_epoch = fx.client.score(&asset).epoch;
    assert!(!fx.client.score(&asset).stale);

    // Keepers stop posting entirely; advance the clock to the first
    // moment a score computed from last_epoch reads stale, with no
    // further posts at all.
    env.ledger().set_timestamp(first_stale_time(last_epoch));
    assert!(fx.client.check_stale(&asset));
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        1,
        "the first check_stale call to observe the asset as stale must emit asset_stale exactly once"
    );

    assert!(fx.client.check_stale(&asset));
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        0,
        "a repeat check_stale call while still stale must emit nothing"
    );
}

#[test]
fn check_stale_does_not_fire_one_second_before_first_stale_time() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();

    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let last_epoch = fx.client.score(&asset).epoch;

    env.ledger().set_timestamp(first_stale_time(last_epoch) - 1);
    assert!(
        !fx.client.check_stale(&asset),
        "one second before first_stale_time, the asset must not read stale yet"
    );
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        0,
        "no asset_stale before the asset is actually stale"
    );
}

#[test]
fn check_stale_recovers_silently_then_relapses_with_a_fresh_asset_stale() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();

    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let first_last_epoch = fx.client.score(&asset).epoch;

    env.ledger()
        .set_timestamp(first_stale_time(first_last_epoch));
    assert!(fx.client.check_stale(&asset));
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, first_last_epoch),
        1
    );

    // Recovery: post fresh epochs. The flag clears; no event marks
    // recovery (score_updated already signals it). The finality lag
    // (review item C5) means the first of these posts does not itself
    // become Final yet, so post a couple more to let that resolve
    // before checking score() again.
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    let recovery_start = first_last_epoch + backfill_window_epochs();
    for epoch in recovery_start..recovery_start + 3 {
        staking_client.set_aggregate(&asset, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(epoch));
        let mut s = signal_set(&env, epoch, sylox_types::SCALE);
        s.liquidity_2pct = 100_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
        // Checked after each individual post_signals call (events()
        // only exposes the LAST invocation's events): none of these,
        // including the one that eventually clears the stale state,
        // may emit asset_stale.
        assert_eq!(
            asset_stale_count(&env, &contract_id, &asset, first_last_epoch),
            0
        );
    }
    assert!(
        !fx.client.score(&asset).stale,
        "a fresh epoch must clear the stale state"
    );
    let second_last_epoch = fx.client.score(&asset).epoch;

    // Relapse: go quiet again, past first_stale_time for the new
    // stored epoch. asset_stale must fire again, exactly once.
    env.ledger()
        .set_timestamp(first_stale_time(second_last_epoch));
    assert!(fx.client.check_stale(&asset));
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, second_last_epoch),
        1,
        "a relapse into stale after a recovery must emit asset_stale again, exactly once"
    );
}

#[test]
fn a_late_backfill_that_is_stale_on_arrival_emits_asset_stale_via_post_signals() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let contract_id = fx.client.address.clone();
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);

    // Post 0..170 (168 plus 2 flush epochs for the finality lag,
    // review item C5, so the warm-up threshold is actually crossed
    // and a real score gets computed), then skip epoch 170 entirely:
    // it stays genuinely unposted (not just unscored) going into the
    // backfill below.
    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let last_epoch = fx.client.score(&asset).epoch;
    assert!(
        !fx.client.score(&asset).stale,
        "sanity check: a real score must exist here"
    );
    assert!(
        last_epoch < 170,
        "sanity check: last_epoch must be below the never-posted epoch 170"
    );

    // epoch 170 is inside the backfill window, newer than the stored
    // score's own epoch (so recompute_score would otherwise consider
    // it), but already stale on arrival (so recompute_score declines
    // to write it, per its own "very late backfill" branch), with
    // nothing newer posted since. Posting it must still surface the
    // transition via check_stale_internal.
    let backfilled_epoch = 170u64;
    let post_time = first_stale_time(backfilled_epoch);
    assert!(
        post_time <= time_at_epoch(backfilled_epoch) + crate::WINDOW_SECS,
        "the chosen post time must still be inside the backfill window check_epoch_window allows"
    );
    env.ledger().set_timestamp(post_time);
    staking_client.set_aggregate(&asset, &backfilled_epoch, &EndpointStatus::Up);
    let mut s = signal_set(&env, backfilled_epoch, sylox_types::SCALE);
    s.liquidity_2pct = 100_000_000_000;
    s.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s);

    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        1,
        "a late backfill that is already stale on arrival must emit asset_stale once, \
         via this post_signals call's own check_stale_internal"
    );
}

#[test]
fn score_band_and_is_stale_never_emit_or_write_when_checking_staleness() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    for epoch in 0..170u64 {
        post_one_healthy_epoch(&env, &fx.client, &fx.staking, &asset, epoch);
    }
    let last_epoch = fx.client.score(&asset).epoch;
    env.ledger().set_timestamp(first_stale_time(last_epoch));

    // None of these read-only calls may announce asset_stale or set
    // the stale_announced flag: only check_stale (or an internal
    // state changing call) may do that. is_stale()'s own result is
    // not asserted here: it judges staleness against the newest
    // POSTED epoch (169, still fresh at this clock), not the stored
    // score's epoch (167, already stale), so it legitimately reads
    // false here even while score().stale reads true; that divergence
    // is exactly why check_stale exists as a separate, authoritative
    // signal, not a bug in this test.
    assert!(fx.client.score(&asset).stale);
    assert!(fx.client.score(&asset).stale);
    assert_eq!(fx.client.band(&asset), Band::Normal);
    let _ = fx.client.is_stale(&asset);

    let contract_id = fx.client.address.clone();
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        0,
        "score()/band()/is_stale() must never emit asset_stale themselves"
    );

    // Confirm the flag genuinely was never set by any of the calls
    // above: check_stale must still see this as a fresh transition.
    assert!(fx.client.check_stale(&asset));
    assert_eq!(
        asset_stale_count(&env, &contract_id, &asset, last_epoch),
        1,
        "check_stale must still see an unannounced transition, proving the read-only \
         calls above never set stale_announced"
    );
}

// -- Re-review item C7: event topics are ("sylox", <event_name>, <primary key>) --

/// Extracts the raw topic vector (as `ScVal`s) for the most recently
/// emitted event whose topic 1 is exactly `event_name`, from the
/// events published by the single most recent contract invocation.
/// Returns `None` if no such event was emitted.
fn topics_for_event_named(
    env: &Env,
    event_name: &str,
) -> Option<std::vec::Vec<soroban_sdk::xdr::ScVal>> {
    let name_scval =
        soroban_sdk::xdr::ScVal::Symbol(soroban_sdk::xdr::ScSymbol(event_name.try_into().unwrap()));
    for event in env.events().all().events() {
        let soroban_sdk::xdr::ContractEventBody::V0(body) = &event.body;
        if body.topics.get(1) == Some(&name_scval) {
            return Some(body.topics.to_vec());
        }
    }
    None
}

/// Asserts that the most recently emitted event named `event_name` has
/// EXACTLY the topic vector `["sylox", event_name, primary_key]`: the
/// 2 custom prefix topics ADR-007 specifies (re-review item C7,
/// replacing the old literal `"RiskOracle"` prefix), followed by
/// whatever single field every RiskOracle event's `#[topic]` marks
/// (always `asset`).
fn assert_event_topics(env: &Env, event_name: &str, primary_key: &Address) {
    let topics = topics_for_event_named(env, event_name)
        .unwrap_or_else(|| panic!("no event named \"{event_name}\" was emitted"));
    assert_eq!(
        topics.len(),
        3,
        "event \"{event_name}\" must have exactly 3 topics (sylox, name, asset), got {}",
        topics.len()
    );
    assert_eq!(
        topics[0],
        soroban_sdk::xdr::ScVal::Symbol(soroban_sdk::xdr::ScSymbol("sylox".try_into().unwrap())),
        "topic 0 must be \"sylox\""
    );
    assert_eq!(
        topics[1],
        soroban_sdk::xdr::ScVal::Symbol(soroban_sdk::xdr::ScSymbol(event_name.try_into().unwrap())),
        "topic 1 must be the event's own name (\"{event_name}\"), not \"RiskOracle\" \
         (ADR-007, re-review item C7)"
    );
    assert_eq!(
        topics[2],
        soroban_sdk::xdr::ScVal::from_val(env, &primary_key.to_val()),
        "topic 2 must be the event's primary key (asset)"
    );
}

#[test]
fn every_event_uses_the_sylox_event_name_asset_topic_convention() {
    let env = Env::default();
    env.mock_all_auths();
    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);
    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));
    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);
    let keeper = Address::generate(&env);

    // signals_posted: every post_signals call emits it.
    staking_client.set_aggregate(&asset, &0, &EndpointStatus::Up);
    env.ledger().set_timestamp(3_600);
    let mut s = signal_set(&env, 0, sylox_types::SCALE);
    s.liquidity_2pct = 100_000_000_000;
    s.supply_change_bps = 0;
    client.post_signals(&keeper, &asset, &s);
    assert_event_topics(&env, "signals_posted", &asset);

    // signals_disputed: dispute_signals.
    let disputer = Address::generate(&env);
    client.dispute_signals(&disputer, &asset, &0, &BytesN::from_array(&env, &[9u8; 32]));
    assert_event_topics(&env, "signals_disputed", &asset);

    // signals_resolved and signals_final: resolve_signal_dispute,
    // keeper_wins = true emits both in the same call.
    client.resolve_signal_dispute(&asset, &0, &true, &BytesN::from_array(&env, &[0u8; 32]));
    assert_event_topics(&env, "signals_resolved", &asset);
    assert_event_topics(&env, "signals_final", &asset);

    // endpoint_finalized: post_signals with NO aggregate set yet (so
    // MockStaking.aggregate returns Unknown, same as a keeper posting
    // before the endpoint is known), then finalize_endpoint once a
    // real aggregate exists, which is the one case signals.endpoint ==
    // Unknown at the time finalize_endpoint runs.
    env.ledger().set_timestamp(2 * 3_600);
    let mut s1 = signal_set(&env, 1, sylox_types::SCALE);
    s1.liquidity_2pct = 100_000_000_000;
    s1.supply_change_bps = 0;
    client.post_signals(&keeper, &asset, &s1);
    staking_client.set_aggregate(&asset, &1, &EndpointStatus::Up);
    env.ledger().set_timestamp(3 * 3_600);
    client.finalize_endpoint(&asset, &1);
    assert_event_topics(&env, "endpoint_finalized", &asset);

    // score_updated and band_changed: the very first score ever
    // computed for a fresh asset always qualifies as a band change
    // (recompute_score's `stored = None` branch has no previous band
    // to compare against), so a plain healthy run to 168 epochs is
    // enough to trigger both, no depeg scenario needed.
    let asset2 = Address::generate(&env);
    let issuer2 = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset2, &issuer2));
    let keeper2 = Address::generate(&env);
    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset2, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s2 = signal_set(&env, epoch, sylox_types::SCALE);
        s2.liquidity_2pct = 100_000_000_000;
        s2.supply_change_bps = 0;
        client.post_signals(&keeper2, &asset2, &s2);
    }
    assert_event_topics(&env, "score_updated", &asset2);
    assert_event_topics(&env, "band_changed", &asset2);

    // asset_stale: a third asset, posted just enough to reach the 168
    // epoch threshold, then abandoned long enough that the STORED
    // score's own epoch (unchanged, since nothing newer posted in the
    // gap) reads stale against the clock. Re-review item C7:
    // check_stale_internal runs at the end of every post_signals
    // call, so the one post below that resumes after the long gap is
    // what both observes and announces the transition into stale (not
    // because posting itself triggered a NEW stale epoch, but because
    // this call is simply the next state changing call to run the
    // check at all).
    let asset3 = Address::generate(&env);
    let issuer3 = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset3, &issuer3));
    let keeper3 = Address::generate(&env);
    for epoch in 0..170u64 {
        staking_client.set_aggregate(&asset3, &epoch, &EndpointStatus::Up);
        env.ledger().set_timestamp((epoch + 1) * 3_600);
        let mut s3 = signal_set(&env, epoch, sylox_types::SCALE);
        s3.liquidity_2pct = 100_000_000_000;
        s3.supply_change_bps = 0;
        client.post_signals(&keeper3, &asset3, &s3);
    }
    let next_epoch = 300u64;
    env.ledger().set_timestamp((next_epoch + 1) * 3_600);
    staking_client.set_aggregate(&asset3, &next_epoch, &EndpointStatus::Up);
    let mut s3 = signal_set(&env, next_epoch, sylox_types::SCALE);
    s3.liquidity_2pct = 100_000_000_000;
    s3.supply_change_bps = 0;
    client.post_signals(&keeper3, &asset3, &s3);
    assert_event_topics(&env, "asset_stale", &asset3);
}

// -- Issue #12: upgrade / migrate / version --

/// `version()` reads the running code's own `CONTRACT_VERSION`
/// constant, with no auth required (a plain read anyone can make).
#[test]
fn version_reads_contract_version_constant() {
    let env = Env::default();
    let fixture = setup(&env);
    assert_eq!(fixture.client.version(), crate::CONTRACT_VERSION);
}

/// CI's own `demo-time-not-in-release` job proves this against the
/// real release wasm, not this test harness; this test proves the
/// other half - that `cfg!` actually tracks the feature at all,
/// across both configurations `cargo test` can be run under. Without
/// `--features demo-time` (every normal run, including CI's own
/// `cargo test --workspace`), this must read `false`.
#[test]
fn demo_build_is_false_without_the_demo_time_feature() {
    let env = Env::default();
    let fixture = setup(&env);
    assert!(!fixture.client.demo_build());
}

/// The other side: compiled in ONLY when this crate itself is built
/// with `--features demo-time` (`cargo test -p risk-oracle --features
/// demo-time`), so a normal test run never even sees it - confirms
/// the flag actually flips `demo_build()`, not just that the false
/// case reads false.
#[cfg(feature = "demo-time")]
#[test]
fn demo_build_is_true_with_the_demo_time_feature() {
    let env = Env::default();
    let fixture = setup(&env);
    assert!(fixture.client.demo_build());
}

/// Non-governor cannot upgrade: same native auth trap as every other
/// governor-gated call (see
/// `missing_auth_traps_natively_rather_than_returning_unauthorized`
/// above), demonstrated directly with no `mock_all_auths()` so the
/// governor's signature is genuinely missing.
#[test]
#[should_panic]
fn upgrade_without_governor_auth_traps() {
    let env = Env::default();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    let new_wasm_hash = env.upload(RiskOracle);
    // No mock_all_auths(): the governor never actually signs this call.
    client.upgrade(&new_wasm_hash);
}

/// Upgrading to new code (a fresh wasm hash, uploaded via `env.upload`
/// per soroban-sdk's own documented pattern for exercising
/// `update_current_contract` in a unit test without a compiled .wasm
/// fixture) keeps all existing storage: an asset added, and a real
/// `post_signals` call's own posted data, both still read back
/// correctly through the SAME client and address after the swap.
/// Also confirms the `Upgraded` event carries the pre-upgrade
/// version.
#[test]
fn upgrade_keeps_existing_storage() {
    let env = Env::default();
    let fixture = setup(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fixture.staking);
    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fixture
        .client
        .add_asset(&asset_config(&env, &asset, &issuer));
    let keeper = Address::generate(&env);
    staking_client.set_aggregate(&asset, &0, &EndpointStatus::Up);
    env.ledger().set_timestamp(3_600);
    let signals = signal_set(&env, 0, sylox_types::SCALE);
    fixture.client.post_signals(&keeper, &asset, &signals);

    let score_before = fixture.client.score(&asset);
    let asset_config_before = fixture.client.asset_config(&asset);

    let new_wasm_hash = env.upload(RiskOracle);
    fixture.client.upgrade(&new_wasm_hash);
    assert_event_topics(&env, "upgraded", &fixture.client.address);

    assert_eq!(
        fixture.client.score(&asset),
        score_before,
        "score(asset) must read the same after an upgrade with no storage-shape change"
    );
    assert_eq!(
        fixture.client.asset_config(&asset),
        asset_config_before,
        "asset_config(asset) must read the same after an upgrade with no storage-shape change"
    );
    assert_eq!(
        fixture.client.version(),
        crate::CONTRACT_VERSION,
        "the re-uploaded code is the same crate, so its own version() constant is unchanged"
    );
}

/// `migrate()` always targets this code's own `CONTRACT_VERSION` (no
/// caller-chosen argument - see `migrate`'s own doc comment for why),
/// and can't run twice: `AlreadyMigrated` on the second call,
/// confirmed via `try_migrate` since this is a real returned error,
/// not a native auth trap.
#[test]
fn migrate_cannot_run_twice() {
    let env = Env::default();
    let fixture = setup(&env);
    fixture.client.migrate();
    let result = fixture.client.try_migrate();
    assert_eq!(result, Err(Ok(Error::AlreadyMigrated)));
}

/// `set_governor` works exactly once: the current governor hands off
/// to a new address, and every governor-gated call afterward is
/// authorized against the NEW address, not the old one.
#[test]
fn set_governor_succeeds_once_and_the_new_governor_is_accepted() {
    let env = Env::default();
    let fixture = setup(&env);
    let new_governor = Address::generate(&env);

    fixture.client.set_governor(&new_governor);

    // A governor-gated call now succeeds under mock_all_auths() no
    // matter which address signs (the mock approves every
    // require_auth()), so the real proof the new governor is the one
    // actually recorded is the dedicated non-auth trap test below,
    // which registers its own contract with no mock_all_auths() at
    // all and checks the OLD governor is rejected while the NEW one
    // is accepted.
    let new_wasm_hash = env.upload(RiskOracle);
    fixture.client.upgrade(&new_wasm_hash);
    assert_event_topics(&env, "upgraded", &fixture.client.address);
}

/// `set_governor` cannot run twice: `GovernorAlreadySet` on the second
/// call, confirmed via `try_set_governor` since this is a real
/// returned error, not a native auth trap. Same one-time-lock shape
/// as `migrate_cannot_run_twice` and EventRegistry's
/// `set_factory_succeeds_once_then_locks`.
#[test]
fn set_governor_fails_the_second_call() {
    let env = Env::default();
    let fixture = setup(&env);

    fixture.client.set_governor(&Address::generate(&env));
    let result = fixture.client.try_set_governor(&Address::generate(&env));

    assert_eq!(result, Err(Ok(Error::GovernorAlreadySet)));
}

/// Non-governor cannot call `set_governor`: same native auth trap as
/// every other governor-gated call (see
/// `upgrade_without_governor_auth_traps` above), demonstrated with no
/// `mock_all_auths()` so the governor's signature is genuinely
/// missing.
#[test]
#[should_panic]
fn set_governor_without_governor_auth_traps() {
    let env = Env::default();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

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
    let staking = env.register(MockStaking, ());
    let old_governor = Address::generate(&env);
    let new_governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (old_governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

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

    let new_wasm_hash = env.upload(RiskOracle);

    // The old governor is no longer the one recorded: a call it signs
    // for still traps natively, exactly like any other non-governor
    // caller.
    let old_governor_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &old_governor,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "upgrade",
                args: (new_wasm_hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.upgrade(&new_wasm_hash);
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
            args: (new_wasm_hash.clone(),).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.upgrade(&new_wasm_hash);
    assert_event_topics(&env, "upgraded", &client.address);
}

/// Non-governor cannot migrate: same native auth trap pattern as
/// `upgrade_without_governor_auth_traps`.
#[test]
#[should_panic]
fn migrate_without_governor_auth_traps() {
    let env = Env::default();
    let staking = env.register(MockStaking, ());
    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.migrate();
}

/// Issue #21: with a constructor instead of a separate `initialize`
/// call, there is no on-chain way to invoke construction a second time
/// against an already-deployed address - a fresh `env.register` always
/// allocates a new address, and the real deployer flow
/// (`Env::deployer().with_address(...).deploy_contract(...)`) runs the
/// constructor exactly once, atomically with deployment itself.
///
/// The one thing that CAN re-run a constructor against an address that
/// already holds this contract is `Env::register_at`, a testutils-only
/// convenience: per its own doc comment, "Registering a contract that
/// is already registered replaces it... the new contract's constructor
/// will be called again during re-registration. That behavior only
/// exists for this test utility and is not reproducible on-chain." It
/// is not an authenticatable front-run either - `register_at`, like
/// `register`, always mocks authorization for the constructor call, so
/// it proves nothing about `require_auth` and cannot be used to invoke
/// a constructor as some other caller would have to on a real network.
///
/// What this test demonstrates instead: `register_at`'s re-registration
/// genuinely replaces the whole config in storage (not just the
/// address's code), by re-registering RiskOracle at the same contract
/// id with a second, different governor, and showing that a
/// governor-gated call now runs under the SECOND governor's authority,
/// not the first. That is real, observable behavior of the test
/// harness - never something a deployed contract on-chain can be made
/// to do.
#[test]
fn register_at_can_replace_config_but_only_as_a_testutils_convenience() {
    let env = Env::default();
    env.mock_all_auths();

    let staking = env.register(MockStaking, ());
    let first_governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (first_governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    // Re-register the same address with a different governor. On-chain
    // this has no equivalent at all; here it is purely a testutils
    // convenience that reruns __constructor and overwrites Config.
    let second_governor = Address::generate(&env);
    env.register_at(
        &contract_id,
        RiskOracle,
        (second_governor.clone(), registry.clone(), staking.clone()),
    );

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    let cfg = asset_config(&env, &asset, &issuer);
    client.add_asset(&cfg);

    // The call succeeded under mock_all_auths() regardless of which
    // governor is configured, so success alone would prove nothing.
    // What proves the config was really replaced is WHICH address
    // `require_auth` recorded the authorization against: the second
    // governor, never the first.
    assert_eq!(
        env.auths(),
        [(
            second_governor,
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    client.address.clone(),
                    soroban_sdk::symbol_short!("add_asset"),
                    (cfg,).into_val(&env),
                )),
                sub_invocations: std::vec![],
            }
        )]
    );
}

// -- history_status tests (Issue #58) --

#[test]
fn history_status_unknown_asset_returns_error() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);

    let res = fx.client.try_history_status(&asset);
    assert_eq!(res, Err(Ok(Error::UnknownAsset)));
}

#[test]
fn history_status_registered_never_posted() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    let status = fx.client.history_status(&asset);
    assert_eq!(status.first_epoch, None);
    assert_eq!(status.newest_epoch, None);
    assert_eq!(status.newest_final, None);
    assert_eq!(status.span_epochs, 0);
    assert_eq!(status.epochs_until_span_ready, 168);
    assert_eq!(status.present_epochs_7d, 0);
    assert_eq!(status.min_present_epochs_7d, 96);
    assert!(!status.newest_slot_present);
    assert!(status.newest_final_stale);
    assert!(!status.score_ready);
}

#[test]
fn history_status_one_posted_epoch_pending() {
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);
    let epoch = 100u64;

    post(&env, &fx.client, &asset, epoch, 10_000_000);

    let status = fx.client.history_status(&asset);
    assert_eq!(status.first_epoch, Some(epoch));
    assert_eq!(status.newest_epoch, Some(epoch));
    assert_eq!(status.newest_final, None);
    assert_eq!(status.span_epochs, 0);
    assert_eq!(status.epochs_until_span_ready, 168);
    assert_eq!(status.present_epochs_7d, 0);
    assert_eq!(status.min_present_epochs_7d, 96);
    assert!(!status.newest_slot_present);
    assert!(status.newest_final_stale);
    assert!(!status.score_ready);
}

#[test]
fn history_status_167_final_epochs_in_a_row() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // 167 final epochs: epochs BASE..BASE+167.
    // To make BASE+166 final, we post BASE..BASE+168 (169 posts).
    // newest_final will be BASE+166 (167 epochs: BASE to BASE+166).
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    for i in 0..169u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let status = fx.client.history_status(&asset);
    assert_eq!(status.first_epoch, Some(BASE));
    assert_eq!(status.newest_epoch, Some(BASE + 168));
    assert_eq!(status.newest_final, Some(BASE + 166));
    assert_eq!(status.span_epochs, 167);
    assert_eq!(status.epochs_until_span_ready, 1);
    assert_eq!(status.present_epochs_7d, 167);
    assert!(!status.score_ready);

    // Agreement check (test 10): score() must be stale with nothing scored at newest_final
    let score = fx.client.score(&asset);
    assert!(score.stale);
    assert_ne!(score.epoch, BASE + 166);
}

#[test]
fn history_status_168_final_epochs_in_a_row_ready() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // 168 final epochs: epochs BASE..BASE+168.
    // Posting BASE..BASE+169 (170 posts) leaves BASE+167 as newest_final.
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    for i in 0..170u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let status = fx.client.history_status(&asset);
    assert_eq!(status.first_epoch, Some(BASE));
    assert_eq!(status.newest_epoch, Some(BASE + 169));
    assert_eq!(status.newest_final, Some(BASE + 167));
    assert_eq!(status.span_epochs, 168);
    assert_eq!(status.epochs_until_span_ready, 0);
    assert_eq!(status.present_epochs_7d, 168);
    assert_eq!(status.min_present_epochs_7d, 96);
    assert!(status.newest_slot_present);
    assert!(!status.newest_final_stale);
    assert!(status.score_ready);

    // Agreement check (test 10): score() has fresh, non-stale score at newest_final
    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(score.epoch, BASE + 167);
}

#[test]
fn history_status_168_span_with_95_present_gaps() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    // Asset first posted at BASE.
    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    staking_client.set_aggregate(&asset, &BASE, &EndpointStatus::Up);
    env.ledger().set_timestamp(time_at_epoch(BASE));
    let mut s = signal_set(&env, BASE, 10_000_000);
    s.liquidity_2pct = 500_000_000_000;
    s.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s);

    // We want newest_final to be BASE + 167 (span = 168).
    // The window is [BASE, BASE+167].
    // We want exactly 95 present slots in that window.
    // BASE is already posted and will be final.
    // Post 94 more epochs inside the window: from BASE + 74 to BASE + 167.
    // Number of epochs in [74, 167] is 167 - 74 + 1 = 94.
    // Total present in window = 1 + 94 = 95.
    for i in 74..=167u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    // Now finalize up to BASE + 167 by advancing clock and finalizing endpoint
    let target_final = BASE + 167;
    env.ledger()
        .set_timestamp(time_at_epoch(target_final) + crate::SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize_endpoint(&asset, &target_final);

    let status = fx.client.history_status(&asset);
    assert_eq!(status.span_epochs, 168);
    assert_eq!(status.epochs_until_span_ready, 0);
    assert_eq!(status.present_epochs_7d, 95);
    assert!(status.newest_slot_present);
    assert!(!status.score_ready);

    // Agreement check (test 10)
    let score = fx.client.score(&asset);
    assert!(score.stale);
    assert_ne!(score.epoch, target_final);
}

#[test]
fn history_status_exactly_96_present_with_newest_slot_present() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    staking_client.set_aggregate(&asset, &BASE, &EndpointStatus::Up);
    env.ledger().set_timestamp(time_at_epoch(BASE));
    let mut s = signal_set(&env, BASE, 10_000_000);
    s.liquidity_2pct = 500_000_000_000;
    s.supply_change_bps = 0;
    fx.client.post_signals(&keeper, &asset, &s);

    // BASE is 1. Post 95 more in [73, 167]: count = 167 - 73 + 1 = 95.
    // Total present in window = 1 + 95 = 96.
    for i in 73..=167u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let target_final = BASE + 167;
    env.ledger()
        .set_timestamp(time_at_epoch(target_final) + crate::SIGNAL_DISPUTE_SECS + 1);
    fx.client.finalize_endpoint(&asset, &target_final);

    let status = fx.client.history_status(&asset);
    assert_eq!(status.span_epochs, 168);
    assert_eq!(status.epochs_until_span_ready, 0);
    assert_eq!(status.present_epochs_7d, 96);
    assert!(status.newest_slot_present);
    assert!(!status.newest_final_stale);
    assert!(status.score_ready);

    // Agreement check (test 10)
    let score = fx.client.score(&asset);
    assert!(!score.stale);
    assert_eq!(score.epoch, target_final);
}

// The "enough history, newest slot missing" state Issue #58 enumerated
// (span >= 168, newest_final set, slot at newest_final empty) is not
// reachable through public calls: a Final slot can never be overturned
// (dispute requires Pending/Disputed state), and the finality scan always
// picks the newest present Final slot, skipping Empty ones. The closest
// reachable case overturns the newest epoch BEFORE it goes Final, which
// pulls newest_final back by one and drops span under 168 instead. See the
// PR description's "Known gaps" note.
#[test]
fn history_status_overturn_pulls_newest_final_back_so_span_falls_short() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    env.mock_all_auths();

    let staking = env.register(MockStaking, ());
    let governor = env.register(MockGovernor, ());
    let registry = Address::generate(&env);
    let contract_id = env.register(
        RiskOracle,
        (governor.clone(), registry.clone(), staking.clone()),
    );
    let client = RiskOracleClient::new(&env, &contract_id);

    let governor_client = crate::mocks::MockGovernorClient::new(&env, &governor);
    let committee = Address::generate(&env);
    governor_client.set_committee(&committee);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    client.add_asset(&asset_config(&env, &asset, &issuer));

    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &staking);

    // Post 168 epochs: BASE..=BASE+167
    for i in 0..168u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        client.post_signals(&keeper, &asset, &s);
    }

    // Dispute the newest epoch while still Pending (BASE + 167)
    let newest_ep = BASE + 167;
    let disputer = Address::generate(&env);
    client.dispute_signals(
        &disputer,
        &asset,
        &newest_ep,
        &BytesN::from_array(&env, &[9u8; 32]),
    );

    // Overturn: keeper_wins = false, slot clears to Empty.
    client.resolve_signal_dispute(
        &asset,
        &newest_ep,
        &false,
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    // Finalize older epochs up to BASE + 166.
    let target_final = BASE + 166;
    env.ledger()
        .set_timestamp(time_at_epoch(target_final) + crate::SIGNAL_DISPUTE_SECS + 1);
    client.finalize_endpoint(&asset, &target_final);

    let status = client.history_status(&asset);
    // newest_epoch stays at BASE + 167 (RingNewest tracks the newest ever
    // written; the overturn moves the slot to Empty but not that pointer).
    assert_eq!(status.newest_epoch, Some(newest_ep));
    // newest_final rolls back to the previous present Final epoch.
    assert_eq!(status.newest_final, Some(target_final));
    assert_eq!(status.span_epochs, 167);
    assert_eq!(status.epochs_until_span_ready, 1);
    // The 168-slot window ends at newest_final (BASE+166), which IS
    // present; the gate fails on span, not on the newest slot.
    assert!(status.newest_slot_present);
    assert!(!status.score_ready);

    // Agreement check (test 10)
    let score = client.score(&asset);
    assert!(score.stale);
    assert_ne!(score.epoch, newest_ep);
}

#[test]
fn history_status_enough_history_then_clock_stale() {
    const BASE: u64 = 400_000;
    let env = Env::default();
    let (fx, asset) = setup_with_asset(&env);

    let keeper = Address::generate(&env);
    let staking_client = crate::mocks::MockStakingClient::new(&env, &fx.staking);
    for i in 0..170u64 {
        let ep = BASE + i;
        staking_client.set_aggregate(&asset, &ep, &EndpointStatus::Up);
        env.ledger().set_timestamp(time_at_epoch(ep));
        let mut s = signal_set(&env, ep, 10_000_000);
        s.liquidity_2pct = 500_000_000_000;
        s.supply_change_bps = 0;
        fx.client.post_signals(&keeper, &asset, &s);
    }

    let newest_final = BASE + 167;
    // Advance clock past staleness threshold (STALE_AFTER_EPOCHS = 3)
    let stale_time = first_stale_time(newest_final);
    env.ledger().set_timestamp(stale_time);

    let status = fx.client.history_status(&asset);
    assert_eq!(status.span_epochs, 168);
    assert_eq!(status.present_epochs_7d, 168);
    assert!(status.newest_slot_present);
    assert!(status.newest_final_stale);
    assert!(!status.score_ready);

    // Agreement check (test 10)
    let score = fx.client.score(&asset);
    assert!(score.stale);
}
