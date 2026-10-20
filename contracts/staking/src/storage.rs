//! Storage keys for Staking. technical-doc.md Section 15.1.

use soroban_sdk::{contracttype, Address, Env, Symbol, Vec};
use sylox_types::{BondKey, KeeperInfo, ProbeReport, ReporterInfo};

/// A probe plus the submitter's region AT SUBMISSION TIME (lead
/// decision, feat/staking): `aggregate`/`settle_epoch` use only this
/// snapshot, never a live `get_reporter` lookup, so a reporter removed
/// after submitting cannot change an epoch's aggregate retroactively,
/// and `RiskOracle`'s stored endpoint (read once, at posting or
/// `finalize_endpoint` time) stays in agreement with whatever
/// `Staking` later settles for the same epoch.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProbe {
    pub report: ProbeReport,
    pub region_at_submission: Symbol,
}

/// A bond record: owner, amount, and (lead decision, feat/staking) the
/// `subject` address a `SignalDispute` bond names, so `release_bond`/
/// `forfeit_bond` can decrement that keeper's `open_dispute_count`
/// without the caller needing to repeat it. Event bond kinds
/// (`EventProposal`, `EventChallenge`) always carry `subject: None`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BondRecord {
    pub owner: Address,
    pub amount: i128,
    pub subject: Option<Address>,
}

#[contracttype]
pub enum DataKey {
    Config,
    Keeper(Address),
    Reporter(Address),
    /// Every reporter address ever added, instance class. Only used by
    /// `add_reporter` to reject a duplicate registration; `aggregate`
    /// and `settle_epoch` use `Submitters` below, never this list, so
    /// a removed reporter is never silently dropped from a historical
    /// epoch's aggregate (lead decision, feat/staking).
    AllReporters,
    /// Temporary (Section 15.2, 15.3): one reporter's `StoredProbe` for
    /// one asset epoch. TTL extended at write time to
    /// `params::PROBE_TTL_LEDGERS`.
    Probe(Address, u64, Address),
    /// Temporary: the reporters who submitted a probe for this
    /// (asset, epoch), in submission order, capped at
    /// `params::MAX_SUBMITTERS_PER_EPOCH` (lead decision, feat/staking:
    /// "maintain a per (asset, epoch) index of submitting reporters...
    /// cap its length at the maximum reporter count"). `aggregate` and
    /// `settle_epoch` iterate this, never the live reporter set, so a
    /// removed reporter's already submitted probe is still counted.
    /// Same TTL treatment as `Probe`.
    Submitters(Address, u64),
    /// Temporary: marks that `settle_epoch` has already run for this
    /// (asset, epoch), so a second call (even after the probes
    /// themselves have expired) returns `AlreadySettled` rather than
    /// re-processing. Same TTL treatment as `Probe`.
    ProbesSettled(Address, u64),
    Bond(BondKey),
    /// Refunds and winnings owed to a PARTICIPANT (the bond's owner,
    /// or a dispute/slash winner) awaiting `claim` (Section 7.8).
    /// feat/treasury: the treasury's own share of a forfeit or slash
    /// is deposited straight into `Treasury`'s `Slashed` bucket
    /// instead (see `lib.rs`'s `deposit_treasury_share`), never
    /// credited here; this key now holds only participant funds,
    /// per the lead decision "one home for each kind of money".
    Claimable(Address),
}

pub fn get_keeper(env: &Env, keeper: &Address) -> Option<KeeperInfo> {
    env.storage()
        .persistent()
        .get(&DataKey::Keeper(keeper.clone()))
}

pub fn set_keeper(env: &Env, keeper: &Address, info: &KeeperInfo) {
    sylox_types::ttl::set_persistent(env, &DataKey::Keeper(keeper.clone()), info);
}

pub fn get_reporter(env: &Env, reporter: &Address) -> Option<ReporterInfo> {
    env.storage()
        .persistent()
        .get(&DataKey::Reporter(reporter.clone()))
}

pub fn set_reporter(env: &Env, reporter: &Address, info: &ReporterInfo) {
    sylox_types::ttl::set_persistent(env, &DataKey::Reporter(reporter.clone()), info);
}

pub fn get_all_reporters(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&DataKey::AllReporters)
        .unwrap_or_else(|| Vec::new(env))
}

pub fn add_to_all_reporters(env: &Env, reporter: &Address) {
    let mut reporters = get_all_reporters(env);
    if !reporters.contains(reporter) {
        reporters.push_back(reporter.clone());
        env.storage()
            .instance()
            .set(&DataKey::AllReporters, &reporters);
    }
}

pub fn get_probe(
    env: &Env,
    asset: &Address,
    epoch: u64,
    reporter: &Address,
) -> Option<StoredProbe> {
    env.storage()
        .temporary()
        .get(&DataKey::Probe(asset.clone(), epoch, reporter.clone()))
}

pub fn set_probe(
    env: &Env,
    asset: &Address,
    epoch: u64,
    reporter: &Address,
    probe: &StoredProbe,
    ttl_ledgers: u32,
) {
    let key = DataKey::Probe(asset.clone(), epoch, reporter.clone());
    env.storage().temporary().set(&key, probe);
    env.storage()
        .temporary()
        .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
}

pub fn get_submitters(env: &Env, asset: &Address, epoch: u64) -> Vec<Address> {
    env.storage()
        .temporary()
        .get(&DataKey::Submitters(asset.clone(), epoch))
        .unwrap_or_else(|| Vec::new(env))
}

/// Adds `reporter` to the (asset, epoch) submitter index. Returns
/// `false` without writing if `reporter` is already present (the
/// caller's own `DuplicateProbe` check should normally catch this
/// first) or if the index is already at `MAX_SUBMITTERS_PER_EPOCH`.
pub fn add_submitter(
    env: &Env,
    asset: &Address,
    epoch: u64,
    reporter: &Address,
    max_submitters: u32,
    ttl_ledgers: u32,
) -> bool {
    let key = DataKey::Submitters(asset.clone(), epoch);
    let mut submitters = get_submitters(env, asset, epoch);
    if submitters.contains(reporter) {
        return false;
    }
    if submitters.len() >= max_submitters {
        return false;
    }
    submitters.push_back(reporter.clone());
    env.storage().temporary().set(&key, &submitters);
    env.storage()
        .temporary()
        .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
    true
}

pub fn get_probes_settled(env: &Env, asset: &Address, epoch: u64) -> bool {
    env.storage()
        .temporary()
        .get(&DataKey::ProbesSettled(asset.clone(), epoch))
        .unwrap_or(false)
}

pub fn set_probes_settled(env: &Env, asset: &Address, epoch: u64, ttl_ledgers: u32) {
    let key = DataKey::ProbesSettled(asset.clone(), epoch);
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, ttl_ledgers, ttl_ledgers);
}

pub fn get_bond(env: &Env, key: &BondKey) -> Option<BondRecord> {
    env.storage().persistent().get(&DataKey::Bond(key.clone()))
}

pub fn set_bond(env: &Env, key: &BondKey, record: &BondRecord) {
    sylox_types::ttl::set_persistent(env, &DataKey::Bond(key.clone()), record);
}

pub fn clear_bond(env: &Env, key: &BondKey) {
    env.storage()
        .persistent()
        .remove(&DataKey::Bond(key.clone()));
}

pub fn get_claimable(env: &Env, who: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::Claimable(who.clone()))
        .unwrap_or(0)
}

pub fn add_claimable(env: &Env, who: &Address, amount: i128) {
    let current = get_claimable(env, who);
    sylox_types::ttl::set_persistent(env, &DataKey::Claimable(who.clone()), &(current + amount));
}

pub fn clear_claimable(env: &Env, who: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::Claimable(who.clone()));
}
