//! Shared TTL-extension constants and helpers for `MarketFactory` and
//! `Series`. markets design note, Section 3: unlike the four existing
//! contracts (none of which call `extend_ttl` on their own current-state
//! storage yet, known gap, issue #28), these two extend from day one -
//! instance entries on every state-changing call, persistent entries on
//! every write plus the specific hot-path reads each contract's own
//! `lib.rs` calls this for.
//!
//! One shared implementation rather than one copy per contract, since
//! both contracts apply the identical policy (same ledger counts, same
//! threshold/extend_to shape) - the same reasoning
//! `sylox_types::treasury_deposit` and `sylox_types::upgrade` already
//! follow for their own cross-contract mechanics.

use soroban_sdk::{Env, IntoVal, Val};

/// Instance entries (`Terms`/`State`/`Totals`/`Quotes` on `Series`;
/// `Config`/`SeriesCounter` on `MarketFactory`): re-extend once
/// remaining TTL drops below 20 days.
pub const INSTANCE_TTL_THRESHOLD: u32 = 345_600;
/// ... to 30 days. Matches `deploy/extend-ttl.sh`'s own
/// `EXTEND_TARGET_LEDGERS` exactly, so that script's periodic sweep
/// becomes a redundant safety net for these two contracts rather than
/// the only thing keeping their instance entries alive.
pub const INSTANCE_TTL_EXTEND_TO: u32 = 518_400;

/// Persistent entries (`Position`, the cover token's `Balance`;
/// `OpenCover`, `Deployed`, `SeriesList`, `Pins`, `LiveCoverCount`,
/// `RelatedSellers`, `HardCap`): re-extend once remaining TTL drops
/// below 300 days.
pub const PERSISTENT_TTL_THRESHOLD: u32 = 5_184_000;
/// ... to 365 days, matching Section 15.2's own explicit "minimum 1
/// year" floor for Series positions and cover balances.
pub const PERSISTENT_TTL_EXTEND_TO: u32 = 6_307_200;

/// Extends the calling contract's own instance entry. Call at the top
/// of every state-changing function, after `require_auth`/validation
/// so a rejected call never pays for an extension it then reverts.
pub fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

/// Extends `key`'s persistent TTL if the entry exists; a no-op
/// otherwise (nothing to extend). Safe to call unconditionally after
/// every write, and from the specific hot-path reads each contract's
/// own `lib.rs` names.
pub fn extend_persistent<K>(env: &Env, key: &K)
where
    K: IntoVal<Env, Val>,
{
    let storage = env.storage().persistent();
    if storage.has(key) {
        storage.extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO);
    }
}

/// Writes `val` under persistent `key` and extends that entry with the
/// shared constants above, so no write can leave an entry to drift
/// toward archival (issue #28). The entry exists right after the write,
/// so this needs no `has` check, unlike `extend_persistent`.
pub fn set_persistent<K, V>(env: &Env, key: &K, val: &V)
where
    K: IntoVal<Env, Val>,
    V: IntoVal<Env, Val>,
{
    let storage = env.storage().persistent();
    storage.set(key, val);
    storage.extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO);
}

/// Write-once history entries (`RiskOracle`'s `Signals`, `HourPostedVia`,
/// `SubSignals`, `Overturned`, `SubOverturned`, `SubRepostUsed`): extend
/// to 10 days. That is the ring's own span (240 hourly slots), and it
/// covers the longest live need of a single dispute, the 2 hour dispute
/// window plus the 6 day ruling, with almost 4 days of margin. The
/// spec's longer case, an hour waiting on two dispute cycles (about
/// 12.3 days, or 13.2 with a late timeout, Section 5.9 S2), is covered
/// by re-extending the hour's other `SubSignals` when one is
/// overturned. Today that case is capped at about 7.2 days, since a
/// held repost can't be disputed; the re-extension is the safeguard for
/// when it can.
/// Not the 365 day persistent target: these entries are created on
/// every post, up to 288 times a day per asset, so their rent is the
/// one cost this policy must keep small. Older history lives in the
/// indexer and the inputs bundles. technical-doc.md Section 15.2.
pub const HISTORY_TTL_EXTEND_TO: u32 = 172_800;
/// ... re-extended on a later write only once less than 8 days remain.
/// After any write, the longest a history entry can still be needed is
/// a repost (2 hours) plus its dispute window (2 hours) plus a ruling
/// that times out late (up to 7 days, `SubDispute`'s own lifetime):
/// about 7 days 4 hours. 8 days left at any write covers that, and the
/// sibling re-extension on an overturn relies on it. Below the extend-to
/// value, so a rewrite soon after the first write (for example
/// `HourPostedVia` on each sub-epoch post of the same hour) pays no
/// extra rent.
pub const HISTORY_TTL_THRESHOLD: u32 = 138_240;

/// Writes a write-once history entry and extends it with the
/// `HISTORY_TTL_*` constants (10 days), not the 365 day persistent
/// target `set_persistent` uses.
pub fn set_history<K, V>(env: &Env, key: &K, val: &V)
where
    K: IntoVal<Env, Val>,
    V: IntoVal<Env, Val>,
{
    let storage = env.storage().persistent();
    storage.set(key, val);
    storage.extend_ttl(key, HISTORY_TTL_THRESHOLD, HISTORY_TTL_EXTEND_TO);
}
