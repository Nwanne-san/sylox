import {
  Networks,
  SyloxError,
  assetDisplayName,
  riskOracleClient,
  eventRegistryClient,
  type EffectiveRingSlot,
  type EventKind,
  type CoverGate,
  type SlotState,
  type SubEpoch,
} from "@sylox/sdk";
import { al } from "./sylox-client";
import { riskBandFor } from "./band";
import type { RiskBand } from "@sylox/ui/risk-bands";

export const PEG_RATIO_SCALE = 10_000_000; // i128, scale 1e7 (RiskOracle's own doc comment on peg_ratio).
const EPOCH_SECS = 3600; // technical-doc.md Section 23: epoch_secs is frozen at 3600 (1 hour) for v1.
const AGGREGATE_SLOTS_7D = 168; // contracts/risk-oracle/src/score.rs's AGGREGATE_SLOTS_7D - the 7-day score baseline (review decision D2), same constant lib.rs's own scoring gate checks against first_epoch.
const SIGNAL_DISPUTE_SECS = 7_200; // contracts/staking/src/params.rs's SIGNAL_DISPUTE_SECS (2h) - the dispute window an hour must clear before Pending can be promoted to Final; also re-declared locally in contracts/risk-oracle/src/lib.rs with the same value.
// contracts/risk-oracle/src/lib.rs's own HOUR_PENDING_SENTINEL
// (refresh_waiting_hour): the pending_until a Pending/Disputed hour
// carries while it's still waiting on its own sub-epochs to all decide,
// before the hour is actually built - there is no real promotion
// deadline yet, so the contract writes u64::MAX rather than leaving the
// field unset (RingSlot.pending_until is non-optional). Number(bigint)
// on this value overflows past Number.MAX_SAFE_INTEGER, which is what
// produced "can still be challenged until NaN:NaN UTC": new Date(huge
// number) is an Invalid Date, and getUTCHours()/getUTCMinutes() on an
// Invalid Date both return NaN. Confirmed directly against live testnet
// USDC data (113 of 240 ring hours currently carry this exact sentinel
// while their sub-epochs are still posting).
const HOUR_PENDING_SENTINEL = BigInt("18446744073709551615"); // u64::MAX

// `al.oracle`/`al.events` deliberately only wrap a subset of reads (see
// oracle.ts/events/list.ts's own "thinner surface" doc comments) -
// asset_config/event_in_progress (RiskOracle) and current_version/
// definition (EventRegistry) have no al.* method. riskOracleClient/
// eventRegistryClient (newly re-exported from @sylox/sdk - see this
// PR's description's "SDK export gap" section) are the same
// generated-bindings clients al.oracle/al.events are themselves built
// on, used here only for the handful of fields that have no al.*
// wrapper. Every other read in this file goes through al.oracle/
// al.events, never these directly.
function oracleContract() {
  return riskOracleClient(Networks.Testnet);
}

function registryContract() {
  return eventRegistryClient(Networks.Testnet);
}

/** A Result<T, {message}> from a generated client, normalized to a plain outcome. */
type SectionResult<T> = { status: "ok"; value: T } | { status: "error"; message: string };

function ok<T>(value: T): SectionResult<T> {
  return { status: "ok", value };
}

function err<T>(e: unknown): SectionResult<T> {
  return { status: "error", message: e instanceof Error ? e.message : String(e) };
}

async function section<T>(fn: () => Promise<T>): Promise<SectionResult<T>> {
  try {
    return ok(await fn());
  } catch (e) {
    return err(e);
  }
}

// ---------------------------------------------------------------------------
// 1. Header
// ---------------------------------------------------------------------------

export interface AssetHeader {
  asset: string;
  code: string;
  homeDomain: string | null;
  /**
   * Null when the contract has no real score yet - `RiskScore.epoch === 0`
   * means `score()` is returning `stale_score()`'s never-scored fallback,
   * whose `band` field is hardcoded to `Band::Normal` (lib.rs's
   * `stale_score`) as a struct-literal placeholder, not a real "Normal"
   * verdict. Showing that band would read as "this asset is healthy",
   * which is not something the contract has actually determined yet.
   *
   * See `scoringStart` below for when a real score might arrive.
   */
  band: RiskBand | null;
  /** 0-100, or null when the contract has no score yet (same `epoch === 0` condition as `band`). */
  score: number | null;
  stale: boolean;
  eventInProgress: boolean;
  eventDeclared: boolean;
  /**
   * When a real score might first appear, for the "no score yet" state
   * only (null once `score` is non-null - nothing left to predict).
   * `lib.rs`'s own scoring gate (`fn score`, re-review item D2) requires
   * `newest_final >= first_epoch + AGGREGATE_SLOTS_7D - 1` - 168 FINAL
   * hours from this asset's own first posted epoch, the 7-day baseline
   * every score component needs (technical-doc.md Section 6.5), not a
   * fixed 168 hours of wall-clock time. `(first_epoch + 168h) +
   * SIGNAL_DISPUTE_SECS` is the EARLIEST that could happen (every hour
   * finalizing exactly on schedule, no disputes, no backfills): the
   * 168th hour itself only becomes Final after clearing its own
   * SIGNAL_DISPUTE_SECS dispute window (contracts/staking/src/params.rs),
   * so the real 168th-hour-Final moment can land later than this. Never
   * shown as a promise - see `formatScoringStart` in
   * `AssetPageClient.tsx`, which always renders this with "around"
   * wording, never a bare date.
   */
  scoringStart: ScoringStart;
}

export type ScoringStart =
  // first_epoch() resolved to None (never posted) or the read itself
  // failed - both read the same to a visitor ("not scored yet"), and
  // matches tryFetchFirstEpoch's own fallback below (never distinguish
  // "never posted" from "couldn't tell" - see its own doc comment).
  | { status: "unknown" }
  | { status: "pending"; earliestUnixSecs: number }; // (first_epoch + 168h) + SIGNAL_DISPUTE_SECS, in Unix seconds - always rendered with "around"/"earliest" wording, never as a guaranteed date.

function codeFromDisplayName(name: string, asset: string): string {
  const [code] = name.split(":");
  return code && code !== name ? code : `${asset.slice(0, 4)}…${asset.slice(-4)}`;
}

async function fetchHeader(asset: string): Promise<AssetHeader> {
  const oracle = oracleContract();

  const [riskScore, staleTx, inProgressTx, configTx, name] = await Promise.all([
    // al.oracle.score() already throws a SyloxError (its message is
    // the right plain sentence) on the contract's Err case - no more
    // separate extractErrorCode/describeScoreError call needed; this
    // function's own section() wrapper (in fetchAssetPageData below)
    // already reshapes any thrown error into {status: "error", message}.
    al.oracle.score(asset),
    oracle.check_stale({ asset }),
    oracle.event_in_progress({ asset }),
    oracle.asset_config({ asset }),
    assetDisplayName(Networks.Testnet, asset),
  ]);

  const hasRealScore = riskScore.epoch !== BigInt(0);

  const staleResult = staleTx.result;
  const stale = staleResult.isErr() ? true : staleResult.unwrap();

  const config = configTx.result;

  // Only worth asking when there's no real score yet - once a score
  // exists, nothing is left to predict (scoringStart is shown only in
  // the "no score yet" UI state).
  const scoringStart = hasRealScore
    ? { status: "unknown" as const } // Unused in this branch - the UI never reads scoringStart once score is non-null.
    : await fetchScoringStart(asset);

  return {
    asset,
    code: codeFromDisplayName(name, asset),
    homeDomain: config?.home_domain || null,
    band: hasRealScore ? riskBandFor(riskScore.band) : null,
    score: hasRealScore ? riskScore.score : null,
    stale,
    eventInProgress: inProgressTx.result,
    eventDeclared: hasRealScore && riskScore.band.tag === "Event",
    scoringStart,
  };
}

/**
 * The earliest a real score could appear - see AssetHeader.scoringStart's
 * own doc comment for the full "earliest, not a promise" reasoning.
 *
 * Hour `first_epoch + AGGREGATE_SLOTS_7D - 1` (the 168th hour, 0-indexed
 * from first_epoch) is the LAST hour the scoring gate needs to be Final;
 * it only reaches Final after clearing its own SIGNAL_DISPUTE_SECS
 * dispute window, so the earliest moment is that hour's own posting time
 * (`first_epoch + AGGREGATE_SLOTS_7D` hours, i.e. the START of the next
 * hour after it) plus SIGNAL_DISPUTE_SECS, not a bare `+ 168h`.
 *
 * `al.oracle.firstEpoch(asset)` deliberately DEVIATES from this file's
 * own historical "never throw" contract (see its own doc comment in
 * packages/sdk/src/oracle.ts): it throws a SyloxError on a genuine
 * read failure, only resolving `null` for the real "never posted" case.
 * This function restores the old silent-fallback UX at exactly this
 * call site - both a thrown SyloxError and a resolved `null` read the
 * same here ("unknown"), matching tryFetchFirstEpoch's documented
 * behavior exactly, so a transient RPC failure reading first_epoch can
 * never crash the whole asset page, only leave this one field showing
 * "scoring start unknown."
 */
export async function fetchScoringStart(asset: string): Promise<ScoringStart> {
  const firstEpoch = await tryFetchFirstEpoch(asset);
  if (firstEpoch === undefined) return { status: "unknown" };
  const earliestUnixSecs =
    (Number(firstEpoch) + AGGREGATE_SLOTS_7D) * EPOCH_SECS + SIGNAL_DISPUTE_SECS;
  return { status: "pending", earliestUnixSecs };
}

// ---------------------------------------------------------------------------
// 2. Confirmed / Live
// ---------------------------------------------------------------------------

export interface ConfirmedValue {
  /** The hourly epoch this value belongs to. */
  epoch: bigint;
  pegRatio: number;
  score: number;
  band: RiskBand;
}

/**
 * The newest hour that is still genuinely Pending (now < pending_until):
 * posted, but still inside its dispute window, so not yet Confirmed. Shown
 * separately from Confirmed rather than folded into it - ADR-008 only
 * treats a slot as effectively final once its own pending_until has passed,
 * and this screen holds the same line between "settled" and "can still be
 * challenged."
 */
export interface LatestPendingHour {
  epoch: bigint;
  pegRatio: number;
  /**
   * Ledger timestamp after which this hour becomes Confirmed - from the
   * slot's own pending_until, never a placeholder. Null specifically
   * when the contract's own HOUR_PENDING_SENTINEL is set: the hour is
   * still waiting on its sub-epochs and genuinely has no real deadline
   * yet (see HOUR_PENDING_SENTINEL's own doc comment above) - the UI
   * must never format this as a date.
   */
  pendingUntil: bigint | null;
}

/**
 * Walks `ring` newest-to-oldest for the newest EFFECTIVELY Final slot's
 * peg_ratio, descaled to a plain decimal - the one source of truth for
 * "the asset's current peg" every screen that shows that number reads
 * from (the asset page's Confirmed card via fetchConfirmed below, and
 * the Explorer card via fetchExplorerRow in explorer-data.ts). Never
 * RiskOracle.latest(), which only covers the hourly-fallback posting
 * path and reads back null for an asset currently on the sub-epoch path
 * (confirmed directly against live testnet USDC data: latest() → null,
 * while this walk finds a real settled value) - that mismatch is what
 * previously made the Explorer card show "—" while the asset page's
 * Confirmed card, which already used this same walk, showed a real
 * number for the same asset at the same moment.
 */
export function newestEffectiveFinalPegRatio(ring: EffectiveRingSlot[]): { epoch: bigint; pegRatio: number } | null {
  for (let i = ring.length - 1; i >= 0; i--) {
    const slot = ring[i];
    if (slot.effectiveState === "Final") {
      return { epoch: slot.epoch, pegRatio: Number(slot.peg_ratio) / PEG_RATIO_SCALE };
    }
  }
  return null;
}

export async function fetchConfirmed(
  asset: string,
): Promise<{ confirmed: ConfirmedValue | null; latestPending: LatestPendingHour | null }> {
  const [ring, riskScore] = await Promise.all([al.oracle.ring(asset), al.oracle.score(asset)]);

  // "score" and "band" come from RiskOracle.score() - itself defined off
  // the newest Final epoch (lib.rs's score() doc comment) - so pairing
  // them with newestEffectiveFinalPegRatio's own walk here is pairing
  // like with like.
  const newestFinal = newestEffectiveFinalPegRatio(ring);
  const confirmed: ConfirmedValue | null = newestFinal && {
    epoch: newestFinal.epoch,
    pegRatio: newestFinal.pegRatio,
    score: riskScore.score,
    band: riskBandFor(riskScore.band),
  };

  // The newest slot overall, only if it's a real posted hour still inside
  // its own dispute window (genuinely Pending, not yet promoted to
  // effectively Final).
  let latestPending: LatestPendingHour | null = null;
  const newest = ring[ring.length - 1];
  if (newest && newest.state.tag === "Pending" && newest.effectiveState === "Pending") {
    latestPending = {
      epoch: newest.epoch,
      pegRatio: Number(newest.peg_ratio) / PEG_RATIO_SCALE,
      pendingUntil: newest.pending_until === HOUR_PENDING_SENTINEL ? null : newest.pending_until,
    };
  }

  return { confirmed, latestPending };
}

export type LiveValue =
  | { status: "none" }
  | {
      status: "value";
      subEpoch: SubEpoch;
      pegRatio: number;
      slotState: SlotState["tag"];
      /** `SignalSet.posted_at`, Ledger timestamp in Unix seconds - for "how old it is" display. */
      postedAt: number;
    };

/**
 * technical-doc.md Section 5.9/12.1 (v1.5): the newest POSTED
 * sub-epoch's Pending/challengeable data, distinct from `latest()`'s
 * hourly-only Final/Pending semantics. Never used for `require_holding`
 * or any other payout-adjacent valuation.
 *
 * `al.oracle.live(asset)` returns the raw decoded tuple
 * `[SubEpoch, SignalSet, SlotState] | null` directly (never this file's
 * own `LiveValue` display shape) - that reshaping still happens here,
 * same as it always did, just from the SDK's already-decoded tuple
 * instead of a raw tx result.
 */
export async function fetchLive(asset: string): Promise<LiveValue> {
  const result = await al.oracle.live(asset);
  if (result === null) return { status: "none" };
  const [subEpoch, signals, slotState] = result;
  return {
    status: "value",
    subEpoch,
    pegRatio: Number(signals.peg_ratio) / PEG_RATIO_SCALE,
    slotState: slotState.tag,
    postedAt: Number(signals.posted_at),
  };
}

// ---------------------------------------------------------------------------
// 3. Peg history (ring)
// ---------------------------------------------------------------------------

export interface PegHistoryPoint {
  /** Unix seconds (epoch * EPOCH_SECS), the real time axis - never slot index. */
  timestamp: number;
  epoch: bigint;
  /** The EFFECTIVE state (Pending promoted to Final once pending_until has passed), never the raw stored tag. */
  state: SlotState["tag"];
  /** Null for Empty/Pending/Disputed slots - a gap, never a fake 0. */
  pegRatio: number | null;
  /**
   * False for an hour before the asset's own `first_epoch` - the window
   * always spans the full RING_SLOTS hours regardless of how long the
   * asset has actually existed, so a leading stretch of `Empty` can mean
   * either "before this asset was added" or "a real gap happened here."
   * Only `first_epoch` tells them apart. An untracked hour is never
   * counted as `missing` by the chart's legend - the keeper didn't fail
   * to post something that was never trackable in the first place - and
   * is drawn as a plain unshaded region, not a gap. True when
   * `first_epoch` is unknown (never posted) or its read failed, so a
   * failure here falls back to treating every hour as tracked (today's
   * behavior) rather than guessing which ones aren't.
   */
  tracked: boolean;
  /**
   * `RingSlot.provisional_sub_coverage` passthrough. `lib.rs`'s own
   * `refresh_waiting_hour` sets this on EVERY hour still waiting on its
   * sub-epochs, not only disputed ones - it is set for a Pending slot (N
   * of 12 sub-epochs posted, none yet disputed) exactly as much as for a
   * Disputed one (N of 12 undisputed sub-epochs still posted, 0 included
   * when every posted sub-epoch is currently disputed). `null` for an
   * hourly-fallback-path hour or a never-posted one. Final hours stay as
   * they are today - this never affects `pegRatio`/`state` above, only
   * whether the chart marks the hour provisional, and the exact wording
   * (Pending vs. Disputed, N > 0 vs. N = 0) comes from pairing this with
   * `state` - see `PegHistoryChart.tsx`'s own tooltip logic.
   */
  provisionalSubCoverage: number | null;
}

export type { EffectiveRingSlot };

/** `first_epoch()`'s result, or undefined on ANY failure (rejection or a synchronous throw, including a thrown SyloxError) - "treat every hour as tracked" rather than guessing. Normalizes al.oracle.firstEpoch's own `null` ("no value") to this function's `undefined` convention right here, so every caller keeps its existing `=== undefined` check. */
async function tryFetchFirstEpoch(asset: string): Promise<bigint | undefined> {
  try {
    const firstEpoch = await al.oracle.firstEpoch(asset);
    return firstEpoch ?? undefined;
  } catch {
    return undefined;
  }
}

export async function fetchPegHistory(asset: string): Promise<PegHistoryPoint[]> {
  // A separate first_epoch() call (the header makes its own too) so this
  // section keeps failing independently of the header - and so a failure
  // here specifically can fall back to "treat every hour as tracked"
  // rather than taking down the whole chart.
  const [ring, firstEpoch] = await Promise.all([al.oracle.ring(asset), tryFetchFirstEpoch(asset)]);

  return ring.map((slot) => ({
    timestamp: Number(slot.epoch) * EPOCH_SECS,
    epoch: slot.epoch,
    state: slot.effectiveState,
    // Only an EFFECTIVELY Final slot's peg_ratio is settled and
    // trustworthy - a Pending slot still inside its dispute window,
    // a Disputed slot, or a missing (Empty) one all render as gaps
    // (null), never as 0. A missing or still-contested hour must never
    // be drawn as a depeg dip (technical-doc.md Section 5.8: missing
    // epochs "count neither for nor against" anything the contract
    // itself checks; this screen holds the same rule for what it draws).
    pegRatio: slot.effectiveState === "Final" ? Number(slot.peg_ratio) / PEG_RATIO_SCALE : null,
    tracked: firstEpoch === undefined ? true : slot.epoch >= firstEpoch,
    provisionalSubCoverage: slot.provisional_sub_coverage ?? null,
  }));
}

// ---------------------------------------------------------------------------
// 4. Failure definitions (plain words)
// ---------------------------------------------------------------------------

export interface FailureDefinition {
  kind: EventKind["tag"];
  /** Plain-language sentence built only from the contract's own fields - no hardcoded numbers. */
  sentence: string;
}

const EVENT_KINDS: EventKind["tag"][] = [
  "Depeg",
  "IssuerFreeze",
  "MintWithoutBacking",
  "WithdrawalHalt",
  "Insolvency",
];

export function sentenceFor(
  kind: EventKind["tag"],
  def: NonNullable<Awaited<ReturnType<typeof fetchOneDefinition>>>,
): string {
  switch (kind) {
    case "Depeg": {
      const threshold = Number(def.depeg_threshold) / PEG_RATIO_SCALE;
      const hours = Number(def.depeg_window_secs) / 3600;
      return `Depeg: below ${threshold.toFixed(2)} of peg for ${hours} hours, up to ${def.max_missing_epochs} missing hours allowed.`;
    }
    case "IssuerFreeze": {
      const pct = def.freeze_pct_bps / 100;
      const revocationClause =
        def.auth_revocation_threshold > 0
          ? `more than ${def.auth_revocation_threshold} revocations in that window`
          : "any authorization revocation in that window"; // "more than 0 revocations" reads oddly; a threshold of 0 means every revocation counts.
      return `Issuer freeze: clawbacks or authorization revocations above ${pct}% of supply within 7 days, or ${revocationClause}.`;
    }
    case "MintWithoutBacking": {
      const pct = def.mint_spike_bps / 100;
      return `Mint without backing: a supply increase of more than ${pct}% without matching reserves.`;
    }
    case "WithdrawalHalt": {
      const hours = Number(def.halt_window_secs) / 3600;
      return `Withdrawal halt: the issuer's redemption endpoint reports Down or Degraded for ${hours} hours.`;
    }
    case "Insolvency": {
      return `Insolvency: declared directly by governance, no automatic threshold.`;
    }
  }
}

async function fetchOneDefinition(asset: string, kind: EventKind["tag"]) {
  // definition() needs an explicit version; current_version() gives the
  // canonical one to ask for. Neither has a dedicated al.events read in
  // this SDK version (al.events's own surface is event/status/
  // coverGate/list - see events/list.ts) - read directly against the
  // same registry contract client fetchHeader/fetchCoverGate use.
  const registry = registryContract();
  const versionTx = await registry.current_version({ asset, kind: { tag: kind, values: undefined } });
  const version = versionTx.result;
  if (version === 0) return null; // No canonical definition registered for this kind yet.

  const defTx = await registry.definition({
    asset,
    kind: { tag: kind, values: undefined },
    version,
  });
  return defTx.result ?? null;
}

async function fetchFailureDefinitions(asset: string): Promise<FailureDefinition[]> {
  const results = await Promise.all(
    EVENT_KINDS.map(async (kind) => {
      const def = await fetchOneDefinition(asset, kind);
      if (!def) return null;
      return { kind, sentence: sentenceFor(kind, def) };
    }),
  );
  return results.filter((d): d is FailureDefinition => d !== null);
}

// ---------------------------------------------------------------------------
// 5. Cover sales status
// ---------------------------------------------------------------------------

const COVER_GATE_LABEL: Record<CoverGate["tag"], string> = {
  Clear: "Cover can be sold.",
  EventInProgress: "Sales paused: an event is being challenged for this asset.",
  RecentDepeg: "Sales paused: the price was below the threshold in the last depeg window.",
  RecentEndpointOutage: "Sales paused: the issuer's redemption endpoint was recently down or degraded.",
  RecentIssuerAction: "Sales paused: a clawback or authorization revocation happened in the last 7 days.",
  UnbuiltBacklog: "Sales paused: too many unbuilt hours in the depeg window to scan safely.",
};

/**
 * Not just `cover_gate` - `Series.buy_cover`'s real step 2 (technical-
 * doc.md Section 9.4) rejects if the asset is stale OR its band is
 * Distress/Event, OR `cover_gate` is not Clear (the sequence diagram:
 * `Series->>Oracle: is_stale(asset), band(asset)` happens before
 * `Series->>Registry: cover_gate(asset)`, and either can reject on its
 * own). `cover_gate` alone only covers the EventRegistry-side checks
 * (event-in-progress, recent depeg/outage/issuer-action); showing "Cover
 * can be sold" from that alone would claim a sale would go through when
 * staleness or a Distress/Event band would still reject it.
 */
export type CoverSalesStatus =
  | { gate: "Clear"; label: string }
  | { gate: "Stale"; label: string }
  | { gate: "Distressed"; label: string }
  | { gate: CoverGate["tag"]; label: string };

export async function fetchCoverGate(asset: string): Promise<CoverSalesStatus> {
  // al.oracle.score() and al.events.coverGate() already throw
  // SyloxError on their contract Err case - the old code's own
  // isErr()-check-then-describe*Error pattern isn't needed any more.
  // check_stale is read directly (same as fetchHeader) since isStale()
  // throws on Err rather than this file's old fail-safe-to-stale
  // default - restored explicitly here via the same raw contract read
  // fetchHeader itself uses, keeping the exact old fallback behavior.
  const staleTx = await oracleContract().check_stale({ asset });
  const staleResult = staleTx.result;
  const stale = staleResult.isErr() ? true : staleResult.unwrap();
  if (stale) {
    return { gate: "Stale", label: "Sales paused: not enough price history yet." };
  }

  const riskScore = await al.oracle.score(asset);
  const band = riskScore.band.tag;
  if (band === "Distress" || band === "Event") {
    return { gate: "Distressed", label: "Sales paused: the asset is in distress." };
  }

  const gate = await al.events.coverGate(asset);
  return { gate: gate.tag, label: COVER_GATE_LABEL[gate.tag] };
}

// ---------------------------------------------------------------------------
// 6. Active event
// ---------------------------------------------------------------------------

export interface ActiveEvents {
  count: number;
  /** The specific event ids currently InProgress, one per kind that has one - for linking straight to each from the Asset screen's active-event card. active_event_count() alone only gives a number, never which event(s); event_status(asset, kind) is checked per kind (same EVENT_KINDS list fetchFailureDefinitions already walks) to find them. */
  eventIds: bigint[];
}

async function fetchActiveEventCount(asset: string): Promise<ActiveEvents> {
  const [countTx, statuses] = await Promise.all([
    registryContract().active_event_count({ asset }),
    Promise.all(EVENT_KINDS.map((kind) => al.events.status(asset, kind))),
  ]);

  const eventIds = statuses
    .filter((status) => status.tag === "InProgress")
    .map((status) => status.values[0]);

  return { count: countTx.result, eventIds };
}

// ---------------------------------------------------------------------------
// Assembled page data - every section independently failable.
// ---------------------------------------------------------------------------

export interface AssetPageData {
  header: SectionResult<AssetHeader>;
  confirmed: SectionResult<{ confirmed: ConfirmedValue | null; latestPending: LatestPendingHour | null }>;
  live: SectionResult<LiveValue>;
  pegHistory: SectionResult<PegHistoryPoint[]>;
  failureDefinitions: SectionResult<FailureDefinition[]>;
  coverGate: SectionResult<CoverSalesStatus>;
  activeEventCount: SectionResult<ActiveEvents>;
}

export async function fetchAssetPageData(asset: string): Promise<AssetPageData> {
  const [header, confirmed, live, pegHistory, failureDefinitions, coverGate, activeEventCount] =
    await Promise.all([
      section(() => fetchHeader(asset)),
      section(() => fetchConfirmed(asset)),
      section(() => fetchLive(asset)),
      section(() => fetchPegHistory(asset)),
      section(() => fetchFailureDefinitions(asset)),
      section(() => fetchCoverGate(asset)),
      section(() => fetchActiveEventCount(asset)),
    ]);

  return { header, confirmed, live, pegHistory, failureDefinitions, coverGate, activeEventCount };
}

export { SyloxError };
