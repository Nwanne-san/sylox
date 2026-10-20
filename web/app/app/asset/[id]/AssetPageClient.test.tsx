import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { AssetPageClient } from "./AssetPageClient";
import { WalletProvider } from "@/lib/wallet/WalletContext";
import { RISK_BANDS } from "@sylox/ui/risk-bands";
import type { AssetPageData, AssetHeader, PegHistoryPoint } from "@/lib/asset-data";

function renderAssetPage(asset: string) {
  return render(
    <WalletProvider>
      <AssetPageClient asset={asset} />
    </WalletProvider>,
  );
}

const { fetchAssetPageData } = vi.hoisted(() => ({
  fetchAssetPageData: vi.fn(),
}));

vi.mock("@/lib/asset-data", async () => {
  const actual = await vi.importActual<typeof import("@/lib/asset-data")>("@/lib/asset-data");
  return { ...actual, fetchAssetPageData };
});

const normalBand = RISK_BANDS.find((b) => b.name === "normal")!;

const baseHeader: AssetHeader = {
  asset: "CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA",
  code: "USDC",
  homeDomain: "centre.io",
  band: normalBand,
  score: 12,
  stale: false,
  eventInProgress: false,
  eventDeclared: false,
  scoringStart: { status: "unknown" },
};

function okData(overrides: Partial<AssetPageData> = {}): AssetPageData {
  return {
    header: { status: "ok", value: baseHeader },
    confirmed: {
      status: "ok",
      value: {
        confirmed: { epoch: BigInt(100), pegRatio: 0.9998, score: 12, band: normalBand },
        latestPending: null,
      },
    },
    live: { status: "ok", value: { status: "none" } },
    pegHistory: { status: "ok", value: [] },
    failureDefinitions: { status: "ok", value: [] },
    coverGate: { status: "ok", value: { gate: "Clear", label: "Cover can be sold." } },
    activeEventCount: { status: "ok", value: { count: 0, eventIds: [] } },
    ...overrides,
  };
}

describe("AssetPageClient", () => {
  it("always shows the testnet banner", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);
    expect(screen.getByText(/testnet\. prices are sample data/i)).toBeInTheDocument();
  });

  it("shows a loading state before data arrives", () => {
    fetchAssetPageData.mockReturnValue(new Promise(() => {}));
    renderAssetPage(baseHeader.asset);
    expect(screen.getByText(/loading asset from the oracle/i)).toBeInTheDocument();
  });

  it("shows a page-wide error when the fetch itself rejects", async () => {
    fetchAssetPageData.mockRejectedValue(new Error("RPC unreachable"));
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByRole("alert")).toHaveTextContent("RPC unreachable");
  });

  it("renders the header with code, domain, band, and score", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);

    // "USDC" now also appears in the breadcrumb (it shows the real
    // asset code once the header loads, not just the shortened address
    // - see the breadcrumb-specific tests below), so this checks the
    // <h1> specifically rather than any "USDC" text on the page.
    const heading = await screen.findByRole("heading", { level: 1 });
    expect(heading).toHaveTextContent("USDC");
    expect(screen.getByText("centre.io")).toBeInTheDocument();
    expect(screen.getByText("Normal")).toBeInTheDocument();
    expect(screen.getByText("12")).toBeInTheDocument();
    expect(screen.queryByText("Stale")).not.toBeInTheDocument();
  });

  it("shows stale and event tags when the header reports them", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        header: {
          status: "ok",
          value: { ...baseHeader, stale: true, eventInProgress: true, eventDeclared: true },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText("Stale")).toBeInTheDocument();
    expect(screen.getByText("Event in progress")).toBeInTheDocument();
    expect(screen.getByText("Event declared")).toBeInTheDocument();
  });

  it("shows the asset code as the big title, with domain and address small underneath", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);

    const heading = await screen.findByRole("heading", { level: 1 });
    expect(heading).toHaveTextContent("USDC");
  });

  it("shows 'Not scored yet' with an explanation instead of a fake band, when there is no real score and first_epoch is unknown", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        header: {
          status: "ok",
          value: { ...baseHeader, band: null, score: null, scoringStart: { status: "unknown" } },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText("Not scored yet")).toBeInTheDocument();
    expect(screen.getByText(/^not scored yet\.$/i)).toBeInTheDocument();
    // Never a leftover "Normal" (or any other) band badge alongside it.
    expect(screen.queryByText("Normal")).not.toBeInTheDocument();
    // Never a redundant dash + "score" row underneath - the badge and
    // explanation above already say there's no score.
    expect(screen.queryByText("score")).not.toBeInTheDocument();
    expect(screen.queryByText("—")).not.toBeInTheDocument();
  });

  it("shows the earliest scoring date, worded as 'around', when first_epoch is known", async () => {
    // The real USDC test case: first_epoch=497606 -> earliest scoring
    // moment 2026-10-14T16:00:00Z ((497606 + 168) * 3600 + 7200 seconds,
    // the 2h SIGNAL_DISPUTE_SECS the 168th hour needs to clear before
    // going Final).
    const earliestUnixSecs = (497606 + 168) * 3600 + 7200;
    fetchAssetPageData.mockResolvedValue(
      okData({
        header: {
          status: "ok",
          value: {
            ...baseHeader,
            band: null,
            score: null,
            scoringStart: { status: "pending", earliestUnixSecs },
          },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(
      await screen.findByText(/scoring starts around 2026-10-14 16:00 utc/i),
    ).toBeInTheDocument();
  });

  it("falls back to a shortened address when there is no home domain", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({ header: { status: "ok", value: { ...baseHeader, homeDomain: null } } }),
    );
    renderAssetPage(baseHeader.asset);

    // The shortened address fallback appears twice once there's no home
    // domain: the domain line and the address subtitle just below it.
    // The breadcrumb is a separate matter - it shows the real asset
    // code (header.code) once the header has loaded, not the shortened
    // address, regardless of whether homeDomain is set (see the
    // breadcrumb-specific tests below) - "USDC" here, same as the <h1>.
    const heading = await screen.findByRole("heading", { level: 1 });
    expect(heading).toHaveTextContent("USDC");
    expect(screen.getAllByText("CBIE…DAMA")).toHaveLength(2);
  });

  it("breadcrumb shows the real asset code once the header loads, not the shortened address", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);

    await screen.findByRole("heading", { level: 1 });
    const breadcrumb = screen.getByRole("navigation", { name: "Breadcrumb" });
    expect(breadcrumb).toHaveTextContent("USDC");
    expect(breadcrumb).not.toHaveTextContent("CBIE…DAMA");
  });

  it("breadcrumb shows the shortened address before the header has loaded", () => {
    fetchAssetPageData.mockReturnValue(new Promise(() => {})); // Never resolves - stays in the loading state.
    renderAssetPage(baseHeader.asset);

    const breadcrumb = screen.getByRole("navigation", { name: "Breadcrumb" });
    expect(breadcrumb).toHaveTextContent("CBIE…DAMA");
  });

  it("breadcrumb falls back to the shortened address if the header section itself errored", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({ header: { status: "error", message: "boom" } }),
    );
    renderAssetPage(baseHeader.asset);

    await screen.findByText(/couldn't load this section/i);
    const breadcrumb = screen.getByRole("navigation", { name: "Breadcrumb" });
    expect(breadcrumb).toHaveTextContent("CBIE…DAMA");
  });

  it("titles the Confirmed/Live section 'Peg price'", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText("+ PEG PRICE")).toBeInTheDocument();
    expect(screen.queryByText("+ SCORE")).not.toBeInTheDocument();
  });

  it("shows the confirmed peg value", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText("0.9998")).toBeInTheDocument();
  });

  it("shows 'no confirmed value yet' instead of a fake number", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({ confirmed: { status: "ok", value: { confirmed: null, latestPending: null } } }),
    );
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/no confirmed value yet/i)).toBeInTheDocument();
  });

  it("shows the latest pending hour separately, with its real challenge deadline", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        confirmed: {
          status: "ok",
          value: {
            confirmed: { epoch: BigInt(99), pegRatio: 0.9998, score: 12, band: normalBand },
            latestPending: {
              epoch: BigInt(100),
              pegRatio: 0.991,
              pendingUntil: BigInt(Date.UTC(2026, 0, 1, 14, 30) / 1000),
            },
          },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText(/latest hour: 0\.9910 peg/i)).toBeInTheDocument();
    expect(screen.getByText(/can still be challenged until 14:30 utc/i)).toBeInTheDocument();
  });

  it("shows a 'still posting' note instead of a challenge deadline when pendingUntil is null (the hour's own sub-epochs aren't fully decided yet)", async () => {
    // Regression for "can still be challenged until NaN:NaN UTC":
    // asset-data.ts's fetchConfirmed now maps the contract's
    // HOUR_PENDING_SENTINEL to pendingUntil: null rather than a raw
    // bigint a date formatter would overflow on. This asserts the UI
    // side never calls formatChallengeDeadline for that case and never
    // renders "NaN" anywhere.
    fetchAssetPageData.mockResolvedValue(
      okData({
        confirmed: {
          status: "ok",
          value: {
            confirmed: { epoch: BigInt(99), pegRatio: 0.9998, score: 12, band: normalBand },
            latestPending: {
              epoch: BigInt(100),
              pegRatio: 0.991,
              pendingUntil: null,
            },
          },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(
      await screen.findByText(/latest hour: 0\.9910 peg, provisional while its sub-epochs are still posting\./i),
    ).toBeInTheDocument();
    expect(screen.queryByText(/can still be challenged until/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/NaN/i)).not.toBeInTheDocument();
  });

  it("shows 'No live update yet' when live() resolves to None", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/no live update yet/i)).toBeInTheDocument();
  });

  it("shows the sub-epoch, peg ratio, slot state, and age when live() resolves to Some", async () => {
    const postedAt = Math.floor(Date.now() / 1000) - 120; // 2 minutes ago
    fetchAssetPageData.mockResolvedValue(
      okData({
        live: {
          status: "ok",
          value: {
            status: "value",
            subEpoch: { hour: BigInt(497606), sub: 7 },
            pegRatio: 0.9995,
            slotState: "Pending",
            postedAt,
          },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText("Live (challengeable)")).toBeInTheDocument();
    expect(screen.getByText("0.9995")).toBeInTheDocument();
    expect(screen.getByText(/hour 497606, sub-epoch 7/i)).toBeInTheDocument();
    expect(screen.getByText(/pending/i)).toBeInTheDocument();
    expect(screen.getByText(/2 min ago/i)).toBeInTheDocument();
  });

  it("renders failure definitions as plain sentences", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        failureDefinitions: {
          status: "ok",
          value: [
            {
              kind: "Depeg",
              sentence: "Depeg: below 0.95 of peg for 72 hours, up to 6 missing hours allowed.",
            },
          ],
        },
      }),
    );
    renderAssetPage(baseHeader.asset);
    expect(
      await screen.findByText(/depeg: below 0\.95 of peg for 72 hours/i),
    ).toBeInTheDocument();
  });

  it("shows cover-sale status from cover_gate", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        coverGate: {
          status: "ok",
          value: { gate: "RecentDepeg", label: "Sales paused: the price was below the threshold in the last depeg window." },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/sales paused: the price was below/i)).toBeInTheDocument();
  });

  it("shows sales paused for staleness, combined from cover_gate + stale + band", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        coverGate: {
          status: "ok",
          value: { gate: "Stale", label: "Sales paused: not enough price history yet." },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/sales paused: not enough price history yet/i)).toBeInTheDocument();
  });

  it("shows sales paused for distress even when cover_gate alone would be Clear", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        coverGate: {
          status: "ok",
          value: { gate: "Distressed", label: "Sales paused: the asset is in distress." },
        },
      }),
    );
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/sales paused: the asset is in distress/i)).toBeInTheDocument();
  });

  it("shows an active-event card when active_event_count > 0, linking to each event", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({
        activeEventCount: { status: "ok", value: { count: 2, eventIds: [BigInt(7), BigInt(9)] } },
      }),
    );
    renderAssetPage(baseHeader.asset);
    expect(await screen.findByText(/2 active events for this asset/i)).toBeInTheDocument();

    const links = screen.getAllByRole("link", { name: /view event/i });
    expect(links.map((l) => l.getAttribute("href"))).toEqual(["/event/7", "/event/9"]);
  });

  it("shows no active-event card when the count is zero", async () => {
    fetchAssetPageData.mockResolvedValue(okData());
    renderAssetPage(baseHeader.asset);
    await screen.findByRole("heading", { level: 1 });
    expect(screen.queryByText(/active event/i)).not.toBeInTheDocument();
  });

  it("isolates a single section's failure without breaking the rest of the page", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({ coverGate: { status: "error", message: "cover_gate unreachable" } }),
    );
    renderAssetPage(baseHeader.asset);

    // The failing section shows its own error...
    expect(await screen.findByText(/cover_gate unreachable/i)).toBeInTheDocument();
    // ...while an unrelated section still renders normally.
    expect(screen.getByText("0.9998")).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("USDC");
  });

  it("never renders a missing or pending hour as a zero value in the chart", async () => {
    const pegHistory: PegHistoryPoint[] = [
      { timestamp: 0, epoch: BigInt(0), state: "Final", pegRatio: 1.0, tracked: true, provisionalSubCoverage: null },
      { timestamp: 3600, epoch: BigInt(1), state: "Pending", pegRatio: null, tracked: true, provisionalSubCoverage: null },
      { timestamp: 7200, epoch: BigInt(2), state: "Empty", pegRatio: null, tracked: true, provisionalSubCoverage: null },
      { timestamp: 10800, epoch: BigInt(3), state: "Final", pegRatio: 0.999, tracked: true, provisionalSubCoverage: null },
    ];
    fetchAssetPageData.mockResolvedValue(okData({ pegHistory: { status: "ok", value: pegHistory } }));
    renderAssetPage(baseHeader.asset);

    const svg = await screen.findByRole("img", {
      name: /2 confirmed, 1 pending, 1 missing of 4 tracked hours/i,
    });
    expect(svg).toBeInTheDocument();

    // Two known points, zero gaps drawn through: exactly two disconnected
    // single-point paths (no path spans the Pending/Empty gap), and no
    // path coordinate sits at the chart's zero-value baseline.
    const paths = svg.querySelectorAll("path");
    expect(paths).toHaveLength(2);
    for (const path of paths) {
      const d = path.getAttribute("d") ?? "";
      expect(d.trim().split(" ")).toHaveLength(1); // one lone "M x,y" - no "L" segment bridging the gap
    }
  });

  it("shows a 'history starts when the asset was added' note and excludes untracked hours from the legend", async () => {
    const pegHistory: PegHistoryPoint[] = [
      { timestamp: 0, epoch: BigInt(0), state: "Empty", pegRatio: null, tracked: false, provisionalSubCoverage: null },
      { timestamp: 3600, epoch: BigInt(1), state: "Empty", pegRatio: null, tracked: false, provisionalSubCoverage: null },
      { timestamp: 7200, epoch: BigInt(2), state: "Final", pegRatio: 1.0, tracked: true, provisionalSubCoverage: null },
      { timestamp: 10800, epoch: BigInt(3), state: "Empty", pegRatio: null, tracked: true, provisionalSubCoverage: null },
    ];
    fetchAssetPageData.mockResolvedValue(okData({ pegHistory: { status: "ok", value: pegHistory } }));
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText(/history starts when the asset was added/i)).toBeInTheDocument();
    // Legend counts only the 2 tracked hours (1 confirmed, 1 missing) -
    // the 2 untracked hours before first_epoch are never called "missing".
    const svg = await screen.findByRole("img", {
      name: /1 confirmed, 0 pending, 1 missing of 2 tracked hours/i,
    });
    expect(svg).toBeInTheDocument();
  });

  it("shows no 'history starts' note when every hour is tracked", async () => {
    const pegHistory: PegHistoryPoint[] = [
      { timestamp: 0, epoch: BigInt(0), state: "Final", pegRatio: 1.0, tracked: true, provisionalSubCoverage: null },
      { timestamp: 3600, epoch: BigInt(1), state: "Empty", pegRatio: null, tracked: true, provisionalSubCoverage: null },
    ];
    fetchAssetPageData.mockResolvedValue(okData({ pegHistory: { status: "ok", value: pegHistory } }));
    renderAssetPage(baseHeader.asset);

    await screen.findByRole("img", { name: /1 confirmed, 0 pending, 1 missing of 2 tracked hours/i });
    expect(screen.queryByText(/history starts when the asset was added/i)).not.toBeInTheDocument();
  });

  it("shows start and end date labels under the chart", async () => {
    const pegHistory: PegHistoryPoint[] = [
      { timestamp: Date.UTC(2026, 8, 29) / 1000, epoch: BigInt(0), state: "Final", pegRatio: 1.0, tracked: true, provisionalSubCoverage: null },
      { timestamp: Date.UTC(2026, 9, 9) / 1000, epoch: BigInt(1), state: "Final", pegRatio: 1.0, tracked: true, provisionalSubCoverage: null },
    ];
    fetchAssetPageData.mockResolvedValue(okData({ pegHistory: { status: "ok", value: pegHistory } }));
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText(/sep 29/i)).toBeInTheDocument();
    expect(screen.getByText(/oct 9/i)).toBeInTheDocument();
  });

  it("shows the chart's own error without losing other sections", async () => {
    fetchAssetPageData.mockResolvedValue(
      okData({ pegHistory: { status: "error", message: "ring() unreachable" } }),
    );
    renderAssetPage(baseHeader.asset);

    expect(await screen.findByText(/ring\(\) unreachable/i)).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("USDC");
  });
});
