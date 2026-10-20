//! Proves `demo_build()` false against the real, already-built
//! release wasm, not the native test harness this crate's other tests
//! run against (`demo_build_is_false_without_the_demo_time_feature`,
//! `test.rs`, proves the feature flag itself works; this proves the
//! actual artifact `deploy/deploy.sh` ships never carries it).
//!
//! Ignored by default (`#[ignore]`): it reads
//! `target/wasm32v1-none/release/risk_oracle.wasm` from disk, which
//! only exists once `stellar contract build` has actually run. A
//! normal `cargo test` never touches this file and never fails for
//! not having it. CI's own `demo-time-not-in-release` job runs
//! `stellar contract build` (no `--features`, the same command
//! `deploy/deploy.sh` uses) first, then `cargo test -p risk-oracle
//! --lib -- --ignored release_wasm`, so this is the one place in the
//! whole suite that actually checks the shipped binary, not source
//! code that merely compiles correctly.

extern crate std;

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Bytes, BytesN, ContractExecutable, Env};

use crate::RiskOracleClient;

const RELEASE_WASM_PATH: &str = "../../target/wasm32v1-none/release/risk_oracle.wasm";

#[test]
#[ignore = "requires `stellar contract build` (no --features) to have already run; see this module's own doc comment"]
fn demo_build_is_false_in_the_real_release_wasm() {
    let bytes = std::fs::read(RELEASE_WASM_PATH).unwrap_or_else(|e| {
        panic!(
            "could not read {RELEASE_WASM_PATH} ({e}) - run `stellar contract build` \
             (no --features) from the repo root first"
        )
    });

    let env = Env::default();
    env.mock_all_auths();

    let wasm_hash = env
        .deployer()
        .upload_contract_wasm(Bytes::from_slice(&env, &bytes));

    let governor = Address::generate(&env);
    let registry = Address::generate(&env);
    let staking = Address::generate(&env);
    let deployer = Address::generate(&env);
    let salt = BytesN::from_array(&env, &[0u8; 32]);
    let contract_id = env.deployer().with_address(deployer, salt).deploy_contract(
        ContractExecutable::Wasm(wasm_hash),
        (governor, registry, staking),
    );

    let client = RiskOracleClient::new(&env, &contract_id);
    assert!(
        !client.demo_build(),
        "the real release wasm reports demo_build() == true - it was built with the \
         demo-time feature active and must never be deployed as a real contract"
    );
}
