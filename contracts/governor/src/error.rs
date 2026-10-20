//! Governor error codes. technical-doc.md Section 14; docs/design/
//! governor.md Section 11. The shared 1-5 range keeps its numbers;
//! 600-603 are the spec's own Governor codes, 604-609 are appended per
//! Section 14's rule for new codes (D22).

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// Kept for code number compatibility: the Governor uses a
    /// `__constructor`, so there is no second initialization to reject.
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Never actually returned: every authorization check goes through
    /// `Address::require_auth()`, which traps rather than returning a
    /// `Result`, the same as every other contract here.
    Unauthorized = 3,
    /// Kept for code number compatibility: the Governor stores pause
    /// switches but is never itself paused.
    Paused = 4,
    MathOverflow = 5,
    /// Not a multisig signer.
    NotSigner = 600,
    /// `execute` before the action's `eta`.
    TimelockActive = 601,
    /// Not enough approvals from current signers, or not enough current
    /// signers listed in a `cancel`.
    ThresholdNotMet = 602,
    /// `expires_at` passed.
    ActionExpired = 603,
    /// No action with that id.
    UnknownAction = 604,
    /// The action is not in a state that allows this call.
    WrongState = 605,
    /// This signer already approved this action.
    AlreadyApproved = 606,
    /// Empty signer set, a duplicate signer, or a threshold outside
    /// `1..=signers.len()`.
    InvalidSigners = 607,
    /// Rejected at queue time: a `SetParam` key other than
    /// `liquidity_cover_ratio`, or an `Upgrade` of an address that is
    /// neither a target nor the Governor itself.
    InvalidAction = 608,
    /// The guardian called `pause` on a scope that is already paused.
    AlreadyPaused = 609,
}
