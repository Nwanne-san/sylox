//! Governor storage. docs/design/governor.md Section 3. Every entry
//! extends its own TTL (issue #28) with the shared `sylox_types::ttl`
//! constants: the instance bundle on every state-changing call and on
//! the `committee()`/`paused()` reads other contracts make from their
//! own state-changing paths; each `Action(id)` on every write.

use soroban_sdk::{contracttype, symbol_short, Address, Env, Map, Symbol, Vec};
use sylox_types::{PauseScope, QueuedAction};

/// The five contracts the Governor dispatches to. Fixed at
/// construction; changeable only by upgrading the Governor (D2).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Targets {
    pub oracle: Address,
    pub registry: Address,
    pub staking: Address,
    pub treasury: Address,
    pub factory: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub signers: Vec<Address>,
    pub threshold: u32,
    pub committee: Address,
    pub guardian: Address,
    pub targets: Targets,
}

#[contracttype]
pub enum DataKey {
    Config,
    NextActionId,
    /// Mirror of the last value set per protocol-wide `SetParam` key
    /// (D11). Instance, since it only ever holds protocol-wide keys
    /// (D23).
    Params,
    /// The timestamp a pause of this scope lapses; absent when never
    /// paused or unpaused.
    Paused(PauseScope),
    Action(u64),
}

/// `sylox_types::upgrade::apply_migration`'s key for the storage version
/// the Governor's own data is shaped for (Section 6.3).
pub const MIGRATION_VERSION_KEY: Symbol = symbol_short!("MIGRATE");

pub fn get_config(env: &Env) -> Option<Config> {
    env.storage().instance().get(&DataKey::Config)
}

pub fn set_config(env: &Env, config: &Config) {
    env.storage().instance().set(&DataKey::Config, config);
}

/// Returns the next id and stores the one after it.
pub fn take_next_action_id(env: &Env) -> u64 {
    let id: u64 = env
        .storage()
        .instance()
        .get(&DataKey::NextActionId)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::NextActionId, &(id + 1));
    id
}

pub fn get_action(env: &Env, id: u64) -> Option<QueuedAction> {
    env.storage().persistent().get(&DataKey::Action(id))
}

pub fn set_action(env: &Env, action: &QueuedAction) {
    let key = DataKey::Action(action.id);
    env.storage().persistent().set(&key, action);
    sylox_types::ttl::extend_persistent(env, &key);
}

pub fn extend_action(env: &Env, id: u64) {
    sylox_types::ttl::extend_persistent(env, &DataKey::Action(id));
}

pub fn get_params(env: &Env) -> Map<Symbol, i128> {
    env.storage()
        .instance()
        .get(&DataKey::Params)
        .unwrap_or(Map::new(env))
}

pub fn set_param(env: &Env, key: Symbol, value: i128) {
    let mut params = get_params(env);
    params.set(key, value);
    env.storage().instance().set(&DataKey::Params, &params);
}

pub fn get_pause_lapse(env: &Env, scope: PauseScope) -> Option<u64> {
    env.storage().instance().get(&DataKey::Paused(scope))
}

pub fn set_pause_lapse(env: &Env, scope: PauseScope, lapse: u64) {
    env.storage()
        .instance()
        .set(&DataKey::Paused(scope), &lapse);
}

pub fn clear_pause(env: &Env, scope: PauseScope) {
    env.storage().instance().remove(&DataKey::Paused(scope));
}
