//! Minimal client interfaces for the contracts Series calls into, but
//! does not implement. technical-doc.md Section 12.1 (RiskOracle),
//! 12.2 (EventRegistry), 12.4 (MarketFactory); markets design note
//! Section 2 (`PriceGuard`, M2). Each trait lists only the functions
//! this contract actually calls, not that contract's full API; mock
//! contracts implementing these traits live in `test.rs`, never the
//! production build.

use soroban_sdk::{contractclient, Address, Env};
use sylox_types::{
    AssetEventStatus, Band, CoverGate, EventKind, GuardStatus, SignalSet, SlotState, SubEpoch,
};

#[contractclient(name = "RiskOracleClient")]
#[allow(dead_code)]
pub trait RiskOracle {
    fn is_stale(env: Env, asset: Address) -> bool;
    fn band(env: Env, asset: Address) -> Band;
    /// Section 9.4 step 3 (M1): the newest posted sub-epoch.
    fn live(env: Env, asset: Address) -> Option<(SubEpoch, SignalSet, SlotState)>;
    /// Section 9.4 step 6 (`require_holding`): the latest CONFIRMED
    /// (hourly) peg ratio, never `live`'s sub-epoch value.
    fn latest(env: Env, asset: Address) -> Option<SignalSet>;
    fn reference_rate(env: Env, asset: Address) -> i128;
    /// M1 measures sub-epoch boundaries with the length in force for
    /// each hour, which only `RiskOracle` tracks.
    fn sub_epoch_secs_at(env: Env, asset: Address, hour: u64) -> u64;
}

#[contractclient(name = "EventRegistryClient")]
#[allow(dead_code)]
pub trait EventRegistry {
    fn cover_gate(env: Env, asset: Address) -> CoverGate;
    fn event_status(env: Env, asset: Address, kind: EventKind) -> AssetEventStatus;
    fn covers(env: Env, event_id: u64, def_version: u32, start: u64, expiry: u64) -> bool;
    fn definition(
        env: Env,
        asset: Address,
        kind: EventKind,
        version: u32,
    ) -> Option<sylox_types::EventDefinition>;
}

#[contractclient(name = "MarketFactoryClient")]
#[allow(dead_code)]
pub trait MarketFactory {
    fn reserve_cover(env: Env, series: Address, amount: i128);
    fn release_cover(env: Env, series: Address, amount: i128);
    fn price_guard(env: Env, asset: Address) -> Option<Address>;
    fn related_sellers(env: Env, asset: Address) -> Option<soroban_sdk::Vec<Address>>;
}

/// M2 (design note Section 2): a fast, sale-only pause signal.
/// Checked after `cover_gate`; never consulted by `claim`, `trigger`
/// or `withdraw`. Deliberately not built on
/// `contracts/adapters/{amm-soroswap,fx-reference}`'s existing stub
/// signatures - a distinct concern (price/FX reads), not reused here.
/// `GuardStatus` itself lives in `sylox_types`, shared with whatever
/// contract implements `check` (for example `price-guard-mock`), so
/// both sides of this call encode the same type.
#[contractclient(name = "PriceGuardClient")]
#[allow(dead_code)]
pub trait PriceGuard {
    fn check(env: Env, asset: Address) -> GuardStatus;
}
