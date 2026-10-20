//! D13 (`docs/design/governor.md` Section 6.2): when one invocation of a
//! caller calls `target.upgrade(hash)` and then `target.migrate()`, which
//! code runs `migrate()`, the old or the new? The answer decides whether
//! `execute(Upgrade)` can run both or `Action::Migrate` is needed.
//!
//! Uses `env.upload` of native contract types, the same pattern every
//! other contract's `upgrade_keeps_existing_storage` test uses. The same
//! sequence was also run once against two contracts compiled to real Wasm
//! (built with `stellar contract build`, loaded with `contractimport!`)
//! on the host VM, with the same result: the new code runs `migrate()`.

use soroban_sdk::{
    contract, contractclient, contractimpl, symbol_short, Address, BytesN, ContractExecutable, Env,
};

#[contractclient(name = "TargetClient")]
#[allow(dead_code)]
pub trait TargetIface {
    fn version(env: Env) -> u32;
    fn upgrade(env: Env, new_wasm_hash: BytesN<32>);
    fn migrate(env: Env) -> u32;
    fn migrated_by(env: Env) -> u32;
}

fn governor(env: &Env) -> Address {
    env.storage().instance().get(&symbol_short!("gov")).unwrap()
}

macro_rules! target_version {
    ($name:ident, $version:expr) => {
        #[contract]
        pub struct $name;

        #[contractimpl]
        impl $name {
            pub fn __constructor(env: Env, governor: Address) {
                env.storage()
                    .instance()
                    .set(&symbol_short!("gov"), &governor);
            }
            pub fn version(_env: Env) -> u32 {
                $version
            }
            pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
                governor(&env).require_auth();
                env.deployer()
                    .update_current_contract(ContractExecutable::Wasm(new_wasm_hash));
            }
            /// Records and returns the version of the code that ran it.
            pub fn migrate(env: Env) -> u32 {
                governor(&env).require_auth();
                env.storage()
                    .instance()
                    .set(&symbol_short!("migr_by"), &$version);
                $version
            }
            pub fn migrated_by(env: Env) -> u32 {
                env.storage()
                    .instance()
                    .get(&symbol_short!("migr_by"))
                    .unwrap_or(0)
            }
        }
    };
}

target_version!(TargetV1, 1u32);
target_version!(TargetV2, 2u32);

/// Stands in for the Governor's `execute`: one invocation, `upgrade`
/// then `migrate`, the caller being the target's `governor`.
#[contract]
pub struct UpgradeThenMigrate;

#[contractimpl]
impl UpgradeThenMigrate {
    pub fn run(env: Env, target: Address, hash: BytesN<32>) -> u32 {
        let target = TargetClient::new(&env, &target);
        target.upgrade(&hash);
        target.migrate()
    }
}

#[test]
fn upgrade_then_migrate_in_one_invocation_runs_the_new_code() {
    let env = Env::default();
    let caller = env.register(UpgradeThenMigrate, ());
    let target_id = env.register(TargetV1, (caller.clone(),));
    let v2_hash = env.upload(TargetV2);
    let target = TargetClient::new(&env, &target_id);
    assert_eq!(target.version(), 1);

    let ran = UpgradeThenMigrateClient::new(&env, &caller).run(&target_id, &v2_hash);

    assert_eq!(
        ran, 2,
        "migrate() in the same invocation must run the new code"
    );
    assert_eq!(target.migrated_by(), 2);
    assert_eq!(target.version(), 2);
}
