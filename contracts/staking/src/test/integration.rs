//! Required integration test (task brief): deploy the REAL
//! `RiskOracle` and the REAL `Staking` together, no mock between
//! them. Runs the full cycle the brief names: keeper bonds, reporters
//! stake and probe, keeper posts signals, the endpoint comes from the
//! real `aggregate`, a signal dispute is opened, timed out via
//! `resolve_signal_dispute_timeout`, and bonds are released. Proves
//! the Section 12.3 interface matches both sides of the real call,
//! not a mock's approximation of it.
//!
//! Real SAC USDC throughout (`register_stellar_asset_contract_v2`,
//! same as every other test in this module; no mock token).

extern crate std;

use risk_oracle::{RiskOracle, RiskOracleClient};
use soroban_sdk::testutils::{cost_estimate::CostEstimate, Address as _, Ledger as _};
use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env, Symbol};
use sylox_types::network_limits::{
    TX_MAX_INSTRUCTIONS, TX_MAX_READ_LEDGER_ENTRIES, TX_MAX_WRITE_BYTES,
    TX_MAX_WRITE_LEDGER_ENTRIES,
};
use sylox_types::{
    AssetConfig, BondKey, EndpointStatus, IssuerActions, IssuerFlags, ProbeReport, Reference,
    SignalSet, TreasuryBucket,
};
use treasury::{Treasury, TreasuryClient};

use crate::{params, Staking, StakingClient};

/// Minimal `Governor` mock: `RiskOracle.resolve_signal_dispute_timeout`
/// reads `Governor.committee()` fresh at resolution time (ADR-010);
/// this is the one adapter neither `Staking` nor `RiskOracle` owns
/// and that `feat/staking`'s scope explicitly excludes
/// (`contracts/governor` is not implemented), so this test supplies
/// the minimal real contract both production contracts' interfaces
/// actually need, rather than skipping the call.
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

struct IntegrationFixture<'a> {
    staking: StakingClient<'a>,
    staking_id: Address,
    oracle: RiskOracleClient<'a>,
    usdc_client: token::TokenClient<'a>,
    usdc_admin_client: token::StellarAssetClient<'a>,
    treasury: Address,
    committee: Address,
}

fn setup(env: &Env) -> IntegrationFixture<'_> {
    env.mock_all_auths_allowing_non_root_auth();

    // A real Governor contract, not a plain Address: Staking's own
    // `governor` field is never read by anything this test exercises
    // (add_keeper/add_reporter rely on mock_all_auths_allowing_non_root_auth,
    // not a real governance flow), but RiskOracle.resolve_signal_dispute_timeout
    // (ADR-010) reads committee() fresh from its OWN config.governor,
    // which must resolve as a real contract call. contracts/governor
    // is out of this PR's scope (task brief), so this minimal
    // MockGovernor is the one adapter this test must supply itself.
    let governor_contract = env.register(MockGovernor, ());
    let governor_client = MockGovernorClient::new(env, &governor_contract);
    let committee = Address::generate(env);
    governor_client.set_committee(&committee);

    let registry = Address::generate(env);
    let treasury = Address::generate(env);

    let sac_admin = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let usdc_client = token::TokenClient::new(env, &usdc);
    let usdc_admin_client = token::StellarAssetClient::new(env, &usdc);

    // Staking's constructor needs RiskOracle's address and RiskOracle's
    // needs Staking's, so neither contract can be `register`-ed (with
    // its real constructor) before the other's address exists.
    // Generating both addresses up front breaks the cycle - each
    // `register_at` below runs with every address its own constructor
    // needs already in hand.
    let staking_id = Address::generate(env);
    let oracle_id = Address::generate(env);

    // Staking.oracle = the real RiskOracle's own address: this is
    // exactly the cross-contract wiring Section 12.3 describes, and
    // the whole point of this test (no mock standing in for either
    // side of the lock_bond/release_bond/aggregate/is_active_keeper
    // calls that cross this boundary).
    env.register_at(
        &staking_id,
        Staking,
        (
            governor_contract.clone(),
            oracle_id.clone(),
            registry.clone(),
            treasury.clone(),
            usdc.clone(),
        ),
    );
    let staking = StakingClient::new(env, &staking_id);

    env.register_at(
        &oracle_id,
        RiskOracle,
        (
            governor_contract.clone(),
            registry.clone(),
            staking_id.clone(),
        ),
    );
    let oracle = RiskOracleClient::new(env, &oracle_id);

    IntegrationFixture {
        staking,
        staking_id,
        oracle,
        usdc_client,
        usdc_admin_client,
        treasury,
        committee,
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

fn signal_set(env: &Env, epoch: u64, poster: &Address) -> SignalSet {
    SignalSet {
        epoch,
        posted_at: 0,
        peg_ratio: sylox_types::SCALE,
        peg_ratio_p10: sylox_types::SCALE,
        liquidity_2pct: 500_000_000_000,
        redemption_net: 0,
        supply: 10_000_000_000_000,
        supply_change_bps: 0,
        issuer_actions: IssuerActions::default(),
        endpoint: EndpointStatus::Unknown,
        inputs_hash: BytesN::from_array(env, &[7u8; 32]),
        poster: poster.clone(),
    }
}

#[test]
fn full_cycle_real_risk_oracle_and_real_staking_signal_dispute_timeout_releases_bonds() {
    let env = Env::default();
    let fx = setup(&env);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(&env, &asset, &issuer));

    // Keeper bonds in the real Staking.
    let keeper = Address::generate(&env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin_client.mint(&keeper, &params::KEEPER_BOND);
    fx.staking.stake(&keeper, &params::KEEPER_BOND);
    assert!(fx.staking.is_active_keeper(&keeper));

    // 3 reporters across 2 distinct regions stake and probe epoch 0.
    let regions = ["eu", "us", "af"];
    let mut reporters = soroban_sdk::Vec::new(&env);
    env.ledger().set_timestamp(params::EPOCH_SECS / 2);
    for r in regions {
        let reporter = Address::generate(&env);
        fx.staking.add_reporter(&reporter, &Symbol::new(&env, r));
        fx.usdc_admin_client
            .mint(&reporter, &params::REPORTER_STAKE);
        fx.staking.stake(&reporter, &params::REPORTER_STAKE);
        fx.staking.submit_probe(
            &reporter,
            &sylox_types::ProbeReport {
                asset: asset.clone(),
                epoch: 0,
                status: EndpointStatus::Up,
                region: Symbol::new(&env, "ignored"),
                evidence_hash: BytesN::from_array(&env, &[0u8; 32]),
            },
        );
        reporters.push_back(reporter);
    }

    // Real aggregate, read directly, before the keeper ever posts.
    assert_eq!(fx.staking.aggregate(&asset, &0), EndpointStatus::Up);

    // Epoch 0 must be closed (strictly in the past) before
    // post_signals accepts it (check_epoch_window).
    env.ledger().set_timestamp(params::EPOCH_SECS + 10);
    fx.oracle
        .post_signals(&keeper, &asset, &signal_set(&env, 0, &keeper));

    // The posted SignalSet's endpoint came only from Staking's real
    // aggregate, proving post_signals -> Staking.aggregate works
    // end-to-end with no mock on either side.
    let posted = fx.oracle.signals(&asset, &0).unwrap();
    assert_eq!(posted.endpoint, EndpointStatus::Up);

    // A signal dispute is opened: RiskOracle.dispute_signals calls
    // the real Staking.lock_bond with the keeper passed through as
    // subject (the second approved RiskOracle change).
    let disputer = Address::generate(&env);
    let dispute_bond = 10_000_000_000i128;
    fx.usdc_admin_client.mint(&disputer, &dispute_bond);
    fx.oracle
        .dispute_signals(&disputer, &asset, &0, &BytesN::from_array(&env, &[9u8; 32]));

    let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), 0);
    assert_eq!(
        fx.staking.bond(&bond_key),
        Some((disputer.clone(), dispute_bond))
    );
    // The subject pass-through: the keeper's own open_dispute_count
    // reflects the real lock_bond call RiskOracle made, not a mock
    // recording that a call happened.
    assert_eq!(fx.staking.keeper(&keeper).unwrap().open_dispute_count, 1);

    // Nobody rules; time passes the ruling deadline.
    let now = env.ledger().timestamp();
    env.ledger()
        .set_timestamp(now + crate_signal_dispute_ruling_secs());
    fx.oracle.resolve_signal_dispute_timeout(&asset, &0);

    // Both sides of the outcome, read from the real Staking, not a
    // mock's call_count: the disputer's bond is gone (released, not
    // forfeited) and the keeper's own bond and open dispute count are
    // untouched (nobody slashed).
    assert_eq!(fx.staking.bond(&bond_key), None);
    assert_eq!(fx.staking.claimable(&disputer), dispute_bond);
    assert_eq!(fx.staking.keeper(&keeper).unwrap().open_dispute_count, 0);
    assert_eq!(
        fx.staking.keeper(&keeper).unwrap().bond,
        params::KEEPER_BOND
    );

    let claimed = fx.staking.claim(&disputer);
    assert_eq!(claimed, dispute_bond);
    assert_eq!(fx.usdc_client.balance(&disputer), dispute_bond);

    // The keeper's posting stands: epoch 0 is effectively Final. Not
    // a ring() scan for epoch == 0: an untouched Empty placeholder
    // slot elsewhere in the 240-slot ring also carries epoch == 0
    // (RingSlot's default), so ring().iter().find(epoch == 0) can
    // match the wrong slot; is_final() is RiskOracle's own
    // unambiguous read for exactly this question.
    assert!(fx.oracle.is_final(&asset, &0));

    let _ = fx.staking_id;
    let _ = fx.treasury;
    let _ = fx.committee;
    let _ = reporters;
}

/// Mirrors `RiskOracle`'s own private `SIGNAL_DISPUTE_RULING_SECS`
/// (not exported): this integration test lives in a different crate,
/// so it cannot read that constant directly, and the task's own
/// instruction to "derive all times from the parameter constants,
/// never hand-computed" extends to not silently duplicating a magic
/// number either. `RiskOracle`'s `ADR-010` documents the default
/// (7 days) as the value under test; this helper names that fact
/// instead of inlining `604_800` at the call site.
fn crate_signal_dispute_ruling_secs() -> u64 {
    604_800
}

/// Mirrors `RiskOracle`'s own private `keeper_slash()` (not exported):
/// the amount `Staking.slash` is called with on a lost signal dispute
/// (Section 7.8). Same reasoning as `crate_signal_dispute_ruling_secs`
/// above: name the fact instead of inlining the magic number at the
/// call site.
fn crate_keeper_slash() -> i128 {
    10_000_000_000
}

/// Full task-brief integration test: the REAL `RiskOracle`, `Staking`
/// AND `Treasury` together (no mock standing in for any of the three),
/// against a real USDC SAC throughout. Funds both reward buckets,
/// bonds a keeper and stakes 3 reporters, posts 2 epochs so the first
/// becomes Final and the keeper is rewarded exactly once for it,
/// disputes the second and has the keeper LOSE that dispute (so the
/// slashed treasury share lands in Treasury's Slashed bucket, split
/// 50/50 with the disputer per Section 7.8), settles probes for the
/// first epoch (rewarding the matching reporters from Treasury), and
/// has the keeper and every rewarded reporter claim from Treasury
/// while the disputer claims from Staking. Every balance is asserted
/// at the end: Treasury's own USDC balance, every one of its 4 bucket
/// balances, every accrued-but-unclaimed amount, and Staking's own
/// USDC balance against its tracked liabilities.
struct TreasuryIntegrationFixture<'a> {
    staking: StakingClient<'a>,
    oracle: RiskOracleClient<'a>,
    treasury: TreasuryClient<'a>,
    treasury_id: Address,
    usdc_client: token::TokenClient<'a>,
    usdc_admin_client: token::StellarAssetClient<'a>,
}

fn setup_with_treasury(env: &Env) -> TreasuryIntegrationFixture<'_> {
    env.mock_all_auths_allowing_non_root_auth();

    let governor_contract = env.register(MockGovernor, ());
    let governor_client = MockGovernorClient::new(env, &governor_contract);
    let committee = Address::generate(env);
    governor_client.set_committee(&committee);

    let registry = Address::generate(env);

    let sac_admin = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let usdc_client = token::TokenClient::new(env, &usdc);
    let usdc_admin_client = token::StellarAssetClient::new(env, &usdc);

    // A 3-way address cycle: Staking's constructor needs oracle_id and
    // treasury_id, Treasury's needs staking_id, and RiskOracle's needs
    // staking_id too. Generating all three addresses up front (same
    // reasoning as `setup` above) breaks the cycle so each
    // `register_at` runs with every address its own constructor needs
    // already in hand, regardless of registration order.
    let staking_id = Address::generate(env);
    let oracle_id = Address::generate(env);
    let treasury_id = Address::generate(env);

    env.register_at(
        &staking_id,
        Staking,
        (
            governor_contract.clone(),
            oracle_id.clone(),
            registry.clone(),
            treasury_id.clone(),
            usdc.clone(),
        ),
    );
    let staking = StakingClient::new(env, &staking_id);

    env.register_at(
        &oracle_id,
        RiskOracle,
        (
            governor_contract.clone(),
            registry.clone(),
            staking_id.clone(),
        ),
    );
    let oracle = RiskOracleClient::new(env, &oracle_id);

    env.register_at(
        &treasury_id,
        Treasury,
        (governor_contract.clone(), staking_id.clone(), usdc.clone()),
    );
    let treasury = TreasuryClient::new(env, &treasury_id);

    TreasuryIntegrationFixture {
        staking,
        oracle,
        treasury,
        treasury_id,
        usdc_client,
        usdc_admin_client,
    }
}

fn fund_bucket(fx: &TreasuryIntegrationFixture, env: &Env, bucket: TreasuryBucket, amount: i128) {
    let funder = Address::generate(env);
    fx.usdc_admin_client.mint(&funder, &amount);
    fx.treasury.deposit(&funder, &bucket, &amount);
}

/// Mirrors `RiskOracle`'s own private `FINALITY_LOOKBACK_EPOCHS` (not
/// exported): `WINDOW_SECS / EPOCH_SECS + 1` at the defaults (73).
/// Same reasoning as `crate_signal_dispute_ruling_secs` and
/// `crate_keeper_slash` above.
fn crate_finality_lookback_epochs() -> u64 {
    73
}

/// PR #13 review, item 1: the PR's own `risk-oracle` budget test for
/// this same worst case
/// (`budget_reward_keeper_grouping_across_a_full_backfill_window_several_keepers_mock_staking`)
/// uses `MockStaking`, whose `reward_keeper` is a cheap storage write
/// with no further cross-contract call. Production's real
/// `Staking.reward_keeper` itself calls `Treasury.accrue_reward`, a
/// second hop the mock never makes, so that test's own number
/// UNDERSTATES the real cost. This test measures the same scenario
/// (a full backfill window, `FINALITY_LOOKBACK_EPOCHS` epochs, 73 at
/// the defaults, becoming Final in one call, 5 rotating keepers) with
/// the REAL `RiskOracle`, REAL `Staking`, and REAL `Treasury` wired
/// together, against a real USDC SAC, so the two numbers can be
/// reported side by side in the PR description.
#[test]
fn budget_reward_keeper_grouping_with_real_staking_and_treasury() {
    let env = Env::default();
    let fx = setup_with_treasury(&env);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(&env, &asset, &issuer));

    const KEEPER_COUNT: usize = 5;
    let keepers: std::vec::Vec<Address> =
        (0..KEEPER_COUNT).map(|_| Address::generate(&env)).collect();
    for keeper in &keepers {
        fx.staking.add_keeper(keeper);
        fx.usdc_admin_client.mint(keeper, &params::KEEPER_BOND);
        fx.staking.stake(keeper, &params::KEEPER_BOND);
    }

    // Funded generously: worst case is every one of the
    // backfill_count epochs below paying out
    // KEEPER_REWARD_PER_ACCEPTED_EPOCH once.
    let lookback = crate_finality_lookback_epochs();
    let backfill_count = lookback - 1;
    fund_bucket(
        &fx,
        &env,
        TreasuryBucket::KeeperRewards,
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH * backfill_count as i128,
    );

    // Same backfill construction as the mock-staking version: every
    // post lands at the SAME fixed `now`, each posting an older
    // epoch than RiskOracle's own notion of "current", so none of
    // them can become Final during this loop.
    let now = backfill_count * params::EPOCH_SECS + params::EPOCH_SECS;
    env.ledger().set_timestamp(now);
    for epoch in 0..backfill_count {
        let keeper = &keepers[epoch as usize % KEEPER_COUNT];
        fx.oracle
            .post_signals(keeper, &asset, &signal_set(&env, epoch, keeper));
    }

    // Same cumulative-harness-budget reasoning as the mock-staking
    // version's own comment: the backfill loop above already spent
    // most of the shared 100M instruction default before this
    // measured call starts, so it is lifted here, right before the
    // one call actually being measured.
    env.cost_estimate().budget().reset_unlimited();
    let final_epoch = backfill_count;
    env.ledger()
        .set_timestamp((final_epoch + 1) * params::EPOCH_SECS + params::SIGNAL_DISPUTE_SECS);
    fx.oracle.post_signals(
        &keepers[0],
        &asset,
        &signal_set(&env, final_epoch, &keepers[0]),
    );

    let estimate = env.cost_estimate();
    print_resources(
        "post_signals, full backfill window (73 epochs) crossing into Final in one call, \
         5 keepers, REAL Staking + REAL Treasury (the real cost; see the MockStaking lower \
         bound in risk-oracle's own budget_test.rs)",
        &estimate,
    );

    // Sanity check this test actually measures what it claims: all 5
    // keepers must have been rewarded (accrued in Treasury) by this
    // one call, not 0 and not partially.
    for keeper in &keepers {
        assert!(
            fx.treasury.accrued(keeper) > 0,
            "every one of the 5 rotating keepers must have been rewarded by this call"
        );
    }

    let resources = estimate.resources();
    assert!(
        (resources.instructions as u64) < TX_MAX_INSTRUCTIONS / 2,
        "must stay under 50% of tx_max_instructions ({TX_MAX_INSTRUCTIONS}) even with \
         every keeper in the window rewarded in one call through the real Staking and \
         Treasury contracts; got {}",
        resources.instructions
    );
    assert!(
        (resources.write_bytes as u64) < TX_MAX_WRITE_BYTES / 2,
        "must stay under 50% of tx_max_write_bytes ({TX_MAX_WRITE_BYTES}); got {}",
        resources.write_bytes
    );
    assert!(
        (resources.disk_read_entries as u64) < TX_MAX_READ_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_disk_read_entries ({TX_MAX_READ_LEDGER_ENTRIES}); got {}",
        resources.disk_read_entries
    );
    assert!(
        (resources.write_entries as u64) < TX_MAX_WRITE_LEDGER_ENTRIES as u64 / 2,
        "must stay under 50% of tx_max_write_ledger_entries ({TX_MAX_WRITE_LEDGER_ENTRIES}); got {}",
        resources.write_entries
    );
}

fn print_resources(label: &str, estimate: &CostEstimate) {
    let resources = estimate.resources();
    let fee = estimate.fee();
    std::println!("--- {label} ---");
    std::println!("  instructions:        {}", resources.instructions);
    std::println!("  mem_bytes:           {}", resources.mem_bytes);
    std::println!("  disk_read_entries:   {}", resources.disk_read_entries);
    std::println!("  memory_read_entries: {}", resources.memory_read_entries);
    std::println!("  disk_read_bytes:     {}", resources.disk_read_bytes);
    std::println!("  write_entries:       {}", resources.write_entries);
    std::println!("  write_bytes:         {}", resources.write_bytes);
    std::println!("  fee.total (stroops): {}", fee.total);
}

#[test]
fn full_cycle_real_risk_oracle_real_staking_and_real_treasury() {
    let env = Env::default();
    let fx = setup_with_treasury(&env);

    let asset = Address::generate(&env);
    let issuer = Address::generate(&env);
    fx.oracle.add_asset(&asset_config(&env, &asset, &issuer));

    // Fund both reward buckets up front, generously (the task brief's
    // own instruction: "fund both reward buckets via deposit"), well
    // above what this scenario will ever draw down, so no shortfall
    // path is exercised here (that is already covered by the
    // dedicated unfunded-reward tests in both crates).
    fund_bucket(&fx, &env, TreasuryBucket::KeeperRewards, 1_000_000_000);
    fund_bucket(&fx, &env, TreasuryBucket::ReporterRewards, 1_000_000_000);

    // Keeper bonds.
    let keeper = Address::generate(&env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin_client.mint(&keeper, &params::KEEPER_BOND);
    fx.staking.stake(&keeper, &params::KEEPER_BOND);
    assert!(fx.staking.is_active_keeper(&keeper));

    // 3 reporters across 2 distinct regions stake and probe epoch 0.
    let regions = ["eu", "us", "af"];
    let mut reporters = soroban_sdk::Vec::new(&env);
    env.ledger().set_timestamp(params::EPOCH_SECS / 2);
    for r in regions {
        let reporter = Address::generate(&env);
        fx.staking.add_reporter(&reporter, &Symbol::new(&env, r));
        fx.usdc_admin_client
            .mint(&reporter, &params::REPORTER_STAKE);
        fx.staking.stake(&reporter, &params::REPORTER_STAKE);
        fx.staking.submit_probe(
            &reporter,
            &ProbeReport {
                asset: asset.clone(),
                epoch: 0,
                status: EndpointStatus::Up,
                region: Symbol::new(&env, "ignored"),
                evidence_hash: BytesN::from_array(&env, &[0u8; 32]),
            },
        );
        reporters.push_back(reporter);
    }
    assert_eq!(fx.staking.aggregate(&asset, &0), EndpointStatus::Up);

    // Keeper posts epoch 0, then epoch 1 (closed, inside the backfill
    // window, matching check_epoch_window): epoch 0 crosses into
    // Final only once epoch 1's own post advances the finality scan
    // past it (the same 2-epoch SIGNAL_DISPUTE_SECS lag every other
    // test in this workspace accounts for), rewarding the keeper
    // exactly once for exactly 1 accepted epoch.
    env.ledger().set_timestamp(params::EPOCH_SECS + 10);
    fx.oracle
        .post_signals(&keeper, &asset, &signal_set(&env, 0, &keeper));
    env.ledger()
        .set_timestamp(params::EPOCH_SECS * 2 + params::SIGNAL_DISPUTE_SECS + 10);
    fx.oracle
        .post_signals(&keeper, &asset, &signal_set(&env, 1, &keeper));

    assert!(fx.oracle.is_final(&asset, &0));
    assert_eq!(
        fx.treasury.accrued(&keeper),
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH,
        "the keeper must be accrued exactly 1 accepted epoch's worth, \
         for epoch 0 alone; epoch 1 is not Final yet"
    );

    // A signal dispute is opened against epoch 1 and the keeper LOSES
    // it: Section 7.8's 50/50 split sends half the keeper_slash to the
    // disputer (Staking's own Claimable, a participant fund) and half
    // to Treasury's Slashed bucket (a protocol fund), via the real
    // cross-contract Treasury.deposit call feat/treasury introduced.
    let disputer = Address::generate(&env);
    let dispute_bond = 10_000_000_000i128;
    fx.usdc_admin_client.mint(&disputer, &dispute_bond);
    fx.oracle
        .dispute_signals(&disputer, &asset, &1, &BytesN::from_array(&env, &[9u8; 32]));

    let now = env.ledger().timestamp();
    env.ledger()
        .set_timestamp(now + crate_signal_dispute_ruling_secs() / 2);
    fx.oracle
        .resolve_signal_dispute(&asset, &1, &false, &BytesN::from_array(&env, &[0u8; 32]));

    let keeper_slash = crate_keeper_slash();
    let to_disputer = keeper_slash / 2;
    let to_treasury = keeper_slash - to_disputer;
    assert_eq!(fx.staking.claimable(&disputer), dispute_bond + to_disputer);
    assert_eq!(
        fx.treasury.balance(&TreasuryBucket::Slashed),
        to_treasury,
        "the keeper's lost-dispute slash must land in Treasury's Slashed bucket, \
         not a Staking-side Claimable credit"
    );
    assert_eq!(
        fx.staking.keeper(&keeper).unwrap().bond,
        params::KEEPER_BOND - keeper_slash
    );
    // The bond lock for the dispute itself is released back to the
    // disputer alongside their winnings (BondKey::SignalDispute),
    // untouched by the keeper's separate slash.
    assert_eq!(
        fx.staking.bond(&BondKey::SignalDispute(asset.clone(), 1)),
        None
    );

    // settle_probes for epoch 0: the window opens PROBE_GRACE_SECS
    // after epoch 0's own close.
    let settle_at = params::EPOCH_SECS * 2 + params::PROBE_GRACE_SECS + 10;
    env.ledger().set_timestamp(settle_at);
    fx.staking.settle_probes(&asset, &0);

    let per_reporter_reward = params::REPORTER_REWARD_PER_EPOCH / reporters.len() as i128;
    for reporter in reporters.iter() {
        assert_eq!(
            fx.treasury.accrued(&reporter),
            per_reporter_reward,
            "every one of the 3 matching reporters must be accrued its equal share"
        );
    }

    // Claims: the keeper and all 3 reporters claim from Treasury; the
    // disputer claims from Staking.
    let keeper_claimed = fx.treasury.claim_reward(&keeper);
    assert_eq!(keeper_claimed, params::KEEPER_REWARD_PER_ACCEPTED_EPOCH);
    assert_eq!(fx.usdc_client.balance(&keeper), keeper_claimed);

    let mut reporter_claims_total = 0i128;
    for reporter in reporters.iter() {
        let claimed = fx.treasury.claim_reward(&reporter);
        assert_eq!(claimed, per_reporter_reward);
        assert_eq!(fx.usdc_client.balance(&reporter), claimed);
        reporter_claims_total += claimed;
    }

    let disputer_claimed = fx.staking.claim(&disputer);
    assert_eq!(disputer_claimed, dispute_bond + to_disputer);
    assert_eq!(fx.usdc_client.balance(&disputer), disputer_claimed);

    // -- final balance assertions --

    // Treasury's own USDC balance equals every bucket's balance plus
    // every still-accrued-but-unclaimed amount (T1, exact here since
    // no direct donation was ever made to this contract instance):
    // KeeperRewards and ReporterRewards are both drawn down by
    // exactly what was claimed above, Slashed holds the treasury's
    // half of the keeper's slash, Fees is untouched, and nothing
    // remains accrued (every accrual above was claimed).
    let expected_keeper_bucket = 1_000_000_000 - params::KEEPER_REWARD_PER_ACCEPTED_EPOCH;
    let expected_reporter_bucket = 1_000_000_000 - reporter_claims_total;
    assert_eq!(
        fx.treasury.balance(&TreasuryBucket::KeeperRewards),
        expected_keeper_bucket
    );
    assert_eq!(
        fx.treasury.balance(&TreasuryBucket::ReporterRewards),
        expected_reporter_bucket
    );
    assert_eq!(fx.treasury.balance(&TreasuryBucket::Slashed), to_treasury);
    assert_eq!(fx.treasury.balance(&TreasuryBucket::Fees), 0);
    assert_eq!(fx.treasury.accrued(&keeper), 0);
    for reporter in reporters.iter() {
        assert_eq!(fx.treasury.accrued(&reporter), 0);
    }
    assert_eq!(
        fx.usdc_client.balance(&fx.treasury_id),
        expected_keeper_bucket + expected_reporter_bucket + to_treasury,
        "Treasury's real USDC balance must equal the sum of its bucket balances \
         (T1), with nothing left accrued"
    );

    // Staking's own USDC balance equals exactly its tracked
    // liabilities (the S1 invariant this PR tightened to exact
    // equality): the keeper's remaining bond, the 3 reporters' stakes,
    // and nothing left Claimable (the disputer already claimed).
    let keeper_bond_remaining = params::KEEPER_BOND - keeper_slash;
    let reporter_stakes_total = params::REPORTER_STAKE * reporters.len() as i128;
    assert_eq!(fx.staking.claimable(&disputer), 0);
    assert_eq!(
        fx.usdc_client.balance(&fx.staking.address),
        keeper_bond_remaining + reporter_stakes_total,
        "Staking's real USDC balance must equal exactly its tracked \
         liabilities once every claim has settled"
    );
}

/// docs/design/held-repost-disputes.md D4: an hourly dispute nobody
/// closes before the ring rotates past its epoch. The timeout, called 11
/// days after the dispute (the ring holds 240 hours), still releases the
/// disputer's bond and brings the keeper's `open_dispute_count` back to
/// zero. Only the epoch's own Final mark is gone with the rotated slot.
#[test]
fn an_hourly_dispute_timed_out_after_the_ring_rotated_still_releases_the_bonds() {
    let env = Env::default();
    // The real Treasury: every epoch that goes Final pays its keeper.
    let fx = setup_with_treasury(&env);
    fund_bucket(&fx, &env, TreasuryBucket::KeeperRewards, 1_000_000_000_000);
    let asset = Address::generate(&env);
    fx.oracle
        .add_asset(&asset_config(&env, &asset, &Address::generate(&env)));
    let keeper = Address::generate(&env);
    fx.staking.add_keeper(&keeper);
    fx.usdc_admin_client.mint(&keeper, &params::KEEPER_BOND);
    fx.staking.stake(&keeper, &params::KEEPER_BOND);

    env.ledger().set_timestamp(params::EPOCH_SECS + 10);
    fx.oracle
        .post_signals(&keeper, &asset, &signal_set(&env, 0, &keeper));
    let disputer = Address::generate(&env);
    let dispute_bond = 10_000_000_000i128;
    fx.usdc_admin_client.mint(&disputer, &dispute_bond);
    let disputed_at = env.ledger().timestamp();
    fx.oracle
        .dispute_signals(&disputer, &asset, &0, &BytesN::from_array(&env, &[9u8; 32]));
    assert_eq!(fx.staking.keeper(&keeper).unwrap().open_dispute_count, 1);

    // 240 newer epochs rotate the ring past epoch 0's slot.
    for epoch in 1..=240u64 {
        env.ledger()
            .set_timestamp((epoch + 1) * params::EPOCH_SECS + 10);
        fx.oracle
            .post_signals(&keeper, &asset, &signal_set(&env, epoch, &keeper));
    }

    env.ledger().set_timestamp(disputed_at + 11 * 86_400);
    fx.oracle.resolve_signal_dispute_timeout(&asset, &0);

    let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), 0);
    assert_eq!(fx.staking.bond(&bond_key), None);
    assert_eq!(fx.staking.claimable(&disputer), dispute_bond);
    assert_eq!(fx.staking.keeper(&keeper).unwrap().open_dispute_count, 0);
    assert!(
        !fx.oracle.is_final(&asset, &0),
        "the rotated slot is gone, so epoch 0 is never marked Final"
    );
}
