//! Shared upgrade/migrate logic for every Sylox contract. technical-doc.md
//! Section 17.3: "Core contracts expose upgrade(wasm_hash), callable only
//! by the governor, which calls env.deployer().update_current_contract_wasm
//! (hash)." This module is that one implementation, called from each
//! contract's own 2-3 line upgrade()/migrate() wrapper (each contract's own
//! Config and Error are distinct types, so the AlreadyMigrated mapping
//! stays in the caller; the auth check itself lives here - see do_upgrade/
//! do_migrate below - so a future wrapper can't forget it).
//!
//! update_current_contract_wasm itself is deprecated since soroban-sdk v28
//! in favor of update_current_contract(ContractExecutable::Wasm(hash)),
//! which does the same thing for a plain Wasm executable and is the
//! officially recommended replacement; used here instead of the literal
//! call the spec names.
//!
//! The Upgraded event carries old_version, not old_wasm_hash: there is no
//! SDK call that reads a contract's own current wasm hash from within its
//! own execution (checked env.deployer()'s full API: upload/update only, no
//! getter), so a wasm hash here would have to come from the caller,
//! unverified. old_version is the running code's own version() constant,
//! read before the swap, which cannot be wrong - the new version is
//! confirmed afterward by calling version() post-upgrade (see deploy/
//! README.md's upgrade procedure).
//!
//! Section 17.3's 14 day timelock for upgrades is NOT enforced here: it
//! belongs to `Governor` (docs/design/governor.md Section 6), which is
//! the only caller of do_upgrade/do_migrate once a contract's `governor`
//! address is the Governor contract, and only from its own `execute`
//! after the timelock. Until that handover, whoever holds the
//! `governor` address can call them immediately. apply_upgrade and
//! apply_migration are the same mechanics without the auth check, for
//! the Governor's own in-process self-upgrade only.

use soroban_sdk::{contractevent, Address, BytesN, ContractExecutable, Env, Symbol};

/// technical-doc.md Section 17.3. `contract` is `env.current_contract_
/// address()`, so an indexer watching this topic across every Sylox
/// contract can tell which one just upgraded without decoding the event's
/// own emitting address separately.
#[contractevent(topics = ["sylox", "upgraded"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Upgraded {
    #[topic]
    pub contract: Address,
    pub old_version: u32,
    pub new_wasm_hash: BytesN<32>,
}

/// Call from each contract's own `upgrade(env, new_wasm_hash)`. Checks
/// `governor.require_auth()` itself - the caller just extracts `governor`
/// from its own `Config` type (which this module cannot name generically)
/// and passes it straight through, so a wrapper can never forget the
/// check.
///
/// `current_version` is the calling contract's own `version()` constant,
/// read BEFORE the swap (the whole reason `Upgraded` can carry a true
/// `old_version` with no on-chain read-back: it is read from the code
/// that is about to stop running, by that same code, not guessed by the
/// caller).
pub fn do_upgrade(env: &Env, governor: &Address, current_version: u32, new_wasm_hash: BytesN<32>) {
    governor.require_auth();
    apply_upgrade(env, current_version, new_wasm_hash);
}

/// The swap and the `Upgraded` event, with NO auth check of its own.
/// Only for `Governor`'s in-process self-upgrade (docs/design/
/// governor.md Section 6.3): the Governor's own address cannot
/// authorize a call into itself, so its `execute` checks the signer
/// threshold and the 14 day timelock first and then calls this
/// directly. Every other contract goes through `do_upgrade`.
pub fn apply_upgrade(env: &Env, current_version: u32, new_wasm_hash: BytesN<32>) {
    env.deployer()
        .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
    Upgraded {
        contract: env.current_contract_address(),
        old_version: current_version,
        new_wasm_hash,
    }
    .publish(env);
}

/// Call from each contract's own `migrate(env) -> Result<(), Error>`.
/// Checks `governor.require_auth()` itself, same reasoning as
/// `do_upgrade`. Always migrates to the CALLING CODE'S OWN
/// `CONTRACT_VERSION` (`target_version` here, passed by the caller as its
/// own constant) - never a caller-chosen version number: letting a
/// governor mark an arbitrary future version as already migrated (e.g.
/// passing 5 while the deployed code is still at 2) would permanently
/// block that migration from ever running once the code genuinely
/// reaches version 5. `key` is the instance storage key the CALLER's own
/// DataKey enum reserves for "the storage version this contract's data is
/// currently shaped for" - a plain Symbol, not a shared DataKey variant,
/// since every contract's own DataKey is a distinct type this module
/// cannot name.
///
/// Returns `true` if `target_version` had not yet been applied (and
/// applies it now - the caller's own migration body, if any, runs around
/// this call); `false` if it was already applied (the caller maps this to
/// its own `Error::AlreadyMigrated`-shaped variant). Comparing against the
/// STORED version, not merely "has this key ever been set", so a LATER
/// `CONTRACT_VERSION` after a future code upgrade still migrates
/// correctly even though this same key already holds an earlier value -
/// each version can run at most once, not "migrate can run at most once
/// ever".
pub fn do_migrate(env: &Env, governor: &Address, key: &Symbol, target_version: u32) -> bool {
    governor.require_auth();
    apply_migration(env, key, target_version)
}

/// `do_migrate`'s version check and write, with NO auth check of its
/// own. Only for `Governor`, whose own migration runs automatically on
/// its first state-changing call under new code (docs/design/
/// governor.md Section 6.3), not through a governor-authorized call.
pub fn apply_migration(env: &Env, key: &Symbol, target_version: u32) -> bool {
    let current: u32 = env.storage().instance().get(key).unwrap_or(0);
    if current >= target_version {
        return false;
    }
    env.storage().instance().set(key, &target_version);
    true
}
