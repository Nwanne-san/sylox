#![no_std]

//! RiskOracle: stores per asset signals per epoch, computes the score,
//! exposes bands and staleness. technical-doc.md Section 5, 6.
//!
//! Scope: everything in Section 12.1 except the parts that need
//! `EventRegistry`'s cover gate / event-in-progress state, which
//! `RiskOracle` has no API path to read (see "Known gaps" in the PR
//! description). `Staking`, the `PriceAdapter`/`FxAdapter` adapters and
//! `Governor`'s committee lookup are called through minimal client
//! interfaces (`clients.rs`); none of those contracts are implemented
//! here.

mod clients;
mod error;
mod events;
mod math;
mod score;
mod storage;

use soroban_sdk::{
    contract, contractimpl, symbol_short, Address, Bytes, BytesN, Env, Map, Symbol, Vec,
};
use sylox_types::time::EPOCH_SECS;
use sylox_types::{
    AssetConfig, Band, EndpointStatus, HistoryStatus, RingSlot, RiskScore, SignalSet, SCALE,
};

use clients::{FxAdapterClient, GovernorClient, PriceAdapterClient, StakingClient};
pub use error::Error;
use score::{Formula, AGGREGATE_SLOTS_7D};
use storage::DisputeRecord;

/// Signal posting window: how far back a closed epoch may still be posted.
/// technical-doc.md Section 5.2, Section 23 `window_secs` default: 72
/// epochs. Written as a multiple of `EPOCH_SECS`, not its own literal,
/// so the `demo-time` feature (`sylox_types::time`) scales it along
/// with everything else built the same way, never needing its own gate.
const WINDOW_SECS: u64 = 72 * EPOCH_SECS;

/// Default signal dispute window. technical-doc.md Section 23
/// `signal_dispute_secs`: 2 epochs.
const SIGNAL_DISPUTE_SECS: u64 = 2 * EPOCH_SECS;

/// ADR-010 amendment: how long the committee has to rule on an open
/// signal dispute, from `dispute_signals`, before
/// `resolve_signal_dispute_timeout` becomes
/// callable. Was 7 days, matching ADR-002's `ruling_deadline_secs`
/// precedent for event disputes; lowered to 6 days because the full
/// worst-case timeline (`WINDOW_SECS + SIGNAL_DISPUTE_SECS +
/// SIGNAL_DISPUTE_RULING_SECS`, a late post disputed right before
/// `pending_until` and never ruled on) must fit inside the ring's own
/// capacity (`RING_SLOTS * EPOCH_SECS`, 240h) with room for the epoch
/// that triggers the overwrite to itself close — see the const
/// assertion below. At 7 days the worst case was 242h, already past
/// the 240h ring: a keeper's own still-open dispute could be silently
/// overwritten by the ring wrapping around before anyone ruled.
/// 144 epochs.
const SIGNAL_DISPUTE_RULING_SECS: u64 = 144 * EPOCH_SECS;

/// ADR-010 amendment: the worst-case dispute timeline (posted at the
/// last legal instant, disputed immediately, never ruled on) must
/// resolve (time out) before the ring wraps around
/// and `write_ring_slot` overwrites the still-open dispute. `+
/// EPOCH_SECS` covers the one additional epoch that closes, and so
/// becomes postable, in the time it takes `resolve_signal_dispute_timeout`
/// to actually run at the deadline instant. Any future change to
/// `WINDOW_SECS`, `SIGNAL_DISPUTE_SECS`, `SIGNAL_DISPUTE_RULING_SECS`,
/// `EPOCH_SECS` or `storage::RING_SLOTS` that breaks this fails the
/// build rather than silently reintroducing the bug.
const _: () = assert!(
    WINDOW_SECS + SIGNAL_DISPUTE_SECS + SIGNAL_DISPUTE_RULING_SECS + EPOCH_SECS
        <= storage::RING_SLOTS as u64 * EPOCH_SECS
);

/// technical-doc.md Section 23 `stale_after_epochs` default. Gates
/// `is_stale(asset)` only (`Series`/`MarketFactory` read this through
/// `is_stale` to block new cover/series), judged against the newest
/// *posted* epoch (`storage::get_newest_epoch_pub`). Section 8.6's
/// depeg coverage logic depends on this exact bound staying on
/// the newest posted epoch, not the newest Final one: "the open epoch
/// plus any keeper backlog, which the staleness rule in Section 5.5
/// caps below `stale_after_epochs`" is the caveat that keeps a depeg
/// window from starting before a purchase, and it is unaffected by
/// `SCORE_STALE_AFTER_EPOCHS` below, which governs a different signal.
const STALE_AFTER_EPOCHS: u64 = 3;

/// technical-doc.md Section 23 `score_stale_after_epochs` default.
/// Gates `score(asset).stale` and `check_stale`/`check_stale_internal`
/// only, judged against a Final epoch (`newest_final`, or the stored
/// score's own epoch): the signal Section 5.5 calls "is this asset's
/// risk data current," not the depeg-coverage bound above. Sized from
/// the real structural lag a sub-epoch-built hour can carry between
/// its own close and the next epoch becoming eligible to score from:
/// one epoch for the hour's own close (an hour isn't even a build
/// candidate until the epoch after it closes), plus
/// `SIGNAL_DISPUTE_SECS` (2 epochs at the default) for its last
/// sub-epoch's own dispute window, plus one epoch for the live
/// poster's own cron interval (`SUB_EPOCH_GRID_SECS`, 5 minutes,
/// rounded up to a full epoch) before a build is even attempted, plus
/// one epoch of slack: 1 + 2 + 1 + 1 = 5. Confirmed against this
/// revision's own observed testnet builds (deploy/post-demo-signals.sh's
/// live poster, redeployed with PR #97): every hour that built took
/// well under 2 hours from first becoming a build candidate to
/// landing Final, so 5 leaves comfortable margin above the measured
/// worst case while still being a real, derived bound rather than an
/// arbitrarily large one. `STALE_AFTER_EPOCHS` above stayed at 3
/// because a sub-epoch-built hour's own finality lag was never part
/// of what it was sized for in the first place (it predates v1.5's
/// sub-epoch posting path entirely) and `is_stale`'s depeg-coverage
/// role must not loosen.
const SCORE_STALE_AFTER_EPOCHS: u64 = 5;

/// Build-time check (normal build only): the arithmetic in
/// `SCORE_STALE_AFTER_EPOCHS`'s own doc comment (1 epoch for the
/// hour's own close, plus `SIGNAL_DISPUTE_SECS` in epochs for the
/// last sub-epoch's dispute window, plus 1 epoch for the poster's own
/// cron interval rounded up, plus 1 epoch of slack) stays true to its
/// real components if any of them ever changes, rather than silently
/// drifting out of sync with the numbers it was derived from.
///
/// This does NOT prove the poster-margin term is safe; it only checks
/// the formula's own internal arithmetic. The "+1 epoch" term assumes
/// the poster's own round trip (about 10 to 12 seconds, measured on
/// testnet) is a small fraction of `EPOCH_SECS`, which no assertion
/// here checks. In the real build that is 12 of 3,600 seconds. In
/// demo-time (`EPOCH_SECS` and `SUB_EPOCH_GRID_SECS` both 60s, so one
/// sub-epoch per hour) it is about 12 of 60 seconds: a smaller margin,
/// still positive, but not the same comfortable headroom the real
/// build has. Demo-time exists only to play the scenario on a fast
/// clock (see `sylox_types::time`'s own doc comment), not to keep
/// `SCORE_STALE_AFTER_EPOCHS` correctly tuned at that scale, so this
/// assertion is scoped to the normal build only rather than claiming
/// a promise demo-time does not actually need or get.
#[cfg(not(feature = "demo-time"))]
const _: () = assert!(SCORE_STALE_AFTER_EPOCHS == 1 + (SIGNAL_DISPUTE_SECS / EPOCH_SECS) + 1 + 1);

/// technical-doc.md Section 23 `band_down_epochs` default.
const BAND_DOWN_EPOCHS: u32 = 3;

/// Review item C6 (re-review): every state-changing call's finality
/// scan (`try_advance_finality`) looks back across the full backfill
/// window, `WINDOW_SECS / EPOCH_SECS + 1` epochs (73 at the defaults),
/// never more than `RING_SLOTS` (nothing older could still be
/// physically present). This is a fixed bound per call, not a
/// sequential cursor that only advances one epoch per call: a single
/// call can now observe and announce every epoch across the whole
/// window that just became final, matching the re-review's "scan
/// back... bounded by the ring" instruction.
const FINALITY_LOOKBACK_EPOCHS: u32 = (WINDOW_SECS / EPOCH_SECS) as u32 + 1;

/// technical-doc.md Section 23 `amm_tolerance_bps` default.
const AMM_TOLERANCE_BPS: i128 = 300;

/// Median liquidity window for `median_liquidity` (Section 11.1) and the
/// score's 7 day aggregates (Section 6.5): the newest 168 of the ring's
/// 240 slots.
const MEDIAN_WINDOW_SLOTS: u32 = 168;

/// Section 11.3's `liquidity_2pct`, `supply` upper bound.
const MAX_LIQUIDITY_OR_SUPPLY: i128 = i128::MAX / SCALE;

/// Section 11.3's `supply_change_bps` consistency tolerance.
const SUPPLY_CHANGE_TOLERANCE_BPS: i128 = 1;

// -- sub-epochs (technical-doc.md Section 5.9, v1.5) --

/// Default `sub_epoch_secs` for an asset that has never called
/// `set_sub_epoch_secs`: no `SubEpochConfig(asset)` entry exists, and
/// `post_sub_signals` has nothing to post against, so the asset
/// behaves exactly as it did before this revision (hourly `post_signals`
/// only). This constant is only ever used to compute `EPOCH_SECS /
/// SUB_EPOCH_SECS_DEFAULT` sizing and validation; it is never written
/// as an implicit `SubEpochConfig` default (see `current_sub_epoch_secs`).
/// The same value as `sylox_types::time::SUB_EPOCH_GRID_SECS`, by
/// definition - this is that same 5 minute floor, just under its own
/// name here for the doc comment above to read naturally.
const SUB_EPOCH_SECS_DEFAULT: u64 = sylox_types::time::SUB_EPOCH_GRID_SECS;

/// Section 5.9 S1: the least time, in hours, between a
/// `sub_epoch_secs` change taking effect and the next change: 240 hours
/// (10 days), the ring's own span (`storage::RING_SLOTS` hourly slots).
/// It only limits how often the length changes. What keeps an old hour
/// correct is the per-hour record (`HourSubEpochSecs`), since an unbuilt
/// hour can be built at any age; the spacing keeps the config resolver
/// exact for every recent hour, which the gate relies on.
const SUB_EPOCH_CHANGE_SPACING_HOURS: u64 = storage::RING_SLOTS as u64;

/// Section 5.9 S1: the only values `set_sub_epoch_secs` accepts, each
/// a whole multiple of `SUB_EPOCH_GRID_SECS` and each dividing
/// `EPOCH_SECS` evenly (1, 2, 3, 4, 6 and 12 times the grid - at the
/// defaults, 300/600/900/1,200/1,800/3,600).
const SUB_EPOCH_SECS_ALLOWED: [u64; 6] = [
    sylox_types::time::SUB_EPOCH_GRID_SECS,
    2 * sylox_types::time::SUB_EPOCH_GRID_SECS,
    3 * sylox_types::time::SUB_EPOCH_GRID_SECS,
    4 * sylox_types::time::SUB_EPOCH_GRID_SECS,
    6 * sylox_types::time::SUB_EPOCH_GRID_SECS,
    12 * sylox_types::time::SUB_EPOCH_GRID_SECS,
];

/// Section 5.9 S2: how long a sub-epoch can be backfilled before the
/// keeper must fall back to the hourly path. 2 epochs.
const SUB_BACKFILL_SECS: u64 = 2 * EPOCH_SECS;

/// Section 5.9 S4: the fraction of an hour's sub-epochs that must be
/// Final for the hour to count as present once built, rather than
/// Empty.
const MIN_SUB_COVERAGE_BPS: u32 = 7_500;

/// `Sub(asset)`'s own ring span, in seconds: `SUB_RING_SLOTS *
/// SUB_EPOCH_GRID_SECS`, fixed regardless of `sub_epoch_secs` (Section
/// 5.9 S3). Used only by the const assertion below; every other call
/// site reasons in terms of `sylox_types::time`'s own constants
/// directly.
const SUB_RING_SPAN_SECS: u64 =
    sylox_types::time::SUB_RING_SLOTS as u64 * sylox_types::time::SUB_EPOCH_GRID_SECS;

/// The worst case `Sub(asset)`'s fixed ring must outlive: a sub-epoch
/// backfilled right at the edge of `SUB_BACKFILL_SECS`, disputed
/// immediately, and never ruled on until `SIGNAL_DISPUTE_RULING_SECS`
/// later, plus one more hour of margin for the scan/resolution that
/// finally closes it out to actually run (the same reasoning
/// `risk-oracle/src/lib.rs`'s own hourly dispute-timeline assertion
/// uses for `Ring(asset)`). Mirrors that assertion's own shape, for
/// `Sub(asset)` instead.
const _: () = assert!(SUB_BACKFILL_SECS + SIGNAL_DISPUTE_SECS + EPOCH_SECS <= SUB_RING_SPAN_SECS);

/// How long `SubDispute(asset, hour, sub)` must survive once opened:
/// long enough to outlive `SIGNAL_DISPUTE_RULING_SECS`, the worst
/// case before a ruling or a timeout clears it, with the same 1 day
/// margin `Staking::params::PROBE_TTL_MARGIN_SECS` uses for its own
/// TTL computations. `SECONDS_PER_LEDGER` mirrors
/// `Staking::params::SECONDS_PER_LEDGER` (5s), the same fixed
/// assumption every TTL computation in this workspace uses.
const SECONDS_PER_LEDGER: u64 = 5;
/// 24 epochs (1 day at the defaults).
const SUB_DISPUTE_TTL_MARGIN_SECS: u64 = 24 * EPOCH_SECS;
const SUB_DISPUTE_TTL_LEDGERS: u32 =
    ((SIGNAL_DISPUTE_RULING_SECS + SUB_DISPUTE_TTL_MARGIN_SECS) / SECONDS_PER_LEDGER) as u32;
/// docs/design/held-repost-disputes.md D3: `HeldHour(asset, hour)`
/// outlives the hour's last open `SubDispute` by one day. Every dispute
/// in the hour writes its own `HeldHour` entry, so a timeout at the last
/// ledger of a `SubDispute` still finds the held hour live and can build
/// it in the same call.
const HELD_HOUR_TTL_LEDGERS: u32 = SUB_DISPUTE_TTL_LEDGERS + (86_400 / SECONDS_PER_LEDGER) as u32;

#[contract]
pub struct RiskOracle;

#[contractimpl]
impl RiskOracle {
    /// Issue #21: a constructor, not a separate `initialize` call. The
    /// old two-step "deploy, then invoke initialize" flow left a real
    /// window on-chain where this contract existed with no config at
    /// all, and `initialize` had no way to tell a legitimate deployer's
    /// call from a front-runner's (both are unauthenticated against a
    /// not-yet-configured contract - there is no governor to check
    /// `require_auth` against yet). A constructor runs atomically with
    /// deployment itself: the contract is never live without this
    /// config already set, so there is no window to front-run at all.
    /// `registry`/`staking` are the OTHER contracts' own addresses,
    /// computed in advance from a deterministic salt (deploy/
    /// deploy.sh's own "Constructor flow" section) - they do not need
    /// to exist yet when this constructor runs, since it only stores
    /// them, never calls them (see this constructor's own doc comment
    /// below for why).
    pub fn __constructor(env: Env, governor: Address, registry: Address, staking: Address) {
        sylox_types::ttl::extend_instance(&env);
        env.storage().instance().set(
            &CONFIG_KEY,
            &Config {
                governor,
                registry,
                staking,
            },
        );
        storage::set_formula(&env, &score::default_formula(&env));
    }

    /// Issue #12. The currently deployed code's own version, bumped by
    /// hand alongside each `upgrade()` this contract ships (see deploy/
    /// README.md's upgrade procedure). Lets anyone confirm what is
    /// actually deployed without guessing from a wasm hash.
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// Whether this exact binary was built with the `demo-time`
    /// feature (`sylox_types::time`'s own doc comment): `true` only
    /// for a demo deployment's own wasm, `false` for every real one,
    /// a pure `cfg!` check of what was actually compiled in, not a
    /// stored flag anything could set after the fact. CI's own
    /// `demo-time-not-in-release` job builds the normal release wasm
    /// (no `--features`, same command `deploy/deploy.sh testnet`
    /// uses) and calls this against it, failing the build if it ever
    /// reads anything but `false` - the one thing standing between a
    /// demo's compressed windows and a real deployment.
    pub fn demo_build(_env: Env) -> bool {
        cfg!(feature = "demo-time")
    }

    /// technical-doc.md Section 17.3: "Core contracts expose
    /// upgrade(wasm_hash), callable only by the governor." Does NOT
    /// carry the 14 day timelock Section 17.2 assigns to `Upgrade`
    /// actions - that belongs to `Governor` (Section 17), which is not
    /// built yet (known gap, see deploy/README.md). This is the same
    /// direct, immediate governor-gated call every other call here
    /// already uses.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        sylox_types::upgrade::do_upgrade(&env, &config.governor, CONTRACT_VERSION, new_wasm_hash);
        Ok(())
    }

    /// Issue #12. Storage migration hook for a future `upgrade()`: runs
    /// at most once per `CONTRACT_VERSION`. Takes no argument - always
    /// migrates to this code's OWN current version, never a
    /// caller-chosen one (a governor could otherwise mark a future
    /// version "already migrated" before the code ever reaches it,
    /// permanently blocking that migration). Empty for now - no storage
    /// shape change ships with this PR - but every future upgrade that
    /// DOES change a stored type's shape migrates existing data here,
    /// gated the same way, with its own storage test (existing keys
    /// decode under the new types) alongside it.
    pub fn migrate(env: Env) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        if !sylox_types::upgrade::do_migrate(
            &env,
            &config.governor,
            &MIGRATED_TO_KEY,
            CONTRACT_VERSION,
        ) {
            return Err(Error::AlreadyMigrated);
        }
        Ok(())
    }

    /// One-time governor handover, for the Governor deployment cutover:
    /// callable only by the CURRENT governor, exactly once. Same
    /// one-time-lock pattern as `EventRegistry::set_factory` (a storage
    /// flag checked before the write, set after it), so a second call
    /// always fails rather than letting the governor be reassigned
    /// again later through this same path.
    pub fn set_governor(env: Env, new_admin: Address) -> Result<(), Error> {
        let mut config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        if env.storage().instance().has(&GOVERNOR_SET_KEY) {
            return Err(Error::GovernorAlreadySet);
        }
        config.governor = new_admin;
        env.storage().instance().set(&CONFIG_KEY, &config);
        env.storage().instance().set(&GOVERNOR_SET_KEY, &true);
        Ok(())
    }

    /// technical-doc.md Section 12.1. Rejects an asset that is already
    /// registered (`update_asset` is for changing one). Review decision
    /// D3: also rejects `Reference::Asset` (no USD rate is defined
    /// anywhere in the spec for an asset pegged reference; see the PR's
    /// "Spec deviations").
    pub fn add_asset(env: Env, cfg: AssetConfig) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        reject_asset_reference(&cfg.reference)?;
        if storage::get_asset_config(&env, &cfg.asset).is_some() {
            return Err(Error::AlreadyInitialized);
        }
        storage::set_asset_config(&env, &cfg.asset, &cfg);
        storage::add_to_asset_list(&env, &cfg.asset);
        Ok(())
    }

    /// technical-doc.md Section 12.1: "rejects a change to cfg.reference".
    /// Review decision D3: also rejects `Reference::Asset`, same as
    /// `add_asset` (an existing asset can never have had this reference
    /// in the first place, since `add_asset` rejects it, but the check
    /// is repeated here rather than relying on that invariant holding
    /// forever).
    pub fn update_asset(env: Env, asset: Address, cfg: AssetConfig) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        reject_asset_reference(&cfg.reference)?;
        let existing = storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        if existing.reference != cfg.reference {
            return Err(Error::ReferenceImmutable);
        }
        storage::set_asset_config(&env, &asset, &cfg);
        Ok(())
    }

    /// technical-doc.md Section 12.1.
    pub fn disable_asset(env: Env, asset: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let mut cfg = storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        cfg.enabled = false;
        storage::set_asset_config(&env, &asset, &cfg);
        Ok(())
    }

    /// technical-doc.md Section 12.1, 5.2, 5.3, 11.3.
    ///
    /// Checks keeper eligibility against `Staking`, applies the full
    /// Section 11.3 sanity bounds, cross checks configured `PriceAdapter`s,
    /// and sources `endpoint` only from `Staking.aggregate` (never the
    /// keeper's payload). `s.endpoint` is only ever a placeholder on input.
    pub fn post_signals(
        env: Env,
        keeper: Address,
        asset: Address,
        mut s: SignalSet,
    ) -> Result<(), Error> {
        keeper.require_auth();
        sylox_types::ttl::extend_instance(&env);

        let config = Self::require_config(&env)?;
        if !StakingClient::new(&env, &config.staking).is_active_keeper(&keeper) {
            return Err(Error::KeeperNotActive);
        }

        let cfg = storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        if !cfg.enabled {
            return Err(Error::UnknownAsset);
        }

        let now = env.ledger().timestamp();
        check_epoch_window(s.epoch, now)?;
        if storage::get_signals(&env, &asset, s.epoch).is_some() {
            // Review item C4: an epoch that was overturned by a resolved
            // dispute has already been moved out of here (to the
            // Overturned history key, by resolve_signal_dispute), so
            // get_signals no longer finds it and this check does not
            // block a repost for it. Any OTHER existing posting for this
            // epoch (Pending, Disputed or still Final) blocks a repost,
            // matching Section 11.3.
            return Err(Error::EpochAlreadyPosted);
        }
        // Section 5.9 S2 (v1.5): one writer per hour. This hourly
        // fallback post is accepted only for an hour with no sub-epoch
        // posted yet; once an hour has a sub-epoch, every later post
        // for it (including a fallback attempt) must go through
        // post_sub_signals instead.
        if storage::get_hour_posted_via(&env, &asset, s.epoch)
            == Some(storage::HourPostedVia::SubEpoch)
        {
            return Err(Error::HourAlreadyPosted);
        }
        storage::set_hour_posted_via(
            &env,
            &asset,
            s.epoch,
            storage::HourPostedVia::HourlyFallback,
        );
        check_sanity_bounds(&s)?;
        check_supply_change_consistency(&env, &asset, &s)?;
        check_amm_cross_check(&env, &cfg, &s)?;

        s.poster = keeper.clone();
        s.posted_at = now;
        s.endpoint = StakingClient::new(&env, &config.staking).aggregate(&asset, &s.epoch);

        storage::set_signals(&env, &asset, s.epoch, &s);
        storage::set_first_epoch_if_unset(&env, &asset, s.epoch);

        let pending_until = now + SIGNAL_DISPUTE_SECS;
        // Hourly fallback path: peg_ratio etc. are the single real
        // posted reading, not a sub-epoch roll-up, so no coverage
        // concept applies.
        let wrote = storage::write_ring_slot(&env, &asset, s.epoch, &s, pending_until, None);
        if !wrote {
            // The ring position for this epoch already holds a strictly
            // newer epoch (the buffer wrapped past it). check_epoch_window
            // should already reject a posting this stale via WINDOW_SECS;
            // reaching here would mean the two checks have drifted apart.
            return Err(Error::WrongEpoch);
        }

        events::SignalsPosted {
            asset: asset.clone(),
            epoch: s.epoch,
            keeper,
            inputs_hash: s.inputs_hash.clone(),
            pending_until,
        }
        .publish(&env);

        // This posting cannot itself be final yet (pending_until is
        // always in the future at post time), but posting is also the
        // natural moment to sweep the whole backfill window for any
        // OLDER epoch that has quietly crossed finality since the
        // last state changing call touched it.
        if let Some(newest_final) =
            try_advance_finality(&env, &config, &asset, FINALITY_LOOKBACK_EPOCHS)
        {
            recompute_score(&env, &asset, newest_final)?;
        }
        check_stale_internal(&env, &asset)?;

        Ok(())
    }

    /// technical-doc.md Section 5.9 S2 (v1.5). Posts a sub-epoch,
    /// keyed by `(hour, sub)` instead of a single `epoch`. Mirrors
    /// `post_signals`'s own checks (keeper eligibility, sanity bounds,
    /// AMM cross check), disputable for `signal_dispute_secs` exactly
    /// as the hourly path already specifies. Recomputes the hour's own
    /// provisional roll-up on every call, and attempts the hour's
    /// build inline, the same way `post_signals` already
    /// inline-triggers the hourly finality scan.
    pub fn post_sub_signals(
        env: Env,
        keeper: Address,
        asset: Address,
        hour: u64,
        sub: u32,
        mut s: SignalSet,
    ) -> Result<(), Error> {
        keeper.require_auth();
        sylox_types::ttl::extend_instance(&env);

        let config = Self::require_config(&env)?;
        if !StakingClient::new(&env, &config.staking).is_active_keeper(&keeper) {
            return Err(Error::KeeperNotActive);
        }

        let cfg = storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        if !cfg.enabled {
            return Err(Error::UnknownAsset);
        }

        let now = env.ledger().timestamp();
        let sub_epoch_secs = current_sub_epoch_secs(&env, &asset, hour);
        if sub >= sub_epochs_per_hour(sub_epoch_secs) {
            return Err(Error::InvalidSubEpochInterval);
        }
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        let sub_close = sub_start + sub_epoch_secs;

        // A repost of an overturned sub-epoch (ADR-005, extended to
        // sub-epochs) is accepted only while the hour itself is still
        // unbuilt: once built, a built hour never changes (I12), so a
        // repost at this point must fail the same way any other post
        // against an already-committed hour does, never silently
        // succeed into a slot nothing reads any more. Checked before
        // either window check below, since a built hour must reject a
        // repost even if its own repost window is technically still
        // open.
        if storage::is_final(&env, &asset, hour, now) {
            return Err(Error::HourAlreadyPosted);
        }

        let overturn = storage::get_overturned_sub_signals(&env, &asset, hour, sub);
        let is_repost = match &overturn {
            Some(record) if !record.reposted => {
                // One repost per overturn (Section 5.9 S2): a second
                // overturn of this same original posting gets its OWN
                // fresh SubOverturned record (written by resolve_sub_
                // signal_dispute, never this check), so reaching here
                // with `reposted` already true only happens for the
                // SAME overturn being reposted twice, which this
                // rejects.
                check_sub_epoch_repost_window(sub_close, record.overturned_at, now)?;
                true
            }
            Some(_) => return Err(Error::EpochAlreadyPosted),
            None => {
                check_sub_epoch_window(sub_start, sub_epoch_secs, now)?;
                false
            }
        };

        if storage::get_sub_signals(&env, &asset, hour, sub).is_some() {
            return Err(Error::EpochAlreadyPosted);
        }
        // Section 5.9 S2: one writer per hour, the symmetric guard to
        // post_signals's own check above.
        if storage::get_hour_posted_via(&env, &asset, hour)
            == Some(storage::HourPostedVia::HourlyFallback)
        {
            return Err(Error::HourAlreadyPosted);
        }
        // Section 5.9 S1: the hour's first sub-epoch post records the
        // length it is posted at, so every later reader resolves this
        // hour the same way at any age.
        if storage::get_hour_sub_epoch_secs(&env, &asset, hour).is_none() {
            storage::set_hour_sub_epoch_secs(&env, &asset, hour, sub_epoch_secs);
        }
        storage::set_hour_posted_via(&env, &asset, hour, storage::HourPostedVia::SubEpoch);

        check_sanity_bounds(&s)?;
        check_amm_cross_check(&env, &cfg, &s)?;

        s.epoch = hour;
        s.poster = keeper.clone();
        s.posted_at = now;
        s.endpoint = StakingClient::new(&env, &config.staking).aggregate(&asset, &hour);

        storage::set_sub_signals(&env, &asset, hour, sub, &s);
        storage::set_first_epoch_if_unset(&env, &asset, hour);

        let pending_until = now + SIGNAL_DISPUTE_SECS;
        let held_slot = storage::HeldSubSlot {
            state: sylox_types::SlotState::Pending,
            pending_until,
            peg_ratio: s.peg_ratio,
            liquidity_2pct: s.liquidity_2pct,
            redemption_net: s.redemption_net,
            supply: s.supply,
            clawback_amount: s.issuer_actions.clawback_amount,
            auth_revocations: s.issuer_actions.auth_revocations,
        };

        if is_repost && !storage::sub_slot_still_belongs_to(&env, &asset, sub_start, now) {
            // The ring has rotated past this sub-epoch's own position
            // since it was overturned (a slow ruling, Section 5.9 S2):
            // the repost goes into HeldHour directly, never overwrite
            // whatever newer sub-epoch's data now occupies that ring
            // position. sub_slot_still_belongs_to is the one check
            // that decides this; the ring write below is skipped
            // entirely on this path, by construction, not by relying
            // on write_sub_slot_entry's own newer-wins guard to refuse
            // it after the fact.
            storage::insert_held_sub_slot(
                &env,
                &asset,
                hour,
                sub,
                &held_slot,
                HELD_HOUR_TTL_LEDGERS,
            );
        } else {
            let wrote = storage::write_sub_slot_entry(
                &env,
                &asset,
                sub_start,
                &s,
                sylox_types::SlotState::Pending,
                pending_until,
            );
            if !wrote {
                // sub_slot_still_belongs_to already confirmed this
                // position is safe for a repost; for a fresh post
                // (is_repost false), the window check above already
                // bounds how stale sub_start can be relative to now,
                // so reaching here would mean the two checks have
                // drifted apart (the same invariant post_signals's
                // own equivalent comment states for the hourly ring).
                return Err(Error::WrongEpoch);
            }
            storage::extend_sub_ring_ttl(&env, &asset, 100, SUB_DISPUTE_TTL_LEDGERS);

            // Section 5.9 S3: if this hour already has an open dispute
            // (HeldHour exists), capture this sub-epoch's own data too,
            // so it survives the ring rotating past it even if that
            // happens before this specific sub-epoch's own dispute window
            // closes.
            if storage::get_held_hour(&env, &asset, hour).is_some() {
                storage::insert_held_sub_slot(
                    &env,
                    &asset,
                    hour,
                    sub,
                    &held_slot,
                    HELD_HOUR_TTL_LEDGERS,
                );
            }
        }

        if is_repost {
            storage::mark_sub_overturn_reposted(&env, &asset, hour, sub);
        }

        events::SubSignalsPosted {
            asset: asset.clone(),
            hour,
            sub,
            keeper,
            inputs_hash: s.inputs_hash.clone(),
            pending_until,
        }
        .publish(&env);

        refresh_waiting_hour(&env, &config, &asset, hour)?;

        Ok(())
    }

    /// technical-doc.md Section 12.1, 5.4, 7.8.
    ///
    /// Records the dispute and locks the disputer's bond in `Staking`.
    /// Only the signal dispute STATE lives here; the bond itself is held
    /// and later split by `Staking` on instruction from
    /// `resolve_signal_dispute`.
    pub fn dispute_signals(
        env: Env,
        disputer: Address,
        asset: Address,
        epoch: u64,
        alt_hash: BytesN<32>,
    ) -> Result<(), Error> {
        disputer.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;

        let slot = storage::get_slot(&env, &asset, epoch).ok_or(Error::WrongEpoch)?;
        if slot.state != sylox_types::SlotState::Pending {
            // Only a Pending posting can still be disputed; Final is past
            // the window, Disputed already has an open dispute, Empty has
            // nothing to dispute.
            return Err(Error::DisputeWindowClosed);
        }
        if env.ledger().timestamp() >= slot.pending_until {
            return Err(Error::DisputeWindowClosed);
        }
        if storage::get_dispute(&env, &asset, epoch).is_some() {
            return Err(Error::DisputeWindowClosed);
        }
        let signals = storage::get_signals(&env, &asset, epoch).ok_or(Error::WrongEpoch)?;

        let bond = signal_dispute_bond();
        let now = env.ledger().timestamp();
        StakingClient::new(&env, &config.staking).lock_bond(
            &sylox_types::BondKey::SignalDispute(asset.clone(), epoch),
            &disputer,
            &bond,
            &Some(signals.poster.clone()),
        );
        storage::set_dispute(
            &env,
            &asset,
            epoch,
            &DisputeRecord {
                disputer: disputer.clone(),
                alt_hash: alt_hash.clone(),
                opened_at: now,
            },
        );
        storage::set_slot_disputed(&env, &asset, epoch);

        events::SignalsDisputed {
            asset: asset.clone(),
            epoch,
            disputer,
            alt_hash,
        }
        .publish(&env);
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 5.9 S2, S3 (v1.5). Mirrors
    /// `dispute_signals` exactly, against `Sub(asset)`'s own ring: on
    /// dispute, the sub-epoch's record is copied out to
    /// `SubDispute(asset, hour, sub)` (so `Sub(asset)`'s fixed 60 slot
    /// ring can keep rotating underneath a ruling that takes up to
    /// `SIGNAL_DISPUTE_RULING_SECS`), and the HOUR's own `Ring(asset)`
    /// slot moves to `Disputed` via `refresh_waiting_hour`.
    pub fn dispute_sub_signals(
        env: Env,
        disputer: Address,
        asset: Address,
        hour: u64,
        sub: u32,
        alt_hash: BytesN<32>,
    ) -> Result<(), Error> {
        disputer.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let config = Self::require_config(&env)?;

        let sub_epoch_secs = current_sub_epoch_secs(&env, &asset, hour);
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        // docs/design/held-repost-disputes.md D1: once an hour is held,
        // `HeldHour` is its only source of truth, the same precedence
        // `sub_disposition` uses, with no fallback to the ring. A repost
        // made after the ring span lives only there and stays
        // disputable within its own `pending_until`.
        let slot = match storage::get_held_hour(&env, &asset, hour) {
            Some(held) => {
                let h = held.get(sub).ok_or(Error::WrongEpoch)?;
                storage::SubRingSlot {
                    sub_start,
                    state: h.state,
                    pending_until: h.pending_until,
                    peg_ratio: h.peg_ratio,
                    liquidity_2pct: h.liquidity_2pct,
                    redemption_net: h.redemption_net,
                    supply: h.supply,
                    supply_change_bps: 0,
                    clawback_amount: h.clawback_amount,
                    auth_revocations: h.auth_revocations,
                    endpoint: sylox_types::EndpointStatus::Unknown,
                }
            }
            None => storage::get_sub_slot(&env, &asset, sub_start).ok_or(Error::WrongEpoch)?,
        };
        if slot.state != sylox_types::SlotState::Pending {
            return Err(Error::DisputeWindowClosed);
        }
        if env.ledger().timestamp() >= slot.pending_until {
            return Err(Error::DisputeWindowClosed);
        }
        if storage::get_sub_dispute(&env, &asset, hour, sub).is_some() {
            return Err(Error::DisputeWindowClosed);
        }
        let signals = storage::get_sub_signals(&env, &asset, hour, sub).ok_or(Error::WrongEpoch)?;

        let bond = signal_dispute_bond();
        let now = env.ledger().timestamp();
        // Reuses BondKey::SignalDispute's own (Address, u64) shape with
        // sub_start in place of epoch: an hourly epoch number (~497,000
        // on a real network, now/EPOCH_SECS) and a sub-epoch's own
        // absolute start time (~1.79 billion, now itself, roughly
        // EPOCH_SECS times larger) occupy numerically disjoint ranges
        // in practice, so a collision between an hourly and a
        // sub-epoch dispute for the same asset cannot arise on a real
        // network. If it ever did (e.g. a test constructing adversarial
        // epoch/sub_start values by hand), lock_bond's own existing
        // BondExists check rejects the second lock cleanly; nothing
        // silently mixes two disputes' bonds together.
        StakingClient::new(&env, &config.staking).lock_bond(
            &sylox_types::BondKey::SignalDispute(asset.clone(), sub_start),
            &disputer,
            &bond,
            &Some(signals.poster.clone()),
        );
        storage::set_sub_dispute(
            &env,
            &asset,
            hour,
            sub,
            &DisputeRecord {
                disputer: disputer.clone(),
                alt_hash: alt_hash.clone(),
                opened_at: now,
            },
            SUB_DISPUTE_TTL_LEDGERS,
        );
        storage::set_sub_slot_state(&env, &asset, sub_start, sylox_types::SlotState::Disputed);
        // Section 5.9 S3: the first dispute against this hour copies
        // every one of its OTHER currently-readable sub-epochs (plus
        // this one's own pre-dispute data, in case the dispute ends up
        // keeper-wins) out of Sub(asset) into HeldHour, so a ruling
        // that arrives after the ring has rotated past this hour's
        // own slots still has a full picture to roll up from, not
        // just this one sub-epoch's. This sub-epoch's own held entry
        // stores its PRE-dispute state (Pending, with its original
        // pending_until) rather than Disputed: while the dispute is
        // open, sub_disposition checks SubDispute first regardless of
        // what HeldHour says, so this value is dormant; once resolved
        // keeper-wins, resolve_sub_signal_dispute updates this same
        // entry's state to Final directly (the ring's own
        // set_slot_final-equivalent write is no longer the source of
        // truth once Sub(asset) has rotated past this slot).
        ensure_hour_held(&env, &asset, hour, sub_epoch_secs);
        storage::insert_held_sub_slot(
            &env,
            &asset,
            hour,
            sub,
            &storage::HeldSubSlot {
                // Disputed, not slot.state's own pre-dispute Pending:
                // every reader of a held sub-epoch's disposition now
                // trusts this stored state directly (no separate
                // SubDispute lookup, R10's own footprint fix), so it
                // must already read Disputed the instant the dispute
                // opens. The roll-up FIELDS below still keep this
                // sub-epoch's own real pre-dispute values, so a later
                // keeper-wins ruling has real data to bring back.
                state: sylox_types::SlotState::Disputed,
                pending_until: slot.pending_until,
                peg_ratio: slot.peg_ratio,
                liquidity_2pct: slot.liquidity_2pct,
                redemption_net: slot.redemption_net,
                supply: slot.supply,
                clawback_amount: slot.clawback_amount,
                auth_revocations: slot.auth_revocations,
            },
            HELD_HOUR_TTL_LEDGERS,
        );

        events::SubSignalsDisputed {
            asset: asset.clone(),
            hour,
            sub,
            disputer,
            alt_hash,
        }
        .publish(&env);

        refresh_waiting_hour(&env, &config, &asset, hour)?;
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 12.1, 5.4, 7.8. Auth: the committee
    /// address, read fresh from `Governor.committee()` on every call (the
    /// committee can rotate; `RiskOracle` never caches it).
    pub fn resolve_signal_dispute(
        env: Env,
        asset: Address,
        epoch: u64,
        keeper_wins: bool,
        reason: BytesN<32>,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        let committee = GovernorClient::new(&env, &config.governor).committee();
        committee.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let _ = &reason; // stored only by EventRegistry-style callers in Phase 3; RiskOracle has nowhere to persist it yet (known gap).

        let dispute =
            storage::get_dispute(&env, &asset, epoch).ok_or(Error::DisputeWindowClosed)?;
        // D2: a ruling closes at the same instant the timeout opens, so
        // exactly one of the two can end the dispute at any moment.
        if env.ledger().timestamp() >= dispute.opened_at + SIGNAL_DISPUTE_RULING_SECS {
            return Err(Error::RulingDeadlinePassed);
        }
        let signals = storage::get_signals(&env, &asset, epoch).ok_or(Error::WrongEpoch)?;
        let staking = StakingClient::new(&env, &config.staking);
        let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), epoch);

        if keeper_wins {
            staking.forfeit_bond(&bond_key, &Some(signals.poster.clone()));
            // Review item C6 (re-review): the slot is now immediately,
            // definitely Final, decided right here, not merely
            // "effectively" final pending a clock check elsewhere.
            // set_slot_final sets the final_announced flag in the same
            // write that sets the state, per the re-review's "emit
            // signals_final immediately and set the flag in the same
            // write" instruction; this contract then emits the event
            // for THIS epoch directly, rather than waiting for the
            // independent backward scan below (which exists to pick up
            // any OTHER epoch this resolution may have unblocked, not
            // this one).
            if storage::set_slot_final(&env, &asset, epoch) {
                events::SignalsFinal {
                    asset: asset.clone(),
                    epoch,
                }
                .publish(&env);
            }
            if let Some(newest_final) =
                try_advance_finality(&env, &config, &asset, FINALITY_LOOKBACK_EPOCHS)
            {
                recompute_score(&env, &asset, newest_final)?;
            }
        } else {
            staking.release_bond(&bond_key);
            staking.slash(
                &signals.poster,
                &keeper_slash(),
                &Some(dispute.disputer.clone()),
                &BytesN::from_array(&env, &[0u8; 32]),
            );
            storage::set_slot_overturned(&env, &asset, epoch);
            // Review item C4: move the overturned Signals entry to
            // history so post_signals accepts a fresh posting for this
            // same epoch (ADR-005: "an overturned epoch reopens for
            // reposting").
            storage::overturn_signals(&env, &asset, epoch);
            // Review item C6 (re-review): an overturned epoch, even if
            // never reposted, must not block any LATER epoch's
            // finality. The independent backward scan already
            // guarantees this by construction (it skips past this
            // epoch, now Empty, without stopping), but a resolution
            // can still be the event that unblocks something later
            // that was waiting on this one, so sweep here too.
            if let Some(newest_final) =
                try_advance_finality(&env, &config, &asset, FINALITY_LOOKBACK_EPOCHS)
            {
                recompute_score(&env, &asset, newest_final)?;
            }
        }
        storage::clear_dispute(&env, &asset, epoch);

        events::SignalsResolved {
            asset: asset.clone(),
            epoch,
            keeper_wins,
            reason,
        }
        .publish(&env);
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 5.9 S2, S4 (v1.5). Mirrors
    /// `resolve_signal_dispute` exactly, against `Sub(asset)`. On
    /// `keeper_wins`, the sub-epoch's slot becomes Final; on the
    /// disputer winning, it clears back to Empty (the overturned
    /// sub-epoch's own `SubSignals` entry is left in place for audit,
    /// the same way `Signals(asset, epoch)` is kept but no longer
    /// live after an hourly overturn) and can be reposted. Either way,
    /// `refresh_waiting_hour` re-derives the HOUR's own `Ring(asset)`
    /// state from every sub-epoch's current disposition, which may
    /// move the hour out of `Disputed` (if no other sub-epoch in it is
    /// still disputed) and, if this was the last undecided sub-epoch,
    /// trigger the hour's build.
    pub fn resolve_sub_signal_dispute(
        env: Env,
        asset: Address,
        hour: u64,
        sub: u32,
        keeper_wins: bool,
        reason: BytesN<32>,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        let committee = GovernorClient::new(&env, &config.governor).committee();
        committee.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let _ = &reason;

        let dispute =
            storage::get_sub_dispute(&env, &asset, hour, sub).ok_or(Error::DisputeWindowClosed)?;
        let now = env.ledger().timestamp();
        // D2: see resolve_signal_dispute.
        if now >= dispute.opened_at + SIGNAL_DISPUTE_RULING_SECS {
            return Err(Error::RulingDeadlinePassed);
        }
        let sub_epoch_secs = current_sub_epoch_secs(&env, &asset, hour);
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        let signals = storage::get_sub_signals(&env, &asset, hour, sub).ok_or(Error::WrongEpoch)?;
        let staking = StakingClient::new(&env, &config.staking);
        let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), sub_start);

        if keeper_wins {
            staking.forfeit_bond(&bond_key, &Some(signals.poster.clone()));
            storage::set_sub_slot_state(&env, &asset, sub_start, sylox_types::SlotState::Final);
            // Mirrors the ring write above for HeldHour: once this
            // hour has a held entry, sub_disposition reads THAT, not
            // the ring, so the ring-only write above alone would
            // never be seen for an hour whose ring has rotated on.
            storage::set_held_sub_slot_state(
                &env,
                &asset,
                hour,
                sub,
                sylox_types::SlotState::Final,
            );
            events::SubSignalsFinal {
                asset: asset.clone(),
                hour,
                sub,
            }
            .publish(&env);
        } else {
            staking.release_bond(&bond_key);
            staking.slash(
                &signals.poster,
                &keeper_slash(),
                &Some(dispute.disputer.clone()),
                &BytesN::from_array(&env, &[0u8; 32]),
            );
            storage::clear_sub_slot(&env, &asset, sub_start);
            // Overturned: this sub-epoch must never contribute to the
            // roll-up, including from HeldHour (Section 5.9 S4, I23).
            storage::remove_held_sub_slot(&env, &asset, hour, sub);
            // ADR-005, extended to sub-epochs: move the overturned
            // posting to history so get_sub_signals no longer finds
            // it, unblocking post_sub_signals's own EpochAlreadyPosted
            // check for a repost of this exact (hour, sub). Without
            // this, a rejected dispute would permanently kill the
            // sub-epoch: it could never be reposted, so every
            // successful dispute would permanently cost the hour that
            // one sub-epoch's worth of coverage.
            storage::overturn_sub_signals(
                &env,
                &asset,
                hour,
                sub,
                sub_epochs_per_hour(sub_epoch_secs),
                now,
            );
        }
        storage::clear_sub_dispute(&env, &asset, hour, sub);

        events::SubSignalsResolved {
            asset: asset.clone(),
            hour,
            sub,
            keeper_wins,
        }
        .publish(&env);

        refresh_waiting_hour(&env, &config, &asset, hour)?;
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// ADR-010 (issue #4 fix). Permissionless, callable once
    /// `SIGNAL_DISPUTE_RULING_SECS` has passed since `dispute_signals`
    /// opened this dispute, if the committee still has not ruled via
    /// `resolve_signal_dispute`. Default outcome mirrors ADR-002's
    /// rule for data backed claims: the keeper's posting stands (slot
    /// `Final`, same as a `keeper_wins: true` committee ruling). "Both
    /// bonds released in full, nobody slashed" (the task's own
    /// wording): there is only one actual `BondKey::SignalDispute`
    /// lock to release, the disputer's (technical-doc.md Section 24.2
    /// notes a keeper has no symmetric per-signal bond in v1; only
    /// `slash`, by address, ever touches a keeper's stake), so in
    /// practice this means the disputer's bond is refunded
    /// (`release_bond`, not `forfeit_bond`) AND the keeper's stake is
    /// left untouched (no `slash` call either) — both parties come
    /// out exactly as they would from a `keeper_wins: true` ruling,
    /// which is the committee's silence being read as "no evidence
    /// the posting was wrong", not as a loss for either side.
    pub fn resolve_signal_dispute_timeout(
        env: Env,
        asset: Address,
        epoch: u64,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let dispute =
            storage::get_dispute(&env, &asset, epoch).ok_or(Error::DisputeWindowClosed)?;
        let now = env.ledger().timestamp();
        if now < dispute.opened_at + SIGNAL_DISPUTE_RULING_SECS {
            return Err(Error::RulingDeadlineNotReached);
        }

        let staking = StakingClient::new(&env, &config.staking);
        let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), epoch);
        staking.release_bond(&bond_key);
        if storage::set_slot_final(&env, &asset, epoch) {
            events::SignalsFinal {
                asset: asset.clone(),
                epoch,
            }
            .publish(&env);
        }
        if let Some(newest_final) =
            try_advance_finality(&env, &config, &asset, FINALITY_LOOKBACK_EPOCHS)
        {
            recompute_score(&env, &asset, newest_final)?;
        }
        storage::clear_dispute(&env, &asset, epoch);

        let committee = GovernorClient::new(&env, &config.governor).committee();
        storage::record_committee_miss(&env, &committee);

        events::SignalDisputeTimedOut {
            asset: asset.clone(),
            epoch,
            disputer: dispute.disputer,
            committee,
        }
        .publish(&env);
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 5.9 S2 (v1.5). Mirrors
    /// `resolve_signal_dispute_timeout` exactly, against
    /// `SubDispute(asset, hour, sub)`: the committee's silence is read
    /// the same way for a sub-epoch dispute as for an hourly one, the
    /// keeper's posting stands.
    pub fn resolve_sub_dispute_timeout(
        env: Env,
        asset: Address,
        hour: u64,
        sub: u32,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let dispute =
            storage::get_sub_dispute(&env, &asset, hour, sub).ok_or(Error::DisputeWindowClosed)?;
        let now = env.ledger().timestamp();
        if now < dispute.opened_at + SIGNAL_DISPUTE_RULING_SECS {
            return Err(Error::RulingDeadlineNotReached);
        }

        let sub_epoch_secs = current_sub_epoch_secs(&env, &asset, hour);
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        let staking = StakingClient::new(&env, &config.staking);
        let bond_key = sylox_types::BondKey::SignalDispute(asset.clone(), sub_start);
        staking.release_bond(&bond_key);
        storage::set_sub_slot_state(&env, &asset, sub_start, sylox_types::SlotState::Final);
        storage::set_held_sub_slot_state(&env, &asset, hour, sub, sylox_types::SlotState::Final);
        events::SubSignalsFinal {
            asset: asset.clone(),
            hour,
            sub,
        }
        .publish(&env);
        storage::clear_sub_dispute(&env, &asset, hour, sub);

        let committee = GovernorClient::new(&env, &config.governor).committee();
        storage::record_committee_miss(&env, &committee);

        events::SubSignalDisputeTimedOut {
            asset: asset.clone(),
            hour,
            sub,
            disputer: dispute.disputer,
            committee,
        }
        .publish(&env);

        refresh_waiting_hour(&env, &config, &asset, hour)?;
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 12.1, 7.4. Reads `Staking.aggregate` and
    /// books it into the epoch's ring slot and `Signals` entry if
    /// `post_signals` had not already resolved one by the time it closed;
    /// then calls `Staking.settle_probes` exactly once regardless, per
    /// Section 7.4's "booked exactly once" requirement.
    pub fn finalize_endpoint(env: Env, asset: Address, epoch: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        let now = env.ledger().timestamp();
        let epoch_close = (epoch + 1) * EPOCH_SECS;
        if now < epoch_close {
            return Err(Error::WrongEpoch);
        }

        let staking = StakingClient::new(&env, &config.staking);
        let aggregate = staking.aggregate(&asset, &epoch);

        if let Some(mut signals) = storage::get_signals(&env, &asset, epoch) {
            if signals.endpoint == EndpointStatus::Unknown {
                signals.endpoint = aggregate;
                storage::set_signals(&env, &asset, epoch, &signals);
                storage::set_slot_endpoint(&env, &asset, epoch, aggregate);
                events::EndpointFinalized {
                    asset: asset.clone(),
                    epoch,
                    status: aggregate,
                }
                .publish(&env);
            }
        }
        staking.settle_probes(&asset, &epoch);

        // finalize_endpoint is the one permissionless, callable-by-anyone
        // function guaranteed to run after an epoch's dispute window has
        // had time to elapse (the caller only needs the epoch itself to
        // have closed, Section 7.4), so it is also where the lazy
        // finality sweep gets a chance to run even if no new SignalSet
        // is ever posted for a later epoch on this asset.
        if let Some(newest_final) =
            try_advance_finality(&env, &config, &asset, FINALITY_LOOKBACK_EPOCHS)
        {
            recompute_score(&env, &asset, newest_final)?;
        }
        check_stale_internal(&env, &asset)?;
        Ok(())
    }

    /// technical-doc.md Section 12.1, 8.5, 8.8. Auth: the registry
    /// contract itself (the `registry` address `RiskOracle` was
    /// initialized with), per Section 16.1's contract-to-contract auth
    /// pattern.
    pub fn set_event_band(env: Env, asset: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.registry.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let mut score = storage::get_score(&env, &asset).unwrap_or(RiskScore {
            epoch: 0,
            score: 0,
            band: Band::Normal,
            formula_version: 0,
            stale: true,
        });
        score.band = Band::Event;
        storage::set_score(&env, &asset, &score);
        storage::set_down_streak(&env, &asset, 0);
        Ok(())
    }

    /// technical-doc.md Section 12.1, 8.8.
    pub fn clear_event_band(env: Env, asset: Address) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.registry.require_auth();
        sylox_types::ttl::extend_instance(&env);
        let mut score = storage::get_score(&env, &asset).ok_or(Error::UnknownAsset)?;
        score.band = score::band_for_score(score.score);
        storage::set_score(&env, &asset, &score);
        Ok(())
    }

    /// Review decision D1, not in the original Section 12.1 list. Auth:
    /// the registry contract, same pattern as `set_event_band` /
    /// `clear_event_band` (Section 16.1). While `true`, `score()` floors
    /// the band at `Distress` on every read (Section 6.3's "forced to
    /// Distress if a credit event... is Proposed, Challenged or
    /// Escalated"), independent of the hysteresis-protected band
    /// actually stored; `RiskOracle` never calls into `EventRegistry` to
    /// check this itself, matching the instruction that "the oracle
    /// never calls the registry" — this is pushed in, the same
    /// direction `set_event_band` already works.
    pub fn set_event_in_progress(env: Env, asset: Address, in_progress: bool) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.registry.require_auth();
        sylox_types::ttl::extend_instance(&env);
        storage::set_event_in_progress(&env, &asset, in_progress);
        // No recompute needed: `score()` applies this flag as a live
        // floor on every read (see its doc comment), so the change is
        // visible immediately with no write to the stored RiskScore.
        Ok(())
    }

    /// technical-doc.md Section 12.1: read-only, no auth. Lets
    /// `EventRegistry` assert invariant E4 (its own active-event count
    /// agrees with this flag) directly, rather than inferring the flag
    /// only through its one visible effect on `band()`'s own Distress
    /// floor.
    pub fn event_in_progress(env: Env, asset: Address) -> bool {
        storage::get_event_in_progress(&env, &asset)
    }

    /// Read-only, no auth. Wraps the existing
    /// `storage::get_newest_epoch_pub`, which reads the
    /// dedicated `RingNewest` key directly rather than the newest
    /// ring POSITION's own stored epoch. The two disagree exactly
    /// when the newest position is `Empty` (its epoch was overturned
    /// and not yet reposted): the position's slot reports epoch `0`
    /// (`empty_slot()`'s default), while `RingNewest` still correctly
    /// reports the real newest epoch ever written. `EventRegistry`
    /// needs the latter for its own `slot_for_epoch` arithmetic
    /// (`ring()`'s layout is "oldest first, ending at RingNewest");
    /// reading the former made every subsequent `slot_for_epoch` call
    /// miss, misclassifying every cure-window epoch as permanently
    /// missing.
    pub fn newest_epoch(env: Env, asset: Address) -> Option<u64> {
        storage::get_newest_epoch_pub(&env, &asset)
    }

    /// Read-only, no auth. The first epoch ever
    /// successfully posted for this asset. `EventRegistry`'s own
    /// Tier 1 Depeg and IssuerFreeze history baselines need this for
    /// the same reason `RiskOracle`'s own score and `median_liquidity`
    /// do: the newest epoch's own absolute number is always far
    /// larger than any window length on a real network, so it alone
    /// can never tell a brand-new asset from one with years of
    /// history. `None` before the asset's first posting.
    pub fn first_epoch(env: Env, asset: Address) -> Option<u64> {
        storage::get_first_epoch(&env, &asset)
    }

    /// technical-doc.md Section 12.1, 6.2. `set_formula` rejects any
    /// weight set that does not sum to 10,000.
    pub fn set_formula(
        env: Env,
        version: u32,
        weights: Vec<u32>,
        params: Map<Symbol, i128>,
    ) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        score::validate_weights(&weights)?;
        storage::set_formula(
            &env,
            &Formula {
                version,
                weights,
                params,
            },
        );
        Ok(())
    }

    /// technical-doc.md Section 5.9 S1, 16, 17.4 (v1.5). Auth: governor,
    /// the same `SetParam`-style path every other parameter in Section
    /// 23 already uses; no new role or action type is needed. Takes
    /// effect from the NEXT hour boundary only (never the current,
    /// possibly-in-progress hour), so no sub-epoch already posted, or
    /// postable before that boundary, is ever reinterpreted under a
    /// different length.
    ///
    /// A new change is rejected (`SubEpochChangeTooSoon`) until
    /// `SUB_EPOCH_CHANGE_SPACING_HOURS` (240 hours, 10 days) after the
    /// previous `effective_from_hour`; a change still pending (its hour
    /// not yet arrived) may be replaced at any time. The spacing only
    /// limits how often the length changes. `SubEpochConfig` stores one
    /// transition, so after a change the config alone resolves hours
    /// from before the previous one to the wrong length; every hour with
    /// a sub-epoch posted is resolved from its own record
    /// (`HourSubEpochSecs`) instead, which keeps it correct at any age.
    /// Within the spacing, the config resolver stays exact for recent
    /// hours, which is all the gate's batched read needs.
    pub fn set_sub_epoch_secs(env: Env, asset: Address, value: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        config.governor.require_auth();
        sylox_types::ttl::extend_instance(&env);
        storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        if !SUB_EPOCH_SECS_ALLOWED.contains(&value) {
            return Err(Error::InvalidSubEpochInterval);
        }

        let now = env.ledger().timestamp();
        let current_hour = now / EPOCH_SECS;
        let effective_from_hour = current_hour + 1;
        if let Some(previous) =
            storage::get_sub_epoch_config(&env, &asset).and_then(|c| c.effective_from_hour)
        {
            if current_hour >= previous && current_hour < previous + SUB_EPOCH_CHANGE_SPACING_HOURS
            {
                return Err(Error::SubEpochChangeTooSoon);
            }
        }

        // The interval actually in force for the current hour carries
        // forward as `sub_epoch_secs`: an earlier pending value whose
        // hour has arrived is promoted here, and one still pending is
        // replaced. A config that has never been set is implicitly
        // SUB_EPOCH_SECS_DEFAULT (current_sub_epoch_secs's own
        // fallback). Using `value` (the NEW, not-yet-effective
        // interval) instead would make the change apply retroactively
        // to the current, still-open hour, exactly what
        // effective_from_hour exists to prevent.
        let current = current_sub_epoch_secs(&env, &asset, current_hour);
        storage::set_sub_epoch_config(
            &env,
            &asset,
            &sylox_types::SubEpochConfig {
                sub_epoch_secs: current,
                pending_sub_epoch_secs: Some(value),
                effective_from_hour: Some(effective_from_hour),
            },
        );

        events::SubEpochSecsChanged {
            asset,
            sub_epoch_secs: value,
            effective_from_hour,
        }
        .publish(&env);
        Ok(())
    }

    /// technical-doc.md Section 5.9 S1 (v1.5): read-only, no auth. The
    /// asset's sub-epoch configuration, including a pending change not
    /// yet in effect.
    pub fn sub_epoch_config(env: Env, asset: Address) -> Option<sylox_types::SubEpochConfig> {
        storage::get_sub_epoch_config(&env, &asset)
    }

    /// technical-doc.md Section 5.9 S1, 12.1: read-only, no auth. The
    /// sub-epoch length in force for `hour`: the pending value from its
    /// `effective_from_hour` on, the current value before it. The same
    /// resolver every internal read uses, so a caller computing
    /// sub-epoch boundaries matches the oracle exactly.
    pub fn sub_epoch_secs_at(env: Env, asset: Address, hour: u64) -> u64 {
        current_sub_epoch_secs(&env, &asset, hour)
    }

    /// technical-doc.md Section 5.9 S5, 12.1 (v1.5): read-only, no auth.
    /// The newest posted sub-epoch and its effective state. Unlike
    /// `latest`, which keeps returning the newest HOUR's `SignalSet`
    /// (`Series`'s `require_holding` valuation and every existing
    /// integration already depend on that exact meaning, Section 5.9
    /// S5), `live` surfaces Pending, challengeable sub-epoch data; the
    /// app shows it as "Live" next to `latest()`/`score()`'s
    /// "Confirmed" values. Never used for `require_holding` or any
    /// other payout-adjacent valuation.
    pub fn live(
        env: Env,
        asset: Address,
    ) -> Option<(sylox_types::SubEpoch, SignalSet, sylox_types::SlotState)> {
        let sub_start = storage::get_sub_ring_newest_pub(&env, &asset)?;
        let slot = storage::get_sub_slot(&env, &asset, sub_start)?;
        let (hour, sub) = hour_and_sub_of(&env, &asset, sub_start);
        let signals = storage::get_sub_signals(&env, &asset, hour, sub)?;
        let now = env.ledger().timestamp();
        let state = effective_sub_state(slot.state, slot.pending_until, now);
        Some((sylox_types::SubEpoch { hour, sub }, signals, state))
    }

    /// technical-doc.md Section 5.9 S4 (v1.5): permissionless. Builds
    /// `hour` if every one of its sub-epochs is Final, permanently
    /// missing, or rejected; `post_sub_signals` already attempts this
    /// inline on every post, so this exists for anyone to trigger a
    /// build that is ready but that no further posting has happened to
    /// trigger automatically (the same role `finalize_endpoint` plays
    /// for the hourly backward finality scan). Returns
    /// `SubEpochNotReady` if at least one sub-epoch is still Pending
    /// or Disputed; a never-posted hour (no sub-epoch at all, not even
    /// a missing one within an active interval) is equally not ready,
    /// since there is nothing to build from.
    pub fn build_hour(env: Env, asset: Address, hour: u64) -> Result<(), Error> {
        let config = Self::require_config(&env)?;
        sylox_types::ttl::extend_instance(&env);
        storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;

        let now = env.ledger().timestamp();
        let sub_epoch_secs = current_sub_epoch_secs(&env, &asset, hour);
        let per_hour = sub_epochs_per_hour(sub_epoch_secs);

        let mut final_slots: Vec<RingSlot> = Vec::new(&env);
        let mut final_subs: Vec<u32> = Vec::new(&env);
        let mut any_posted = false;
        for sub in 0..per_hour {
            let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
            match sub_disposition(&env, &asset, hour, sub, sub_start, sub_epoch_secs, now) {
                SubDisposition::Final(slot) => {
                    final_slots.push_back(slot);
                    final_subs.push_back(sub);
                    any_posted = true;
                }
                SubDisposition::Pending(_) => return Err(Error::SubEpochNotReady),
                SubDisposition::Disputed => return Err(Error::SubEpochNotReady),
                SubDisposition::MissingWithinBackfill => return Err(Error::SubEpochNotReady),
                SubDisposition::AwaitingRepost => return Err(Error::SubEpochNotReady),
                SubDisposition::PermanentlyMissing => any_posted = true,
            }
        }
        if !any_posted {
            return Err(Error::SubEpochNotReady);
        }

        try_build_hour(&env, &config, &asset, hour, &final_slots, &final_subs)
    }

    /// technical-doc.md Section 5.9 S5 (v1.5): read-only, no auth. Each
    /// of `hour`'s sub-epochs' own `peg_ratio`, in `sub` order, `None`
    /// for a sub-epoch that is not posted or not yet effectively Final
    /// or Pending (the same "Pending or Final" convention
    /// `EventRegistry.cover_gate`'s existing `RecentDepeg` check
    /// already reads from `Ring(asset)`, extended to `Sub(asset)` for
    /// an hour that has not built yet). `EventRegistry`'s own
    /// `cover_gate` calls this only for the current, not-yet-built
    /// hour (or hours, in the up-to-2h window an hour can stay
    /// waiting): every already-built hour inside its own trailing
    /// window is still read through `ring()` exactly as before this
    /// revision.
    pub fn sub_peg_ratios(env: Env, asset: Address, hour: u64) -> Vec<Option<i128>> {
        sub_peg_ratios_for_one_hour(&env, &asset, hour)
    }

    /// Section 5.9 S5 (v1.5, footprint-fix revision): `EventRegistry.
    /// cover_gate`'s own `RecentDepeg` check, for unbuilt hours still
    /// INSIDE `Sub(asset)`'s own 5-hour span (`SUB_RING_SLOTS *
    /// SUB_EPOCH_GRID_SECS`), never calling this for an hour outside
    /// it (the caller's own job to tell apart, using `newest_epoch`
    /// and those same constants: this function trusts its caller
    /// completely and does not re-check). Unlike `sub_peg_ratios` /
    /// the since-removed `sub_peg_ratios_batch`, this NEVER reads
    /// `HeldHour`: every hour it is asked about is assumed still
    /// physically present in `Sub(asset)`'s own ring, so `HeldHour` (a
    /// write-path, build/dispute-only concern, never a gate concern,
    /// per the Section 5.9 S5 footprint-fix review) plays no part
    /// here. One cross-contract call for every hour the gate needs,
    /// touching exactly ONE key (`Sub(asset)`, read once regardless of
    /// how many hours or sub-epochs are requested) rather than one key
    /// per hour: this is what makes the gate's own footprint constant
    /// regardless of how many hours are unbuilt (R10).
    pub fn sub_peg_ratios_in_span_batch(
        env: Env,
        asset: Address,
        hours: Vec<u64>,
    ) -> Vec<Vec<Option<i128>>> {
        // One fetch of Sub(asset)'s own packed entry for the WHOLE
        // call, not one per (hour, sub) pair: the entry is small and
        // fixed-size regardless of how many hours this call scans
        // (Section 5.9 S3), so re-fetching it per sub-epoch (up to
        // 876 times at the structural cap, 73 hours * 12 subs) was
        // pure waste, never a real footprint or memory concern this
        // call's own single-key read already avoids.
        let packed = storage::get_sub_ring_packed_for_batch(&env, &asset);
        let mut out = Vec::new(&env);
        for hour in hours.iter() {
            // The config resolver, not the per-hour record: reading
            // `HourSubEpochSecs` here would add one ledger key per hour
            // scanned and break the gate's fixed footprint (Section
            // 15.3). It is exact for every hour this reads, since only
            // hours inside `Sub(asset)`'s 5 hour span carry sub-epoch data
            // here, far less than the 240 hours between changes.
            let sub_epoch_secs = config_sub_epoch_secs(&env, &asset, hour);
            let per_hour = sub_epochs_per_hour(sub_epoch_secs);
            let mut ratios = Vec::new(&env);
            for sub in 0..per_hour {
                let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
                match storage::get_sub_slot_from_packed(&packed, sub_start) {
                    Some(slot)
                        if slot.state != sylox_types::SlotState::Empty
                            && slot.state != sylox_types::SlotState::Disputed =>
                    {
                        ratios.push_back(Some(slot.peg_ratio));
                    }
                    _ => ratios.push_back(None),
                }
            }
            out.push_back(ratios);
        }
        out
    }

    // -- reads --

    pub fn asset_config(env: Env, asset: Address) -> Option<AssetConfig> {
        storage::get_asset_config(&env, &asset)
    }

    pub fn signals(env: Env, asset: Address, epoch: u64) -> Option<SignalSet> {
        storage::get_signals(&env, &asset, epoch)
    }

    /// Review item C4: the `SignalSet` an overturned epoch's posting had
    /// before `resolve_signal_dispute` moved it out of `signals`'s live
    /// key, kept for audit. `None` if `epoch` was never overturned (or
    /// was overturned, reposted, and overturned again, which keeps only
    /// the most recent overturn, not a full history of every attempt).
    pub fn overturned_signals(env: Env, asset: Address, epoch: u64) -> Option<SignalSet> {
        storage::get_overturned_signals(&env, &asset, epoch)
    }

    pub fn latest(env: Env, asset: Address) -> Option<SignalSet> {
        let newest = storage::get_newest_epoch_pub(&env, &asset)?;
        storage::get_signals(&env, &asset, newest)
    }

    /// technical-doc.md Section 5.8, 12.1: one storage read, oldest slot
    /// first. See `storage::get_ring`'s doc comment for exactly what
    /// "oldest first" means once the buffer has wrapped.
    pub fn ring(env: Env, asset: Address) -> Vec<RingSlot> {
        storage::get_ring(&env, &asset)
    }

    /// Review item C5: whether epoch `epoch` is EFFECTIVELY final right
    /// now: its stored state is `Final`, or it is `Pending` and
    /// `pending_until` has already passed. `false` for a missing epoch.
    pub fn is_final(env: Env, asset: Address, epoch: u64) -> bool {
        let now = env.ledger().timestamp();
        storage::is_final(&env, &asset, epoch, now)
    }

    /// Review item C5: a window read of effective state per epoch, for
    /// `EventRegistry`'s Tier 1 checks (Section 8.2), so it can learn
    /// which epochs in a window are final without one call per epoch.
    /// Missing epochs read `None`, the same convention as `ring`'s
    /// underlying storage.
    pub fn effective_window(
        env: Env,
        asset: Address,
        start_epoch: u64,
        count: u32,
    ) -> Vec<Option<sylox_types::SlotState>> {
        let now = env.ledger().timestamp();
        storage::get_effective_window(&env, &asset, start_epoch, count, now)
    }

    /// technical-doc.md Section 12.1, 6. Review item C1: a PURE read.
    /// Returns whatever `RiskScore` is currently stored, with `stale`
    /// recomputed fresh against the clock (so a score that was fresh
    /// when last written but has since gone quiet is reported stale
    /// without needing a write to say so). Never calls
    /// `storage::set_score` or any other write; the actual computation
    /// happens in `recompute_score`, called from `post_signals`, the
    /// finality sweep, and `finalize_endpoint` (see those for exactly
    /// when).
    ///
    /// Review decision D1: the stored `band` is always the plain
    /// hysteresis band, computed with no knowledge of the event-in-
    /// progress flag (see `recompute_score`), so that flipping the
    /// flag off is never confused with a genuine new epoch of evidence
    /// for the hysteresis streak. The "forced to at least Distress
    /// while in progress" override is applied here instead, on every
    /// read, which also makes it take effect immediately on
    /// `set_event_in_progress` with no recompute required.
    pub fn score(env: Env, asset: Address) -> Result<RiskScore, Error> {
        storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        let stored = storage::get_score(&env, &asset);
        let newest_final = storage::get_newest_final(&env, &asset);

        let result = match (stored, newest_final) {
            (Some(current), Some(newest_final)) if current.band != Band::Event => RiskScore {
                stale: is_stale_epoch(&env, newest_final, SCORE_STALE_AFTER_EPOCHS),
                ..current
            },
            (Some(current), _) => current, // Event band stays as stored regardless of staleness.
            (None, _) => stale_score(&None),
        };
        if result.band != Band::Event
            && storage::get_event_in_progress(&env, &asset)
            && result.band < Band::Distress
        {
            return Ok(RiskScore {
                band: Band::Distress,
                ..result
            });
        }
        Ok(result)
    }

    pub fn band(env: Env, asset: Address) -> Result<Band, Error> {
        Ok(Self::score(env, asset)?.band)
    }

    /// Read-only, no auth. Reports how much history an asset has and
    /// whether it satisfies all conditions for score computation.
    /// Shares the history gate with scoring so the two never drift.
    pub fn history_status(env: Env, asset: Address) -> Result<HistoryStatus, Error> {
        storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;

        let first_epoch = storage::get_first_epoch(&env, &asset);
        let newest_epoch = storage::get_newest_epoch_pub(&env, &asset);
        let newest_final = storage::get_newest_final(&env, &asset);

        match (first_epoch, newest_final) {
            (Some(first), Some(final_ep)) => {
                let (gate, _) = score::history_gate(&env, &asset, final_ep, first);
                let newest_final_stale = is_stale_epoch(&env, final_ep, SCORE_STALE_AFTER_EPOCHS);
                let score_ready = gate.passes && !newest_final_stale;

                Ok(HistoryStatus {
                    first_epoch,
                    newest_epoch,
                    newest_final,
                    span_epochs: gate.span_epochs,
                    epochs_until_span_ready: gate.epochs_until_span_ready,
                    present_epochs_7d: gate.present_epochs_7d,
                    min_present_epochs_7d: score::MIN_FINAL_EPOCHS_7D,
                    newest_slot_present: gate.newest_slot_present,
                    newest_final_stale,
                    score_ready,
                })
            }
            _ => Ok(HistoryStatus {
                first_epoch,
                newest_epoch,
                newest_final,
                span_epochs: 0,
                epochs_until_span_ready: AGGREGATE_SLOTS_7D as u64,
                present_epochs_7d: 0,
                min_present_epochs_7d: score::MIN_FINAL_EPOCHS_7D,
                newest_slot_present: false,
                newest_final_stale: true,
                score_ready: false,
            }),
        }
    }

    /// `asset_stale` means "the asset TRANSITIONED into stale", not "a
    /// newly observed epoch
    /// happened to already be stale on arrival" (the old design's
    /// condition, which never fired at all for the realistic "keepers
    /// stopped posting" case, since nothing new ever arrives to
    /// trigger it). Permissionless: evaluates staleness against the
    /// epoch of the STORED score (never `newest_final`, which can run
    /// ahead of the stored score when `recompute_score` short
    /// circuits without writing, e.g. a sticky `Event` band or not
    /// enough history yet), emits `asset_stale` once if the asset is
    /// stale and `stale_announced` is not already set, sets the flag,
    /// and returns the current stale state either way. Calling this
    /// again while still stale emits nothing. Every state changing
    /// call that touches an asset runs the same check internally (see
    /// `check_stale_internal`), so a late backfill that is stale on
    /// arrival also emits, once, without needing anyone to call this
    /// explicitly; this function exists for a monitor that wants to
    /// poll every asset itself (Section 22.4 in the upcoming spec
    /// v1.2 PR) without waiting for keeper activity to drive it.
    pub fn check_stale(env: Env, asset: Address) -> Result<bool, Error> {
        sylox_types::ttl::extend_instance(&env);
        storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        check_stale_internal(&env, &asset)
    }

    pub fn is_stale(env: Env, asset: Address) -> bool {
        match storage::get_newest_epoch_pub(&env, &asset) {
            Some(newest) => is_stale_epoch(&env, newest, STALE_AFTER_EPOCHS),
            None => true,
        }
    }

    /// technical-doc.md Section 11.1: median of the newest 168 slots'
    /// `liquidity_2pct`, for the asset wide cover cap. Missing slots in
    /// the window are excluded, not treated as zero liquidity, so a short
    /// run of missing epochs does not crater the cap the way real zero
    /// liquidity would.
    pub fn median_liquidity(env: Env, asset: Address) -> i128 {
        let Some(newest) = storage::get_newest_epoch_pub(&env, &asset) else {
            return 0;
        };
        // `newest` alone is always far larger than
        // `MEDIAN_WINDOW_SLOTS` on a real network (unix-time-derived
        // epoch numbers), so this must be measured against how long
        // THIS asset has actually been posting, not against the
        // absolute epoch number.
        let Some(first) = storage::get_first_epoch(&env, &asset) else {
            return 0;
        };
        if newest + 1 < first + MEDIAN_WINDOW_SLOTS as u64 {
            return 0;
        }
        let start = newest + 1 - MEDIAN_WINDOW_SLOTS as u64;
        let window = storage::get_window(&env, &asset, start, MEDIAN_WINDOW_SLOTS);
        let mut liquidity = Vec::new(&env);
        for i in 0..MEDIAN_WINDOW_SLOTS {
            if let Some(slot) = window.get(i).flatten() {
                liquidity.push_back(slot.liquidity_2pct);
            }
        }
        if liquidity.is_empty() {
            0
        } else {
            math::median(&liquidity)
        }
    }

    /// technical-doc.md Section 12.1: reference to USD, SCALE 1e7. `Usd`
    /// is always `SCALE`; `Fiat` reads the asset's `FxAdapter` on the
    /// reference's recorded basis and fails closed on a stale or missing
    /// rate; `Asset` has no USD rate defined anywhere in the spec, so this
    /// fails rather than guessing one (see the PR's "Spec deviations").
    pub fn reference_rate(env: Env, asset: Address) -> Result<i128, Error> {
        let cfg = storage::get_asset_config(&env, &asset).ok_or(Error::UnknownAsset)?;
        match cfg.reference {
            sylox_types::Reference::Usd => Ok(SCALE),
            sylox_types::Reference::Fiat(code, rate_source) => {
                let adapter = cfg.fx_adapter.ok_or(Error::ReferenceRateUnavailable)?;
                let (rate, timestamp) =
                    FxAdapterClient::new(&env, &adapter).rate(&code, &rate_source);
                if is_stale_timestamp(&env, timestamp) {
                    return Err(Error::ReferenceRateUnavailable);
                }
                Ok(rate)
            }
            sylox_types::Reference::Asset(_) => Err(Error::ReferenceRateUnavailable),
        }
    }

    pub fn assets(env: Env) -> Vec<Address> {
        storage::get_assets(&env)
    }

    fn require_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&CONFIG_KEY)
            .ok_or(Error::NotInitialized)
    }
}

fn stale_score(stored: &Option<RiskScore>) -> RiskScore {
    match stored {
        Some(s) => RiskScore { stale: true, ..*s },
        None => RiskScore {
            epoch: 0,
            score: 0,
            band: Band::Normal,
            formula_version: 0,
            stale: true,
        },
    }
}

fn l_target_for(cfg: &AssetConfig) -> i128 {
    let _ = cfg;
    // Section 6.1: "L_target per asset". AssetConfig has no l_target field
    // in contracts/types as it exists today; see the PR's "Spec
    // deviations" section. A conservative stand-in (min_liquidity, the one
    // per-asset liquidity figure AssetConfig does carry) is used until the
    // type is extended.
    cfg_min_liquidity_as_l_target(cfg)
}

fn cfg_min_liquidity_as_l_target(cfg: &AssetConfig) -> i128 {
    cfg.min_liquidity
}

fn signal_dispute_bond() -> i128 {
    10_000_000_000
}

fn keeper_slash() -> i128 {
    10_000_000_000
}

fn is_stale_timestamp(env: &Env, timestamp: u64) -> bool {
    let now = env.ledger().timestamp();
    now.saturating_sub(timestamp) > STALE_AFTER_EPOCHS * EPOCH_SECS
}

/// technical-doc.md Section 5.5: an asset (or, here, a specific epoch
/// candidate) is stale if more than `threshold` epochs have passed
/// since it. Shared by every caller that judges staleness from an
/// epoch (as opposed to `is_stale_timestamp`'s external-adapter
/// timestamps, a different signal entirely): each passes its own
/// threshold explicitly rather than reaching for a shared constant,
/// since `STALE_AFTER_EPOCHS` (the depeg-coverage bound `is_stale`
/// reads) and `SCORE_STALE_AFTER_EPOCHS` (the score-freshness bound
/// `score()`/`check_stale` read) are deliberately different numbers
/// for different questions (see both constants' own doc comments).
fn is_stale_epoch(env: &Env, epoch: u64, threshold: u64) -> bool {
    let now = env.ledger().timestamp();
    let current_epoch = now / EPOCH_SECS;
    current_epoch.saturating_sub(epoch) > threshold
}

/// The shared implementation behind both the
/// public, permissionless `check_stale` and the internal call every
/// state changing function makes on its own asset. Judges staleness
/// against the STORED score's own epoch (a brand new asset with no
/// score at all is stale by definition, matching `score()`'s own
/// `(None, _)` branch), emits `asset_stale` exactly on the transition
/// into stale, and clears `stale_announced` the moment a fresh,
/// non-stale score is on record (no separate "recovered" event:
/// `score_updated` already signals that a fresh score landed).
fn check_stale_internal(env: &Env, asset: &Address) -> Result<bool, Error> {
    let stored = storage::get_score(env, asset);
    let stale = match &stored {
        Some(current) => is_stale_epoch(env, current.epoch, SCORE_STALE_AFTER_EPOCHS),
        None => true,
    };
    let announced = storage::get_stale_announced(env, asset);
    if stale && !announced {
        events::AssetStale {
            asset: asset.clone(),
            last_epoch: stored.map(|s| s.epoch).unwrap_or(0),
        }
        .publish(env);
        storage::set_stale_announced(env, asset, true);
    } else if !stale && announced {
        storage::set_stale_announced(env, asset, false);
    }
    Ok(stale)
}

#[derive(Clone)]
#[soroban_sdk::contracttype]
struct Config {
    governor: Address,
    registry: Address,
    staking: Address,
}

const CONFIG_KEY: Symbol = symbol_short!("CONFIG");

/// `set_governor`'s own one-time lock, same shape as EventRegistry's
/// `FACTORY_SET_KEY`: absent until the first successful `set_governor`
/// call, then permanently present, so a second call always fails.
const GOVERNOR_SET_KEY: Symbol = symbol_short!("GOV_SET");

/// Issue #12. Bump by hand on every `upgrade()` this contract ships.
const CONTRACT_VERSION: u32 = 2;
/// Issue #12. Instance key for `sylox_types::upgrade::do_migrate`'s own
/// "which version has this contract's storage already been migrated to"
/// marker.
const MIGRATED_TO_KEY: Symbol = symbol_short!("MIGRATE");

/// technical-doc.md Section 5.2 (backfill), 11.3 (epoch bound). Epoch `n`
/// covers `[n * epoch_secs, (n + 1) * epoch_secs)` from the Unix epoch
/// (`genesis = 0`), approved: epoch numbering is a pure function of
/// ledger time, with no stored genesis to initialize or keep in sync
/// across contracts. See the PR's "Spec deviations" section.
fn check_epoch_window(epoch: u64, now: u64) -> Result<(), Error> {
    let current_epoch = now / EPOCH_SECS;
    if epoch >= current_epoch {
        return Err(Error::WrongEpoch);
    }
    let epoch_close = (epoch + 1) * EPOCH_SECS;
    if now.saturating_sub(epoch_close) > WINDOW_SECS {
        return Err(Error::WrongEpoch);
    }
    Ok(())
}

// -- sub-epochs (technical-doc.md Section 5.9, v1.5) --

/// Shared body of `sub_peg_ratios` and `sub_peg_ratios_batch`: `hour`'s
/// own sub-epoch peg ratios, in `sub` order, `None` for a sub-epoch
/// that is not posted, Disputed, or not yet effectively Pending/Final
/// (the same "Pending or Final" convention `cover_gate`'s own
/// `RecentDepeg` check already read from `Ring(asset)` before v1.5,
/// extended to `Sub(asset)`/`HeldHour` for an hour that has not built
/// yet).
fn sub_peg_ratios_for_one_hour(env: &Env, asset: &Address, hour: u64) -> Vec<Option<i128>> {
    let sub_epoch_secs = current_sub_epoch_secs(env, asset, hour);
    let per_hour = sub_epochs_per_hour(sub_epoch_secs);
    let mut out = Vec::new(env);
    // Section 5.9 S3: checks HeldHour the same way sub_disposition
    // does, not Sub(asset)'s ring alone. Without this, a held
    // hour (one with an open or formerly-open dispute, whose data
    // Sub(asset)'s own ring may since have rotated past) would
    // read every sub-epoch as None here, going blind to EXACTLY
    // the depeg-window reads cover_gate's own RecentDepeg check
    // depends on for an hour that has not built yet. This touches
    // at most ONE extra key for the WHOLE hour (R10): `state ==
    // Disputed` is read straight off whichever slot (held or ring)
    // this function already fetched, never a separate `SubDispute`
    // lookup per sub-epoch.
    let held = storage::get_held_hour(env, asset, hour);
    for sub in 0..per_hour {
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        if let Some(map) = &held {
            match map.get(sub) {
                // A currently Disputed sub-epoch reads None here,
                // same as the ring path below: spec Section 5.9 S5's
                // own "Pending or Final" wording for what cover_gate
                // reads, Disputed deliberately excluded (its own
                // value is unverified while a ruling is pending).
                Some(slot) if slot.state != sylox_types::SlotState::Disputed => {
                    out.push_back(Some(slot.peg_ratio));
                }
                _ => out.push_back(None),
            }
            continue;
        }
        match storage::get_sub_slot(env, asset, sub_start) {
            Some(slot)
                if slot.state != sylox_types::SlotState::Empty
                    && slot.state != sylox_types::SlotState::Disputed =>
            {
                out.push_back(Some(slot.peg_ratio));
            }
            _ => out.push_back(None),
        }
    }
    out
}

/// Which `sub_epoch_secs` governs `hour`: the asset's current value,
/// unless a pending change's own `effective_from_hour` is at or
/// before `hour`, in which case the pending value governs instead
/// (Section 5.9 S1's own "takes effect from the next hour boundary
/// only"). `SUB_EPOCH_SECS_DEFAULT` if the asset has no
/// `SubEpochConfig` at all yet (governance has never called
/// `set_sub_epoch_secs`): this value is used only to size an
/// otherwise-unconfigured asset's FIRST sub-epoch post, since
/// `post_sub_signals` itself requires a config to already exist (see
/// its own doc comment for why an asset that never opts in simply
/// never sees a sub-epoch post at all, keeping I22 satisfied by
/// construction).
///
/// An hour that has had a sub-epoch posted resolves to the length it was
/// posted at (`HourSubEpochSecs`, recorded by its first post), whatever
/// the config says now: an unbuilt hour can be built at any age, so the
/// config alone is not enough once a later change has been promoted over
/// it. Any other hour resolves through the config.
fn current_sub_epoch_secs(env: &Env, asset: &Address, hour: u64) -> u64 {
    storage::get_hour_sub_epoch_secs(env, asset, hour)
        .unwrap_or_else(|| config_sub_epoch_secs(env, asset, hour))
}

/// The length the config puts in force for `hour`: the pending value
/// from `effective_from_hour` on, the current value before it. Exact
/// for any hour less than 240 hours older than the previous change
/// (`SUB_EPOCH_CHANGE_SPACING_HOURS`), which covers every hour still
/// inside `Sub(asset)`'s 5 hour span.
fn config_sub_epoch_secs(env: &Env, asset: &Address, hour: u64) -> u64 {
    match storage::get_sub_epoch_config(env, asset) {
        Some(cfg) => match (cfg.pending_sub_epoch_secs, cfg.effective_from_hour) {
            (Some(pending), Some(effective_from_hour)) if hour >= effective_from_hour => pending,
            _ => cfg.sub_epoch_secs,
        },
        None => SUB_EPOCH_SECS_DEFAULT,
    }
}

/// How many sub-epochs `hour` is divided into under whichever
/// `sub_epoch_secs` governs it (Section 5.9 S2: `sub` runs
/// `0..(3,600 / sub_epoch_secs)`).
fn sub_epochs_per_hour(sub_epoch_secs: u64) -> u32 {
    (EPOCH_SECS / sub_epoch_secs) as u32
}

/// Sub-epoch `sub` of hour `hour`'s own absolute start time: `hour *
/// EPOCH_SECS + sub * sub_epoch_secs` (Section 5.9 S2). This is what
/// `Sub(asset)`'s own ring stores as each slot's identity (Section
/// 5.9 S3), converted from the `(hour, sub)` pair the posting API
/// addresses a sub-epoch by.
fn sub_start_of(hour: u64, sub: u32, sub_epoch_secs: u64) -> u64 {
    hour * EPOCH_SECS + sub as u64 * sub_epoch_secs
}

/// The inverse of `sub_start_of`: recovers `(hour, sub)` from a
/// sub-epoch's own start time, using whichever `sub_epoch_secs`
/// governs that hour.
fn hour_and_sub_of(env: &Env, asset: &Address, sub_start: u64) -> (u64, u32) {
    let hour = sub_start / EPOCH_SECS;
    let sub_epoch_secs = current_sub_epoch_secs(env, asset, hour);
    let sub = ((sub_start % EPOCH_SECS) / sub_epoch_secs) as u32;
    (hour, sub)
}

/// Section 5.9 S4: a waiting hour's own effective state, the sub-epoch
/// analog of `storage::effective_state`. A `Pending` slot with
/// `pending_until = u64::MAX` (the sentinel every waiting hour and
/// every individual sub-epoch post uses while genuinely open) can
/// never satisfy `now >= pending_until`, so it never auto-promotes;
/// an ordinary, finite `pending_until` (a sub-epoch's own post, before
/// its dispute window closes) promotes exactly like the hourly ring
/// already does.
fn effective_sub_state(
    state: sylox_types::SlotState,
    pending_until: u64,
    now: u64,
) -> sylox_types::SlotState {
    if state == sylox_types::SlotState::Pending && now >= pending_until {
        sylox_types::SlotState::Final
    } else {
        state
    }
}

/// Section 5.9 S4: the sentinel `pending_until` a waiting hour's own
/// `Ring(asset)` slot carries while any of its sub-epochs is still
/// Pending or Disputed. Never derived by adding anything to `now` or
/// any other value; always this bare literal, so it can never
/// overflow at a write site the way `now + u64::MAX` would.
const HOUR_PENDING_SENTINEL: u64 = u64::MAX;

/// Section 5.9 S2: a sub-epoch may be backfilled for `SUB_BACKFILL_SECS`
/// only; past that, the keeper falls back to the hourly path
/// (`check_epoch_window`, unchanged). Mirrors `check_epoch_window`'s
/// own shape exactly, with `SUB_BACKFILL_SECS` in place of `WINDOW_SECS`.
fn check_sub_epoch_window(sub_start: u64, sub_epoch_secs: u64, now: u64) -> Result<(), Error> {
    let sub_close = sub_start + sub_epoch_secs;
    if sub_close > now {
        return Err(Error::WrongEpoch);
    }
    if now.saturating_sub(sub_close) > SUB_BACKFILL_SECS {
        return Err(Error::WrongEpoch);
    }
    Ok(())
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): a repost of an
/// overturned sub-epoch gets its OWN window, anchored to whichever
/// is later, the sub-epoch's own original close or the ruling that
/// overturned it: `max(sub_close, overturned_at) + sub_backfill_secs`.
/// A ruling can take up to `signal_dispute_ruling_secs` (6 days),
/// far longer than `sub_backfill_secs` (2 hours), so anchoring to the
/// original close alone (`check_sub_epoch_window`'s own rule) would
/// make a repost unreachable for any ruling slower than the backfill
/// window itself; anchoring to the overturn instead keeps a repost
/// reliably reachable regardless of how long the ruling took, the
/// same guarantee ADR-005 already gives the hourly path (where no
/// such race exists, since an hourly epoch has no separate backfill
/// window racing its own dispute timeline the way a sub-epoch does).
fn check_sub_epoch_repost_window(
    sub_close: u64,
    overturned_at: u64,
    now: u64,
) -> Result<(), Error> {
    let anchor = sub_close.max(overturned_at);
    if now.saturating_sub(anchor) > SUB_BACKFILL_SECS {
        return Err(Error::WrongEpoch);
    }
    Ok(())
}

/// Section 5.9 S4: one sub-epoch's disposition, within the hour-build
/// and provisional-roll-up logic. Distinct from (but structurally the
/// same shape as) `EventRegistry`'s own `epoch_disposition`: this one
/// reads `Sub(asset)`'s ring directly (same contract), never crosses
/// a contract boundary, and treats "missing" as two separate cases a
/// caller needs to tell apart (still within `sub_backfill_secs`, vs.
/// genuinely gone).
enum SubDisposition {
    Final(RingSlot),
    /// Posted, not yet past its own `pending_until`: still Pending,
    /// never Disputed (that case is `Disputed` below, checked first).
    /// Carries the slot's own real posted data, same shape as
    /// `Final`, so a provisional roll-up (Section 5.9 S4's own
    /// "recomputed on every sub-epoch post") can use a Pending
    /// sub-epoch's real values instead of treating it as absent.
    Pending(RingSlot),
    /// `state == Disputed` on the already-fetched slot (`HeldHour` or
    /// the ring, whichever `sub_disposition` is currently reading):
    /// never a separate `SubDispute` lookup (the Section 5.9 S5
    /// footprint fix), so this reads correctly even once `Sub(asset)`'s
    /// own ring has rotated past this sub-epoch's slot and would
    /// otherwise misreport it `PermanentlyMissing`.
    Disputed,
    /// Never posted, still within `sub_backfill_secs` of its own
    /// close: could still be backfilled.
    MissingWithinBackfill,
    /// Overturned (a dispute the disputer won), with its own
    /// `SubOverturned` record present and not yet `reposted`, and
    /// still within its OWN repost window (`check_sub_epoch_
    /// repost_window`, anchored to the later of the original close
    /// or the overturn itself, Section 5.9 S2). Distinct from
    /// `PermanentlyMissing`: an hour must NOT build while any
    /// sub-epoch is awaiting a possible repost, the same way it must
    /// not build while one is still `MissingWithinBackfill`.
    AwaitingRepost,
    /// Never posted (or posted, then overturned, with no repost
    /// either within its own repost window or at all), past
    /// `sub_backfill_secs` AND past the repost window if an overturn
    /// exists: permanently missing, exactly the hourly ring's own
    /// `PermanentlyMissing` concept.
    PermanentlyMissing,
}

/// Converts a `SubRingSlot` into the shape `roll_up_sub_slots` (and
/// the existing `median`/averaging helpers, which already operate on
/// `RingSlot`-shaped data) can consume, tagging it with `hour` since
/// `RingSlot.epoch` is unused by the roll-up but required by the
/// type.
fn sub_slot_to_ring_slot(hour: u64, slot: &storage::SubRingSlot) -> RingSlot {
    RingSlot {
        epoch: hour,
        state: slot.state,
        pending_until: slot.pending_until,
        peg_ratio: slot.peg_ratio,
        liquidity_2pct: slot.liquidity_2pct,
        redemption_net: slot.redemption_net,
        supply: slot.supply,
        supply_change_bps: slot.supply_change_bps,
        clawback_amount: slot.clawback_amount,
        auth_revocations: slot.auth_revocations,
        endpoint: slot.endpoint,
        // A single sub-epoch's own data, not the hour's rolled-up
        // slot: coverage is a property of the hour, not one sub-epoch.
        provisional_sub_coverage: None,
    }
}

/// Checks `HeldHour(asset, hour)` first (if this hour has one at all,
/// i.e. SOME sub-epoch in it was disputed at some point), since the
/// ring may have rotated past this sub-epoch's own slot even though
/// it was never itself disputed; only when `HeldHour` does not exist
/// for this hour at all does this fall back to reading `Sub(asset)`'s
/// ring directly, the ordinary path for the overwhelming majority of
/// hours that never have any dispute. Either way, `state ==
/// Disputed` is read straight off the already-fetched slot (held or
/// ring), never from a separate `SubDispute` lookup: `dispute_sub_
/// signals` writes `Disputed` into BOTH the instant a dispute opens
/// (and always creates a `HeldHour` entry for the hour in the same
/// call, so there is no window where the ring says `Disputed` but no
/// held entry exists yet to carry that forward past ring rotation).
/// `SubDispute(asset, hour, sub)` itself is read only by the actual
/// dispute-resolution paths (`dispute_sub_signals`, `resolve_sub_
/// signal_dispute`, `resolve_sub_dispute_timeout`), which need its
/// own `disputer`/`alt_hash`/`opened_at` fields; this disposition
/// check and `sub_peg_ratios_for_one_hour`'s own provisional read
/// never touch that key at all, so a scan across many sub-epochs
/// touches at most one key per hour (`HeldHour` or `Sub(asset)`), not
/// one per sub-epoch, keeping footprint flat regardless of how many
/// hours are scanned (R10).
fn sub_disposition(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
    sub_start: u64,
    sub_epoch_secs: u64,
    now: u64,
) -> SubDisposition {
    if let Some(held) = storage::get_held_hour(env, asset, hour) {
        return match held.get(sub) {
            Some(slot) => {
                if slot.state == sylox_types::SlotState::Disputed {
                    return SubDisposition::Disputed;
                }
                let effective = effective_sub_state(slot.state, slot.pending_until, now);
                let ring_slot = held_slot_to_ring_slot(hour, &slot);
                match effective {
                    sylox_types::SlotState::Final => SubDisposition::Final(ring_slot),
                    _ => SubDisposition::Pending(ring_slot),
                }
            }
            // Removed from HeldHour by resolve_sub_signal_dispute's
            // rejection branch (remove_held_sub_slot): an overturn, not
            // simply never-held. Same missing-vs-awaiting-repost check
            // as the ring's own fallback below.
            None => {
                sub_disposition_for_missing(env, asset, hour, sub, sub_start, sub_epoch_secs, now)
            }
        };
    }
    match storage::get_sub_slot(env, asset, sub_start) {
        Some(slot) => {
            if slot.state == sylox_types::SlotState::Disputed {
                return SubDisposition::Disputed;
            }
            let effective = effective_sub_state(slot.state, slot.pending_until, now);
            let ring_slot = sub_slot_to_ring_slot(hour, &slot);
            match effective {
                sylox_types::SlotState::Final => SubDisposition::Final(ring_slot),
                _ => SubDisposition::Pending(ring_slot),
            }
        }
        None => sub_disposition_for_missing(env, asset, hour, sub, sub_start, sub_epoch_secs, now),
    }
}

/// Section 5.9 S2 (v1.5, footprint-fix revision): shared by both of
/// `sub_disposition`'s own "nothing here" branches (no `HeldHour`
/// entry for this sub, no ring slot for this `sub_start` either): a
/// sub-epoch with no live data anywhere is either genuinely never
/// posted, or was posted and then overturned. An overturned sub-epoch
/// not yet reposted, still within its own repost window, must read
/// `AwaitingRepost`, never `PermanentlyMissing`: the hour must not
/// build (treating the overturned value as finally, permanently
/// absent) while a correction could still arrive.
fn sub_disposition_for_missing(
    env: &Env,
    asset: &Address,
    hour: u64,
    sub: u32,
    sub_start: u64,
    sub_epoch_secs: u64,
    now: u64,
) -> SubDisposition {
    let sub_close = sub_start + sub_epoch_secs;
    if let Some(record) = storage::get_overturned_sub_signals(env, asset, hour, sub) {
        if !record.reposted
            && check_sub_epoch_repost_window(sub_close, record.overturned_at, now).is_ok()
        {
            return SubDisposition::AwaitingRepost;
        }
        return SubDisposition::PermanentlyMissing;
    }
    if now.saturating_sub(sub_close) > SUB_BACKFILL_SECS {
        SubDisposition::PermanentlyMissing
    } else {
        SubDisposition::MissingWithinBackfill
    }
}

/// Section 5.9 S3: the first time ANY sub-epoch of `hour` is disputed,
/// copies every OTHER currently-readable (Pending or Final; a
/// Disputed sub-epoch is tracked separately via its own `SubDispute`
/// entry, which `sub_disposition` always checks first regardless of
/// `HeldHour`) sub-epoch of that hour out of `Sub(asset)`'s ring into
/// `HeldHour(asset, hour)`, so the hour's full picture survives the
/// ring rotating past it while this (or any later) dispute against
/// the same hour is still open. A sub-epoch captured while still
/// Pending keeps becoming effectively Final on its own over time: its
/// `state`/`pending_until` are stored alongside it, and
/// `sub_disposition`'s own `HeldHour` branch applies
/// `effective_sub_state` the same way a ring read would. A no-op if
/// `hour` already has a held entry (a second dispute against the same
/// hour, or a dispute reopened after a resolution): the earlier
/// dispute already did this scan, and every sub-epoch posted since
/// has kept the held map current via `post_sub_signals`'s own insert.
fn ensure_hour_held(env: &Env, asset: &Address, hour: u64, sub_epoch_secs: u64) {
    if storage::get_held_hour(env, asset, hour).is_some() {
        return;
    }
    let per_hour = sub_epochs_per_hour(sub_epoch_secs);
    for sub in 0..per_hour {
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        if let Some(slot) = storage::get_sub_slot(env, asset, sub_start) {
            // Defensive: in practice the ring never shows Disputed
            // here except for the ONE sub whose own dispute_sub_
            // signals call is what triggered this scan, and that
            // sub's own held entry is inserted separately, right
            // after this scan, with state forced to Disputed
            // directly (not whatever this stale Pending read would
            // otherwise capture).
            if slot.state == sylox_types::SlotState::Disputed {
                continue;
            }
            storage::insert_held_sub_slot(
                env,
                asset,
                hour,
                sub,
                &storage::HeldSubSlot {
                    state: slot.state,
                    pending_until: slot.pending_until,
                    peg_ratio: slot.peg_ratio,
                    liquidity_2pct: slot.liquidity_2pct,
                    redemption_net: slot.redemption_net,
                    supply: slot.supply,
                    clawback_amount: slot.clawback_amount,
                    auth_revocations: slot.auth_revocations,
                },
                HELD_HOUR_TTL_LEDGERS,
            );
        }
    }
}

/// Converts a held sub-epoch's roll-up fields into `RingSlot`'s
/// shape, the same conversion `sub_slot_to_ring_slot` gives a
/// ring-read `SubRingSlot`. Called for both a Final and a Pending
/// disposition (`sub_disposition`'s own caller decides which one this
/// slot's data feeds into: the final build or the provisional
/// roll-up); `state`/`pending_until` are never read back out of the
/// result by either (`roll_up_sub_slots` only reads the roll-up
/// fields themselves), so `RingSlot::state` here is a placeholder,
/// not a real disposition. `endpoint` has no held equivalent (Section
/// 5.9 S4: probes stay hourly), filled with its own placeholder for
/// the same reason.
fn held_slot_to_ring_slot(hour: u64, slot: &storage::HeldSubSlot) -> RingSlot {
    RingSlot {
        epoch: hour,
        state: sylox_types::SlotState::Final,
        pending_until: 0,
        peg_ratio: slot.peg_ratio,
        liquidity_2pct: slot.liquidity_2pct,
        redemption_net: slot.redemption_net,
        supply: slot.supply,
        supply_change_bps: 0,
        clawback_amount: slot.clawback_amount,
        auth_revocations: slot.auth_revocations,
        endpoint: EndpointStatus::Unknown,
        // A single sub-epoch's own data, same as sub_slot_to_ring_slot.
        provisional_sub_coverage: None,
    }
}

/// Section 5.9 S4: re-derives `hour`'s own `Ring(asset)` slot state
/// from its sub-epochs' current dispositions, called after every
/// sub-epoch post, dispute, resolution or timeout that could have
/// changed the hour's own picture. Three outcomes:
///
/// - Every sub-epoch decided (Final, PermanentlyMissing, or genuinely
///   absent past its own backfill window) and none Disputed: attempts
///   the build (`try_build_hour`).
/// - Otherwise (at least one sub-epoch Disputed, still Pending, or
///   MissingWithinBackfill): if at least one sub-epoch has actually
///   been POSTED (Pending or Final; a currently Disputed sub-epoch is
///   excluded from this list by the match arms below, same rule
///   `sub_peg_ratios` uses), the hour's slot becomes `Pending` or
///   `Disputed` (matching whether any sub-epoch is currently
///   disputed) with `pending_until = HOUR_PENDING_SENTINEL`, and its
///   fields hold a provisional roll-up computed from every posted,
///   non-disputed sub-epoch, tagged with `provisional_sub_coverage =
///   Some(posted_slots.len())`, so a Pending-tolerant reader (the
///   cover gate, display) sees the real posted values immediately,
///   not zeros waiting for the first sub-epoch to clear its own
///   dispute window, AND NOT a zeroed slot just because one OTHER
///   sub-epoch happens to be under dispute (Section 5.9 S5's own
///   footprint-fix review: a disputed sub-epoch must only exclude
///   itself, never its whole hour's other, healthy sub-epochs). If
///   NOTHING has been posted for this hour at all yet (including the
///   case where every posted sub-epoch is currently disputed, i.e.
///   `provisional_sub_coverage` would be `Some(0)`), the hour's own
///   ring slot is written (if disputed, so a dispute without ANY
///   healthy sub-epoch is still visible as `Disputed`) or left
///   untouched (if not disputed at all, genuinely `Empty`, not a
///   `Pending` slot holding an all-zero roll-up a Pending-tolerant
///   reader could mistake for real, if implausible, data).
fn refresh_waiting_hour(
    env: &Env,
    config: &Config,
    asset: &Address,
    hour: u64,
) -> Result<(), Error> {
    let now = env.ledger().timestamp();
    let sub_epoch_secs = current_sub_epoch_secs(env, asset, hour);
    let per_hour = sub_epochs_per_hour(sub_epoch_secs);

    let mut any_disputed = false;
    let mut all_decided = true;
    let mut final_slots: Vec<RingSlot> = Vec::new(env);
    let mut final_subs: Vec<u32> = Vec::new(env);
    // Every POSTED sub-epoch (Pending or Final, never Disputed), for
    // the provisional roll-up only; `final_slots`/`final_subs` above
    // stay Final-only, feeding the real build exactly as before.
    let mut posted_slots: Vec<RingSlot> = Vec::new(env);
    let mut posted_subs: Vec<u32> = Vec::new(env);

    for sub in 0..per_hour {
        let sub_start = sub_start_of(hour, sub, sub_epoch_secs);
        match sub_disposition(env, asset, hour, sub, sub_start, sub_epoch_secs, now) {
            SubDisposition::Final(slot) => {
                final_slots.push_back(slot.clone());
                final_subs.push_back(sub);
                posted_slots.push_back(slot);
                posted_subs.push_back(sub);
            }
            SubDisposition::Pending(slot) => {
                all_decided = false;
                posted_slots.push_back(slot);
                posted_subs.push_back(sub);
            }
            SubDisposition::Disputed => {
                any_disputed = true;
                all_decided = false;
            }
            SubDisposition::MissingWithinBackfill => all_decided = false,
            SubDisposition::AwaitingRepost => all_decided = false,
            SubDisposition::PermanentlyMissing => {}
        }
    }

    if all_decided && !any_disputed {
        return try_build_hour(env, config, asset, hour, &final_slots, &final_subs);
    }

    if posted_slots.is_empty() {
        if any_disputed {
            // Every posted sub-epoch is currently disputed: zero real
            // coverage, but the dispute itself must still be visible
            // (a brand-new hour's slot may never have been written at
            // all, so force_set_slot_disputed's own unconditional
            // write, not write_ring_slot's newer-wins-guarded one, is
            // still needed here). Some(0), never a bare zero peg_ratio
            // read as a real value: the gate treats this as no signal.
            storage::force_set_slot_disputed(
                env,
                asset,
                hour,
                HOUR_PENDING_SENTINEL,
                &empty_hour_signal_set(env, asset, hour),
                Some(0),
            );
        }
        // Nothing posted and nothing disputed either: leave the ring
        // slot untouched rather than writing a Pending sentinel over
        // genuinely no data.
        return Ok(());
    }

    let provisional = roll_up_sub_slots(env, config, asset, hour, &posted_slots, &posted_subs)?;
    let coverage = Some(posted_slots.len());
    if any_disputed {
        storage::force_set_slot_disputed(
            env,
            asset,
            hour,
            HOUR_PENDING_SENTINEL,
            &provisional,
            coverage,
        );
    } else {
        storage::write_ring_slot(
            env,
            asset,
            hour,
            &provisional,
            HOUR_PENDING_SENTINEL,
            coverage,
        );
    }
    Ok(())
}

/// technical-doc.md Section 5.9 S4: the roll-up table, applied to
/// whichever sub-epochs in `hour` are currently Final (`final_slots`,
/// in `sub` order). Produces a `SignalSet`-shaped value whether called
/// for the provisional (still-waiting) roll-up or the final build;
/// the two differ only in whether coverage was met and what state the
/// caller writes alongside this result, never in how a field is
/// computed.
///
/// - `peg_ratio`: mean of the Final sub-epochs' `peg_ratio`. Each
///   sub-epoch's own `peg_ratio` is already a TWAP, so the mean of
///   several TWAPs over contiguous, equal-length sub-windows is the
///   TWAP over their union; with missing sub-epochs, this is the TWAP
///   over the covered part of the hour only.
/// - `liquidity_2pct`: median of the Final sub-epochs.
/// - `supply`: the last Final sub-epoch's value (by `sub` order), a
///   point-in-time read, not an accumulation.
/// - `redemption_net`, `issuer_actions.clawback_amount`,
///   `issuer_actions.auth_revocations`: summed across the Final
///   sub-epochs.
/// - `supply_change_bps`: recomputed against the PREVIOUS BUILT hour's
///   own `supply` (read from `Ring(asset)`'s slot at `hour - 1`), not
///   summed or averaged from the sub-epochs' own per-sub-epoch change,
///   which would compound incorrectly.
/// - `endpoint`: unchanged, read from `Staking.aggregate(asset, hour)`
///   directly, since probes stay hourly (Section 5.9 S6) and there is
///   no sub-epoch endpoint value to roll up.
/// - `inputs_hash`: the hash of the Final sub-epochs' own `inputs_hash`
///   values, in `sub` order, so the result stays recomputable offchain
///   from exactly the sub-epoch data that built it.
///
/// `final_slots.is_empty()` still produces a well-defined (if
/// degenerate, all-zero) result: the caller (`refresh_waiting_hour`
/// for the provisional case, `try_build_hour` for the final case)
/// decides what to do with an empty or below-coverage result, this
/// function only computes the roll-up itself.
fn roll_up_sub_slots(
    env: &Env,
    config: &Config,
    asset: &Address,
    hour: u64,
    final_slots: &Vec<RingSlot>,
    final_subs: &Vec<u32>,
) -> Result<SignalSet, Error> {
    if final_slots.is_empty() {
        return Ok(empty_hour_signal_set(env, asset, hour));
    }

    let mut peg_ratio_sum: i128 = 0;
    let mut liquidity_values: Vec<i128> = Vec::new(env);
    let mut redemption_net: i128 = 0;
    let mut clawback_amount: i128 = 0;
    let mut auth_revocations: u32 = 0;
    let mut last_supply: i128 = 0;

    for slot in final_slots.iter() {
        peg_ratio_sum = peg_ratio_sum
            .checked_add(slot.peg_ratio)
            .ok_or(Error::MathOverflow)?;
        liquidity_values.push_back(slot.liquidity_2pct);
        redemption_net = redemption_net
            .checked_add(slot.redemption_net)
            .ok_or(Error::MathOverflow)?;
        clawback_amount = clawback_amount
            .checked_add(slot.clawback_amount)
            .ok_or(Error::MathOverflow)?;
        auth_revocations = auth_revocations
            .checked_add(slot.auth_revocations)
            .ok_or(Error::MathOverflow)?;
        last_supply = slot.supply;
    }

    let count = final_slots.len() as i128;
    let peg_ratio = peg_ratio_sum / count;
    let liquidity_2pct = math::median(&liquidity_values);

    let previous_supply = storage::get_slot(env, asset, hour.saturating_sub(1)).map(|s| s.supply);
    let supply_change_bps = match previous_supply {
        Some(prev) if prev > 0 => {
            let diff = last_supply.checked_sub(prev).ok_or(Error::MathOverflow)?;
            let scaled = diff.checked_mul(10_000).ok_or(Error::MathOverflow)?;
            (scaled / prev) as i32
        }
        _ => 0,
    };

    let inputs_hash = hash_sub_epoch_inputs(env, asset, hour, final_subs);
    // Same call post_signals already makes for an hourly post: if the
    // aggregate isn't ready yet, MockStaking/Staking's own aggregate
    // returns Unknown, exactly like a fresh hourly post starts; a later
    // finalize_endpoint(asset, hour) call corrects it once probes
    // settle, the same two-step path an hourly post already relies on.
    let endpoint = StakingClient::new(env, &config.staking).aggregate(asset, &hour);

    Ok(SignalSet {
        epoch: hour,
        posted_at: 0,
        peg_ratio,
        peg_ratio_p10: peg_ratio,
        liquidity_2pct,
        redemption_net,
        supply: last_supply,
        supply_change_bps,
        issuer_actions: sylox_types::IssuerActions {
            clawbacks: 0,
            clawback_amount,
            auth_revocations,
            flag_changes: 0,
        },
        endpoint,
        inputs_hash,
        poster: asset.clone(),
    })
}

fn empty_hour_signal_set(env: &Env, asset: &Address, hour: u64) -> SignalSet {
    SignalSet {
        epoch: hour,
        posted_at: 0,
        peg_ratio: 0,
        peg_ratio_p10: 0,
        liquidity_2pct: 0,
        redemption_net: 0,
        supply: 0,
        supply_change_bps: 0,
        issuer_actions: sylox_types::IssuerActions::default(),
        endpoint: EndpointStatus::Unknown,
        inputs_hash: BytesN::from_array(env, &[0u8; 32]),
        poster: asset.clone(),
    }
}

/// Hashes the Final sub-epochs' own `inputs_hash` values (read from
/// `SubSignals`, in `sub` order) into one combined hash, so a built
/// hour stays recomputable offchain from exactly the sub-epoch data
/// that built it (Section 5.1's own promise, extended). `final_subs`
/// is already in `sub` order (constructed that way by the one caller
/// that builds it, `refresh_waiting_hour`'s own `0..per_hour` scan).
fn hash_sub_epoch_inputs(
    env: &Env,
    asset: &Address,
    hour: u64,
    final_subs: &Vec<u32>,
) -> BytesN<32> {
    let mut combined = Bytes::new(env);
    for sub in final_subs.iter() {
        if let Some(signals) = storage::get_sub_signals(env, asset, hour, sub) {
            combined.append(&Bytes::from_array(env, &signals.inputs_hash.to_array()));
        }
    }
    env.crypto().sha256(&combined).into()
}

/// technical-doc.md Section 5.9 S4: builds `hour`, once every one of
/// its sub-epochs is decided (the only way `refresh_waiting_hour`
/// calls this). Writes `Final` with the full roll-up if
/// `MIN_SUB_COVERAGE_BPS` of the hour's sub-epochs are Final, or
/// `Empty` (a genuinely missing hour, exactly like today) if not.
/// Rewards every distinct keeper who posted a Final sub-epoch in this
/// hour (Section 5.9 S6), and, on a successful build, runs the same
/// hourly finality/score machinery `post_signals` already triggers
/// inline, since a built hour is a brand new Final epoch the score
/// aggregates have not yet seen. Public entry point is `build_hour`
/// (below); this is the shared implementation `refresh_waiting_hour`
/// also calls inline.
fn try_build_hour(
    env: &Env,
    config: &Config,
    asset: &Address,
    hour: u64,
    final_slots: &Vec<RingSlot>,
    final_subs: &Vec<u32>,
) -> Result<(), Error> {
    // A built hour never flips (I21/I23): write_ring_slot's own
    // newer-wins guard only blocks a STRICTLY newer epoch from being
    // overwritten (existing.epoch > epoch), not a second write for
    // the SAME epoch, so without this check a build_hour call
    // reached after the hour is already Final (e.g. a late dispute
    // resolution's own refresh_waiting_hour re-scan, running after
    // the ring has long since rotated past every one of this hour's
    // own sub-epoch slots) could otherwise overwrite an already-Final
    // hour back down to Pending-then-Final-or-Empty from a stale
    // recomputation. Once Final, this hour is done; nothing short of
    // a resolved dispute reaching it BEFORE this point ever runs
    // try_build_hour on it again in practice, but this guard makes
    // that guarantee explicit rather than incidental.
    let now = env.ledger().timestamp();
    if storage::is_final(env, asset, hour, now) {
        return Ok(());
    }

    let sub_epoch_secs = current_sub_epoch_secs(env, asset, hour);
    let per_hour = sub_epochs_per_hour(sub_epoch_secs);
    let coverage_bps = (final_slots.len() as u64 * 10_000 / per_hour as u64) as u32;

    if coverage_bps < MIN_SUB_COVERAGE_BPS {
        // A built hour below coverage is written straight to Empty,
        // exactly like a missing hour today (Section 5.9 S4), never
        // left in the Pending state write_ring_slot always produces
        // on its own: write first (so the position holds this hour's
        // own identity), then immediately clear it back to Empty,
        // reusing set_slot_overturned for the clear.
        storage::write_ring_slot(
            env,
            asset,
            hour,
            &empty_hour_signal_set(env, asset, hour),
            0,
            // Built (even below coverage, down to Empty): no longer a
            // waiting hour, so no provisional coverage concept applies.
            None,
        );
        storage::set_slot_overturned(env, asset, hour);
        // This hour's own held data (if any) is no longer needed once
        // built, Empty or Final: a build is a one-way transition
        // (storage.rs's own newer-wins guards never let it un-build).
        storage::clear_held_hour(env, asset, hour);
        events::HourBuilt {
            asset: asset.clone(),
            hour,
            sub_count_final: final_slots.len(),
            coverage_bps,
        }
        .publish(env);
        return Ok(());
    }

    let built = roll_up_sub_slots(env, config, asset, hour, final_slots, final_subs)?;
    // Mirrors post_signals's own set_signals call: without this, a
    // built hour has no Signals(asset, hour) entry at all, so latest(),
    // signals() and finalize_endpoint's own get_signals lookup would
    // all silently miss it (finalize_endpoint's late endpoint-aggregate
    // write in particular would just no-op forever).
    storage::set_signals(env, asset, hour, &built);
    // Built hours are written straight to Final (Section 5.9 S4):
    // every sub-epoch inside already went through the full
    // Pending/dispute lifecycle on its own, so this never re-enters
    // Pending or Disputed at the hour level. write_ring_slot itself
    // always writes Pending; set_slot_final immediately after moves
    // it to Final in the same call, mirroring how resolve_signal_
    // dispute's keeper-wins path already composes these two calls for
    // the hourly path.
    // Built, Final: no longer a waiting hour.
    let wrote = storage::write_ring_slot(env, asset, hour, &built, 0, None);
    if wrote {
        storage::set_slot_final(env, asset, hour);
    }
    storage::clear_held_hour(env, asset, hour);

    events::HourBuilt {
        asset: asset.clone(),
        hour,
        sub_count_final: final_slots.len(),
        coverage_bps,
    }
    .publish(env);

    reward_sub_epoch_keepers(env, config, asset, hour, final_subs, sub_epoch_secs);

    // A built hour is a brand new Final epoch the hourly score
    // machinery has not seen yet; run it exactly as post_signals
    // already does inline for the hourly path.
    if let Some(newest_final) = try_advance_finality(env, config, asset, FINALITY_LOOKBACK_EPOCHS) {
        recompute_score(env, asset, newest_final)?;
    }
    check_stale_internal(env, asset)?;
    Ok(())
}

/// Section 5.9 S6: rewards every distinct keeper who posted a Final
/// sub-epoch in `hour`, grouped by poster, mirroring
/// `reward_posters_for_newly_final_epochs`'s own grouping for the
/// hourly path. Called once per successful build, never per
/// sub-epoch, so a keeper who posted several sub-epochs in the same
/// hour is paid in one call.
fn reward_sub_epoch_keepers(
    env: &Env,
    config: &Config,
    asset: &Address,
    hour: u64,
    final_subs: &Vec<u32>,
    sub_epoch_secs: u64,
) {
    let mut counts: Map<Address, u32> = Map::new(env);
    for sub in final_subs.iter() {
        let Some(signals) = storage::get_sub_signals(env, asset, hour, sub) else {
            continue;
        };
        let count = counts.get(signals.poster.clone()).unwrap_or(0);
        counts.set(signals.poster, count + 1);
    }
    let staking = StakingClient::new(env, &config.staking);
    for (poster, count) in counts.iter() {
        staking.reward_keeper_sub_epochs(&poster, &count, &sub_epoch_secs);
    }
}

/// Review decision D3: `Reference::Asset` is rejected in v1. No part of
/// the spec defines a USD rate for an asset pegged reference (it would
/// presumably need that other asset's own `reference_rate`, recursively,
/// but nothing bounds how deep that could go); see the PR's "Spec
/// deviations" section.
fn reject_asset_reference(reference: &sylox_types::Reference) -> Result<(), Error> {
    if matches!(reference, sylox_types::Reference::Asset(_)) {
        return Err(Error::ReferenceNotSupported);
    }
    Ok(())
}

/// technical-doc.md Section 11.3, the bounds that do not need the ring:
/// - `peg_ratio`, `peg_ratio_p10`: 0 to 2 * SCALE each, and (per the
///   reviewer's required change) `peg_ratio_p10 <= peg_ratio`. Flagged in
///   the PR description: Section 11.3's own text says explicitly "No
///   ordering between them: a 10th percentile can sit above a volume
///   weighted mean when a few large trades print far below the rest",
///   which this check contradicts; implemented per the explicit review
///   instruction, not silently.
/// - `liquidity_2pct`, `supply`: 0 to `i128::MAX / SCALE`.
fn check_sanity_bounds(s: &SignalSet) -> Result<(), Error> {
    if s.peg_ratio < 0 || s.peg_ratio > 2 * SCALE {
        return Err(Error::SanityBoundFailed);
    }
    if s.peg_ratio_p10 < 0 || s.peg_ratio_p10 > 2 * SCALE {
        return Err(Error::SanityBoundFailed);
    }
    if s.peg_ratio_p10 > s.peg_ratio {
        return Err(Error::SanityBoundFailed);
    }
    if s.liquidity_2pct < 0 || s.liquidity_2pct > MAX_LIQUIDITY_OR_SUPPLY {
        return Err(Error::SanityBoundFailed);
    }
    if s.supply < 0 || s.supply > MAX_LIQUIDITY_OR_SUPPLY {
        return Err(Error::SanityBoundFailed);
    }
    Ok(())
}

/// `supply_change_bps` must match `supply` vs. the previous epoch's ring
/// slot (if any) within `SUPPLY_CHANGE_TOLERANCE_BPS`. Epoch 0 and an
/// asset with no previous slot both pass unconditionally: there is no
/// supply history yet to be inconsistent with.
fn check_supply_change_consistency(env: &Env, asset: &Address, s: &SignalSet) -> Result<(), Error> {
    if s.epoch == 0 {
        return Ok(());
    }
    let Some(previous) = storage::get_slot(env, asset, s.epoch - 1) else {
        return Ok(());
    };
    if previous.supply == 0 {
        // A zero previous supply makes "percent change" undefined; Section
        // 11.3 does not say what happens here, so this skips the
        // consistency check rather than dividing by zero or guessing a
        // convention the spec does not state.
        return Ok(());
    }

    let expected_bps = ((s.supply - previous.supply) * 10_000) / previous.supply;
    let diff = (expected_bps - s.supply_change_bps as i128).abs();
    if diff > SUPPLY_CHANGE_TOLERANCE_BPS {
        return Err(Error::SanityBoundFailed);
    }
    Ok(())
}

/// technical-doc.md Section 5.3 step 3, 11.3: cross checks every
/// configured `PriceAdapter` and rejects a posting whose `peg_ratio`
/// deviates more than `AMM_TOLERANCE_BPS` from an adapter's price, unless
/// that adapter's liquidity is below the asset's `min_liquidity` or its
/// timestamp is stale.
fn check_amm_cross_check(env: &Env, cfg: &AssetConfig, s: &SignalSet) -> Result<(), Error> {
    for adapter_address in cfg.amm_adapters.iter() {
        let (price, liquidity, timestamp) =
            PriceAdapterClient::new(env, &adapter_address).spot_price(&cfg.asset);
        if liquidity < cfg.min_liquidity {
            continue;
        }
        if is_stale_timestamp(env, timestamp) {
            continue;
        }
        let deviation_bps = ((s.peg_ratio - price).abs() * 10_000) / price.max(1);
        if deviation_bps > AMM_TOLERANCE_BPS {
            return Err(Error::AmmCrossCheckFailed);
        }
    }
    Ok(())
}

/// Review item C6 (re-review): finality is per epoch and INDEPENDENT.
/// A permanently missing epoch, an overturned epoch that is never
/// reposted, or an unresolved dispute must never block any OTHER
/// epoch's finality or the score from advancing past it. The previous
/// design (a sequential cursor walking forward one epoch at a time,
/// stopping dead at the first non-Final epoch) violated this; see
/// `finality_keeps_advancing_past_a_permanently_missing_epoch` and its
/// two siblings in `test.rs`, written to fail against that design
/// before this fix.
///
/// `newest_final` is now found by scanning BACKWARD from the newest
/// posted epoch, stopping at the first epoch (in that backward order)
/// found effectively Final, bounded by `max_lookback` epochs (at most
/// `RING_SLOTS`, since nothing older could still be physically
/// present in the ring). A missing, Empty, Disputed, or still-pending
/// epoch anywhere in that scanned range is simply skipped, never a
/// reason to give up early. The storage-layer scan itself
/// (`storage::advance_finality`) does the heavy lifting; this wrapper
/// just supplies `now` and the posting/lookback bounds, and turns the
/// storage layer's list of newly-announced epochs into actual
/// `signals_final` events (announcing is storage's job; the EVENT
/// belongs at this layer, next to every other event this contract
/// emits).
///
/// Every call site in this contract passes `FINALITY_LOOKBACK_EPOCHS`
/// (the full backfill window): the re-review's instruction is that
/// EVERY state changing call sweeps the whole window, not that some
/// callers get a smaller budget than others the way the old
/// `max_steps` design did. `max_lookback` stays a parameter (rather
/// than hardcoding the constant inside this function) so a future
/// caller with a genuine reason to scan less can still do so
/// explicitly, and so this function's own tests can probe a specific
/// bound without depending on the module-level constant.
///
/// Returns the newest epoch now known Final, if any.
fn try_advance_finality(
    env: &Env,
    config: &Config,
    asset: &Address,
    max_lookback: u32,
) -> Option<u64> {
    let now = env.ledger().timestamp();
    let Some(newest_posted) = storage::get_newest_epoch_pub(env, asset) else {
        return storage::get_newest_final(env, asset);
    };
    let (newest_final, newly_announced) =
        storage::advance_finality(env, asset, now, newest_posted, max_lookback);

    // Issue #11 fix (feat/treasury): reward the posting keeper of
    // every epoch newly observed Final here, the same place
    // `signals_final` is emitted. Grouped by poster so a single
    // reward_keeper call covers every epoch this one scan found for
    // that keeper, never one call per epoch. An epoch already
    // announced Final before this call (not in `newly_announced`)
    // never rewards again: `newly_announced` is, by construction
    // (`storage::advance_finality`'s own `final_announced` flag
    // check), exactly the set of epochs crossing into Final for the
    // first time on this call. An overturned epoch never reaches
    // Final at all (its slot state is `Overturned`, not `Final`), so
    // it can never appear in `newly_announced` and is never rewarded.
    if !newly_announced.is_empty() {
        reward_posters_for_newly_final_epochs(env, config, asset, &newly_announced);
    }

    for epoch in newly_announced.iter() {
        events::SignalsFinal {
            asset: asset.clone(),
            epoch,
        }
        .publish(env);
    }

    // Never move the cached pointer backward: the fresh scan's own
    // bound only looks back `max_lookback` epochs from the newest
    // POSTED epoch, which is strictly a caller-side budget, not a
    // claim that nothing Final exists further back than that. A
    // smaller `max_lookback` (e.g. dispute_signals's bounded call)
    // must never erase a larger result an earlier, wider scan already
    // found and cached.
    let previous = storage::get_newest_final(env, asset);
    let advanced = match (newest_final, previous) {
        (Some(fresh), Some(cached)) => Some(fresh.max(cached)),
        (Some(fresh), None) => Some(fresh),
        (None, cached) => cached,
    };
    if let Some(advanced) = advanced {
        if Some(advanced) != previous {
            storage::set_newest_final(env, asset, advanced);
        }
    }
    advanced
}

/// Issue #11 fix (feat/treasury): groups `newly_announced` by the
/// epoch's own poster (read from `Signals(asset, epoch)`, the only
/// place that field lives; `RingSlot` itself carries no poster) and
/// calls `Staking.reward_keeper(poster, count)` exactly once per
/// distinct poster, with `count` the number of newly Final epochs
/// that poster posted in this one scan. `Map` is small and bounded
/// by `max_lookback` (at most `RING_SLOTS`), the same bound every
/// other part of the finality scan already carries, so this never
/// grows unbounded. A missing `Signals` entry (should not happen for
/// a genuinely Final epoch, since only a resolved, overturned
/// dispute ever removes one, and an overturned epoch's slot is never
/// Final) is skipped rather than panicking, since this reward step
/// must never be the reason a finality scan itself fails.
fn reward_posters_for_newly_final_epochs(
    env: &Env,
    config: &Config,
    asset: &Address,
    newly_announced: &Vec<u64>,
) {
    let mut counts: Map<Address, u32> = Map::new(env);
    for epoch in newly_announced.iter() {
        let Some(signals) = storage::get_signals(env, asset, epoch) else {
            continue;
        };
        let count = counts.get(signals.poster.clone()).unwrap_or(0);
        counts.set(signals.poster, count + 1);
    }
    let staking = StakingClient::new(env, &config.staking);
    for (poster, count) in counts.iter() {
        staking.reward_keeper(&poster, &count);
    }
}

/// The one place a `RiskScore` is actually computed and
/// written. Called from `post_signals` (after writing the new slot and
/// advancing finality), `finalize_endpoint`, and the dispute resolution
/// path, each time finality may have moved forward; a no-op (no write,
/// no `score_updated`/`band_changed` event) if the newest final epoch
/// has not moved past what is already stored, so repeated calls
/// within the same epoch do not spam those events. Every caller also
/// runs `check_stale_internal` afterward, which is
/// where `asset_stale` actually gets decided, independent of whether
/// this function wrote anything.
///
/// The hysteresis down streak in `apply_hysteresis` only advances when
/// called here with a strictly newer final epoch than what is stored,
/// so it advances at most once per new epoch, satisfied by
/// construction rather than by an extra check, because `score()`
/// itself never calls this function.
///
/// The "at least Distress while an event is in progress" override is
/// deliberately NOT applied here: it is a
/// read-time floor applied in `score()` instead (see that function's
/// doc comment), so that the `RiskScore` persisted here — and the
/// hysteresis streak computed from it — always reflects the plain
/// signal-derived band, never confusing a flag flip with a new epoch of
/// evidence.
fn recompute_score(env: &Env, asset: &Address, newest_final: u64) -> Result<(), Error> {
    let stored = storage::get_score(env, asset);
    if let Some(current) = &stored {
        if newest_final <= current.epoch {
            return Ok(()); // Nothing newer to score from.
        }
        if current.band == Band::Event {
            return Ok(()); // Sticky; only clear_event_band moves out of it.
        }
    }
    if is_stale_epoch(env, newest_final, SCORE_STALE_AFTER_EPOCHS) {
        // The epoch that just became final is itself already outside
        // score_stale_after_epochs (a very late backfill); recording it
        // would immediately read back as stale, which score() already
        // reports correctly from the stored epoch without a write. Re-review
        // item C7: asset_stale emission is no longer this function's
        // job at all; every caller runs check_stale_internal
        // afterward, which detects exactly this "stale on arrival"
        // case from the stored score's own (unchanged) epoch and
        // announces the transition from there.
        return Ok(());
    }
    // Comparing `newest_final` directly against `AGGREGATE_SLOTS_7D`
    // only guards the subtraction below from underflowing; it is not
    // a real history check. On a real network, epoch numbers are
    // unix-time-derived (around 497,000 as of writing), always far
    // larger than 168, so that comparison never fires and a brand-new
    // asset gets scored from however many epochs it has actually
    // posted, not the 168 (7 days) the 7 day baseline below requires.
    // The real check is against how long THIS asset has actually been
    // posting, tracked by `FirstEpoch`.
    let Some(first_epoch) = storage::get_first_epoch(env, asset) else {
        return Ok(()); // Never posted; nothing to score.
    };
    if newest_final + 1 < first_epoch + AGGREGATE_SLOTS_7D as u64 {
        // Not enough history yet for the 7 day baseline every component
        // needs (Section 6.5): a new asset with fewer than 168 epochs
        // of its OWN history reads as stale, by design.
        return Ok(());
    }

    let cfg = storage::get_asset_config(env, asset).ok_or(Error::UnknownAsset)?;
    let formula = storage::get_formula(env).ok_or(Error::NotInitialized)?;
    let l_target = l_target_for(&cfg);
    // aggregate_from_ring fails with AggregationFailed when the
    // window is calendar-eligible but
    // too sparse (MIN_AGGREGATE_FINAL_SLOTS). Every other "not enough
    // history yet" condition in this function degrades to Ok(()),
    // i.e. post_signals must never abort just because a score could
    // not yet be computed; match that here instead of propagating the
    // error out of post_signals via `?`.
    let aggregates = match score::aggregate_from_ring(env, asset, newest_final, first_epoch) {
        Ok(aggregates) => aggregates,
        Err(Error::AggregationFailed) => return Ok(()),
        Err(e) => return Err(e),
    };
    let score::ScoreResult {
        score: raw,
        forced_warning,
    } = score::combined_score(&formula, &aggregates, l_target)?;
    let mut raw_band = score::band_for_score(raw);
    if forced_warning && raw_band < Band::Warning {
        // Section 6.3: "Forced to at least Warning if P = 100 or E =
        // 100", checked on the pre-weighting components in
        // combined_score; applied here, before hysteresis, so a
        // forced-up move is subject to the same "upward moves apply
        // immediately" rule (Section 6.4) as any other band increase.
        raw_band = Band::Warning;
    }

    let down_streak = storage::get_down_streak(env, asset);
    let (band, new_streak) = match &stored {
        Some(current) => score::apply_hysteresis(current, raw_band, down_streak, BAND_DOWN_EPOCHS),
        None => (raw_band, 0),
    };
    storage::set_down_streak(env, asset, new_streak);

    let result = RiskScore {
        epoch: newest_final,
        score: raw,
        band,
        formula_version: formula.version,
        stale: false,
    };
    storage::set_score(env, asset, &result);

    events::ScoreUpdated {
        asset: asset.clone(),
        epoch: newest_final,
        score: raw,
        formula_version: formula.version,
    }
    .publish(env);
    if stored.as_ref().map(|s| s.band) != Some(band) {
        events::BandChanged {
            asset: asset.clone(),
            from: stored.map(|s| s.band).unwrap_or(Band::Normal),
            to: band,
            epoch: newest_final,
        }
        .publish(env);
    }
    Ok(())
}

#[cfg(test)]
mod budget_test;
#[cfg(test)]
mod mocks;
#[cfg(test)]
mod test;
