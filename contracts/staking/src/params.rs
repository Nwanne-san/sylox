//! Every tunable value Staking uses, in one module with an accessor per
//! value, so they can move to Governor-controlled parameters later
//! (out of scope for this PR) without touching call sites. Values
//! derived FROM other parameters are `const` expressions of them, not
//! separately hardcoded numbers, so changing one input can never
//! silently break the guarantee it was derived to satisfy; see
//! `params_test` for the assertions that prove each relationship by
//! computation, not just by reading the expression.
//!
//! technical-doc.md Section 23 lists the parameters that existed before
//! this contract; `keeper_bond`, `keeper_slash`, `keeper_max_faults`,
//! `reporter_stake`, `reporter_max_faults`, `reporter_slash_bps`,
//! `min_reporters`, `probe_secs`, `degraded_ms` all match it exactly.
//! Every other constant below is new, not yet in the spec; see the
//! PR's "Spec deviations" section for the full list with defaults and
//! derivations, to fold into the next spec update.

/// technical-doc.md Section 23.
pub const KEEPER_BOND: i128 = 50_000_000_000; // 5,000 USDC
/// technical-doc.md Section 23, "count per 30 days". `keeper_slash`
/// itself (the amount) is NOT duplicated here: `slash`'s caller
/// (`RiskOracle.resolve_signal_dispute`) already owns and passes that
/// amount; only the fault-count threshold for auto-suspension is this
/// contract's own concern.
pub const KEEPER_MAX_FAULTS: u32 = 3;
/// technical-doc.md Section 23, "USDC per accepted epoch". Used by
/// `reward_keeper` (lead decision, feat/staking: a local accrual from
/// the same reward pool `fund_rewards`/`settle_probes` share, see the
/// module and `reward_keeper`'s own doc comments). Lowered from
/// 0.50 (feat/staking's own original default) to 0.05 in spec v1.3,
/// so reward parameters stay within a sustainable operating cost at
/// the target scale (10 assets, hourly epochs).
pub const KEEPER_REWARD_PER_ACCEPTED_EPOCH: i128 = 500_000; // 0.05 USDC
/// technical-doc.md Section 23.
pub const REPORTER_STAKE: i128 = 10_000_000_000; // 1,000 USDC
/// technical-doc.md Section 23, "count per 30 days".
pub const REPORTER_MAX_FAULTS: u32 = 10;
/// technical-doc.md Section 23, bps.
pub const REPORTER_SLASH_BPS: i128 = 1_000; // 10%
/// technical-doc.md Section 23, "count, 2+ regions".
pub const MIN_REPORTERS: u32 = 3;
/// A fault only counts if the majority it disagreed with had at least
/// this many reporters (Section 7.5's "a majority of 3 or more").
pub const FAULT_MAJORITY_THRESHOLD: u32 = 3;
/// The minimum distinct regions required alongside `MIN_REPORTERS` for
/// a non `Unknown` aggregate (Section 7.4).
pub const MIN_DISTINCT_REGIONS: u32 = 2;
/// The rolling window faults are counted within (Section 7.5, "30 days").
/// 720 epochs.
pub const FAULT_WINDOW_SECS: u64 = 720 * EPOCH_SECS;

/// New (feat/staking). Cap on the per (asset, epoch) submitter index
/// `aggregate`/`settle_epoch` iterate (lead decision item 2: "cap its
/// length at the maximum reporter count and state the cap"). Section
/// 7.6 targets 5 to 9 reporters in v1; set generously above that so no
/// legitimate network of reporters could ever be rejected by this cap,
/// while still bounding the loop's cost.
pub const MAX_SUBMITTERS_PER_EPOCH: u32 = 32;

/// `RiskOracle`'s own epoch length (technical-doc.md Section 23),
/// re-exported from `sylox_types::time` rather than duplicated: Staking
/// has no epochs of its own, but needs this to compute
/// `REPORTER_EXIT_DELAY_SECS` and the probe acceptance window
/// correctly against `RiskOracle`'s epoch boundaries, and to scale
/// along with it under the `demo-time` feature.
pub use sylox_types::time::EPOCH_SECS;
/// technical-doc.md Section 23 (`RiskOracle`'s own signal dispute
/// window; needed here for `KEEPER_EXIT_DELAY_SECS`). 2 epochs.
pub const SIGNAL_DISPUTE_SECS: u64 = 2 * EPOCH_SECS;

/// New (feat/staking). How long after an epoch closes a reporter may
/// still submit a probe for it (Section 7.3's "the just-closed epoch
/// within a grace period"). Default: one epoch, matching the cadence
/// probes are expected at (`probe_secs` is far shorter, 900s, but a
/// full epoch of slack tolerates a reporter that was briefly down).
pub const PROBE_GRACE_SECS: u64 = EPOCH_SECS;

/// New (feat/staking). How long the settlement window stays open after
/// an epoch's probe grace period ends (lead decision: settlement is
/// only allowed while every probe for that epoch is GUARANTEED to
/// still exist, never on a partial set, since probes are separate
/// temporary entries with independently extendable TTLs and settling
/// on a partial set is attackable). 24 epochs.
pub const SETTLE_WINDOW_SECS: u64 = 24 * EPOCH_SECS;

/// New (feat/staking). Margin added on top of the settlement window's
/// own close when computing how long a probe's TTL must guarantee
/// (lead decision item 4: "probe TTL >= grace period + settle_window_secs
/// + 1 day"). 24 epochs.
pub const PROBE_TTL_MARGIN_SECS: u64 = 24 * EPOCH_SECS;

/// New (feat/staking). Cooldown for a keeper or reporter's OWN,
/// voluntary `unstake_request` while still registered and active (not
/// `remove_keeper`/`remove_reporter`, which use the exit delays below
/// instead). Must be at least `REPORTER_EXIT_DELAY_SECS`, asserted in
/// `params_test`, so a reporter cannot use a voluntary unstake to exit
/// faster than a governor removal would allow; stake in cooldown stays
/// slashable until actually withdrawn (lead decision item 4).
/// 168 epochs.
pub const UNSTAKE_COOLDOWN_SECS: u64 = 168 * EPOCH_SECS;

/// New (feat/staking). After `remove_keeper`, how long `Staking` must
/// wait before `withdraw_keeper_bond` can pay out: long enough that the
/// dispute window on every posting the keeper could have made (up to
/// one full epoch before removal) has definitely closed. Derived, not
/// hardcoded, from `SIGNAL_DISPUTE_SECS` and `EPOCH_SECS` (lead
/// decision item 3).
pub const KEEPER_EXIT_DELAY_SECS: u64 = SIGNAL_DISPUTE_SECS + EPOCH_SECS;

/// New (feat/staking). After `remove_reporter`, how long `Staking` must
/// wait before the reporter's stake can leave cooldown: long enough
/// that every epoch the reporter could have probed (up to one full
/// epoch before removal) can still run through its full probe grace
/// period and settlement window, so a late settlement can still fault
/// and slash a removed reporter if warranted. Derived from
/// `EPOCH_SECS`, `PROBE_GRACE_SECS` and `SETTLE_WINDOW_SECS` (lead
/// decision item 1's correction: the first draft of this value was one
/// epoch short).
pub const REPORTER_EXIT_DELAY_SECS: u64 = EPOCH_SECS + PROBE_GRACE_SECS + SETTLE_WINDOW_SECS;

/// New (feat/staking). The USDC amount accrued per settled (asset,
/// epoch), split equally among reporters who matched the aggregate
/// (lead decision item 3; a stand-in for Section 7.5's
/// `reporter_reward_pool` until `Treasury` exists). At 10 assets
/// posting every epoch this is about 8,800 USDC/year; flagged in the
/// PR against `keeper_reward`'s existing default (0.50 USDC per
/// accepted epoch, about 44,000 USDC/year at 10 assets) as something
/// the next spec update should revisit together.
pub const REPORTER_REWARD_PER_EPOCH: i128 = 1_000_000; // 0.1 USDC

/// Average Stellar ledger close time, for converting the second based
/// parameters above into the ledger counts `extend_ttl` actually takes.
/// Not itself a protocol parameter; a fixed assumption documented here
/// so every TTL computation in this crate uses the same number.
pub const SECONDS_PER_LEDGER: u64 = 5;

/// The ledger count a probe's TTL must be extended to at write time, so
/// it is guaranteed to outlive the settlement window with the stated
/// margin, regardless of when within an epoch it was actually
/// submitted. Computed from seconds, not hardcoded, and checked
/// against the live network's actual close time only approximately
/// (an assumption, not an onchain read); see `params_test` for the
/// bound in seconds this corresponds to.
pub const PROBE_TTL_LEDGERS: u32 =
    ((EPOCH_SECS + PROBE_GRACE_SECS + SETTLE_WINDOW_SECS + PROBE_TTL_MARGIN_SECS)
        / SECONDS_PER_LEDGER) as u32;

#[cfg(test)]
// These assertions are provably true at compile time, which is
// exactly the point: they are the test the task asked for ("add a
// test asserting each relationship"), not a leftover runtime check
// clippy would be right to flag if it were dead code.
#[allow(clippy::assertions_on_constants)]
mod params_test {
    use super::*;

    #[test]
    fn keeper_exit_delay_covers_every_posting_dispute_window() {
        // A keeper's most recent posting can be up to EPOCH_SECS before
        // removal (posted right before the epoch closed); its own
        // dispute window is SIGNAL_DISPUTE_SECS from its post time.
        assert_eq!(KEEPER_EXIT_DELAY_SECS, SIGNAL_DISPUTE_SECS + EPOCH_SECS);
        assert!(KEEPER_EXIT_DELAY_SECS >= SIGNAL_DISPUTE_SECS);
    }

    #[test]
    fn reporter_exit_delay_covers_every_probe_settlement_window() {
        assert_eq!(
            REPORTER_EXIT_DELAY_SECS,
            EPOCH_SECS + PROBE_GRACE_SECS + SETTLE_WINDOW_SECS
        );
        assert!(REPORTER_EXIT_DELAY_SECS >= PROBE_GRACE_SECS + SETTLE_WINDOW_SECS);
    }

    #[test]
    fn voluntary_unstake_cooldown_is_at_least_the_removal_exit_delay() {
        assert!(UNSTAKE_COOLDOWN_SECS >= REPORTER_EXIT_DELAY_SECS);
        assert!(UNSTAKE_COOLDOWN_SECS >= KEEPER_EXIT_DELAY_SECS);
    }

    #[test]
    fn probe_ttl_ledgers_covers_the_settlement_window_with_margin() {
        let required_secs =
            EPOCH_SECS + PROBE_GRACE_SECS + SETTLE_WINDOW_SECS + PROBE_TTL_MARGIN_SECS;
        let required_ledgers = (required_secs / SECONDS_PER_LEDGER) as u32;
        assert_eq!(PROBE_TTL_LEDGERS, required_ledgers);
        assert!(PROBE_TTL_LEDGERS as u64 * SECONDS_PER_LEDGER >= required_secs);
    }
}
