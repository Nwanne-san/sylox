//! Staking error codes. technical-doc.md Section 14. The shared 1-5
//! range keeps its numbers; the 300 range is Staking's own, continued
//! from the 9 codes technical-doc.md already specified (300-308); new
//! codes for this build are appended at 309 and up.

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
    /// compatibility, the same reasoning `RiskOracle` documents for
    /// its own copy of this code.
    Unauthorized = 3,
    /// Not currently reachable: this contract has no pause integration
    /// yet (Section 16.2 lists no Staking-specific pause scope). Kept
    /// for code number compatibility.
    Paused = 4,
    MathOverflow = 5,
    NotReporter = 300,
    DuplicateProbe = 301,
    StakeTooLow = 302,
    UnstakeCooldown = 303,
    NotKeeper = 304,
    BondExists = 305,
    UnknownBond = 306,
    NothingToClaim = 307,
    /// technical-doc.md Section 14 lists this for `settle_epoch`
    /// (named `settle_probes` here; see `lib.rs`'s doc comment on
    /// that function). Never actually returned by this build:
    /// `settle_probes`'s own window checks (`SettlementNotOpen`,
    /// `SettlementWindowExpired`) supersede it, covering both "too
    /// early" and "too late" with codes that distinguish those two
    /// cases, which this single code could not. Kept for Section 14
    /// code-number compatibility, same reasoning `Unauthorized`/
    /// `Paused` above document for their own unreachability.
    EpochNotClosed = 308,
    /// `add_keeper`/`add_reporter` called for an address already
    /// registered.
    AlreadyRegistered = 309,
    /// A keeper or reporter action attempted while suspended.
    Suspended = 310,
    /// `unstake_request` called while an unstake is already pending.
    UnstakePending = 311,
    /// `unstake` called with no pending `unstake_request`.
    NoUnstakeRequested = 312,
    /// `submit_probe` for an epoch outside the current epoch or the
    /// just-closed epoch's grace period (Section 7.3).
    ProbeWindowClosed = 313,
    /// `settle_epoch` called before its settlement window opens
    /// (before `probe_grace_secs` has elapsed since the epoch closed).
    SettlementNotOpen = 314,
    /// `settle_epoch` called after its settlement window has closed
    /// (after `probe_grace_secs + settle_window_secs`). The epoch
    /// simply never settles; see `settle_epoch`'s doc comment.
    SettlementWindowExpired = 315,
    /// `settle_epoch` called twice for the same (asset, epoch).
    AlreadySettled = 316,
    /// technical-doc.md Section 14's code for a caller not registered
    /// as `oracle` or `registry` tried to call a bond or slash
    /// function reserved for them. Never actually returned by this
    /// build: `require_bond_caller`/`require_slash_caller`/
    /// `require_oracle_caller` enforce this through
    /// `Address::require_auth()` on the registered `oracle`/
    /// `registry` address directly (which traps the host call rather
    /// than returning a `Result`), not through a comparison against
    /// the caller's own identity that could instead return this code
    /// (Soroban has no portable "who actually invoked this call"
    /// read to compare against; see `require_slash_caller`'s own doc
    /// comment). Kept for Section 14 code-number compatibility, same
    /// reasoning `Unauthorized` above documents for itself.
    UnknownCaller = 317,
    /// `lock_bond`'s `subject` did not match its `BondKey` kind: a
    /// `SignalDispute` key requires `Some(keeper)`, an event bond kind
    /// requires `None` (lead decision, feat/staking).
    InvalidBondSubject = 318,
    /// Review fix S6: `add_keeper` for an address already registered
    /// as a reporter, or `add_reporter` for an address already
    /// registered as a keeper. One role per address, so `slash` (and
    /// every other keeper-or-reporter branch in this contract) always
    /// has exactly one target to act on for a given address.
    RoleConflict = 319,
    /// Review fix S7: `unstake`/`withdraw_keeper_bond` called for a
    /// keeper with `open_dispute_count > 0`. Distinct from
    /// `Suspended` (a fault/evidence outcome, a different condition):
    /// this keeper is not suspended, its funds are simply still
    /// needed as collateral for a dispute that has not resolved yet.
    DisputesOpen = 320,
    /// PR #7 approval fix: `lock_bond` called with `amount <= 0`. A
    /// zero (or negative) bond would still increment the subject
    /// keeper's `open_dispute_count` without ever locking any real
    /// collateral, leaving that counter permanently out of sync with
    /// what `release_bond`/`forfeit_bond` could ever actually settle.
    InvalidAmount = 321,
    /// Issue #12: `migrate` called with a `target_version` already
    /// applied (or an older one). Each storage-shape migration runs at
    /// most once; a second call for the same (or an earlier) version is
    /// a no-op request, not a retry of something that failed.
    AlreadyMigrated = 322,
    /// Governor handover (feat/set-governor-handover): `set_governor`
    /// called a second time. One-time lock, same shape as
    /// EventRegistry's `FactoryAlreadySet`.
    GovernorAlreadySet = 323,
}
