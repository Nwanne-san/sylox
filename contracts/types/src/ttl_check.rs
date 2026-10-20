//! Test-only TTL assertions for issue #28 (feature `testutils`, never in
//! a contract build). Diff a ledger snapshot taken before a call with
//! one taken after: every persistent entry of `contract` that the call
//! created or changed must have at least `PERSISTENT_TTL_EXTEND_TO`
//! ledgers left (`HISTORY_TTL_EXTEND_TO` for the write-once history
//! keys in `HISTORY_VARIANTS`), and the contract's instance at least
//! `INSTANCE_TTL_EXTEND_TO`. This checks every entry actually written,
//! not a hand-picked list that could miss one.
//!
//! Temporary entries manage their own lifetime and are skipped, as are
//! keys whose enum variant name is in `own_policy` (entries with their
//! own spec-sized lifetime, such as `RiskOracle`'s `Sub`, `SubDispute`
//! and `HeldHour`, Section 5.9 S3).

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec as StdVec;

use soroban_sdk::xdr::{
    ContractDataDurability, LedgerEntryData, LedgerKey, Limits, ScAddress, ScVal, WriteXdr,
};
use soroban_sdk::{Address, Env};

use crate::ttl::{HISTORY_TTL_EXTEND_TO, INSTANCE_TTL_EXTEND_TO, PERSISTENT_TTL_EXTEND_TO};

/// Key variants written through `ttl::set_history`, held to
/// `HISTORY_TTL_EXTEND_TO` instead of `PERSISTENT_TTL_EXTEND_TO`. Mirrors
/// `RiskOracle`'s own `set_history` call sites; no other contract writes
/// a key with these names.
pub const HISTORY_VARIANTS: &[&str] = &[
    "Signals",
    "HourPostedVia",
    "HourSubEpochSecs",
    "SubSignals",
    "Overturned",
    "SubOverturned",
    "SubRepostUsed",
];

/// One contract's entries: key XDR -> (entry data XDR, live_until,
/// durability, is the instance, enum variant name if any).
#[derive(Clone)]
pub struct Entries(BTreeMap<StdVec<u8>, Entry>);

#[derive(Clone)]
struct Entry {
    data: StdVec<u8>,
    live_until: u32,
    durability: ContractDataDurability,
    instance: bool,
    variant: Option<std::string::String>,
}

fn variant_name(key: &ScVal) -> Option<std::string::String> {
    match key {
        ScVal::Vec(Some(items)) => match items.first() {
            Some(ScVal::Symbol(s)) => Some(s.to_utf8_string_lossy()),
            _ => None,
        },
        ScVal::Symbol(s) => Some(s.to_utf8_string_lossy()),
        _ => None,
    }
}

/// Every contract-data entry `contract` owns right now.
pub fn entries(env: &Env, contract: &Address) -> Entries {
    let target: ScAddress = contract.into();
    let mut out = BTreeMap::new();
    for (key, (entry, live_until)) in env.to_ledger_snapshot().ledger_entries {
        let LedgerKey::ContractData(k) = key.as_ref() else {
            continue;
        };
        if k.contract != target {
            continue;
        }
        let LedgerEntryData::ContractData(d) = &entry.data else {
            continue;
        };
        out.insert(
            key.to_xdr(Limits::none()).unwrap(),
            Entry {
                data: d.val.to_xdr(Limits::none()).unwrap(),
                live_until: live_until.unwrap_or(0),
                durability: k.durability,
                instance: matches!(k.key, ScVal::LedgerKeyContractInstance),
                variant: variant_name(&k.key),
            },
        );
    }
    Entries(out)
}

/// Asserts the issue #28 policy for the call made between `before` and
/// now. Returns how many persistent entries were checked, so a caller
/// can confirm the call wrote what it was expected to.
pub fn assert_extended(
    env: &Env,
    contract: &Address,
    before: &Entries,
    own_policy: &[&str],
    label: &str,
) -> usize {
    let seq = env.ledger().sequence();
    let after = entries(env, contract);
    let mut checked = 0;
    let mut saw_instance = false;
    for (key, e) in after.0.iter() {
        let ttl = e.live_until.saturating_sub(seq);
        if e.instance {
            saw_instance = true;
            assert!(
                ttl >= INSTANCE_TTL_EXTEND_TO,
                "{label}: instance TTL {ttl} < {INSTANCE_TTL_EXTEND_TO}"
            );
            continue;
        }
        if e.durability != ContractDataDurability::Persistent {
            continue;
        }
        if let Some(v) = &e.variant {
            if own_policy.contains(&v.as_str()) {
                continue;
            }
        }
        let written = match before.0.get(key) {
            None => true,
            Some(old) => old.data != e.data,
        };
        if written {
            checked += 1;
            let history = e
                .variant
                .as_deref()
                .is_some_and(|v| HISTORY_VARIANTS.contains(&v));
            let want = if history {
                HISTORY_TTL_EXTEND_TO
            } else {
                PERSISTENT_TTL_EXTEND_TO
            };
            assert!(
                ttl >= want,
                "{label}: persistent {:?} written with TTL {ttl} < {want}",
                e.variant
            );
        }
    }
    assert!(saw_instance, "{label}: no instance entry for the contract");
    checked
}

/// Moves the ledger forward far enough that the instance and every
/// persistent entry extended so far are below their re-extend
/// thresholds, so the next call must extend whatever it touches for the
/// assertions above to pass. The test host does not archive expired
/// entries, so nothing else in the test stops working.
pub fn age_past_thresholds(env: &Env) {
    use soroban_sdk::testutils::Ledger as _;
    let seq = env.ledger().sequence();
    env.ledger().set_sequence_number(seq + 1_200_000);
}
