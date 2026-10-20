//! Minimal clients for the five targets, one trait per contract with
//! only the governor-gated functions the Governor dispatches to
//! (docs/design/governor.md Section 4.3), the same pattern every other
//! contract here uses for its own cross-contract calls. Return types
//! are declared without the targets' own `Result`: a target that
//! rejects a call traps, which reverts the whole `execute` and leaves
//! the action Approved (Section 4.2).

use soroban_sdk::{contractclient, Address, BytesN, Env, Map, Symbol, Vec};
use sylox_types::{AssetConfig, EventDefinition, SeriesTerms, TreasuryBucket};

/// `upgrade`/`migrate`, identical on all five targets (issue #12).
#[allow(dead_code)]
#[contractclient(name = "UpgradeableClient")]
pub trait Upgradeable {
    fn upgrade(env: Env, new_wasm_hash: BytesN<32>);
    fn migrate(env: Env);
}

#[allow(dead_code)]
#[contractclient(name = "RiskOracleClient")]
pub trait RiskOracle {
    fn add_asset(env: Env, cfg: AssetConfig);
    fn update_asset(env: Env, asset: Address, cfg: AssetConfig);
    fn disable_asset(env: Env, asset: Address);
    fn set_formula(env: Env, version: u32, weights: Vec<u32>, params: Map<Symbol, i128>);
    fn set_sub_epoch_secs(env: Env, asset: Address, value: u64);
}

#[allow(dead_code)]
#[contractclient(name = "EventRegistryClient")]
pub trait EventRegistry {
    fn register_definition(env: Env, def: EventDefinition) -> u32;
    fn set_factory(env: Env, factory: Address);
}

#[allow(dead_code)]
#[contractclient(name = "StakingClient")]
pub trait Staking {
    fn add_keeper(env: Env, keeper: Address);
    fn remove_keeper(env: Env, keeper: Address);
    fn add_reporter(env: Env, reporter: Address, region: Symbol);
    fn remove_reporter(env: Env, reporter: Address);
}

#[allow(dead_code)]
#[contractclient(name = "TreasuryClient")]
pub trait Treasury {
    fn allocate(env: Env, from: TreasuryBucket, to: TreasuryBucket, amount: i128);
    fn spend(env: Env, bucket: TreasuryBucket, to: Address, amount: i128);
}

#[allow(dead_code)]
#[contractclient(name = "MarketFactoryClient")]
pub trait MarketFactory {
    fn open_series(env: Env, terms: SeriesTerms) -> Address;
    fn set_series_wasm(env: Env, hash: BytesN<32>);
    fn set_related_sellers(env: Env, asset: Address, list: Vec<Address>);
    fn set_hard_cap(env: Env, asset: Address, amount: i128);
    fn set_liquidity_cover_ratio(env: Env, ratio: i128);
    fn set_price_guard(env: Env, asset: Address, guard: Option<Address>);
}
