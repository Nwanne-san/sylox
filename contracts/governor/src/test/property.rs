//! docs/design/governor.md Section 13, property: random sequences of
//! queue, approve, execute, cancel, `SetSigners` and time jumps never
//! break G2 (an action executes at most once, never before `eta`, never
//! after `expires_at`), G3 (at execution, approvals from distinct
//! current signers reach the current threshold) or G9 (the signer set
//! has no duplicates and the threshold is within `1..=len`).

extern crate std;

use proptest::prelude::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env, Vec};
use std::collections::BTreeSet;
use sylox_types::{Action, ActionState, PauseScope};

use super::{set_time, setup, DAY};
use crate::storage::{Config, DataKey};
use crate::GRACE_SECS;

#[derive(Clone, Debug)]
enum Op {
    /// 0: SetCommittee (14 days), 1: Pause (0 days), 2: SetSigners
    /// over a random subset of the pool with a random threshold.
    Queue {
        proposer: usize,
        kind: u8,
        subset: u8,
        threshold: u32,
    },
    Approve {
        signer: usize,
        action: usize,
    },
    /// Every address that is a signer right now approves, the way
    /// signers coordinate in practice.
    Endorse {
        action: usize,
    },
    Execute {
        action: usize,
    },
    /// A bitmask over the address pool.
    Cancel {
        who: u8,
        action: usize,
    },
    Jump {
        secs: u64,
    },
}

const POOL: usize = 5;

/// Action indices stay small and approvals and executions are weighted
/// up, so most sequences actually reach executions instead of passing
/// vacuously.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => (0..POOL, 0u8..3, any::<u8>(), 0u32..4).prop_map(|(proposer, kind, subset, threshold)| {
            Op::Queue { proposer, kind, subset, threshold }
        }),
        5 => (0..POOL, 0usize..4).prop_map(|(signer, action)| Op::Approve { signer, action }),
        2 => (0usize..4).prop_map(|action| Op::Endorse { action }),
        3 => (0usize..4).prop_map(|action| Op::Execute { action }),
        1 => (any::<u8>(), 0usize..4).prop_map(|(who, action)| Op::Cancel { who, action }),
        1 => prop_oneof![Just(14 * DAY), Just(GRACE_SECS + 1), 0u64..3 * DAY]
            .prop_map(|secs| Op::Jump { secs }),
    ]
}

fn subset(env: &Env, pool: &[Address], mask: u8) -> Vec<Address> {
    let mut out = Vec::new(env);
    for (i, a) in pool.iter().enumerate() {
        if mask & (1 << i) != 0 {
            out.push_back(a.clone());
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn g2_g3_g9_hold_under_random_sequences(ops in prop::collection::vec(op(), 1..60)) {
        let env = Env::default();
        let fx = setup(&env);
        let mut pool: std::vec::Vec<Address> = fx.signers.to_vec();
        while pool.len() < POOL {
            pool.push(Address::generate(&env));
        }
        let config = || -> Config {
            env.as_contract(&fx.gov.address, || {
                env.storage().instance().get(&DataKey::Config).unwrap()
            })
        };
        let mut ids: std::vec::Vec<u64> = std::vec::Vec::new();
        // Any index picks one of the actions queued so far, so steps
        // land on real actions instead of mostly missing.
        let pick = |ids: &std::vec::Vec<u64>, i: usize| -> Option<u64> {
            if ids.is_empty() { None } else { Some(ids[i % ids.len()]) }
        };
        let mut executed: BTreeSet<u64> = BTreeSet::new();

        for op in ops {
            match op {
                Op::Queue { proposer, kind, subset: mask, threshold } => {
                    let action = match kind {
                        0 => Action::SetCommittee(Address::generate(&env)),
                        1 => Action::Pause(PauseScope::Deposits),
                        _ => Action::SetSigners(subset(&env, &pool, mask), threshold),
                    };
                    if let Ok(Ok(id)) = fx.gov.try_queue(&pool[proposer], &action) {
                        ids.push(id);
                    }
                }
                Op::Approve { signer, action } => {
                    if let Some(id) = pick(&ids, action) {
                        let _ = fx.gov.try_approve(&pool[signer], &id);
                    }
                }
                Op::Endorse { action } => {
                    if let Some(id) = pick(&ids, action) {
                        for signer in config().signers.iter() {
                            let _ = fx.gov.try_approve(&signer, &id);
                        }
                    }
                }
                Op::Execute { action } => {
                    let Some(id) = pick(&ids, action) else { continue };
                    let before = fx.gov.action(&id).unwrap();
                    let cfg = config();
                    let current = before
                        .approvals
                        .iter()
                        .filter(|a| cfg.signers.contains(a))
                        .count() as u32;
                    let now = env.ledger().timestamp();
                    if let Ok(Ok(())) = fx.gov.try_execute(&id) {
                        // G2
                        prop_assert!(executed.insert(id), "executed twice");
                        prop_assert!(now >= before.eta);
                        prop_assert!(now <= before.expires_at);
                        // G3
                        prop_assert!(current >= cfg.threshold);
                        prop_assert_eq!(fx.gov.action(&id).unwrap().state, ActionState::Executed);
                    }
                }
                Op::Cancel { who, action } => {
                    if let Some(id) = pick(&ids, action) {
                        let _ = fx.gov.try_cancel(&subset(&env, &pool, who), &id);
                    }
                }
                Op::Jump { secs } => {
                    let now = env.ledger().timestamp();
                    set_time(&env, now + secs);
                }
            }

            // G9, after every step.
            let cfg = config();
            prop_assert!(cfg.threshold >= 1 && cfg.threshold <= cfg.signers.len());
            for i in 0..cfg.signers.len() {
                for j in (i + 1)..cfg.signers.len() {
                    prop_assert_ne!(cfg.signers.get(i), cfg.signers.get(j));
                }
            }
        }
    }
}
