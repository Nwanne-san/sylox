//! Governor events. technical-doc.md Section 13, ADR-007: `["sylox",
//! <event_name>]` followed by the event's one `#[topic]` field.
//! `Upgraded` for the Governor's own self-upgrade is the shared
//! `sylox_types::upgrade::Upgraded`, emitted by `apply_upgrade`.

use soroban_sdk::{contractevent, Address};
use sylox_types::{Action, ActionState, PauseScope};

#[contractevent(topics = ["sylox", "action_queued"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionQueued {
    #[topic]
    pub action_id: u64,
    pub action: Action,
    pub eta: u64,
    pub expires_at: u64,
    pub state: ActionState,
}

#[contractevent(topics = ["sylox", "action_approved"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionApproved {
    #[topic]
    pub action_id: u64,
    pub action: Action,
    pub eta: u64,
    pub expires_at: u64,
    pub state: ActionState,
}

#[contractevent(topics = ["sylox", "action_executed"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionExecuted {
    #[topic]
    pub action_id: u64,
    pub action: Action,
    pub eta: u64,
    pub expires_at: u64,
    pub state: ActionState,
}

#[contractevent(topics = ["sylox", "action_cancelled"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionCancelled {
    #[topic]
    pub action_id: u64,
    pub action: Action,
    pub eta: u64,
    pub expires_at: u64,
    pub state: ActionState,
}

/// `by` is the guardian for `pause`, or the Governor's own address for
/// `Action::Pause`.
#[contractevent(topics = ["sylox", "paused"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Paused {
    #[topic]
    pub scope: PauseScope,
    pub by: Address,
}

/// `by` is always the Governor's own address: unpausing is only ever
/// `Action::Unpause`.
#[contractevent(topics = ["sylox", "unpaused"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unpaused {
    #[topic]
    pub scope: PauseScope,
    pub by: Address,
}
