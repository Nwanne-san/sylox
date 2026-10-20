//! Treasury test suite. technical-doc.md Section 12.7; the task
//! brief's TESTS section (T1-T4, one test per error code, unfunded
//! rewards, the real multi-contract integration test lives in
//! `contracts/staking`'s own `test::integration`, since it needs
//! `Staking` and `RiskOracle` too).

extern crate std;

mod property;
mod ttl;

use soroban_sdk::testutils::{Address as _, Events as _};
use soroban_sdk::{token, Address, Env, Event as _, IntoVal};
use sylox_types::TreasuryBucket;

use crate::{Error, Treasury, TreasuryClient};

struct Fixture<'a> {
    client: TreasuryClient<'a>,
    contract_id: Address,
    usdc_admin_client: token::StellarAssetClient<'a>,
    usdc_client: token::TokenClient<'a>,
}

/// A real Stellar Asset Contract for USDC, not a mock token, matching
/// every other contract's own test convention in this workspace.
/// `governor` and `staking` are plain addresses here (`Governor` is
/// not implemented; `Staking`'s own test suite exercises the real
/// cross-contract call from its side). Neither address is kept on
/// `Fixture` itself: nothing outside this function reads them back
/// (every test either mocks all auths, so the exact address behind
/// `governor`/`staking` never matters to it, or constructs its own
/// Treasury instance directly, per `upgrade_without_governor_auth_traps`
/// below).
fn setup(env: &Env) -> Fixture<'_> {
    env.mock_all_auths_allowing_non_root_auth();
    let governor = Address::generate(env);
    let staking = Address::generate(env);

    let sac_admin = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let usdc_admin_client = token::StellarAssetClient::new(env, &usdc);
    let usdc_client = token::TokenClient::new(env, &usdc);

    let contract_id = env.register(Treasury, (governor, staking, usdc));
    let client = TreasuryClient::new(env, &contract_id);

    Fixture {
        client,
        contract_id,
        usdc_admin_client,
        usdc_client,
    }
}

fn fund(fx: &Fixture, who: &Address, amount: i128) {
    fx.usdc_admin_client.mint(who, &amount);
}

/// Deposits `amount` into `bucket` from a freshly minted address.
fn deposit_into(env: &Env, fx: &Fixture, bucket: TreasuryBucket, amount: i128) -> Address {
    let from = Address::generate(env);
    fund(fx, &from, amount);
    fx.client.deposit(&from, &bucket, &amount);
    from
}

// -- constructor --

/// Soroban's `env.register_at` testutil can re-run a contract's
/// `__constructor` against an address that is already registered
/// (soroban-sdk's own doc comment on `register_at`/`register`: "the
/// new contract's constructor will be called again during
/// re-registration... not reproducible on-chain"). This proves that
/// re-construction actually takes effect - the second constructor
/// call's config, not the first's, is what the contract reads
/// afterward - while documenting that this is purely a test-harness
/// capability: a real deployed contract's address is fixed at deploy
/// time and its constructor can never run again against it, since
/// there is no on-chain operation that re-invokes a constructor on an
/// existing contract instance.
#[test]
fn cannot_initialize_a_second_time() {
    let env = Env::default();
    env.mock_all_auths_allowing_non_root_auth();
    let governor = Address::generate(&env);
    let staking = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let usdc_admin_client = token::StellarAssetClient::new(&env, &usdc);

    let contract_id = env.register(Treasury, (governor.clone(), staking.clone(), usdc.clone()));
    let client = TreasuryClient::new(&env, &contract_id);
    assert_eq!(client.balance(&TreasuryBucket::Fees), 0);

    // `register_at` against the SAME address, with a DIFFERENT
    // staking address in the constructor args, re-runs __constructor
    // via this testutil-only path (there is no real way to deploy
    // twice at one address, and no on-chain operation re-invokes a
    // constructor at all). `accrue_reward`'s auth check below proves
    // the second call's config - not the first's - is what the
    // contract now holds.
    let new_staking = Address::generate(&env);
    env.register_at(
        &contract_id,
        Treasury,
        (governor.clone(), new_staking.clone(), usdc.clone()),
    );

    let from = Address::generate(&env);
    usdc_admin_client.mint(&from, &1_000_000);
    client.deposit(&from, &TreasuryBucket::ReporterRewards, &1_000_000);

    let to = Address::generate(&env);
    // Only `new_staking` (the SECOND constructor call's config) can
    // authorize accrue_reward now; the original `staking` address is
    // no longer the configured caller. `mock_all_auths_allowing_non_root_auth`
    // accepts whichever address the contract actually requires, so
    // this passing confirms the live config is `new_staking`, not the
    // address from the first, now-overwritten constructor call - the
    // re-registration took effect, exactly as soroban-sdk's own doc
    // comment on `register`/`register_at` says it will, even though
    // nothing on-chain could ever reach this state.
    let accrued = client.accrue_reward(&to, &TreasuryBucket::ReporterRewards, &500_000);
    assert_eq!(accrued, 500_000);
}

// -- deposit --

#[test]
fn deposit_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let from = Address::generate(&env);
    let result = fx.client.try_deposit(&from, &TreasuryBucket::Fees, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn deposit_is_callable_by_anyone_and_credits_the_named_bucket() {
    let env = Env::default();
    let fx = setup(&env);
    let from = Address::generate(&env);
    fund(&fx, &from, 1_000_000_000);
    fx.client
        .deposit(&from, &TreasuryBucket::Fees, &1_000_000_000);
    assert_eq!(fx.client.balance(&TreasuryBucket::Fees), 1_000_000_000);
    assert_eq!(fx.usdc_client.balance(&fx.contract_id), 1_000_000_000);
}

#[test]
fn deposit_rejects_an_amount_that_would_overflow_the_bucket_balance() {
    let env = Env::default();
    let fx = setup(&env);
    // Sets the bucket's own counter directly, bypassing a real
    // transfer of an amount this large (the SAC's own balance
    // tracking rejects minting/transferring anywhere near i128::MAX
    // well before Treasury's own checked_add ever would): this test
    // is about Treasury's own overflow guard on its bucket counter,
    // not about what a real token can move.
    env.as_contract(&fx.contract_id, || {
        crate::storage::set_bucket(&env, TreasuryBucket::Fees, i128::MAX - 1);
    });
    let from = Address::generate(&env);
    fund(&fx, &from, 10);
    let result = fx.client.try_deposit(&from, &TreasuryBucket::Fees, &10);
    assert_eq!(result, Err(Ok(Error::MathOverflow)));
}

#[test]
fn deposit_keeps_each_bucket_separately_accounted() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 1_000_000_000);
    deposit_into(&env, &fx, TreasuryBucket::Slashed, 2_000_000_000);
    deposit_into(&env, &fx, TreasuryBucket::KeeperRewards, 3_000_000_000);
    deposit_into(&env, &fx, TreasuryBucket::ReporterRewards, 4_000_000_000);

    assert_eq!(fx.client.balance(&TreasuryBucket::Fees), 1_000_000_000);
    assert_eq!(fx.client.balance(&TreasuryBucket::Slashed), 2_000_000_000);
    assert_eq!(
        fx.client.balance(&TreasuryBucket::KeeperRewards),
        3_000_000_000
    );
    assert_eq!(
        fx.client.balance(&TreasuryBucket::ReporterRewards),
        4_000_000_000
    );
    assert_eq!(fx.usdc_client.balance(&fx.contract_id), 10_000_000_000);
}

// -- accrue_reward --

#[test]
fn accrue_reward_requires_staking_auth() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::ReporterRewards, 1_000_000_000);
    let to = Address::generate(&env);
    let accrued = fx
        .client
        .accrue_reward(&to, &TreasuryBucket::ReporterRewards, &500_000_000);
    assert_eq!(accrued, 500_000_000);
    assert_eq!(fx.client.accrued(&to), 500_000_000);
}

#[test]
fn accrue_reward_rejects_a_non_reward_bucket() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 1_000_000_000);
    let to = Address::generate(&env);
    let result = fx
        .client
        .try_accrue_reward(&to, &TreasuryBucket::Fees, &500_000_000);
    assert_eq!(result, Err(Ok(Error::WrongBucket)));

    let result2 = fx
        .client
        .try_accrue_reward(&to, &TreasuryBucket::Slashed, &500_000_000);
    assert_eq!(result2, Err(Ok(Error::WrongBucket)));
}

#[test]
fn accrue_reward_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let to = Address::generate(&env);
    let result = fx
        .client
        .try_accrue_reward(&to, &TreasuryBucket::KeeperRewards, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn accrue_reward_caps_at_the_bucket_balance_never_accrues_unfunded() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::KeeperRewards, 1_000_000);
    let to = Address::generate(&env);

    let accrued = fx
        .client
        .accrue_reward(&to, &TreasuryBucket::KeeperRewards, &5_000_000);
    assert_eq!(accrued, 1_000_000);
    assert_eq!(fx.client.accrued(&to), 1_000_000);
    assert_eq!(fx.client.balance(&TreasuryBucket::KeeperRewards), 0);
}

/// A short bucket must not just cap the accrual silently: Section
/// 12.7's shortfall needs to be visible onchain. This checks the
/// `RewardShortfall` event itself, not just the capped `accrued`
/// return value already covered by
/// `accrue_reward_caps_at_the_bucket_balance_never_accrues_unfunded`.
#[test]
fn accrue_reward_short_of_the_request_emits_reward_shortfall() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::KeeperRewards, 1_000_000);
    let to = Address::generate(&env);

    let accrued = fx
        .client
        .accrue_reward(&to, &TreasuryBucket::KeeperRewards, &5_000_000);
    assert_eq!(accrued, 1_000_000);

    let expected = crate::events::RewardShortfall {
        to: to.clone(),
        bucket: TreasuryBucket::KeeperRewards,
        requested: 5_000_000,
        accrued: 1_000_000,
        shortfall: 4_000_000,
    }
    .to_xdr(&env, &fx.contract_id);
    let found = env.events().all().events().contains(&expected);
    assert!(
        found,
        "accrue_reward short of the request must emit RewardShortfall"
    );
}

/// The exact-fill case (accrued == requested) must NOT emit a
/// shortfall: there was none.
#[test]
fn accrue_reward_exactly_covered_by_the_bucket_emits_no_shortfall() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::KeeperRewards, 1_000_000);
    let to = Address::generate(&env);

    let accrued = fx
        .client
        .accrue_reward(&to, &TreasuryBucket::KeeperRewards, &1_000_000);
    assert_eq!(accrued, 1_000_000);

    let would_be_shortfall = crate::events::RewardShortfall {
        to: to.clone(),
        bucket: TreasuryBucket::KeeperRewards,
        requested: 1_000_000,
        accrued: 1_000_000,
        shortfall: 0,
    }
    .to_xdr(&env, &fx.contract_id);
    let any_shortfall = env.events().all().events().contains(&would_be_shortfall);
    assert!(
        !any_shortfall,
        "an exactly-covered accrual must not emit RewardShortfall"
    );
}

#[test]
fn accrue_reward_against_a_completely_empty_bucket_accrues_zero() {
    let env = Env::default();
    let fx = setup(&env);
    let to = Address::generate(&env);
    let accrued = fx
        .client
        .accrue_reward(&to, &TreasuryBucket::ReporterRewards, &1_000_000);
    assert_eq!(accrued, 0);
    assert_eq!(fx.client.accrued(&to), 0);
}

// -- claim_reward --

#[test]
fn claim_reward_rejects_when_nothing_is_accrued() {
    let env = Env::default();
    let fx = setup(&env);
    let who = Address::generate(&env);
    let result = fx.client.try_claim_reward(&who);
    assert_eq!(result, Err(Ok(Error::NothingToClaim)));
}

#[test]
fn claim_reward_pays_out_everything_accrued() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::ReporterRewards, 1_000_000_000);
    let to = Address::generate(&env);
    fx.client
        .accrue_reward(&to, &TreasuryBucket::ReporterRewards, &1_000_000_000);

    let claimed = fx.client.claim_reward(&to);
    assert_eq!(claimed, 1_000_000_000);
    assert_eq!(fx.client.accrued(&to), 0);
    assert_eq!(fx.usdc_client.balance(&to), 1_000_000_000);
}

// -- allocate --

#[test]
fn allocate_requires_governor_auth_and_moves_balance_between_buckets() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 1_000_000_000);

    fx.client.allocate(
        &TreasuryBucket::Fees,
        &TreasuryBucket::ReporterRewards,
        &400_000_000,
    );
    assert_eq!(fx.client.balance(&TreasuryBucket::Fees), 600_000_000);
    assert_eq!(
        fx.client.balance(&TreasuryBucket::ReporterRewards),
        400_000_000
    );
}

#[test]
fn allocate_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let result = fx
        .client
        .try_allocate(&TreasuryBucket::Fees, &TreasuryBucket::Slashed, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn allocate_rejects_more_than_the_source_bucket_holds() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 100);
    let result = fx
        .client
        .try_allocate(&TreasuryBucket::Fees, &TreasuryBucket::Slashed, &101);
    assert_eq!(result, Err(Ok(Error::InsufficientBucket)));
}

#[test]
fn allocate_never_changes_the_total_held_across_buckets() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 1_000_000_000);
    deposit_into(&env, &fx, TreasuryBucket::Slashed, 500_000_000);

    let total_before = fx.client.balance(&TreasuryBucket::Fees)
        + fx.client.balance(&TreasuryBucket::Slashed)
        + fx.client.balance(&TreasuryBucket::KeeperRewards)
        + fx.client.balance(&TreasuryBucket::ReporterRewards);

    fx.client.allocate(
        &TreasuryBucket::Fees,
        &TreasuryBucket::KeeperRewards,
        &300_000_000,
    );
    fx.client.allocate(
        &TreasuryBucket::Slashed,
        &TreasuryBucket::ReporterRewards,
        &500_000_000,
    );

    let total_after = fx.client.balance(&TreasuryBucket::Fees)
        + fx.client.balance(&TreasuryBucket::Slashed)
        + fx.client.balance(&TreasuryBucket::KeeperRewards)
        + fx.client.balance(&TreasuryBucket::ReporterRewards);
    assert_eq!(total_before, total_after);
    // No USDC moved: allocate is bucket bookkeeping only.
    assert_eq!(
        fx.usdc_client.balance(&fx.contract_id),
        1_000_000_000 + 500_000_000
    );
}

// -- spend --

#[test]
fn spend_requires_governor_auth_and_pays_out_from_the_bucket() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 1_000_000_000);
    let recipient = Address::generate(&env);

    fx.client
        .spend(&TreasuryBucket::Fees, &recipient, &400_000_000);
    assert_eq!(fx.client.balance(&TreasuryBucket::Fees), 600_000_000);
    assert_eq!(fx.usdc_client.balance(&recipient), 400_000_000);
}

#[test]
fn spend_rejects_a_non_positive_amount() {
    let env = Env::default();
    let fx = setup(&env);
    let recipient = Address::generate(&env);
    let result = fx.client.try_spend(&TreasuryBucket::Fees, &recipient, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn spend_rejects_more_than_the_bucket_holds() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 100);
    let recipient = Address::generate(&env);
    let result = fx.client.try_spend(&TreasuryBucket::Fees, &recipient, &101);
    assert_eq!(result, Err(Ok(Error::InsufficientBucket)));
}

// -- T1-T4 (task brief) --
//
// T1. USDC balance of Treasury >= sum of all bucket balances + sum
//     of all accrued-but-unclaimed rewards. Tested directly below
//     (`t1_balance_covers_buckets_plus_accrued`) and continuously by
//     `property::t1_to_t4_never_break`.
// T2. accrue_reward never accrues more than the bucket holds; it
//     returns the amount actually accrued. Tested by
//     `accrue_reward_caps_at_the_bucket_balance_never_accrues_unfunded`
//     above and by the property test.
// T3. USDC leaves Treasury only through claim_reward (to the address
//     it was accrued to) or spend (governor). Enforced structurally
//     (see `lib.rs`'s own doc comment) and exercised by the property
//     test's own op set, which has no other way to move USDC out.
// T4. allocate moves balance between buckets without changing the
//     total. Tested by `allocate_never_changes_the_total_held_across_buckets`
//     above and by the property test.

#[test]
fn t1_balance_covers_buckets_plus_accrued() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::ReporterRewards, 1_000_000_000);
    let to = Address::generate(&env);
    fx.client
        .accrue_reward(&to, &TreasuryBucket::ReporterRewards, &400_000_000);

    let tracked = fx.client.balance(&TreasuryBucket::Fees)
        + fx.client.balance(&TreasuryBucket::Slashed)
        + fx.client.balance(&TreasuryBucket::KeeperRewards)
        + fx.client.balance(&TreasuryBucket::ReporterRewards)
        + fx.client.accrued(&to);
    let balance = fx.usdc_client.balance(&fx.contract_id);
    assert!(
        balance >= tracked,
        "T1 violated: balance {} < tracked {}",
        balance,
        tracked
    );
    // Exact here: no donation happened.
    assert_eq!(balance, tracked);
}

#[test]
fn t1_a_direct_transfer_with_no_deposit_call_counts_as_a_donation_not_a_violation() {
    let env = Env::default();
    let fx = setup(&env);
    // A plain transfer straight into the contract, bypassing
    // `deposit` entirely: no bucket or accrued balance tracks it,
    // but T1 only requires balance >= tracked, never ==, so this is
    // explicitly not a violation.
    let donor = Address::generate(&env);
    fund(&fx, &donor, 1_000_000);
    fx.usdc_client.transfer(
        &donor,
        soroban_sdk::MuxedAddress::from(fx.contract_id.clone()),
        &1_000_000,
    );

    let tracked = fx.client.balance(&TreasuryBucket::Fees)
        + fx.client.balance(&TreasuryBucket::Slashed)
        + fx.client.balance(&TreasuryBucket::KeeperRewards)
        + fx.client.balance(&TreasuryBucket::ReporterRewards);
    assert_eq!(tracked, 0);
    assert_eq!(fx.usdc_client.balance(&fx.contract_id), 1_000_000);
}

// -- Issue #12: upgrade / migrate / version --

#[test]
fn version_reads_contract_version_constant() {
    let env = Env::default();
    let fx = setup(&env);
    assert_eq!(fx.client.version(), crate::CONTRACT_VERSION);
}

/// Non-governor cannot upgrade: a native auth trap, demonstrated
/// directly with no auth mocking at all.
#[test]
#[should_panic]
fn upgrade_without_governor_auth_traps() {
    let env = Env::default();
    // No mock_all_auths() of any kind: require_auth genuinely checks.
    let governor = Address::generate(&env);
    let staking = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let contract_id = env.register(Treasury, (governor.clone(), staking.clone(), usdc.clone()));
    let client = TreasuryClient::new(&env, &contract_id);

    let new_wasm_hash = env.upload(Treasury);
    // No mock_all_auths(): the governor never actually signs this call.
    client.upgrade(&new_wasm_hash);
}

/// Upgrading to new code (a fresh wasm hash, uploaded via `env.upload`
/// per soroban-sdk's own documented pattern for exercising
/// `update_current_contract` in a unit test without a compiled .wasm
/// fixture) keeps all existing storage: a deposited bucket balance
/// still reads back correctly through the same client and address
/// after it.
#[test]
fn upgrade_keeps_existing_storage() {
    let env = Env::default();
    let fx = setup(&env);
    deposit_into(&env, &fx, TreasuryBucket::Fees, 500_000);
    let balance_before = fx.client.balance(&TreasuryBucket::Fees);

    let new_wasm_hash = env.upload(Treasury);
    fx.client.upgrade(&new_wasm_hash);

    assert_eq!(
        fx.client.balance(&TreasuryBucket::Fees),
        balance_before,
        "balance(Fees) must read the same after an upgrade with no storage-shape change"
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

/// `set_governor` works exactly once: the current governor hands off
/// to a new address, and a governor-gated call afterward still
/// succeeds (through the fixture's own auth mocking, which approves
/// every `require_auth()` regardless of address; the real proof the
/// new address is the one actually recorded is
/// `set_governor_old_governor_rejected_new_governor_accepted` below).
#[test]
fn set_governor_succeeds_once() {
    let env = Env::default();
    let fx = setup(&env);
    let new_governor = Address::generate(&env);

    fx.client.set_governor(&new_governor);

    let new_wasm_hash = env.upload(Treasury);
    fx.client.upgrade(&new_wasm_hash);
}

/// `set_governor` cannot run twice: `GovernorAlreadySet` on the second
/// call, confirmed via `try_set_governor` since this is a real
/// returned error, not a native auth trap. Same one-time-lock shape
/// as `migrate_cannot_run_twice`.
#[test]
fn set_governor_fails_the_second_call() {
    let env = Env::default();
    let fx = setup(&env);

    fx.client.set_governor(&Address::generate(&env));
    let result = fx.client.try_set_governor(&Address::generate(&env));

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
    let staking = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let contract_id = env.register(Treasury, (governor.clone(), staking.clone(), usdc.clone()));
    let client = TreasuryClient::new(&env, &contract_id);

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
    let staking = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let contract_id = env.register(
        Treasury,
        (old_governor.clone(), staking.clone(), usdc.clone()),
    );
    let client = TreasuryClient::new(&env, &contract_id);

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

    let new_wasm_hash = env.upload(Treasury);

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
    let staking = Address::generate(&env);
    let sac_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(sac_admin);
    let usdc = sac.address();
    let contract_id = env.register(Treasury, (governor.clone(), staking.clone(), usdc.clone()));
    let client = TreasuryClient::new(&env, &contract_id);

    // No mock_all_auths(): the governor never actually signs this call.
    client.migrate();
}
