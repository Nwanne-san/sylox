//! EventRegistry error codes. technical-doc.md Section 14. The 200
//! range is this contract's own; existing codes keep their numbers,
//! new ones (feat/event-registry) are appended at the end.

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
    /// compatibility, the same reasoning `RiskOracle`/`Staking`/
    /// `Treasury` document for their own copies of this code.
    Unauthorized = 3,
    /// Not currently reachable: this contract has no pause
    /// integration in this build (Section 16.2 lists no
    /// EventRegistry-specific pause scope). Kept for code number
    /// compatibility.
    Paused = 4,
    /// Checked arithmetic failed: the IssuerFreeze check's own running
    /// clawback-amount and revocation-count sums over its 7 day
    /// window, and the clawback ratio's own scale-up before dividing
    /// by supply. Bounded by realistic USDC amounts in practice
    /// (each term already passed `RiskOracle.post_signals`'s own
    /// sanity bounds), but checked rather than assumed, matching
    /// `Staking`'s/`Treasury`'s own convention.
    MathOverflow = 5,
    /// No `EventDefinition` stored under the requested (asset, kind,
    /// version).
    UnknownDefinition = 200,
    /// `propose_tier1` while a non-terminal event already exists for
    /// this (asset, kind, version) (invariant E3).
    EventInProgress = 201,
    /// A Tier 1 proposal's ring-buffer check did not meet the
    /// definition: too many missing epochs, a present epoch past
    /// threshold, or a liquidity baseline below `min_liquidity`
    /// (design note Section 3).
    Tier1CheckFailed = 202,
    /// `challenge`'s bond lock in `Staking` failed. Not reachable as
    /// built: `challenge` calls `Staking.lock_bond` directly (not a
    /// `try_*` variant), so a real failure on `Staking`'s own side
    /// (for example, the challenger's USDC balance too low for the
    /// transfer) traps the call rather than returning a `Result` this
    /// contract could translate into its own error code, the same
    /// reasoning every other cross-contract call in this workspace
    /// follows. Kept for code number compatibility.
    InsufficientBond = 203,
    /// `finalize` called before `challenge_secs` has elapsed since
    /// `proposed_at`.
    ChallengeWindowOpen = 204,
    /// `challenge` called after `challenge_secs` has elapsed, or
    /// against an event no longer `Proposed`.
    ChallengeWindowClosed = 205,
    /// A function's own required state (`Proposed`, `Escalated`, ...)
    /// does not match the event's current one.
    WrongState = 206,
    /// Unreachable as built (design note review item D4): a fixed
    /// post-resolution cooldown was in the original spec design but is
    /// replaced here by a new-data test (`window_start > left_at`),
    /// which `propose_tier1` enforces itself rather than returning a
    /// distinct "still cooling down" code. Kept for code number
    /// compatibility.
    CooldownActive = 207,
    /// `register_definition`'s own validation failed: asset unknown,
    /// `reference` mismatch, a parameter unused by `kind` is non
    /// zero, or the window plus baseline does not fit the ring buffer.
    InvalidDefinition = 208,
    /// An IssuerFreeze definition for an asset whose `issuer_flags`
    /// allow neither revocation nor clawback.
    FreezeImpossible = 209,
    /// A live series still pins the version `register_definition`
    /// would supersede. Unreachable in this build (no `MarketFactory`
    /// to report a live series at all, so this check never finds one
    /// to object to); kept for code number compatibility and for the
    /// day `MarketFactory` exists.
    DefinitionInUse = 210,
    /// `rule` called after the ruling deadline; use `resolve_timeout`.
    RulingDeadlinePassed = 211,
    /// `resolve_timeout` called before the ruling deadline.
    RulingDeadlineNotReached = 212,
    /// New (feat/event-registry, design note review item D2/R2/R3):
    /// `finalize` on a Depeg proposal whose cure window still has an
    /// epoch that is neither effectively Final nor permanently
    /// missing (still `Pending` before its own `pending_until`, or
    /// `Disputed` and unresolved, or `Empty` but still inside its own
    /// backfill window). Not a failure: callable again once the data
    /// settles one way or the other.
    DataNotFinal = 213,
    /// New (feat/event-registry, design note review item D3): `version`
    /// is registered but not acceptable: neither the current canonical
    /// version for (asset, kind) nor pinned by any live series (the
    /// latter check is a stand-in returning `false` until
    /// `MarketFactory` exists, Section 2 of the design note).
    VersionNotCovered = 214,
    /// New (feat/event-registry): `challenge`, `finalize`, `rule` or
    /// `resolve_timeout` against an `event_id` with no stored
    /// `EventRecord`. Kept distinct from `UnknownDefinition` (no
    /// canonical definition, or no such version), since these are
    /// different questions: one is about a DEFINITION, the other
    /// about a specific PROPOSAL.
    UnknownEvent = 215,
    /// Issue #12: `migrate` called with a `target_version` already
    /// applied (or an older one). Each storage-shape migration runs at
    /// most once; a second call for the same (or an earlier) version is
    /// a no-op request, not a retry of something that failed.
    AlreadyMigrated = 216,
    /// markets design note Section 10, deviation 14: `set_factory`
    /// called a second time. The real `MarketFactory` address is
    /// written once, then permanently locked.
    FactoryAlreadySet = 217,
    /// Governor handover (feat/set-governor-handover): `set_governor`
    /// called a second time. Same one-time-lock shape as
    /// `FactoryAlreadySet` above.
    GovernorAlreadySet = 218,
}
