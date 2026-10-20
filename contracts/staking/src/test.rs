//! Staking test suite. technical-doc.md Section 7, 12.3; the task
//! brief's TESTS section; and the lead-decision rounds' own test
//! lists (settlement-expiry safety, removed-reporter/keeper handling,
//! keeper-dispute linkage, parameter derivation, Treasury split).
//!
//! S1-S4 (accounting invariants, task brief, tightened in
//! feat/treasury now that every protocol fund lives in `Treasury`):
//! S1. USDC balance of Staking EQUALS total keeper bonds + total
//!     reporter stake (including stake in cooldown) + total locked
//!     bonds + unclaimed `claimable` balances, apart from direct
//!     donations. Tested by `property::s1_s2_s3_never_break` and
//!     `staking_balance_equals_tracked_liabilities_exactly`.
//! S2. A bond is either Locked, Released or Forfeited, and moves at
//!     most once. Tested by `release_bond_and_forfeit_bond_on_the_same_key_decrements_once`
//!     and `property::s1_s2_s3_never_break`.
//! S3. No function moves a participant's stake or bond to anyone
//!     other than that participant or a dispute winner (the treasury
//!     half of a slash/forfeit now leaves via a real `Treasury.deposit`
//!     call, never a local `Claimable` credit). Tested by
//!     `property::s1_s2_s3_never_break`.
//! S4. aggregate never writes. Tested by
//!     `aggregate_never_writes_to_storage`.

extern crate std;

mod aggregation_table;
mod integration;
mod property;
mod ttl;

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{token, Address, BytesN, Env, IntoVal, Symbol};
use sylox_types::{BondKey, EndpointStatus, ProbeReport, TreasuryBucket};

use crate::{params, Error, Staking, StakingClient};

// -- setup --

struct Fixture<'a> {
    client: StakingClient<'a>,
    contract_id: Address,
    treasury_client: treasury::TreasuryClient<'a>,
    usdc_admin_client: token::StellarAssetClient<'a>,
    usdc_client: token::TokenClient<'a>,
}

/// A real Stellar Asset Contract for USDC (the task's own requirement:
/// "Use a real Stellar Asset Contract for USDC in tests
/// (register_stellar_asset_contract_v2), not a mock token"), not a
/// mock token contract. A real `Treasury` contract, not a mock,
/// since feat/treasury's whole point is that `Staking` now calls the
/// genuine `Treasury.deposit`/`accrue_reward`, and this suite's own
/// slash/forfeit/reward tests need to see those calls actually land.
fn setup(env: &Env) -> Fixture<'_> {
    // Not plain mock_all_auths(): test helpers like lock_bond's caller
    // (the "oracle") authorizing a transfer FROM a third address (the
    // disputer) is an auth that is not tied to that call's own root
    // invocation, which mock_all_auths() alone does not mock (see its
    // own doc comment). Every call in this suite is still a genuine
    // Result-returning call through the contract's real auth checks
    // (require_auth is still evaluated, just always satisfied); no
    // auth requirement anywhere in Staking is skipped or weakened by
    // this choice.
    env.mock_all_auths_allowing_non_root_auth();
    let governor = Address::generate(env);
    let oracle = Address::generate(env);
    let registry = Address::generate(env);

    let sac_admin = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let usdc_admin_client = token::StellarAssetClient::new(env, &usdc);
    let usdc_client = token::TokenClient::new(env, &usdc);

    // Staking's constructor needs `treasury_id` and `Treasury`'s needs
    // `contract_id`, so neither can be registered (with its real
    // `initialize`-replacing constructor) before the other exists.
    // `Address::generate` hands out both addresses up front - no
    // on-chain equivalent is needed here, since these are just test
    // addresses - breaking the cycle so each `register_at` below runs
    // with every address its own constructor needs already in hand.
    let contract_id = Address::generate(env);
    let treasury_id = Address::generate(env);

    env.register_at(
        &treasury_id,
        treasury::Treasury,
        (governor.clone(), contract_id.clone(), usdc.clone()),
    );
    let treasury_client = treasury::TreasuryClient::new(env, &treasury_id);

    env.register_at(
        &contract_id,
        Staking,
        (
            governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury_id.clone(),
            usdc.clone(),
        ),
    );
    let client = StakingClient::new(env, &contract_id);

    Fixture {
        client,
        contract_id,
        treasury_client,
        usdc_admin_client,
        usdc_client,
    }
}

/// Mints `amount` of the fixture's USDC to `who` and returns it so a
/// test can immediately `stake`/etc.
fn fund(fx: &Fixture, who: &Address, amount: i128) {
    fx.usdc_admin_client.mint(who, &amount);
}

/// Funds `Treasury`'s `bucket` with `amount`, via a genuine
/// `Treasury.deposit` call from a freshly minted address, so reward
/// tests (`settle_probes`, `reward_keeper`) have a real, non-empty
/// bucket to accrue from.
fn fund_treasury_bucket(env: &Env, fx: &Fixture, bucket: TreasuryBucket, amount: i128) {
    let funder = Address::generate(env);
    fund(fx, &funder, amount);
    fx.treasury_client.deposit(&funder, &bucket, &amount);
}

fn add_and_fund_keeper(env: &Env, fx: &Fixture, amount: i128) -> Address {
    let keeper = Address::generate(env);
    fx.client.add_keeper(&keeper);
    fund(fx, &keeper, amount);
    fx.client.stake(&keeper, &amount);
    keeper
}

fn add_and_fund_reporter(env: &Env, fx: &Fixture, region: &Symbol, amount: i128) -> Address {
    let reporter = Address::generate(env);
    fx.client.add_reporter(&reporter, region);
    fund(fx, &reporter, amount);
    fx.client.stake(&reporter, &amount);
    reporter
}

fn region(env: &Env, s: &str) -> Symbol {
    Symbol::new(env, s)
}

fn probe(env: &Env, asset: &Address, epoch: u64, status: EndpointStatus) -> ProbeReport {
    ProbeReport {
        asset: asset.clone(),
        epoch,
        status,
        // The report's own region is ignored by submit_probe (Section
        // 7.3); set to something obviously wrong to prove that.
        region: Symbol::new(env, "ignored"),
        evidence_hash: BytesN::from_array(env, &[0u8; 32]),
    }
}

/// The ledger timestamp at which `epoch` closes, matching
/// `submit_probe`/`settle_probes`'s own `(epoch + 1) * EPOCH_SECS`
/// (re-review convention, never hand-computed in a test).
fn epoch_close(epoch: u64) -> u64 {
    (epoch + 1) * params::EPOCH_SECS
}

/// The first timestamp `settle_probes`'s window opens at for `epoch`.
fn settlement_opens(epoch: u64) -> u64 {
    epoch_close(epoch) + params::PROBE_GRACE_SECS
}

/// The last timestamp `settle_probes`'s window still accepts a call at.
fn settlement_closes(epoch: u64) -> u64 {
    settlement_opens(epoch) + params::SETTLE_WINDOW_SECS
}

// -- constructor --

/// Issue #21: `initialize` is gone, replaced by `__constructor`, which
/// Soroban runs atomically at deploy time - there is no longer a
/// window where a contract address exists but is unconfigured, so
/// there is nothing left for an `AlreadyInitialized`/`NotInitialized`
/// pair of tests to prove on a freshly `register`-ed address (every
/// `register` call makes a brand new address, and the constructor for
/// that address has necessarily already run by the time the call
/// returns).
///
/// The one honest thing left to demonstrate is `register_at`'s own
/// testutils-only escape hatch: it re-runs a contract's constructor
/// against an address that is already registered (soroban-sdk's own
/// doc comment on `Env::register_at`: "Registering a contract that is
/// already registered replaces it... the new contract's constructor
/// will be called again during re-registration. That behavior only
/// exists for this test utility and is not reproducible on-chain").
/// This is not a way to "call the constructor twice" on a live
/// contract - there is no deployed equivalent, since on-chain, an
/// address's executable and constructor are fixed at creation and
/// `update_current_contract_wasm` (what `upgrade()` uses) never
/// re-invokes a constructor.
///
/// What re-registration actually does is narrower than a full reset:
/// the second `__constructor` run overwrites `CONFIG_KEY` in instance
/// storage exactly like the first did, but it is still the same
/// address and the same instance storage, so anything the first
/// constructor's *callers* wrote under a different key (here, a
/// keeper `add_keeper` added after the first registration) is left
/// alone. This test proves both halves at once: the config really is
/// replaced (the new governor/oracle/registry/treasury/usdc values
/// read back, not the old ones), while unrelated storage the
/// constructor itself never touches survives the "second
/// construction" untouched - a real contract's constructor has no
/// such risk, precisely because it can never run a second time there.
#[test]
fn cannot_initialize_a_second_time() {
    let env = Env::default();
    let fx = setup(&env);

    // Something the first constructor never wrote, added after the
    // fact through an ordinary call - this is the "unrelated storage"
    // half of the claim above.
    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    assert!(fx.client.keeper(&keeper).is_some());

    // register_at's "already registered" path (the only way to run a
    // second `__constructor` against the same address at all) installs
    // a wholly different config, naming a fresh governor/oracle/
    // registry/treasury/usdc.
    let new_governor = Address::generate(&env);
    let new_oracle = Address::generate(&env);
    let new_registry = Address::generate(&env);
    let new_treasury = Address::generate(&env);
    let new_usdc = Address::generate(&env);
    env.register_at(
        &fx.contract_id,
        Staking,
        (
            new_governor.clone(),
            new_oracle.clone(),
            new_registry.clone(),
            new_treasury.clone(),
            new_usdc.clone(),
        ),
    );

    // The config half: reading CONFIG_KEY directly (Config/CONFIG_KEY
    // are crate-private, so this test, as a child module of the
    // crate's own root, can see them) proves the second constructor's
    // values are what is actually stored now, not the first's.
    let stored_config: crate::Config = env.as_contract(&fx.contract_id, || {
        env.storage().instance().get(&crate::CONFIG_KEY).unwrap()
    });
    assert_eq!(stored_config.governor, new_governor);
    assert_eq!(stored_config.oracle, new_oracle);
    assert_eq!(stored_config.registry, new_registry);
    assert_eq!(stored_config.treasury, new_treasury);
    assert_eq!(stored_config.usdc, new_usdc);

    // The unrelated-storage half: the keeper added before
    // re-registration is still there, because re-running
    // `__constructor` only overwrites the one key it ever writes
    // (CONFIG_KEY), not the contract's entire instance storage.
    assert!(
        fx.client.keeper(&keeper).is_some(),
        "re-registration must not disturb storage the constructor itself never touches"
    );
}

// -- add_keeper / remove_keeper --

#[test]
fn add_keeper_requires_governor_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    assert!(fx.client.keeper(&keeper).is_some());
}

#[test]
fn add_keeper_rejects_a_duplicate() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    let result = fx.client.try_add_keeper(&keeper);
    assert_eq!(result, Err(Ok(Error::AlreadyRegistered)));
}

#[test]
fn add_keeper_rejects_an_address_already_registered_as_a_reporter() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    fx.client.add_reporter(&who, &region(&env, "eu"));
    let result = fx.client.try_add_keeper(&who);
    assert_eq!(result, Err(Ok(Error::RoleConflict)));
}

#[test]
fn remove_keeper_rejects_an_unregistered_address() {
    let env = Env::default();
    let fx = setup(&env);
    let result = fx.client.try_remove_keeper(&Address::generate(&env));
    assert_eq!(result, Err(Ok(Error::NotKeeper)));
}

#[test]
fn remove_keeper_deactivates_immediately_but_keeps_the_bond_locked() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    assert!(fx.client.is_active_keeper(&keeper));

    fx.client.remove_keeper(&keeper);
    assert!(!fx.client.is_active_keeper(&keeper));
    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, params::KEEPER_BOND);
}

// -- add_reporter / remove_reporter --

#[test]
fn add_reporter_fixes_the_region_at_registration() {
    let env = Env::default();
    let fx = setup(&env);
    let eu = region(&env, "eu");
    let reporter = Address::generate(&env);
    fx.client.add_reporter(&reporter, &eu);
    assert_eq!(fx.client.reporter(&reporter).unwrap().region, eu);
}

#[test]
fn add_reporter_rejects_a_duplicate() {
    let env = Env::default();
    let fx = setup(&env);
    let reporter = Address::generate(&env);
    fx.client.add_reporter(&reporter, &region(&env, "eu"));
    let result = fx.client.try_add_reporter(&reporter, &region(&env, "us"));
    assert_eq!(result, Err(Ok(Error::AlreadyRegistered)));
}

#[test]
fn add_reporter_rejects_an_address_already_registered_as_a_keeper() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    fx.client.add_keeper(&who);
    let result = fx.client.try_add_reporter(&who, &region(&env, "eu"));
    assert_eq!(result, Err(Ok(Error::RoleConflict)));
}

#[test]
fn remove_reporter_rejects_an_unregistered_address() {
    let env = Env::default();
    let fx = setup(&env);
    let result = fx.client.try_remove_reporter(&Address::generate(&env));
    assert_eq!(result, Err(Ok(Error::NotReporter)));
}

// -- stake / unstake_request / unstake --

#[test]
fn stake_transfers_real_usdc_and_increases_the_keeper_bond() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    fund(&fx, &keeper, params::KEEPER_BOND);

    fx.client.stake(&keeper, &params::KEEPER_BOND);

    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, params::KEEPER_BOND);
    assert_eq!(fx.usdc_client.balance(&keeper), 0);
    assert_eq!(fx.usdc_client.balance(&fx.contract_id), params::KEEPER_BOND);
}

#[test]
fn stake_transfers_real_usdc_and_increases_reporter_stake() {
    let env = Env::default();
    let fx = setup(&env);
    let reporter = Address::generate(&env);
    fx.client.add_reporter(&reporter, &region(&env, "eu"));
    fund(&fx, &reporter, params::REPORTER_STAKE);

    fx.client.stake(&reporter, &params::REPORTER_STAKE);

    assert_eq!(
        fx.client.reporter(&reporter).unwrap().stake,
        params::REPORTER_STAKE
    );
}

#[test]
fn stake_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = Address::generate(&env);
    fx.client.add_keeper(&keeper);
    let result = fx.client.try_stake(&keeper, &0);
    assert_eq!(result, Err(Ok(Error::StakeTooLow)));
}

#[test]
fn stake_rejects_an_address_that_is_neither_keeper_nor_reporter() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    fund(&fx, &who, 1_000);
    let result = fx.client.try_stake(&who, &1_000);
    assert_eq!(result, Err(Ok(Error::NotKeeper)));
}

#[test]
fn unstake_request_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let result = fx.client.try_unstake_request(&keeper, &0);
    assert_eq!(result, Err(Ok(Error::StakeTooLow)));
}

#[test]
fn unstake_request_rejects_a_second_request_while_one_is_pending() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.unstake_request(&keeper, &params::KEEPER_BOND);
    let result = fx.client.try_unstake_request(&keeper, &params::KEEPER_BOND);
    assert_eq!(result, Err(Ok(Error::UnstakePending)));
}

#[test]
fn unstake_request_rejects_an_amount_above_the_current_bond() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let result = fx
        .client
        .try_unstake_request(&keeper, &(params::KEEPER_BOND + 1));
    assert_eq!(result, Err(Ok(Error::StakeTooLow)));
}

#[test]
fn unstake_rejects_with_no_pending_request() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let result = fx.client.try_unstake(&keeper);
    assert_eq!(result, Err(Ok(Error::NoUnstakeRequested)));
}

#[test]
fn unstake_rejects_before_the_cooldown_elapses() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.unstake_request(&keeper, &params::KEEPER_BOND);
    env.ledger()
        .set_timestamp(params::UNSTAKE_COOLDOWN_SECS - 1);
    let result = fx.client.try_unstake(&keeper);
    assert_eq!(result, Err(Ok(Error::UnstakeCooldown)));
}

#[test]
fn unstake_succeeds_exactly_at_the_cooldown_boundary_and_pays_out() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.unstake_request(&keeper, &params::KEEPER_BOND);
    env.ledger().set_timestamp(params::UNSTAKE_COOLDOWN_SECS);

    let paid = fx.client.unstake(&keeper);

    assert_eq!(paid, params::KEEPER_BOND);
    assert_eq!(fx.usdc_client.balance(&keeper), params::KEEPER_BOND);
    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, 0);
}

#[test]
fn unstake_rejects_a_keeper_with_an_open_dispute() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &params::KEEPER_BOND, &Some(keeper.clone()));

    fx.client.unstake_request(&keeper, &params::KEEPER_BOND);
    env.ledger().set_timestamp(params::UNSTAKE_COOLDOWN_SECS);
    let result = fx.client.try_unstake(&keeper);
    assert_eq!(result, Err(Ok(Error::DisputesOpen)));
}

// -- withdraw_keeper_bond --

#[test]
fn withdraw_keeper_bond_rejects_a_keeper_never_removed() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let result = fx.client.try_withdraw_keeper_bond(&keeper);
    assert_eq!(result, Err(Ok(Error::NotKeeper)));
}

#[test]
fn withdraw_keeper_bond_rejects_before_the_exit_delay_elapses() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.remove_keeper(&keeper);
    env.ledger()
        .set_timestamp(params::KEEPER_EXIT_DELAY_SECS - 1);
    let result = fx.client.try_withdraw_keeper_bond(&keeper);
    assert_eq!(result, Err(Ok(Error::UnstakeCooldown)));
}

#[test]
fn withdraw_keeper_bond_succeeds_exactly_at_the_exit_delay_boundary() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.remove_keeper(&keeper);
    env.ledger().set_timestamp(params::KEEPER_EXIT_DELAY_SECS);

    let paid = fx.client.withdraw_keeper_bond(&keeper);
    assert_eq!(paid, params::KEEPER_BOND);
    assert_eq!(fx.usdc_client.balance(&keeper), params::KEEPER_BOND);
}

#[test]
fn withdraw_keeper_bond_fails_until_an_open_dispute_resolves_then_succeeds() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &params::KEEPER_BOND, &Some(keeper.clone()));

    fx.client.remove_keeper(&keeper);
    env.ledger().set_timestamp(params::KEEPER_EXIT_DELAY_SECS);
    let result = fx.client.try_withdraw_keeper_bond(&keeper);
    assert_eq!(result, Err(Ok(Error::DisputesOpen)));

    fx.client.release_bond(&key);
    let paid = fx.client.withdraw_keeper_bond(&keeper);
    assert_eq!(paid, params::KEEPER_BOND);
}

#[test]
fn withdraw_keeper_bond_with_no_open_disputes_succeeds_right_after_the_delay() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.remove_keeper(&keeper);
    env.ledger().set_timestamp(params::KEEPER_EXIT_DELAY_SECS);
    let paid = fx.client.withdraw_keeper_bond(&keeper);
    assert_eq!(paid, params::KEEPER_BOND);
}

#[test]
fn keeper_removed_then_a_new_dispute_arrives_still_blocks_withdrawal() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.remove_keeper(&keeper);
    env.ledger().set_timestamp(params::KEEPER_EXIT_DELAY_SECS);

    // A new dispute against a posting made before removal arrives
    // right at the boundary, before withdraw_keeper_bond is called.
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &params::KEEPER_BOND, &Some(keeper.clone()));

    let result = fx.client.try_withdraw_keeper_bond(&keeper);
    assert_eq!(result, Err(Ok(Error::DisputesOpen)));
}

#[test]
fn dispute_resolved_against_a_removed_keeper_before_withdrawal_still_slashes() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &params::KEEPER_BOND, &Some(keeper.clone()));

    fx.client.remove_keeper(&keeper);
    env.ledger().set_timestamp(params::KEEPER_EXIT_DELAY_SECS);

    // Disputer wins: the oracle calls slash and release_bond (not
    // forfeit_bond, matching resolve_signal_dispute's own disputer
    // wins branch) against the removed keeper.
    let slash_amount = 10_000_000_000;
    fx.client.slash(
        &keeper,
        &slash_amount,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    fx.client.release_bond(&key);

    assert_eq!(
        fx.client.keeper(&keeper).unwrap().bond,
        params::KEEPER_BOND - slash_amount
    );
    let paid = fx.client.withdraw_keeper_bond(&keeper);
    assert_eq!(paid, params::KEEPER_BOND - slash_amount);
}

// -- submit_probe --

#[test]
fn submit_probe_requires_reporter_auth_and_rejects_an_unregistered_address() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let who = Address::generate(&env);
    env.ledger().set_timestamp(epoch_close(0));
    let result = fx
        .client
        .try_submit_probe(&who, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::NotReporter)));
}

#[test]
fn submit_probe_rejects_a_reporter_below_reporter_stake() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = Address::generate(&env);
    fx.client.add_reporter(&reporter, &region(&env, "eu"));
    // Never staked: stake is 0, below REPORTER_STAKE.
    env.ledger().set_timestamp(epoch_close(0));
    let result = fx
        .client
        .try_submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::StakeTooLow)));
}

#[test]
fn submit_probe_rejects_a_suspended_reporter() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    // Drive REPORTER_MAX_FAULTS + 1 faults via settle_probes to force
    // a suspension (exercised fully in the settlement tests below);
    // here, assert the contract-level behavior directly against
    // record_fault's effect by running the minimal path: settle an
    // epoch this reporter disagreed with a 3+ majority on, repeated
    // past the threshold.
    let majority = [
        add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "af"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "ap"), params::REPORTER_STAKE),
    ];
    for epoch in 0..(params::REPORTER_MAX_FAULTS + 1) as u64 {
        env.ledger().set_timestamp(epoch_close(epoch));
        fx.client
            .submit_probe(&reporter, &probe(&env, &asset, epoch, EndpointStatus::Down));
        for m in &majority {
            fx.client
                .submit_probe(m, &probe(&env, &asset, epoch, EndpointStatus::Up));
        }
        env.ledger().set_timestamp(settlement_opens(epoch));
        fx.client.settle_probes(&asset, &epoch);
    }
    assert!(fx.client.reporter(&reporter).unwrap().suspended);

    env.ledger()
        .set_timestamp(epoch_close(params::REPORTER_MAX_FAULTS as u64 + 1));
    let result = fx.client.try_submit_probe(
        &reporter,
        &probe(
            &env,
            &asset,
            params::REPORTER_MAX_FAULTS as u64 + 1,
            EndpointStatus::Up,
        ),
    );
    assert_eq!(result, Err(Ok(Error::Suspended)));
}

#[test]
fn submit_probe_rejects_a_removed_reporter() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    fx.client.remove_reporter(&reporter);
    env.ledger().set_timestamp(epoch_close(0));
    let result = fx
        .client
        .try_submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::Suspended)));
}

#[test]
fn submit_probe_rejects_a_duplicate_for_the_same_reporter_asset_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    env.ledger().set_timestamp(epoch_close(0));
    fx.client
        .submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    let result = fx
        .client
        .try_submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Down));
    assert_eq!(result, Err(Ok(Error::DuplicateProbe)));
}

#[test]
fn submit_probe_accepts_the_current_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    // "Current epoch" per submit_probe's own now/EPOCH_SECS: anywhere
    // inside epoch 0's own span, not just at its close.
    env.ledger().set_timestamp(params::EPOCH_SECS / 2);
    fx.client
        .submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(fx.client.probes(&asset, &0).len(), 1);
}

#[test]
fn submit_probe_accepts_the_just_closed_epoch_within_the_grace_period() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    env.ledger()
        .set_timestamp(epoch_close(0) + params::PROBE_GRACE_SECS);
    fx.client
        .submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(fx.client.probes(&asset, &0).len(), 1);
}

#[test]
fn submit_probe_rejects_the_just_closed_epoch_after_the_grace_period() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    env.ledger()
        .set_timestamp(epoch_close(0) + params::PROBE_GRACE_SECS + 1);
    let result = fx
        .client
        .try_submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::ProbeWindowClosed)));
}

#[test]
fn submit_probe_rejects_an_epoch_further_back_than_the_just_closed_one() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    env.ledger().set_timestamp(epoch_close(2));
    let result = fx
        .client
        .try_submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::ProbeWindowClosed)));
}

#[test]
fn submit_probe_ignores_the_report_s_own_region_field() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let eu = region(&env, "eu");
    let reporter = add_and_fund_reporter(&env, &fx, &eu, params::REPORTER_STAKE);
    env.ledger().set_timestamp(epoch_close(0));
    // probe()'s helper already sets report.region to "ignored"; prove
    // the aggregate still treats this reporter as "eu" by requiring
    // 2 distinct regions with only this one plus one other "eu"
    // reporter: aggregate must read Unknown (only 1 distinct region),
    // not 2, if the report's own field were (wrongly) honored.
    let other_eu = add_and_fund_reporter(&env, &fx, &eu, params::REPORTER_STAKE);
    let third = add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE);
    fx.client
        .submit_probe(&reporter, &probe(&env, &asset, 0, EndpointStatus::Up));
    fx.client
        .submit_probe(&other_eu, &probe(&env, &asset, 0, EndpointStatus::Up));
    fx.client
        .submit_probe(&third, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(fx.client.aggregate(&asset, &0), EndpointStatus::Up);
}

#[test]
fn submit_probe_rejects_once_the_submitter_cap_is_reached() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    env.ledger().set_timestamp(epoch_close(0));
    for i in 0..params::MAX_SUBMITTERS_PER_EPOCH {
        let r = add_and_fund_reporter(
            &env,
            &fx,
            &region(&env, if i % 2 == 0 { "eu" } else { "us" }),
            params::REPORTER_STAKE,
        );
        fx.client
            .submit_probe(&r, &probe(&env, &asset, 0, EndpointStatus::Up));
    }
    let one_more = add_and_fund_reporter(&env, &fx, &region(&env, "af"), params::REPORTER_STAKE);
    let result = fx
        .client
        .try_submit_probe(&one_more, &probe(&env, &asset, 0, EndpointStatus::Up));
    assert_eq!(result, Err(Ok(Error::ProbeWindowClosed)));
}

// -- aggregate (S4: never writes) --

#[test]
fn aggregate_never_writes_to_storage() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let r1 = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let r2 = add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE);
    let r3 = add_and_fund_reporter(&env, &fx, &region(&env, "af"), params::REPORTER_STAKE);
    env.ledger().set_timestamp(epoch_close(0));
    fx.client
        .submit_probe(&r1, &probe(&env, &asset, 0, EndpointStatus::Up));
    fx.client
        .submit_probe(&r2, &probe(&env, &asset, 0, EndpointStatus::Up));
    fx.client
        .submit_probe(&r3, &probe(&env, &asset, 0, EndpointStatus::Up));

    // Calling aggregate many times must be fully idempotent: no write
    // it performed could change a later call's own result, and (S4)
    // settle_probes must still see the same probes afterward.
    for _ in 0..5 {
        assert_eq!(fx.client.aggregate(&asset, &0), EndpointStatus::Up);
    }
    assert_eq!(fx.client.probes(&asset, &0).len(), 3);
}

// -- settle_probes --

/// 3 reporters, 2 distinct regions, all submit `status` for `epoch`.
/// Returns the reporters in submission order.
fn submit_unanimous(
    env: &Env,
    fx: &Fixture,
    asset: &Address,
    epoch: u64,
    status: EndpointStatus,
) -> [Address; 3] {
    env.ledger().set_timestamp(epoch_close(epoch));
    let r1 = add_and_fund_reporter(env, fx, &region(env, "eu"), params::REPORTER_STAKE);
    let r2 = add_and_fund_reporter(env, fx, &region(env, "us"), params::REPORTER_STAKE);
    let r3 = add_and_fund_reporter(env, fx, &region(env, "af"), params::REPORTER_STAKE);
    for r in [&r1, &r2, &r3] {
        fx.client.submit_probe(r, &probe(env, asset, epoch, status));
    }
    [r1, r2, r3]
}

#[test]
fn settle_probes_rejects_before_the_window_opens() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0) - 1);
    let result = fx.client.try_settle_probes(&asset, &0);
    assert_eq!(result, Err(Ok(Error::SettlementNotOpen)));
}

#[test]
fn settle_probes_succeeds_at_the_first_second_the_window_opens() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);
}

#[test]
fn settle_probes_succeeds_at_the_last_second_the_window_closes() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_closes(0));
    fx.client.settle_probes(&asset, &0);
}

#[test]
fn settle_probes_rejects_one_second_after_the_window_closes() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_closes(0) + 1);
    let result = fx.client.try_settle_probes(&asset, &0);
    assert_eq!(result, Err(Ok(Error::SettlementWindowExpired)));
}

#[test]
fn settle_probes_rejects_a_second_call_for_the_same_epoch() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);
    let result = fx.client.try_settle_probes(&asset, &0);
    assert_eq!(result, Err(Ok(Error::AlreadySettled)));
}

#[test]
fn settle_probes_funds_rewards_split_equally_among_matching_reporters() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::ReporterRewards,
        params::REPORTER_REWARD_PER_EPOCH,
    );

    let [r1, r2, r3] = submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);

    let share = params::REPORTER_REWARD_PER_EPOCH / 3;
    for r in [&r1, &r2, &r3] {
        assert_eq!(fx.treasury_client.accrued(r), share);
    }
}

#[test]
fn settle_probes_reward_accrual_never_exceeds_the_funded_balance() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    // No fund_treasury_bucket call at all: ReporterRewards is empty.
    let [r1, r2, r3] = submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);

    for r in [&r1, &r2, &r3] {
        assert_eq!(fx.treasury_client.accrued(r), 0);
    }
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::ReporterRewards),
        0
    );
}

#[test]
fn settle_probes_faults_and_eventually_slashes_a_reporter_against_a_3_plus_majority() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);

    env.ledger().set_timestamp(epoch_close(0));
    let dissenter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let majority = [
        add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "af"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "ap"), params::REPORTER_STAKE),
    ];

    for epoch in 0..(params::REPORTER_MAX_FAULTS + 1) as u64 {
        env.ledger().set_timestamp(epoch_close(epoch));
        fx.client.submit_probe(
            &dissenter,
            &probe(&env, &asset, epoch, EndpointStatus::Down),
        );
        for m in &majority {
            fx.client
                .submit_probe(m, &probe(&env, &asset, epoch, EndpointStatus::Up));
        }
        env.ledger().set_timestamp(settlement_opens(epoch));
        fx.client.settle_probes(&asset, &epoch);
    }

    let info = fx.client.reporter(&dissenter).unwrap();
    assert!(info.suspended);
    assert_eq!(
        info.stake,
        params::REPORTER_STAKE - params::REPORTER_STAKE * params::REPORTER_SLASH_BPS / 10_000
    );
}

#[test]
fn settle_probes_does_not_fault_a_minority_dissent_below_the_fault_majority_threshold() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    env.ledger().set_timestamp(epoch_close(0));
    // Only 2 reporters total: MIN_REPORTERS (3) is not met, so
    // aggregate reads Unknown and majority_size is 0 — no fault
    // branch is even reachable regardless of agreement.
    let r1 = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let r2 = add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE);
    fx.client
        .submit_probe(&r1, &probe(&env, &asset, 0, EndpointStatus::Up));
    fx.client
        .submit_probe(&r2, &probe(&env, &asset, 0, EndpointStatus::Down));
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);

    assert!(!fx.client.reporter(&r1).unwrap().suspended);
    assert!(!fx.client.reporter(&r2).unwrap().suspended);
    assert_eq!(
        fx.client.reporter(&r1).unwrap().stake,
        params::REPORTER_STAKE
    );
    assert_eq!(
        fx.client.reporter(&r2).unwrap().stake,
        params::REPORTER_STAKE
    );
}

#[test]
fn an_epoch_nobody_settles_before_probe_ttl_expiry_leaves_state_unchanged() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);

    // Jump well past the settlement window's close, and past the
    // probe TTL margin too: nobody ever called settle_probes.
    env.ledger()
        .set_timestamp(settlement_closes(0) + params::PROBE_TTL_MARGIN_SECS + 1);

    // Safe outcome (task item 5): no funds lost, no stuck state.
    // settle_probes itself now permanently rejects this epoch
    // (window expired), which is the intended "simply never settles"
    // behavior, not a panic or an inconsistent write.
    let result = fx.client.try_settle_probes(&asset, &0);
    assert_eq!(result, Err(Ok(Error::SettlementWindowExpired)));
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::ReporterRewards),
        0
    );
}

#[test]
fn submit_probes_remove_one_reporter_then_aggregate_and_settle_same_result_as_without_removal() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let [r1, r2, r3] = submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);

    // Remove r2 AFTER it already submitted; its already-submitted
    // probe must still count (lead decision: submitter index, never
    // the live reporter set).
    fx.client.remove_reporter(&r2);

    assert_eq!(fx.client.aggregate(&asset, &0), EndpointStatus::Up);
    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);

    for r in [&r1, &r2, &r3] {
        assert!(!fx.client.reporter(r).unwrap().suspended);
    }
}

#[test]
fn removed_reporter_who_disagreed_with_a_3_plus_majority_is_still_faulted_and_slashed_at_settlement(
) {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);

    env.ledger().set_timestamp(epoch_close(0));
    let dissenter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let majority = [
        add_and_fund_reporter(&env, &fx, &region(&env, "us"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "af"), params::REPORTER_STAKE),
        add_and_fund_reporter(&env, &fx, &region(&env, "ap"), params::REPORTER_STAKE),
    ];
    for epoch in 0..(params::REPORTER_MAX_FAULTS + 1) as u64 {
        env.ledger().set_timestamp(epoch_close(epoch));
        fx.client.submit_probe(
            &dissenter,
            &probe(&env, &asset, epoch, EndpointStatus::Down),
        );
        for m in &majority {
            fx.client
                .submit_probe(m, &probe(&env, &asset, epoch, EndpointStatus::Up));
        }
    }
    // Remove the dissenter AFTER it has already submitted every
    // probe above, BEFORE any of them settle: stake must still be
    // locked and slashable (removal is not an escape hatch).
    fx.client.remove_reporter(&dissenter);
    assert_eq!(
        fx.client.try_unstake(&dissenter),
        Err(Ok(Error::NoUnstakeRequested))
    );

    for epoch in 0..(params::REPORTER_MAX_FAULTS + 1) as u64 {
        env.ledger().set_timestamp(settlement_opens(epoch));
        fx.client.settle_probes(&asset, &epoch);
    }

    let info = fx.client.reporter(&dissenter).unwrap();
    assert!(info.suspended);
    assert!(info.stake < params::REPORTER_STAKE);
}

#[test]
fn removed_reporter_who_matched_the_majority_can_still_claim_rewards() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::ReporterRewards,
        params::REPORTER_REWARD_PER_EPOCH,
    );

    let [r1, r2, r3] = submit_unanimous(&env, &fx, &asset, 0, EndpointStatus::Up);
    fx.client.remove_reporter(&r1);

    env.ledger().set_timestamp(settlement_opens(0));
    fx.client.settle_probes(&asset, &0);

    let accrued_before_claim = fx.treasury_client.accrued(&r1);
    assert!(accrued_before_claim > 0);
    let claimed = fx.treasury_client.claim_reward(&r1);
    assert_eq!(claimed, accrued_before_claim);
    assert_eq!(fx.treasury_client.accrued(&r1), 0);
    assert_eq!(fx.usdc_client.balance(&r1), claimed);
    let _ = (r2, r3);
}

// -- S1 (feat/treasury): Staking's balance equals every tracked
// liability, exactly, not merely >= -- across a mixed scenario
// touching every kind of liability at once (task brief, Part 2 item
// 4) --

#[test]
fn staking_balance_equals_tracked_liabilities_exactly() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, 10_000_000_000);

    // Keeper bond.
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    // Reporter stake, including a slice voluntarily put into
    // unstake cooldown (still locked and counted, Section 7.7/
    // ADR-011: stake in cooldown stays slashable and tracked until
    // actually withdrawn).
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    fx.client
        .unstake_request(&reporter, &params::REPORTER_STAKE);
    // A locked signal dispute bond, naming the keeper as subject.
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper.clone()));
    // A claimable balance (release the bond back to the disputer).
    fx.client.release_bond(&key);

    let tracked_liabilities = fx.client.keeper(&keeper).unwrap().bond
        + fx.client.reporter(&reporter).unwrap().stake
        + fx.client.bond(&key).map(|(_, amt)| amt).unwrap_or(0)
        + fx.client.claimable(&disputer);
    let balance = fx.usdc_client.balance(&fx.contract_id);
    assert_eq!(
        balance, tracked_liabilities,
        "S1 violated: Staking balance {} != tracked liabilities {}",
        balance, tracked_liabilities
    );

    // Now slash the keeper (a protocol fund leaves for Treasury) and
    // claim the disputer's refund (a participant fund leaves to its
    // owner); the equality must still hold afterward.
    fx.client.slash(
        &keeper,
        &1_000_000_000,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    fx.client.claim(&disputer);

    let tracked_liabilities = fx.client.keeper(&keeper).unwrap().bond
        + fx.client.reporter(&reporter).unwrap().stake
        + fx.client.claimable(&disputer);
    let balance = fx.usdc_client.balance(&fx.contract_id);
    assert_eq!(
        balance, tracked_liabilities,
        "S1 violated after slash+claim: Staking balance {} != tracked liabilities {}",
        balance, tracked_liabilities
    );
}

// -- lock_bond / release_bond / forfeit_bond --

#[test]
fn lock_bond_requires_oracle_auth_for_a_signal_dispute_key() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &disputer, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper));
    assert_eq!(fx.client.bond(&key), Some((disputer, 1_000_000_000)));
}

#[test]
fn lock_bond_rejects_a_non_positive_amount_and_does_not_touch_open_dispute_count() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let key = BondKey::SignalDispute(asset, 0);

    let result = fx
        .client
        .try_lock_bond(&key, &disputer, &0, &Some(keeper.clone()));
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    let negative_result = fx
        .client
        .try_lock_bond(&key, &disputer, &-1, &Some(keeper.clone()));
    assert_eq!(negative_result, Err(Ok(Error::InvalidAmount)));

    // The approval fix's own concern: a rejected zero/negative amount
    // must never have incremented the subject keeper's
    // open_dispute_count, which release_bond/forfeit_bond could then
    // never actually settle back down (no bond was ever recorded for
    // them to act on).
    assert_eq!(fx.client.keeper(&keeper).unwrap().open_dispute_count, 0);
    assert_eq!(fx.client.bond(&key), None);
}

#[test]
fn lock_bond_rejects_a_duplicate_key() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &disputer, 2_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper.clone()));
    let result = fx
        .client
        .try_lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper));
    assert_eq!(result, Err(Ok(Error::BondExists)));
}

#[test]
fn lock_bond_rejects_a_signal_dispute_key_with_no_subject() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    fund(&fx, &disputer, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    let result = fx
        .client
        .try_lock_bond(&key, &disputer, &1_000_000_000, &None);
    assert_eq!(result, Err(Ok(Error::InvalidBondSubject)));
}

#[test]
fn lock_bond_rejects_an_event_kind_key_with_a_subject() {
    let env = Env::default();
    let fx = setup(&env);
    let proposer = Address::generate(&env);
    fund(&fx, &proposer, 1_000_000_000);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let key = BondKey::EventProposal(0);
    let result = fx
        .client
        .try_lock_bond(&key, &proposer, &1_000_000_000, &Some(keeper));
    assert_eq!(result, Err(Ok(Error::InvalidBondSubject)));
}

#[test]
fn lock_bond_accepts_an_event_proposal_key_with_no_subject_under_registry_auth() {
    let env = Env::default();
    let fx = setup(&env);
    let proposer = Address::generate(&env);
    fund(&fx, &proposer, 1_000_000_000);
    let key = BondKey::EventProposal(0);
    fx.client.lock_bond(&key, &proposer, &1_000_000_000, &None);
    assert_eq!(fx.client.bond(&key), Some((proposer, 1_000_000_000)));
}

#[test]
fn release_bond_rejects_an_unknown_key() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let key = BondKey::SignalDispute(asset, 0);
    let result = fx.client.try_release_bond(&key);
    assert_eq!(result, Err(Ok(Error::UnknownBond)));
}

#[test]
fn release_bond_credits_the_owner_s_claimable_balance_in_full() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &disputer, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper));

    fx.client.release_bond(&key);
    assert_eq!(fx.client.claimable(&disputer), 1_000_000_000);
    assert_eq!(fx.client.bond(&key), None);
}

#[test]
fn forfeit_bond_rejects_an_unknown_key() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let key = BondKey::SignalDispute(asset, 0);
    let winner = Address::generate(&env);
    let result = fx.client.try_forfeit_bond(&key, &Some(winner));
    assert_eq!(result, Err(Ok(Error::UnknownBond)));
}

#[test]
fn forfeit_bond_splits_50_50_between_winner_and_treasury() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let loser = Address::generate(&env);
    let winner = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &loser, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &loser, &1_000_000_000, &Some(keeper));

    fx.client.forfeit_bond(&key, &Some(winner.clone()));
    assert_eq!(fx.client.claimable(&winner), 500_000_000);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::Slashed),
        500_000_000
    );
}

#[test]
fn forfeit_bond_with_no_winner_sends_the_full_amount_to_the_treasury() {
    let env = Env::default();
    let fx = setup(&env);
    let proposer = Address::generate(&env);
    fund(&fx, &proposer, 1_000_000_000);
    let key = BondKey::EventProposal(0);
    fx.client.lock_bond(&key, &proposer, &1_000_000_000, &None);

    fx.client.forfeit_bond(&key, &None);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::Slashed),
        1_000_000_000
    );
}

#[test]
fn release_bond_and_forfeit_bond_on_the_same_key_decrements_the_open_dispute_counter_once() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &disputer, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper.clone()));
    assert_eq!(fx.client.keeper(&keeper).unwrap().open_dispute_count, 1);

    fx.client.release_bond(&key);
    assert_eq!(fx.client.keeper(&keeper).unwrap().open_dispute_count, 0);

    // The same key was already cleared by release_bond: a second
    // settlement call (forfeit_bond this time) now finds UnknownBond,
    // which is itself the proof the counter cannot be decremented a
    // second time for this same bond (there is nothing left to act
    // on). S2: a bond moves at most once.
    let winner = Address::generate(&env);
    let result = fx.client.try_forfeit_bond(&key, &Some(winner));
    assert_eq!(result, Err(Ok(Error::UnknownBond)));
    assert_eq!(fx.client.keeper(&keeper).unwrap().open_dispute_count, 0);
}

// -- slash --

#[test]
fn slash_rejects_an_address_that_is_neither_keeper_nor_reporter() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    let result = fx
        .client
        .try_slash(&who, &1_000, &None, &BytesN::from_array(&env, &[0u8; 32]));
    assert_eq!(result, Err(Ok(Error::NotKeeper)));
}

#[test]
fn slash_a_keeper_reduces_the_bond_and_splits_50_50() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);
    let amount = 10_000_000_000;

    fx.client.slash(
        &keeper,
        &amount,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    assert_eq!(
        fx.client.keeper(&keeper).unwrap().bond,
        params::KEEPER_BOND - amount
    );
    assert_eq!(fx.client.claimable(&disputer), amount / 2);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::Slashed),
        amount - amount / 2
    );
}

// -- S5: slash must never pay out more than it actually deducts --

#[test]
fn slash_a_keeper_below_the_amount_pays_out_only_what_was_actually_held() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);
    // Request more than the keeper's whole bond.
    let requested = params::KEEPER_BOND + 5_000_000_000;

    fx.client.slash(
        &keeper,
        &requested,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    // The keeper had exactly KEEPER_BOND; that is all that can ever be
    // deducted or paid out, regardless of what was requested.
    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, 0);
    let actual = params::KEEPER_BOND;
    assert_eq!(fx.client.claimable(&disputer), actual / 2);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::Slashed),
        actual - actual / 2
    );

    // S1: the contract's own USDC balance must still exactly cover
    // every claimable balance it still tracks (the treasury half has
    // genuinely left, via a real Treasury.deposit call). If slash had
    // paid out the full `requested` amount instead of `actual`, this
    // would fail: claimable(disputer) would exceed what the keeper
    // ever deposited.
    let balance = fx.usdc_client.balance(&fx.contract_id);
    let liabilities = fx.client.claimable(&disputer);
    assert_eq!(
        balance, liabilities,
        "S1 violated: balance {} != liabilities {}",
        balance, liabilities
    );
}

#[test]
fn slash_a_reporter_below_the_amount_pays_out_only_what_was_actually_held() {
    let env = Env::default();
    let fx = setup(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let requested = params::REPORTER_STAKE + 1_000_000_000;

    fx.client.slash(
        &reporter,
        &requested,
        &None,
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    assert_eq!(fx.client.reporter(&reporter).unwrap().stake, 0);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::Slashed),
        params::REPORTER_STAKE
    );

    // S1: nothing is left claimable inside Staking for this slash (no
    // winner was named), so Staking's own balance should be back to 0.
    assert_eq!(fx.usdc_client.balance(&fx.contract_id), 0);
}

#[test]
fn two_slashes_in_a_row_exceeding_the_bond_in_total_never_overpay() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);

    // First slash takes most of the bond.
    let first = params::KEEPER_BOND - 1_000_000_000;
    fx.client.slash(
        &keeper,
        &first,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, 1_000_000_000);

    // Second slash requests more than what remains.
    let second_requested = 5_000_000_000;
    fx.client.slash(
        &keeper,
        &second_requested,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    assert_eq!(fx.client.keeper(&keeper).unwrap().bond, 0);

    let total_actual = params::KEEPER_BOND;
    let balance = fx.usdc_client.balance(&fx.contract_id);
    let liabilities = fx.client.claimable(&disputer);
    let to_treasury = fx.treasury_client.balance(&TreasuryBucket::Slashed);
    assert_eq!(liabilities + to_treasury, total_actual);
    assert_eq!(
        balance, liabilities,
        "S1 violated: balance {} != liabilities {}",
        balance, liabilities
    );
}

#[test]
fn slash_a_reporter_with_no_winner_sends_the_full_amount_to_the_treasury() {
    let env = Env::default();
    let fx = setup(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let amount = 1_000_000_000;

    fx.client.slash(
        &reporter,
        &amount,
        &None,
        &BytesN::from_array(&env, &[0u8; 32]),
    );

    assert_eq!(
        fx.client.reporter(&reporter).unwrap().stake,
        params::REPORTER_STAKE - amount
    );
    assert_eq!(fx.treasury_client.balance(&TreasuryBucket::Slashed), amount);
}

#[test]
fn slash_suspends_a_keeper_after_keeper_max_faults_in_the_fault_window() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);
    for i in 0..(params::KEEPER_MAX_FAULTS + 1) {
        env.ledger().set_timestamp(i as u64 * 10);
        fx.client.slash(
            &keeper,
            &1_000_000,
            &Some(disputer.clone()),
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }
    assert!(fx.client.keeper(&keeper).unwrap().suspended);
}

#[test]
fn slash_does_not_count_a_fault_outside_the_fault_window() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);

    env.ledger().set_timestamp(0);
    for _ in 0..params::KEEPER_MAX_FAULTS {
        fx.client.slash(
            &keeper,
            &1_000_000,
            &Some(disputer.clone()),
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }
    assert!(!fx.client.keeper(&keeper).unwrap().suspended);

    // One more fault, but well outside FAULT_WINDOW_SECS from every
    // prior one: the earlier faults are pruned, so this single fresh
    // fault alone does not exceed KEEPER_MAX_FAULTS.
    env.ledger().set_timestamp(params::FAULT_WINDOW_SECS + 1);
    fx.client.slash(
        &keeper,
        &1_000_000,
        &Some(disputer.clone()),
        &BytesN::from_array(&env, &[0u8; 32]),
    );
    assert!(!fx.client.keeper(&keeper).unwrap().suspended);
}

// -- reward_keeper (issue #11 fix, feat/treasury) / claim --

#[test]
fn reward_keeper_requires_oracle_auth_and_accrues_keeper_reward_times_epochs() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::KeeperRewards,
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH * 3,
    );

    let accrued = fx.client.reward_keeper(&keeper, &3);
    assert_eq!(accrued, params::KEEPER_REWARD_PER_ACCEPTED_EPOCH * 3);
    assert_eq!(
        fx.treasury_client.accrued(&keeper),
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH * 3
    );
}

#[test]
fn reward_keeper_rejects_an_unregistered_keeper() {
    let env = Env::default();
    let fx = setup(&env);
    let result = fx.client.try_reward_keeper(&Address::generate(&env), &1);
    assert_eq!(result, Err(Ok(Error::NotKeeper)));
}

#[test]
fn reward_keeper_is_a_no_op_for_a_suspended_keeper() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let disputer = Address::generate(&env);
    for i in 0..(params::KEEPER_MAX_FAULTS + 1) {
        env.ledger().set_timestamp(i as u64 * 10);
        fx.client.slash(
            &keeper,
            &1_000_000,
            &Some(disputer.clone()),
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }
    assert!(fx.client.keeper(&keeper).unwrap().suspended);

    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::KeeperRewards,
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH,
    );
    let accrued = fx.client.reward_keeper(&keeper, &1);
    assert_eq!(accrued, 0);
    assert_eq!(fx.treasury_client.accrued(&keeper), 0);
}

#[test]
fn reward_keeper_is_a_no_op_for_a_removed_keeper() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fx.client.remove_keeper(&keeper);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::KeeperRewards,
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH,
    );
    let accrued = fx.client.reward_keeper(&keeper, &1);
    assert_eq!(accrued, 0);
}

#[test]
fn reward_keeper_unfunded_bucket_accrues_only_what_is_there() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    // Fund less than keeper_reward * epochs would need.
    let partial = params::KEEPER_REWARD_PER_ACCEPTED_EPOCH;
    fund_treasury_bucket(&env, &fx, TreasuryBucket::KeeperRewards, partial);

    let accrued = fx.client.reward_keeper(&keeper, &3);
    assert_eq!(accrued, partial);
    assert_eq!(
        fx.treasury_client.balance(&TreasuryBucket::KeeperRewards),
        0
    );
}

/// Section 5.9 S6 (v1.5): keeper pay for one hour of real coverage is
/// identical whether that hour was posted as 12 sub-epochs at the
/// 300s default, as 1 sub-epoch at the 3,600s (hourly) interval, or
/// through the existing hourly `reward_keeper` path directly. Proves
/// the brief's own "keeper pay per hour being the same at 300 and at
/// 3,600" requirement against the real reward formula, not just the
/// mock's own accounting.
#[test]
fn reward_keeper_sub_epochs_pays_the_same_per_hour_at_300s_and_at_3_600s() {
    let env = Env::default();
    let fx = setup(&env);
    let keeper_hourly = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let keeper_300 = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    let keeper_3600 = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund_treasury_bucket(
        &env,
        &fx,
        TreasuryBucket::KeeperRewards,
        params::KEEPER_REWARD_PER_ACCEPTED_EPOCH * 3,
    );

    let hourly = fx.client.reward_keeper(&keeper_hourly, &1);
    // 12 sub-epochs of 300s each make up exactly one hour.
    let at_300s = fx.client.reward_keeper_sub_epochs(&keeper_300, &12, &300);
    // 1 sub-epoch of 3,600s is exactly one hour on its own.
    let at_3600s = fx.client.reward_keeper_sub_epochs(&keeper_3600, &1, &3_600);

    assert_eq!(
        hourly, at_300s,
        "one hour of sub-epochs at the 300s default must pay the same as one hourly reward"
    );
    assert_eq!(
        hourly, at_3600s,
        "one hour of sub-epochs at the 3,600s interval must pay the same as one hourly reward"
    );
    assert_eq!(hourly, params::KEEPER_REWARD_PER_ACCEPTED_EPOCH);
}

#[test]
fn claim_reward_on_treasury_rejects_when_nothing_is_accrued() {
    let env = Env::default();
    let fx = setup(&env);
    let reporter = add_and_fund_reporter(&env, &fx, &region(&env, "eu"), params::REPORTER_STAKE);
    let result = fx.treasury_client.try_claim_reward(&reporter);
    assert_eq!(result, Err(Ok(treasury::Error::NothingToClaim)));
}

#[test]
fn claim_rejects_when_nothing_is_claimable() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    let result = fx.client.try_claim(&who);
    assert_eq!(result, Err(Ok(Error::NothingToClaim)));
}

#[test]
fn claim_pays_out_a_bond_settlement_refund() {
    let env = Env::default();
    let fx = setup(&env);
    let asset = Address::generate(&env);
    let disputer = Address::generate(&env);
    let keeper = add_and_fund_keeper(&env, &fx, params::KEEPER_BOND);
    fund(&fx, &disputer, 1_000_000_000);
    let key = BondKey::SignalDispute(asset, 0);
    fx.client
        .lock_bond(&key, &disputer, &1_000_000_000, &Some(keeper));
    fx.client.release_bond(&key);

    let claimed = fx.client.claim(&disputer);
    assert_eq!(claimed, 1_000_000_000);
    assert_eq!(fx.usdc_client.balance(&disputer), 1_000_000_000);
    assert_eq!(fx.client.claimable(&disputer), 0);
}

// -- documented-unreachable error codes (Unauthorized, Paused,
// EpochNotClosed, UnknownCaller): see error.rs's own doc comments for
// why each cannot actually be returned by this build. --

#[test]
#[should_panic]
fn missing_oracle_auth_traps_natively_rather_than_returning_unknown_caller() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks.
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let contract_id = env.register(
        Staking,
        (
            governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury.clone(),
            sac.address(),
        ),
    );
    let client = StakingClient::new(&env, &contract_id);

    let asset = Address::generate(&env);
    let who = Address::generate(&env);
    // slash requires oracle.require_auth(); nothing here signed as
    // oracle, so this traps rather than returning UnknownCaller.
    client.slash(&who, &1_000, &None, &BytesN::from_array(&env, &[0u8; 32]));
    let _ = asset;
}

// -- Issue #12: upgrade / migrate / version --

#[test]
fn version_reads_contract_version_constant() {
    let env = Env::default();
    let fixture = setup(&env);
    assert_eq!(fixture.client.version(), crate::CONTRACT_VERSION);
}

/// Non-governor cannot upgrade: same native auth trap as
/// `missing_oracle_auth_traps_natively_rather_than_returning_unknown_caller`
/// above, demonstrated directly with no auth mocking at all.
#[test]
#[should_panic]
fn upgrade_without_governor_auth_traps() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks.
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let contract_id = env.register(
        Staking,
        (
            governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury.clone(),
            sac.address(),
        ),
    );
    let client = StakingClient::new(&env, &contract_id);

    let new_wasm_hash = env.upload(Staking);
    // No mock_all_auths(): the governor never actually signs this call.
    client.upgrade(&new_wasm_hash);
}

/// Upgrading to new code (a fresh wasm hash, uploaded via `env.upload`
/// per soroban-sdk's own documented pattern for exercising
/// `update_current_contract` in a unit test without a compiled .wasm
/// fixture) keeps all existing storage: a keeper added before the
/// upgrade still reads back correctly through the same client and
/// address after it.
#[test]
fn upgrade_keeps_existing_storage() {
    let env = Env::default();
    let fixture = setup(&env);
    let keeper = Address::generate(&env);
    fixture.client.add_keeper(&keeper);
    let keeper_info_before = fixture.client.keeper(&keeper);

    let new_wasm_hash = env.upload(Staking);
    fixture.client.upgrade(&new_wasm_hash);

    assert_eq!(
        fixture.client.keeper(&keeper),
        keeper_info_before,
        "keeper(keeper) must read the same after an upgrade with no storage-shape change"
    );
    assert_eq!(
        fixture.client.version(),
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
    let fixture = setup(&env);
    fixture.client.migrate();
    let result = fixture.client.try_migrate();
    assert_eq!(result, Err(Ok(Error::AlreadyMigrated)));
}

/// `set_governor` works exactly once: the current governor hands off
/// to a new address, and a governor-gated call afterward still
/// succeeds (through `mock_all_auths_allowing_non_root_auth`, which
/// approves every `require_auth()` regardless of address; the real
/// proof the new address is the one actually recorded is
/// `set_governor_old_governor_rejected_new_governor_accepted` below).
#[test]
fn set_governor_succeeds_once() {
    let env = Env::default();
    let fixture = setup(&env);
    let new_governor = Address::generate(&env);

    fixture.client.set_governor(&new_governor);

    let new_wasm_hash = env.upload(Staking);
    fixture.client.upgrade(&new_wasm_hash);
}

/// `set_governor` cannot run twice: `GovernorAlreadySet` on the second
/// call, confirmed via `try_set_governor` since this is a real
/// returned error, not a native auth trap. Same one-time-lock shape
/// as `migrate_cannot_run_twice`.
#[test]
fn set_governor_fails_the_second_call() {
    let env = Env::default();
    let fixture = setup(&env);

    fixture.client.set_governor(&Address::generate(&env));
    let result = fixture.client.try_set_governor(&Address::generate(&env));

    assert_eq!(result, Err(Ok(Error::GovernorAlreadySet)));
}

/// Non-governor cannot call `set_governor`: same native auth trap
/// pattern as `upgrade_without_governor_auth_traps`, demonstrated with
/// no auth mocking at all.
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
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let contract_id = env.register(
        Staking,
        (
            governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury.clone(),
            sac.address(),
        ),
    );
    let client = StakingClient::new(&env, &contract_id);

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
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let contract_id = env.register(
        Staking,
        (
            old_governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury.clone(),
            sac.address(),
        ),
    );
    let client = StakingClient::new(&env, &contract_id);

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

    let new_wasm_hash = env.upload(Staking);

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
}

/// Non-governor cannot migrate: same native auth trap pattern as
/// `upgrade_without_governor_auth_traps`.
#[test]
#[should_panic]
fn migrate_without_governor_auth_traps() {
    let env = Env::default();
    let governor = Address::generate(&env);
    let oracle = Address::generate(&env);
    let registry = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let contract_id = env.register(
        Staking,
        (
            governor.clone(),
            oracle.clone(),
            registry.clone(),
            treasury.clone(),
            sac.address(),
        ),
    );
    let client = StakingClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.migrate();
}
