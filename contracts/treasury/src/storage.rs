//! Storage keys for Treasury. technical-doc.md Section 15.1.

use soroban_sdk::{contracttype, Address, Env};
use sylox_types::TreasuryBucket;

#[contracttype]
pub enum DataKey {
    Config,
    /// One entry per `TreasuryBucket`, rather than a single `Buckets`
    /// map (Section 15.1's own table names a `Buckets` instance key,
    /// but a map of 4 fixed, known variants gains nothing over 4
    /// direct keys and avoids re-reading and re-writing the whole map
    /// on every single-bucket touch).
    Bucket(TreasuryBucket),
    /// Rewards accrued to a keeper or reporter, awaiting
    /// `claim_reward`.
    Accrued(Address),
}

pub fn get_bucket(env: &Env, bucket: TreasuryBucket) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::Bucket(bucket))
        .unwrap_or(0)
}

pub fn set_bucket(env: &Env, bucket: TreasuryBucket, amount: i128) {
    env.storage()
        .instance()
        .set(&DataKey::Bucket(bucket), &amount);
}

pub fn get_accrued(env: &Env, who: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::Accrued(who.clone()))
        .unwrap_or(0)
}

pub fn add_accrued(env: &Env, who: &Address, amount: i128) {
    let current = get_accrued(env, who);
    sylox_types::ttl::set_persistent(env, &DataKey::Accrued(who.clone()), &(current + amount));
}

pub fn clear_accrued(env: &Env, who: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::Accrued(who.clone()));
}
