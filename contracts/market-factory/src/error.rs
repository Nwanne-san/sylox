//! MarketFactory error codes. technical-doc.md Section 14 (the 400
//! range is this contract's own); markets design note Sections 5, 6,
//! 11.1.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Never actually returned: every authorization check goes through
    /// Soroban's native `Address::require_auth()`, which traps the
    /// host call directly rather than returning a `Result` this
    /// contract could wrap. Kept for cross-contract error code
    /// compatibility, the same reasoning every other contract in this
    /// workspace documents for its own copy of this code.
    Unauthorized = 3,
    /// Not currently reachable: this contract has no pause
    /// integration in this build (`Governor` is a stub). Kept for code
    /// number compatibility against the day it exists.
    Paused = 4,
    MathOverflow = 5,
    /// The asset already has `max_series_per_asset` live series
    /// (Section 11.5).
    TooManySeries = 400,
    /// `reserve_cover` would push `open_cover(asset)` past `cover_cap`
    /// (Section 11.1). Checked and reserved in one call; there is no
    /// separate read-then-reserve path to race.
    CoverCapExceeded = 401,
    /// `open_series`'s own term validation failed: asset unknown,
    /// disabled or stale; `require_holding` with an `Asset` reference;
    /// bad dates, cap, claim window or fee (Section 9.7).
    InvalidTerms = 402,
    /// Settlement equals the covered asset, or shares its issuer
    /// (Section 9.7).
    InvalidSettlement = 403,
    /// A Declared event on the asset blocks new series until
    /// governance re-enables it (Section 8.8).
    AssetBlocked = 404,
    /// `def_versions` is empty, or a pinned version is not the current
    /// canonical version for (asset, kind).
    DefinitionNotCurrent = 405,
    /// Issue #12: `migrate` called again after the contract's own
    /// current `CONTRACT_VERSION` was already applied.
    AlreadyMigrated = 406,
    /// Markets design note, Section 6: `open_series` for an asset
    /// whose related-seller list has never been set.
    RelatedSellerListNotSet = 407,
    /// Markets design note, Section 6: the covered asset's own
    /// readable SAC `admin()` is not present in that asset's
    /// related-seller list.
    IssuerNotListed = 408,
    /// `release_cover` asked to release more than `OpenCover(asset)`
    /// currently holds, or to decrement a `LiveCoverCount` that is
    /// already 0 for one of this series' own pinned `(kind, version)`
    /// pairs. Either means the bookkeeping this call relies on has
    /// already drifted out of sync somewhere else (PR #67 review,
    /// I26 follow-up): silently clamping to 0 would hide exactly that
    /// drift instead of surfacing it.
    AccountingUnderflow = 409,
    /// Governor handover (feat/set-governor-handover): `set_governor`
    /// called a second time. One-time lock, same shape as
    /// EventRegistry's `FactoryAlreadySet`.
    GovernorAlreadySet = 410,
}
