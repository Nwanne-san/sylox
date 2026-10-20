//! Treasury error codes. technical-doc.md Section 14. The shared 1-5
//! range keeps its numbers; the 700 range is Treasury's own, continued
//! from the 3 codes technical-doc.md already specified (700-702).

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
    /// compatibility, the same reasoning `RiskOracle`/`Staking`
    /// document for their own copies of this code.
    Unauthorized = 3,
    /// Not currently reachable: this contract has no pause
    /// integration (Section 16.2 lists no Treasury-specific pause
    /// scope). Kept for code number compatibility.
    Paused = 4,
    MathOverflow = 5,
    /// `allocate` or `spend` for more than the bucket's own balance.
    InsufficientBucket = 700,
    /// `accrue_reward` against a bucket other than `KeeperRewards` or
    /// `ReporterRewards`.
    WrongBucket = 701,
    /// `claim_reward` with nothing accrued.
    NothingToClaim = 702,
    /// `deposit`, `accrue_reward`, `allocate` or `spend` called with a
    /// non-positive amount.
    InvalidAmount = 703,
    /// Issue #12: `migrate` called with a `target_version` already
    /// applied (or an older one). Each storage-shape migration runs at
    /// most once; a second call for the same (or an earlier) version is
    /// a no-op request, not a retry of something that failed.
    AlreadyMigrated = 704,
    /// Governor handover (feat/set-governor-handover): `set_governor`
    /// called a second time. One-time lock, same shape as
    /// EventRegistry's `FactoryAlreadySet`.
    GovernorAlreadySet = 705,
}
