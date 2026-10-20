"use client";

import type { PegHistoryPoint } from "@/lib/asset-data";

export interface PegHistoryChartProps {
  points: PegHistoryPoint[];
  /** Plain decimal, e.g. 0.95 - drawn as a reference line. Null when no Depeg definition exists for this asset. */
  depegThreshold: number | null;
}

const WIDTH = 960;
const HEIGHT = 220;
const PAD_X = 8;
const PAD_Y = 16;
// The non-confirmed band sits in its own reserved strip at the TOP of
// the chart, never anywhere near the value axis below it - this is the
// fix for the bug where 113 provisional hours, each drawn as a small
// circle at a fixed low y, visually merged into what read as a flat
// bar sitting under the depeg threshold line (a viewer's first read:
// "the price crashed to zero"). A muted band up here can never be
// mistaken for a plotted value, because it never shares the value
// axis's coordinate space at all.
const BAND_HEIGHT = 14;
const BAND_GAP = 6; // Visual breathing room between the band and the value plot below it.
const PLOT_TOP = BAND_HEIGHT + BAND_GAP;

function formatAxisDate(timestampSecs: number): string {
  const date = new Date(timestampSecs * 1000);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" });
}

/**
 * `refresh_waiting_hour` (contracts/risk-oracle/src/lib.rs) sets
 * `provisional_sub_coverage` on every hour still waiting on its
 * sub-epochs, Pending or Disputed alike - the wording here follows the
 * slot's own effective state, not just the coverage count: a Pending
 * hour is still accumulating sub-epochs normally, while a Disputed hour
 * has had at least one sub-epoch challenged (and can have zero
 * undisputed ones left).
 */
function provisionalTooltip(state: PegHistoryPoint["state"], coverage: number): string {
  if (state === "Disputed") {
    return coverage > 0
      ? `Under dispute. Based on ${coverage} of 12 undisputed sub-epochs.`
      : "Under dispute. No undisputed sub-epochs yet.";
  }
  return `Provisional. Based on ${coverage} of 12 sub-epochs so far.`;
}

/**
 * Hand-rolled SVG line chart: no charting library. Nothing else in this
 * app or web/landing already depends on one (confirmed before adding
 * this), and the one thing this chart actually needs - breaking the
 * line at a gap instead of drawing a false dip to 0 - is the one thing
 * most line-chart libraries make hardest to get right by default (many
 * null-handling options default to "interpolate through" or "treat as
 * zero"). A plain <path> built from explicit M/L segments gives exact
 * control over that, in about 100 lines, for a single 240-point line
 * with one reference line. Revisit with a library if the Markets/Event
 * screens need genuinely complex charts (zoom, multiple series,
 * tooltips) that this hand-rolled approach can't reasonably grow into.
 *
 * Every point whose pegRatio is null (effectively Empty/Pending/Disputed
 * - see lib/asset-data.ts's fetchEffectiveRing/fetchPegHistory, which
 * promote a Pending slot to Final once its own pending_until has
 * passed, via RiskOracle.effective_window) is a real gap: the line
 * breaks there, nothing is drawn, and it is never treated as a 0
 * value. A missing or still-contested hour must never render as a
 * fake depeg dip (this is the single thing to check first in review).
 *
 * An untracked point (before the asset's own first_epoch - see
 * PegHistoryPoint.tracked) is a different thing from a gap: the ring
 * always spans the full 240-hour window regardless of how long the
 * asset has actually existed, so an hour before the asset was added is
 * not something a keeper failed to post, and must not count toward
 * "missing" or be drawn the same way as a real gap.
 */
export function PegHistoryChart({ points, depegThreshold }: PegHistoryChartProps) {
  const trackedPoints = points.filter((p) => p.tracked);

  const known = trackedPoints.filter(
    (p): p is PegHistoryPoint & { pegRatio: number } => p.pegRatio !== null,
  );
  // state is already the EFFECTIVE state (see fetchEffectiveRing in
  // lib/asset-data.ts) - a Pending hour past its own pending_until
  // counts as confirmed here too, matching known/pegRatio above.
  const confirmedCount = known.length;
  const pendingCount = trackedPoints.filter(
    (p) => p.state === "Pending" || p.state === "Disputed",
  ).length;
  const missingCount = trackedPoints.filter((p) => p.state === "Empty").length;

  const historyStartsLate = trackedPoints.length < points.length;

  if (known.length === 0) {
    return (
      <p className="font-mono text-sm text-cyber-tin">
        No confirmed hourly history yet.
      </p>
    );
  }

  const minTimestamp = points[0].timestamp;
  const maxTimestamp = points[points.length - 1].timestamp;
  const timeSpan = maxTimestamp - minTimestamp || 1;

  const values = known.map((p) => p.pegRatio);
  const minValue = Math.min(...values, depegThreshold ?? Infinity);
  const maxValue = Math.max(...values, depegThreshold ?? -Infinity);
  const valueSpan = maxValue - minValue || 1;
  // A little headroom so the line and the threshold label never sit
  // flush against the chart edge.
  const valuePad = valueSpan * 0.1;

  function x(timestamp: number): number {
    return PAD_X + ((timestamp - minTimestamp) / timeSpan) * (WIDTH - PAD_X * 2);
  }

  // Plots within [PLOT_TOP, HEIGHT - PAD_Y] - never the full [0, HEIGHT]
  // range, so a real value can never be drawn inside the reserved band
  // strip at the very top, keeping the two regions visually (and
  // coordinate-wise) separate.
  function y(value: number): number {
    const t = (value - (minValue - valuePad)) / (valueSpan + valuePad * 2);
    const plotHeight = HEIGHT - PAD_Y - PLOT_TOP;
    return HEIGHT - PAD_Y - t * plotHeight;
  }

  // Build one <path> per contiguous run of known, TRACKED points. A run
  // breaks (M instead of L) whenever the next point is a gap or
  // untracked - this is what keeps a missing/pending/disputed hour from
  // being interpolated across or rendered as a dip, and keeps the line
  // from starting before the asset actually existed.
  const segments: string[] = [];
  let current: string | null = null;
  for (const point of points) {
    if (point.pegRatio === null || !point.tracked) {
      current = null;
      continue;
    }
    const command: string = current === null ? "M" : "L";
    const segment: string = `${command}${x(point.timestamp).toFixed(2)},${y(point.pegRatio).toFixed(2)}`;
    if (current === null) {
      segments.push(segment);
    } else {
      segments[segments.length - 1] += ` ${segment}`;
    }
    current = segment;
  }

  // The "not tracked yet" region: a subtly shaded rect behind everything
  // else, with a dashed line marking exactly where tracked history
  // begins - a visually distinct "this is before the asset existed"
  // band, never a line (there is nothing to draw a line through).
  let untrackedBand: { xStart: number; xEnd: number; widthFrac: number } | null = null;
  if (historyStartsLate) {
    const lastUntrackedIndex = points.findIndex((p) => p.tracked) - 1;
    if (lastUntrackedIndex >= 0) {
      const xStart = x(points[0].timestamp);
      const xEnd = x(points[lastUntrackedIndex].timestamp);
      untrackedBand = { xStart, xEnd, widthFrac: (xEnd - xStart) / (WIDTH - PAD_X * 2) };
    }
  }
  // "Not tracked yet" only fits legibly once the region is wide enough -
  // below that, the dashed boundary line alone still marks it clearly.
  const showUntrackedLabel = untrackedBand !== null && untrackedBand.widthFrac >= 0.15;

  const thresholdY = depegThreshold !== null ? y(depegThreshold) : null;

  // One hour's width in chart pixels - every point is spaced EPOCH_SECS
  // apart on the real time axis (fetchPegHistory in lib/asset-data.ts),
  // so this is the same for every point; a small overlap (1.2x) keeps
  // adjacent band cells touching with no visible seam between them.
  const hourWidthPx =
    trackedPoints.length > 1
      ? ((WIDTH - PAD_X * 2) / timeSpan) * (trackedPoints[1].timestamp - trackedPoints[0].timestamp) * 1.2
      : WIDTH - PAD_X * 2;

  // Final hours stay exactly as drawn above (the line segments). Every
  // other TRACKED hour - provisional (Pending/Disputed, still on the
  // sub-epoch path) or genuinely missing (Empty) - gets its own small
  // cell in the reserved band strip at the top instead of anything
  // drawn in the value-plotting area below: there's no real value to
  // plot for any of them, and the band keeps that fact visually
  // unmistakable rather than letting it read as a value near zero.
  const provisionalCells = trackedPoints.filter(
    (p): p is PegHistoryPoint & { provisionalSubCoverage: number } => p.provisionalSubCoverage !== null,
  );
  const missingCells = trackedPoints.filter((p) => p.state === "Empty");

  return (
    <div>
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="h-auto w-full"
        role="img"
        aria-label={`Peg value over the stored history, ${confirmedCount} confirmed, ${pendingCount} pending, ${missingCount} missing of ${trackedPoints.length} tracked hours`}
      >
        {untrackedBand !== null && (
          <>
            <rect
              x={untrackedBand.xStart}
              y={0}
              width={Math.max(0, untrackedBand.xEnd - untrackedBand.xStart)}
              height={HEIGHT}
              fill="var(--color-cement-grey)"
              fillOpacity={0.13}
            />
            <line
              x1={untrackedBand.xEnd}
              x2={untrackedBand.xEnd}
              y1={0}
              y2={HEIGHT}
              stroke="var(--color-cement-grey)"
              strokeWidth={1}
              strokeDasharray="3 3"
            />
            {showUntrackedLabel && (
              <text
                x={(untrackedBand.xStart + untrackedBand.xEnd) / 2}
                y={HEIGHT / 2}
                fill="var(--color-cyber-tin)"
                fontFamily="var(--font-mono)"
                fontSize={10}
                textAnchor="middle"
                dominantBaseline="middle"
              >
                Not tracked yet
              </text>
            )}
          </>
        )}
        {thresholdY !== null && (
          <line
            x1={PAD_X}
            x2={WIDTH - PAD_X}
            y1={thresholdY}
            y2={thresholdY}
            stroke="var(--color-risk-crimson)"
            strokeWidth={1}
            strokeDasharray="4 4"
          />
        )}
        {/* The non-confirmed band itself: a dedicated strip at the very
            top (y=0 to BAND_HEIGHT), entirely outside the value-plotting
            coordinate range y() ever returns - see PLOT_TOP. Missing
            (Empty) cells get a plain muted fill; pending/disputed cells
            get the hatch pattern, so the two are visually distinct from
            each other as well as from a real value. */}
        {missingCells.map((p) => (
          <rect
            key={`missing-${p.timestamp}`}
            x={x(p.timestamp) - hourWidthPx / 2}
            y={0}
            width={hourWidthPx}
            height={BAND_HEIGHT}
            fill="var(--color-cement-grey)"
            fillOpacity={0.35}
          >
            <title>Missing. No signal posted for this hour.</title>
          </rect>
        ))}
        {provisionalCells.map((p) => (
          <rect
            key={`provisional-${p.timestamp}`}
            x={x(p.timestamp) - hourWidthPx / 2}
            y={0}
            width={hourWidthPx}
            height={BAND_HEIGHT}
            fill="url(#peg-history-pending-hatch)"
          >
            <title>{provisionalTooltip(p.state, p.provisionalSubCoverage)}</title>
          </rect>
        ))}
        {segments.map((d, i) => (
          <path
            key={i}
            d={d}
            fill="none"
            stroke="var(--color-silo-oatmeal)"
            strokeWidth={1.5}
          />
        ))}
        {/* Defs placed last (SVG allows a <pattern> to be referenced from
            anywhere in the document, regardless of where <defs> itself
            sits) - deliberately not first, so it doesn't shift which
            element container.querySelector("rect") finds first in tests
            that assert on the untracked band's own rect. */}
        <defs>
          {/* Diagonal hatch for pending/disputed cells - a texture, not
              a solid fill, so the band reads as "incomplete data" at a
              glance rather than as a plotted value of any kind. */}
          <pattern id="peg-history-pending-hatch" width={4} height={4} patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width={4} height={4} fill="var(--color-cyber-tin)" fillOpacity={0.18} />
            <line x1={0} y1={0} x2={0} y2={4} stroke="var(--color-cyber-tin)" strokeWidth={1.5} />
          </pattern>
        </defs>
      </svg>

      <div className="mt-1 flex items-center justify-between font-mono text-[10px] text-cyber-tin/70">
        <span>{formatAxisDate(points[0].timestamp)}</span>
        <span>{formatAxisDate(points[points.length - 1].timestamp)}</span>
      </div>

      {historyStartsLate && (
        <p className="mt-2 font-mono text-[10px] text-cyber-tin">
          History starts when the asset was added.
        </p>
      )}

      <div className="mt-2 flex flex-col gap-1 font-mono text-[10px] uppercase tracking-wide text-cyber-tin">
        <span className="whitespace-nowrap">
          {confirmedCount} confirmed · {pendingCount} pending · {missingCount} missing
        </span>
        {pendingCount > 0 && (
          <span className="flex items-center gap-1.5 whitespace-nowrap">
            <span
              className="inline-block h-2.5 w-2.5 shrink-0 rounded-[1px]"
              style={{
                backgroundImage:
                  "repeating-linear-gradient(45deg, var(--color-cyber-tin) 0, var(--color-cyber-tin) 1px, transparent 1px, transparent 4px)",
                backgroundColor: "color-mix(in srgb, var(--color-cyber-tin) 18%, transparent)",
              }}
            />
            pending/disputed hour, no confirmed value yet
          </span>
        )}
        {missingCount > 0 && (
          <span className="flex items-center gap-1.5 whitespace-nowrap">
            <span className="inline-block h-2.5 w-2.5 shrink-0 rounded-[1px] bg-cement-grey/35" />
            missing hour, no signal posted
          </span>
        )}
        {depegThreshold !== null && (
          <span className="flex items-center gap-1.5 whitespace-nowrap text-risk-crimson-tint">
            <span className="inline-block h-px w-3 shrink-0 border-t border-dashed border-risk-crimson" />
            depeg threshold ({depegThreshold.toFixed(2)})
          </span>
        )}
      </div>
    </div>
  );
}
