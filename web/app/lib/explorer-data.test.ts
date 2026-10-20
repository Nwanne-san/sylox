import { describe, expect, it, vi, beforeEach } from "vitest";
import type { RingSlot, SlotState, EffectiveRingSlot } from "@sylox/sdk";

const RING_SLOTS = 240;
const PEG_RATIO_SCALE = 10_000_000;

const EMPTY_ENDPOINT = { tag: "Unknown", values: undefined } as const;

function emptySlot(): RingSlot {
  return {
    epoch: BigInt(0),
    state: { tag: "Empty", values: undefined },
    pending_until: BigInt(0),
    peg_ratio: BigInt(0),
    liquidity_2pct: BigInt(0),
    redemption_net: BigInt(0),
    supply: BigInt(0),
    supply_change_bps: 0,
    clawback_amount: BigInt(0),
    auth_revocations: 0,
    endpoint: EMPTY_ENDPOINT,
    provisional_sub_coverage: null,
  };
}

function finalSlot(epoch: bigint, pegRatio: number): RingSlot {
  return {
    ...emptySlot(),
    epoch,
    state: { tag: "Final", values: undefined },
    pending_until: BigInt(0),
    peg_ratio: BigInt(Math.round(pegRatio * PEG_RATIO_SCALE)),
  };
}

function pendingSlot(epoch: bigint, pegRatio: number, pendingUntil: bigint): RingSlot {
  return {
    ...emptySlot(),
    epoch,
    state: { tag: "Pending", values: undefined },
    pending_until: pendingUntil,
    peg_ratio: BigInt(Math.round(pegRatio * PEG_RATIO_SCALE)),
  };
}

function effectiveSlot(slot: RingSlot, effectiveState: SlotState["tag"]): EffectiveRingSlot {
  return { ...slot, effectiveState };
}

function fakeRiskScore(overrides: Partial<{ score: number; band: string; epoch: bigint }> = {}) {
  return {
    epoch: overrides.epoch ?? BigInt(1),
    score: overrides.score ?? 5,
    band: { tag: overrides.band ?? "Normal", values: undefined },
    formula_version: 1,
    stale: false,
  };
}

const { oracleRing, oracleScore, oracleAssets, assetDisplayName } = vi.hoisted(() => ({
  oracleRing: vi.fn(),
  oracleScore: vi.fn(),
  oracleAssets: vi.fn(),
  assetDisplayName: vi.fn(async () => "USDC:GISSUER"),
}));

const { checkStaleFn, eventInProgressFn, assetConfigFn } = vi.hoisted(() => ({
  checkStaleFn: vi.fn(async () => ({ result: { isErr: (): boolean => false, unwrap: (): boolean => false } })),
  eventInProgressFn: vi.fn(async () => ({ result: false })),
  assetConfigFn: vi.fn(async (): Promise<{ result: { home_domain: string } | null }> => ({ result: null })),
}));

vi.mock("./sylox-client", () => ({
  al: {
    oracle: {
      ring: oracleRing,
      score: oracleScore,
      assets: oracleAssets,
    },
  },
}));

vi.mock("@sylox/sdk", async () => {
  const actual = await vi.importActual<typeof import("@sylox/sdk")>("@sylox/sdk");
  return {
    ...actual,
    assetDisplayName,
    riskOracleClient: () => ({
      check_stale: checkStaleFn,
      event_in_progress: eventInProgressFn,
      asset_config: assetConfigFn,
    }),
  };
});

beforeEach(() => {
  vi.clearAllMocks();
  assetDisplayName.mockResolvedValue("USDC:GISSUER");
  checkStaleFn.mockResolvedValue({ result: { isErr: () => false, unwrap: () => false } });
  eventInProgressFn.mockResolvedValue({ result: false });
  assetConfigFn.mockResolvedValue({ result: null });
  oracleScore.mockResolvedValue(fakeRiskScore());
});

describe("fetchExplorerData - peg value", () => {
  it("agrees with the asset page's own walk (newestEffectiveFinalPegRatio) for the same ring fixture - one source of truth for 'current peg'", async () => {
    // The exact mismatch this fixes: an asset currently on the
    // sub-epoch posting path, where the newest hour overall is still
    // Pending but an OLDER hour is already effectively Final. Before
    // this fix, the Explorer card read RiskOracle.latest() here, which
    // only covers the hourly-fallback path and returns null in this
    // shape - the asset page's Confirmed card (fetchConfirmed in
    // asset-data.ts) already walked the ring itself and found the real
    // value. Both must now agree.
    const newestEpoch = BigInt(500_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 2] = effectiveSlot(finalSlot(newestEpoch - BigInt(1), 1.000025), "Final");
    ringSlots[RING_SLOTS - 1] = effectiveSlot(
      pendingSlot(newestEpoch, 0.97, BigInt("18446744073709551615")),
      "Pending",
    );

    oracleRing.mockResolvedValue(ringSlots);
    oracleAssets.mockResolvedValue(["ASSET"]);

    const { fetchExplorerData } = await import("./explorer-data");
    const { newestEffectiveFinalPegRatio } = await import("./asset-data");

    const { rows } = await fetchExplorerData();
    const expected = newestEffectiveFinalPegRatio(ringSlots);

    expect(rows).toHaveLength(1);
    expect(rows[0].pegRatio).not.toBeNull();
    expect(rows[0].pegRatio).toBeCloseTo(expected!.pegRatio, 5);
    expect(rows[0].pegRatio).toBeCloseTo(1.000025, 5);
  });

  it("shows null (never a stale latest() read) when the ring has no effectively-Final slot yet", async () => {
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    oracleRing.mockResolvedValue(ringSlots);
    oracleAssets.mockResolvedValue(["ASSET"]);

    const { fetchExplorerData } = await import("./explorer-data");
    const { rows } = await fetchExplorerData();

    expect(rows).toHaveLength(1);
    expect(rows[0].pegRatio).toBeNull();
  });
});

describe("fetchExplorerData - per-asset error isolation", () => {
  it("collects one asset's failure separately, without losing the other rows", async () => {
    const goodRing: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    goodRing[RING_SLOTS - 1] = effectiveSlot(finalSlot(BigInt(500_000), 1.0), "Final");

    oracleAssets.mockResolvedValue(["GOOD", "BAD"]);
    oracleRing.mockImplementation(async (asset: string) => {
      if (asset === "BAD") throw new Error("RPC unreachable");
      return goodRing;
    });

    const { fetchExplorerData } = await import("./explorer-data");
    const { rows, errors } = await fetchExplorerData();

    expect(rows).toHaveLength(1);
    expect(rows[0].pegRatio).toBeCloseTo(1.0, 5);
    expect(errors).toHaveLength(1);
    expect(errors[0].asset).toBe("BAD");
    expect(errors[0].message).toBe("RPC unreachable");
  });
});
