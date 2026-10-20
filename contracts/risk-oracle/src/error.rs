//! RiskOracle error codes. technical-doc.md Section 14. v1.0 codes keep
//! their numbers; new codes are appended at the end of the 100 range.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Never actually returned by any call in this contract: every
    /// authorization check (`add_asset`, `update_asset`, `set_formula`,
    /// `post_signals`, `dispute_signals`, `resolve_signal_dispute`,
    /// `set_event_band`, `clear_event_band`, `set_event_in_progress`)
    /// goes through Soroban's native `Address::require_auth()`, which
    /// traps the host call directly rather than returning a `Result`
    /// this contract could wrap in `Error::Unauthorized`. Kept for
    /// `technical-doc.md` Section 14 code-number compatibility; see
    /// `missing_auth_traps_natively_rather_than_returning_unauthorized`
    /// in test.rs and the PR's "Review fixes" section for why this is
    /// documented as unreachable rather than tested as reachable.
    Unauthorized = 3,
    Paused = 4,
    MathOverflow = 5,
    UnknownAsset = 100,
    KeeperNotActive = 101,
    WrongEpoch = 102,
    EpochAlreadyPosted = 103,
    SanityBoundFailed = 104,
    AmmCrossCheckFailed = 105,
    DisputeWindowClosed = 106,
    WeightsInvalid = 107,
    ReferenceImmutable = 108,
    ReferenceRateUnavailable = 109,
    /// Distinct from `SanityBoundFailed` (104), which is about one posted
    /// `SignalSet`'s fields; this is about computing the score from the
    /// ring (not enough history, or a required window read came back
    /// empty), a different failure mode a caller may want to handle
    /// differently (for example: retry later vs. a permanently bad
    /// posting). Review item "Aggregation failures must not reuse
    /// SanityBoundFailed."
    AggregationFailed = 110,
    /// `add_asset` / `update_asset` reject `Reference::Asset` in v1
    /// (review decision D3): no USD rate is defined anywhere in the spec
    /// for an asset pegged reference (see the PR's "Spec deviations").
    ReferenceNotSupported = 111,
    /// ADR-010 (feat/staking, issue #4 fix): `resolve_signal_dispute_timeout`
    /// called before `SIGNAL_DISPUTE_RULING_SECS` has passed since the
    /// dispute opened. Named to match ADR-002's `RulingDeadlineNotReached`
    /// precedent for the analogous event-ruling timeout.
    RulingDeadlineNotReached = 112,
    /// technical-doc.md Section 5.9 S1: `sub_epoch_secs` set to a value
    /// outside the allowed set, or one that does not divide 3,600
    /// evenly.
    InvalidSubEpochInterval = 113,
    /// technical-doc.md Section 5.9 S4: `build_hour` called while at
    /// least one of the hour's sub-epochs is still Pending or Disputed,
    /// i.e. not yet Final, permanently missing, or rejected.
    SubEpochNotReady = 114,
    /// technical-doc.md Section 5.9 S2: an hour is posted through
    /// exactly one path, sub-epoch or hourly fallback, never both. A
    /// sub-epoch post against an hour already posted through the
    /// fallback path, or a fallback post against an hour that already
    /// has a sub-epoch posted, both return this.
    HourAlreadyPosted = 115,
    /// Issue #12: `migrate` called with a `target_version` already
    /// applied (or an older one). Each storage-shape migration runs at
    /// most once; a second call for the same (or an earlier) version is
    /// a no-op request, not a retry of something that failed.
    AlreadyMigrated = 116,
    /// technical-doc.md Section 5.9 S1: `set_sub_epoch_secs` called
    /// fewer than 240 hours (10 days) after the previous change took
    /// effect. It only limits how often the length changes; each hour is
    /// resolved from the length recorded at its first post
    /// (`HourSubEpochSecs`), which is what keeps old hours correct.
    SubEpochChangeTooSoon = 117,
    /// docs/design/held-repost-disputes.md D2: `resolve_signal_dispute`
    /// or `resolve_sub_signal_dispute` called at or after
    /// `opened_at + SIGNAL_DISPUTE_RULING_SECS`. From that instant only
    /// the timeout can close the dispute.
    RulingDeadlinePassed = 118,
    /// Governor handover (feat/set-governor-handover): `set_governor`
    /// called a second time. One-time lock, same shape as
    /// EventRegistry's `FactoryAlreadySet`.
    GovernorAlreadySet = 119,
}
