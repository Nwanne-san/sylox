#![no_std]

//! MarketFactory: opens series, tracks the series index, enforces the
//! asset-wide cover cap, deploys `Series` contracts. technical-doc.md
//! Section 9, 11.1, 12.4; `docs/design/markets.md`.
//!
//! **Lead decision (design note Section 10): this contract owns the
//! "does a live series still pin this version" bookkeeping itself**
//! (`Pins`, `LiveCoverCount`, `has_live_cover`), rather than
//! `EventRegistry` loading every live `Series` instance to ask. Pins
//! are recorded at `open_series`, dropped at `release_cover`.
//!
//! **Lead decision (design note Section 6): the related-seller block
//! list lives here, per asset, governor-only setter** (`Governor`
//! does not exist yet; this uses the same governor-address trust
//! model every other governor-gated call in this workspace already
//! relies on). `open_series` refuses an asset whose list was never
//! set, and refuses an asset whose own readable SAC `admin()` is not
//! on it.
//!
//! **Implementation note: the settlement-issuer check (Section 9.7)
//! compares SAC `admin()` addresses, not parsed `name()` strings.**
//! The spec describes reading the settlement SAC's `name()`
//! (`CODE:ISSUER`) and comparing the issuer substring; this contract
//! instead reads `admin()` directly on both sides, the same technique
//! Section 6's own `IssuerNotListed` check uses. For a classic asset
//! wrapped as a Stellar Asset Contract, `admin()` IS the issuer
//! account unless admin has been transferred away from it, so this
//! achieves the same intent (stop a series from settling in an asset
//! whose issuer also backs the asset it covers) without parsing a
//! display string inside `no_std` contract code.

mod clients;
mod error;
mod events;
mod storage;

use soroban_sdk::{
    contract, contractimpl, symbol_short, token, Address, BytesN, ContractExecutable, Env, Symbol,
    Vec,
};
use sylox_types::{EventKind, Reference, SeriesTerms};

pub use error::Error;
use events::{RelatedSellersSet, SeriesOpened};
use storage::Config;

use clients::{EventRegistryClient, RiskOracleClient};

/// Section 11.5 default: the most live series one asset may have open
/// at once.
const MAX_SERIES_PER_ASSET: u32 = 4;

#[contract]
pub struct MarketFactory;

/// Issue #12. Bump by hand on every `upgrade()` this contract ships.
const CONTRACT_VERSION: u32 = 1;
/// Issue #12. Instance key for `sylox_types::upgrade::do_migrate`'s
/// own "which version has this contract's storage already been
/// migrated to" marker.
const MIGRATED_TO_KEY: Symbol = symbol_short!("MIGRATE");

/// `set_governor`'s own one-time lock, same shape as EventRegistry's
/// `FACTORY_SET_KEY`: absent until the first successful `set_governor`
/// call, then permanently present, so a second call always fails.
const GOVERNOR_SET_KEY: Symbol = symbol_short!("GOV_SET");

#[contractimpl]
impl MarketFactory {
    /// Issue #21, #66: a constructor, not a separate `initialize`
    /// call - see `RiskOracle::__constructor`'s own doc comment for
    /// why (the two-step "deploy, then invoke initialize" flow leaves
    /// a real front-runnable window; a constructor runs atomically
    /// with deployment, so there is none). Only stores addresses and
    /// config; never calls another contract, the same rule the four
    /// core contracts' own constructors follow.
    pub fn __constructor(
        env: Env,
        governor: Address,
        oracle: Address,
        registry: Address,
        treasury: Address,
        usdc: Address,
        series_wasm: BytesN<32>,
    ) {
        storage::set_config(
            &env,
            &Config {
                governor,
                oracle,
                registry,
                treasury,
                usdc,
                series_wasm,
            },
        );
    }

    /// Issue #12. The currently deployed code's own version, bumped by
    /// hand alongside each `upgrade()` this contract ships.
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// technical-doc.md Section 17.3: `MarketFactory` is a core
    /// contract, `upgrade(wasm_hash)` governor-only.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::upgrade::do_upgrade(&env, &config.governor, CONTRACT_VERSION, new_wasm_hash);
        Ok(())
    }

    /// Issue #12. Storage migration hook for a future `upgrade()`:
    /// runs at most once per `CONTRACT_VERSION`. Empty for now - no
    /// storage shape change ships with this build.
    pub fn migrate(env: Env) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        if !sylox_types::upgrade::do_migrate(
            &env,
            &config.governor,
            &MIGRATED_TO_KEY,
            CONTRACT_VERSION,
        ) {
            return Err(Error::AlreadyMigrated);
        }
        Ok(())
    }

    /// technical-doc.md Section 9.7, validated in the order stated
    /// there, with the design note's own two additions (the
    /// related-seller list, Section 6) inserted right after basic
    /// asset validity: a missing or incomplete governance setup for
    /// this asset is as fundamental a precondition as the asset
    /// existing at all, checked before the more specific
    /// event/definition/settlement rules. Governor-only in v1
    /// (permissionless later, Section 12.4).
    pub fn open_series(env: Env, terms: SeriesTerms) -> Result<Address, Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);

        let oracle = RiskOracleClient::new(&env, &config.oracle);
        let asset_config = oracle
            .asset_config(&terms.asset)
            .filter(|c| c.enabled)
            .ok_or(Error::InvalidTerms)?;
        if oracle.is_stale(&terms.asset) {
            return Err(Error::InvalidTerms);
        }

        let related_sellers = storage::get_related_sellers(&env, &terms.asset)
            .ok_or(Error::RelatedSellerListNotSet)?;
        if let Some(issuer) = read_sac_admin(&env, &terms.asset) {
            if !related_sellers.contains(&issuer) {
                return Err(Error::IssuerNotListed);
            }
        }

        let registry = EventRegistryClient::new(&env, &config.registry);
        if registry.has_declared(&terms.asset) {
            return Err(Error::AssetBlocked);
        }

        if terms.def_versions.is_empty() {
            return Err(Error::DefinitionNotCurrent);
        }
        for (kind, version) in terms.def_versions.iter() {
            if registry.current_version(&terms.asset, &kind) != version {
                return Err(Error::DefinitionNotCurrent);
            }
        }

        if terms.settlement == terms.asset {
            return Err(Error::InvalidSettlement);
        }
        let settlement_issuer =
            read_sac_admin(&env, &terms.settlement).ok_or(Error::InvalidSettlement)?;
        if settlement_issuer == asset_config.issuer {
            return Err(Error::InvalidSettlement);
        }

        if terms.require_holding && matches!(asset_config.reference, Reference::Asset(_)) {
            return Err(Error::InvalidTerms);
        }

        if terms.start >= terms.expiry
            || terms.claim_window_secs == 0
            || terms.cap <= 0
            || terms.max_cover_per_buyer <= 0
            || terms.fee_bps > 10_000
        {
            return Err(Error::InvalidTerms);
        }

        let live = storage::get_series_list(&env, &terms.asset);
        if live.len() >= MAX_SERIES_PER_ASSET {
            return Err(Error::TooManySeries);
        }

        // Section 3.2, 9.7: the deployment salt is derived from the
        // monotonic series counter, never reused.
        let id = storage::next_series_id(&env);
        let mut salt_bytes = [0u8; 32];
        salt_bytes[24..].copy_from_slice(&id.to_be_bytes());
        let salt = BytesN::from_array(&env, &salt_bytes);

        let series = env.deployer().with_current_contract(salt).deploy_contract(
            ContractExecutable::Wasm(config.series_wasm.clone()),
            (
                terms.clone(),
                env.current_contract_address(),
                config.oracle.clone(),
                config.registry.clone(),
                config.treasury.clone(),
            ),
        );

        storage::set_deployed(&env, &series, &terms.asset);
        let mut new_live = live;
        new_live.push_back(series.clone());
        storage::set_series_list(&env, &terms.asset, &new_live);

        storage::set_pins(&env, &series, &terms.def_versions);
        for (kind, version) in terms.def_versions.iter() {
            let count = storage::get_live_cover_count(&env, &terms.asset, kind, version);
            storage::set_live_cover_count(&env, &terms.asset, kind, version, count + 1);
        }

        SeriesOpened {
            asset: terms.asset.clone(),
            series: series.clone(),
            series_id: id,
            def_versions: terms.def_versions.clone(),
            start: terms.start,
            expiry: terms.expiry,
            cap: terms.cap,
        }
        .publish(&env);
        Ok(series)
    }

    /// technical-doc.md Section 11.1: checks the asset-wide cap and
    /// reserves in the same call, so there is no window between
    /// checking and reserving. Auth: the series contract itself, and
    /// it must be one this factory deployed (Section 16.1).
    pub fn reserve_cover(env: Env, series: Address, amount: i128) -> Result<(), Error> {
        series.require_auth();
        let asset = storage::get_deployed_asset(&env, &series).ok_or(Error::Unauthorized)?;
        sylox_types::ttl::extend_instance(&env);

        let open = storage::get_open_cover(&env, &asset);
        let new_open = open.checked_add(amount).ok_or(Error::MathOverflow)?;
        if new_open > Self::cover_cap(env.clone(), asset.clone())? {
            return Err(Error::CoverCapExceeded);
        }
        storage::set_open_cover(&env, &asset, new_open);
        Ok(())
    }

    /// Runs at Expired or Triggered (Section 1, 9.2): releases this
    /// series' own reserved cover, drops it from the live list, and
    /// drops its `Pins`/`LiveCoverCount` contribution (design note
    /// Section 10). Not idempotent by design: clearing `Deployed`
    /// means a second call for the same series (after the first
    /// already released it) is indistinguishable from a stranger
    /// address that was never deployed at all, so it traps the same
    /// way `reserve_cover` already does for a non-member address -
    /// relying on `Series`' own `sync_state()` to call this at most
    /// once per series, exactly at the moment of its own transition.
    pub fn release_cover(env: Env, series: Address, amount: i128) -> Result<(), Error> {
        series.require_auth();
        let asset = storage::get_deployed_asset(&env, &series).ok_or(Error::Unauthorized)?;
        sylox_types::ttl::extend_instance(&env);

        // PR #67 review: fails loudly rather than clamping to 0.
        // `amount` releasing more than `OpenCover` currently holds, or
        // a pinned `(kind, version)`'s own `LiveCoverCount` already at
        // 0, both mean this contract's own bookkeeping has already
        // drifted out of sync - clamping would hide that, not fix it.
        // A `Result::Err` here traps the whole call (this contract's
        // own convention: a #[contractimpl] function's `Err` is a
        // host-level trap, not a value the caller can inspect), so
        // nothing written earlier in this same call is left partially
        // applied.
        let open = storage::get_open_cover(&env, &asset);
        if amount > open {
            return Err(Error::AccountingUnderflow);
        }
        storage::set_open_cover(&env, &asset, open - amount);

        let mut live = storage::get_series_list(&env, &asset);
        if let Some(pos) = live.iter().position(|s| s == series) {
            live.remove(pos as u32);
            storage::set_series_list(&env, &asset, &live);
        }

        if let Some(pins) = storage::get_pins(&env, &series) {
            for (kind, version) in pins.iter() {
                let count = storage::get_live_cover_count(&env, &asset, kind, version);
                if count == 0 {
                    return Err(Error::AccountingUnderflow);
                }
                storage::set_live_cover_count(&env, &asset, kind, version, count - 1);
            }
            storage::clear_pins(&env, &series);
        }
        storage::clear_deployed(&env, &series);
        Ok(())
    }

    /// Section 17.3: affects series opened afterward only.
    pub fn set_series_wasm(env: Env, hash: BytesN<32>) -> Result<(), Error> {
        let mut config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        config.series_wasm = hash;
        storage::set_config(&env, &config);
        Ok(())
    }

    /// One-time governor handover, for the Governor deployment cutover:
    /// callable only by the CURRENT governor, exactly once. Same
    /// one-time-lock pattern as `EventRegistry::set_factory` (a storage
    /// flag checked before the write, set after it), so a second call
    /// always fails rather than letting the governor be reassigned
    /// again later through this same path.
    pub fn set_governor(env: Env, new_admin: Address) -> Result<(), Error> {
        let mut config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if env.storage().instance().has(&GOVERNOR_SET_KEY) {
            return Err(Error::GovernorAlreadySet);
        }
        config.governor = new_admin;
        storage::set_config(&env, &config);
        env.storage().instance().set(&GOVERNOR_SET_KEY, &true);
        Ok(())
    }

    /// Design note Section 6. An explicitly empty list is accepted
    /// (meaning "no related sellers to block"), but has to be set on
    /// purpose; `open_series` distinguishes this from "never set" via
    /// `storage::get_related_sellers`'s own `Option`.
    pub fn set_related_sellers(env: Env, asset: Address, list: Vec<Address>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let count = list.len();
        storage::set_related_sellers(&env, &asset, &list);
        RelatedSellersSet { asset, count }.publish(&env);
        Ok(())
    }

    /// Design note Section 11 (cover_cap's `hard_cap(a)` input, never
    /// named a storage location by the spec itself). Unset defaults
    /// to `i128::MAX`, see `storage::get_hard_cap`.
    pub fn set_hard_cap(env: Env, asset: Address, amount: i128) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        storage::set_hard_cap(&env, &asset, amount);
        Ok(())
    }

    /// `liquidity_cover_ratio` (`k`, Section 11.1), fixed point at
    /// `sylox_types::SCALE`. Unset defaults to the spec's own stated
    /// 0.25, see `storage::get_liquidity_cover_ratio`.
    pub fn set_liquidity_cover_ratio(env: Env, ratio: i128) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        storage::set_liquidity_cover_ratio(&env, ratio);
        Ok(())
    }

    /// M2 (Section 2): the per-asset `PriceGuard` client address
    /// `Series.buy_cover` checks. `None` clears it - `buy_cover` then
    /// skips the check entirely, the spec's own stated default.
    pub fn set_price_guard(env: Env, asset: Address, guard: Option<Address>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        storage::set_price_guard(&env, &asset, guard);
        Ok(())
    }

    // -- reads --

    /// Live series only (Section 12.4).
    pub fn series_for(env: Env, asset: Address) -> Vec<Address> {
        storage::get_series_list(&env, &asset)
    }

    pub fn series_count(env: Env) -> u64 {
        storage::series_count(&env)
    }

    pub fn open_cover(env: Env, asset: Address) -> i128 {
        storage::get_open_cover(&env, &asset)
    }

    /// Section 11.1: `min(hard_cap(a), k * median_liquidity(a))`.
    pub fn cover_cap(env: Env, asset: Address) -> Result<i128, Error> {
        let config = Self::require_config(&env)?;
        let median = RiskOracleClient::new(&env, &config.oracle).median_liquidity(&asset);
        let ratio = storage::get_liquidity_cover_ratio(&env);
        let liquidity_term = median
            .checked_mul(ratio)
            .and_then(|v| v.checked_div(sylox_types::SCALE))
            .ok_or(Error::MathOverflow)?;
        Ok(liquidity_term.min(storage::get_hard_cap(&env, &asset)))
    }

    /// Design note Section 10: `LiveCoverCount(asset, kind, version) > 0`.
    pub fn has_live_cover(env: Env, asset: Address, kind: EventKind, version: u32) -> bool {
        storage::get_live_cover_count(&env, &asset, kind, version) > 0
    }

    /// `None` if never set (distinct from an explicitly empty list).
    pub fn related_sellers(env: Env, asset: Address) -> Option<Vec<Address>> {
        storage::get_related_sellers(&env, &asset)
    }

    /// `None` if no `PriceGuard` is configured for this asset (M2).
    pub fn price_guard(env: Env, asset: Address) -> Option<Address> {
        storage::get_price_guard(&env, &asset)
    }

    // -- internal --

    fn require_config(env: &Env) -> Result<Config, Error> {
        storage::get_config(env).ok_or(Error::NotInitialized)
    }
}

/// `None` if `asset` is not a Stellar Asset Contract with a readable
/// admin, or has none set (an immutable issuer) - callers then fall
/// back to whatever rule applies when the admin cannot be determined
/// (design note Section 6: skip `IssuerNotListed`; `open_series`'s
/// settlement check: fail closed instead, see this module's own doc
/// comment on that substitution).
fn read_sac_admin(env: &Env, asset: &Address) -> Option<Address> {
    token::StellarAssetClient::new(env, asset)
        .try_admin()
        .ok()?
        .ok()
}

#[cfg(test)]
mod test;
