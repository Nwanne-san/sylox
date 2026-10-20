//! Series error codes. technical-doc.md Section 14 (the 500 range is
//! this contract's own, 500-516 already specified there); markets
//! design note Section 2 (517, new).

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
    /// integration in this build (`Governor` is a stub). Kept for
    /// code number compatibility against the day it exists.
    Paused = 4,
    MathOverflow = 5,
    /// Action not allowed in the series' current state (Section 9.2).
    WrongSeriesState = 500,
    /// `buy_cover`: the oracle reports the asset stale.
    AssetStale = 501,
    /// `buy_cover`: band is Distress or Event, or an event is in
    /// progress (`cover_gate`'s own `EventInProgress`).
    AssetDistressed = 502,
    /// `buy_cover`: over `max_cover_per_buyer`.
    BuyerCapExceeded = 503,
    /// `buy_cover`: over the series' own `cap`.
    SeriesCapExceeded = 504,
    /// `buy_cover`: insurable interest check failed (`require_holding`).
    HoldingTooLow = 505,
    /// `buy_cover`: no quote at or below `max_rate_bps`.
    NoFill = 506,
    /// `quote`: above unencumbered collateral.
    QuoteExceedsFree = 507,
    /// `quote`: rate does not beat the worst quote in a full book.
    QuoteBookFull = 508,
    /// `withdraw`: above the withdrawable amount for the current state.
    WithdrawTooLarge = 509,
    /// `deposit`/`transfer_position`: the seller (or recipient) is on
    /// the asset's related-seller list.
    RelatedSeller = 510,
    /// `claim`/`claim_for`/`trigger`'s own failure path: not Triggered.
    NotTriggered = 511,
    /// `claim_for` called before `claim_window_secs` has elapsed since
    /// `trigger()`.
    ClaimWindowOpen = 512,
    /// `deposit` below the minimum (Section 11.5).
    BelowMinDeposit = 513,
    /// `buy_cover`: `cover_gate`'s `RecentDepeg`, `RecentEndpointOutage`,
    /// `RecentIssuerAction` or `UnbuiltBacklog`.
    RecentFailureSignals = 514,
    /// `transfer_position` to an address that already holds a position.
    RecipientHasPosition = 515,
    /// `buy_cover`: the asset's `PriceGuard` reports `Paused` or
    /// `Unavailable` (M2). Sales only; never blocks a claim or trigger.
    FastSignalPause = 516,
    /// Design note Section 2, M1: the newest posted sub-epoch is older
    /// than the last closed one, outside `feed_grace_secs`.
    FeedBehind = 517,
    /// `buy_cover`: the premium at execution is above the buyer's
    /// signed `max_premium`.
    PremiumAboveMax = 518,
}
