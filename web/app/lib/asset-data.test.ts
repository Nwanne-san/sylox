import { describe, expect, it, vi, beforeEach } from "vitest";
import type { RingSlot, SlotState, SignalSet, CoverGate, EventDefinition, EffectiveRingSlot } from "@sylox/sdk";

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

function fakeSignalSet(overrides: Partial<SignalSet> = {}): SignalSet {
  return {
    endpoint: { tag: "Unknown", values: undefined },
    epoch: BigInt(0),
    inputs_hash: Buffer.alloc(32),
    issuer_actions: { auth_revocations: 0, clawback_amount: BigInt(0), clawbacks: 0, flag_changes: 0 },
    liquidity_2pct: BigInt(0),
    peg_ratio: BigInt(0),
    peg_ratio_p10: BigInt(0),
    posted_at: BigInt(0),
    poster: "GPOSTER",
    supply: BigInt(0),
    supply_change_bps: 0,
    redemption_net: BigInt(0),
    ...overrides,
  };
}

function fakeIssuerFreezeDef(overrides: Partial<EventDefinition> = {}): EventDefinition {
  return {
    asset: "ASSET",
    auth_revocation_threshold: 2,
    challenge_secs: BigInt(86_400),
    cure_threshold: BigInt(0),
    depeg_threshold: BigInt(0),
    depeg_window_secs: BigInt(0),
    freeze_pct_bps: 500,
    halt_window_secs: BigInt(0),
    kind: { tag: "IssuerFreeze", values: undefined },
    max_missing_epochs: 0,
    mint_spike_bps: 0,
    version: 1,
    ...overrides,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any -- a test fixture, not a real generated-client value.
  } as any;
}

const { oracleRing, oracleScore, oracleLive, oracleFirstEpoch, oracleIsStale, eventsCoverGate, assetDisplayName } =
  vi.hoisted(() => ({
    oracleRing: vi.fn(),
    oracleScore: vi.fn(),
    oracleLive: vi.fn(),
    oracleFirstEpoch: vi.fn(),
    oracleIsStale: vi.fn(),
    eventsCoverGate: vi.fn(),
    assetDisplayName: vi.fn(async () => "USDC"),
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
      live: oracleLive,
      firstEpoch: oracleFirstEpoch,
      isStale: oracleIsStale,
    },
    events: {
      coverGate: eventsCoverGate,
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
    eventRegistryClient: () => ({}),
  };
});

beforeEach(() => {
  vi.clearAllMocks();
  assetDisplayName.mockResolvedValue("USDC");
  checkStaleFn.mockResolvedValue({ result: { isErr: () => false, unwrap: () => false } });
  eventInProgressFn.mockResolvedValue({ result: false });
  assetConfigFn.mockResolvedValue({ result: null });
  oracleScore.mockResolvedValue(fakeRiskScore());
});

describe("fetchConfirmed", () => {
  it("uses the newest EFFECTIVELY final slot's peg value, not latest()", async () => {
    const newestEpoch = BigInt(500_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 2] = effectiveSlot(finalSlot(newestEpoch - BigInt(1), 0.9995), "Final");
    ringSlots[RING_SLOTS - 1] = effectiveSlot(pendingSlot(newestEpoch, 0.97, BigInt(9_999_999_999)), "Pending");

    oracleRing.mockResolvedValue(ringSlots);
    oracleScore.mockResolvedValue(fakeRiskScore({ score: 42 }));

    const { fetchConfirmed } = await import("./asset-data");
    const { confirmed, latestPending } = await fetchConfirmed("ASSET");

    expect(confirmed).not.toBeNull();
    expect(confirmed!.epoch).toBe(newestEpoch - BigInt(1));
    expect(confirmed!.pegRatio).toBeCloseTo(0.9995, 5);
    expect(confirmed!.score).toBe(42);

    expect(latestPending).not.toBeNull();
    expect(latestPending!.epoch).toBe(newestEpoch);
    expect(latestPending!.pegRatio).toBeCloseTo(0.97, 5);
    expect(latestPending!.pendingUntil).toBe(BigInt(9_999_999_999));
  });

  it("promotes a Pending-past-deadline newest slot into Confirmed, with no separate latestPending", async () => {
    const newestEpoch = BigInt(500_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 1] = effectiveSlot(pendingSlot(newestEpoch, 0.998, BigInt(1000)), "Final");

    oracleRing.mockResolvedValue(ringSlots);

    const { fetchConfirmed } = await import("./asset-data");
    const { confirmed, latestPending } = await fetchConfirmed("ASSET");

    expect(confirmed).not.toBeNull();
    expect(confirmed!.epoch).toBe(newestEpoch);
    expect(latestPending).toBeNull();
  });

  it("returns null for both when the ring has no real slots yet", async () => {
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    oracleRing.mockResolvedValue(ringSlots);

    const { fetchConfirmed } = await import("./asset-data");
    const { confirmed, latestPending } = await fetchConfirmed("ASSET");

    expect(confirmed).toBeNull();
    expect(latestPending).toBeNull();
  });

  it("maps the sentinel pending_until (u64::MAX, an hour still waiting on its sub-epochs) to pendingUntil: null, never a raw bigint a formatter could overflow on", async () => {
    // The real shape RiskOracle.ring() returns for a waiting hour:
    // HOUR_PENDING_SENTINEL (contracts/risk-oracle/src/lib.rs's
    // refresh_waiting_hour), confirmed directly against live testnet
    // USDC data before this fix (113 of 240 ring hours carried exactly
    // this value). Number(18446744073709551615n) * 1000 overflows past
    // Number.MAX_SAFE_INTEGER, which is what produced "can still be
    // challenged until NaN:NaN UTC" in AssetPageClient.tsx before this
    // fix - new Date() on that overflowed number is an Invalid Date, and
    // getUTCHours()/getUTCMinutes() on an Invalid Date both return NaN.
    const HOUR_PENDING_SENTINEL = BigInt("18446744073709551615");
    const newestEpoch = BigInt(500_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 1] = effectiveSlot(
      pendingSlot(newestEpoch, 0.9995, HOUR_PENDING_SENTINEL),
      "Pending",
    );

    oracleRing.mockResolvedValue(ringSlots);

    const { fetchConfirmed } = await import("./asset-data");
    const { latestPending } = await fetchConfirmed("ASSET");

    expect(latestPending).not.toBeNull();
    expect(latestPending!.pegRatio).toBeCloseTo(0.9995, 5);
    expect(latestPending!.pendingUntil).toBeNull();
  });

  it("keeps a real, finite pending_until as a bigint (not null) when the hour genuinely has a deadline", async () => {
    const newestEpoch = BigInt(500_000);
    const realDeadline = BigInt(1_800_000_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 1] = effectiveSlot(pendingSlot(newestEpoch, 0.9995, realDeadline), "Pending");

    oracleRing.mockResolvedValue(ringSlots);

    const { fetchConfirmed } = await import("./asset-data");
    const { latestPending } = await fetchConfirmed("ASSET");

    expect(latestPending).not.toBeNull();
    expect(latestPending!.pendingUntil).toBe(realDeadline);
  });
});

describe("newestEffectiveFinalPegRatio", () => {
  it("is the same walk fetchConfirmed uses - the one source of truth every screen that shows 'current peg' reads from", async () => {
    const newestEpoch = BigInt(500_000);
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    ringSlots[RING_SLOTS - 2] = effectiveSlot(finalSlot(newestEpoch - BigInt(1), 1.000025), "Final");
    ringSlots[RING_SLOTS - 1] = effectiveSlot(pendingSlot(newestEpoch, 0.97, BigInt(9_999_999_999)), "Pending");

    const { newestEffectiveFinalPegRatio } = await import("./asset-data");
    const result = newestEffectiveFinalPegRatio(ringSlots);

    // Matches the asset page's Confirmed card (fetchConfirmed, tested
    // above with the identical ring shape): the newest EFFECTIVELY Final
    // slot's value, never the still-Pending newer one.
    expect(result).not.toBeNull();
    expect(result!.epoch).toBe(newestEpoch - BigInt(1));
    expect(result!.pegRatio).toBeCloseTo(1.000025, 5);
  });

  it("returns null when no slot is effectively Final yet", async () => {
    const ringSlots: EffectiveRingSlot[] = Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty"));
    const { newestEffectiveFinalPegRatio } = await import("./asset-data");
    expect(newestEffectiveFinalPegRatio(ringSlots)).toBeNull();
  });
});

describe("fetchLive", () => {
  it("returns 'none' when al.oracle.live resolves to null", async () => {
    oracleLive.mockResolvedValue(null);

    const { fetchLive } = await import("./asset-data");
    const value = await fetchLive("ASSET");

    expect(value).toEqual({ status: "none" });
  });

  it("maps the SDK's raw [SubEpoch, SignalSet, SlotState] tuple to the LiveValue display shape", async () => {
    oracleLive.mockResolvedValue([
      { hour: BigInt(497_606), sub: 7 },
      fakeSignalSet({ peg_ratio: BigInt(9_995_000), posted_at: BigInt(1_000_000) }),
      { tag: "Pending", values: undefined },
    ]);

    const { fetchLive } = await import("./asset-data");
    const value = await fetchLive("ASSET");

    expect(value.status).toBe("value");
    if (value.status !== "value") throw new Error("expected 'value'");
    expect(value.subEpoch).toEqual({ hour: BigInt(497_606), sub: 7 });
    expect(value.pegRatio).toBeCloseTo(0.9995, 5);
    expect(value.slotState).toBe("Pending");
    expect(value.postedAt).toBe(1_000_000);
  });
});

describe("fetchScoringStart", () => {
  it("says 'unknown' when al.oracle.firstEpoch resolves to null (never posted)", async () => {
    oracleFirstEpoch.mockResolvedValue(null);

    const { fetchScoringStart } = await import("./asset-data");
    const start = await fetchScoringStart("ASSET");

    expect(start).toEqual({ status: "unknown" });
  });

  it("says 'unknown' when al.oracle.firstEpoch throws a SyloxError - restoring the old silent-fallback UX the SDK's own firstEpoch() deliberately deviates from", async () => {
    oracleFirstEpoch.mockRejectedValue(new Error("couldn't read first_epoch"));

    const { fetchScoringStart } = await import("./asset-data");
    const start = await fetchScoringStart("ASSET");

    expect(start).toEqual({ status: "unknown" });
  });

  it("computes (first_epoch + 168h) + SIGNAL_DISPUTE_SECS for the real USDC test case (first_epoch=497606 -> 2026-10-14T16:00:00Z)", async () => {
    oracleFirstEpoch.mockResolvedValue(BigInt(497_606));

    const { fetchScoringStart } = await import("./asset-data");
    const start = await fetchScoringStart("ASSET");

    expect(start.status).toBe("pending");
    if (start.status !== "pending") throw new Error("expected 'pending'");
    expect(new Date(start.earliestUnixSecs * 1000).toISOString()).toBe("2026-10-14T16:00:00.000Z");
  });
});

describe("fetchCoverGate", () => {
  it("says sales are paused for staleness before ever checking al.oracle.score or al.events.coverGate", async () => {
    checkStaleFn.mockResolvedValue({ result: { isErr: () => false, unwrap: () => true } });

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Stale");
    expect(status.label).toMatch(/not enough price history yet/i);
    expect(oracleScore).not.toHaveBeenCalled();
    expect(eventsCoverGate).not.toHaveBeenCalled();
  });

  it("treats a check_stale Err as stale (fail-safe default)", async () => {
    checkStaleFn.mockResolvedValue({ result: { isErr: () => true, unwrap: () => false } });

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Stale");
  });

  it("says sales are paused for a Distress band, even when coverGate itself would be Clear", async () => {
    checkStaleFn.mockResolvedValue({ result: { isErr: () => false, unwrap: () => false } });
    oracleScore.mockResolvedValue(fakeRiskScore({ band: "Distress" }));

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Distressed");
    expect(status.label).toMatch(/the asset is in distress/i);
    expect(eventsCoverGate).not.toHaveBeenCalled();
  });

  it("says sales are paused for an Event band", async () => {
    oracleScore.mockResolvedValue(fakeRiskScore({ band: "Event" }));

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Distressed");
  });

  it("falls through to coverGate's own reason once stale and band both pass", async () => {
    oracleScore.mockResolvedValue(fakeRiskScore({ band: "Watch" }));
    eventsCoverGate.mockResolvedValue({ tag: "RecentDepeg", values: undefined } as CoverGate);

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("RecentDepeg");
    expect(status.label).toMatch(/below the threshold/i);
  });

  it("only says sales can go through when stale, band, and coverGate all pass", async () => {
    oracleScore.mockResolvedValue(fakeRiskScore({ band: "Normal" }));
    eventsCoverGate.mockResolvedValue({ tag: "Clear", values: undefined } as CoverGate);

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Clear");
    expect(status.label).toBe("Cover can be sold.");
  });

  it("treats Watch band as not-distressed", async () => {
    oracleScore.mockResolvedValue(fakeRiskScore({ band: "Watch" }));
    eventsCoverGate.mockResolvedValue({ tag: "Clear", values: undefined } as CoverGate);

    const { fetchCoverGate } = await import("./asset-data");
    const status = await fetchCoverGate("ASSET");

    expect(status.gate).toBe("Clear");
  });
});

describe("sentenceFor - IssuerFreeze", () => {
  it("says 'more than N revocations' when the threshold is above 0", async () => {
    const { sentenceFor } = await import("./asset-data");
    const sentence = sentenceFor("IssuerFreeze", fakeIssuerFreezeDef({ auth_revocation_threshold: 2 }));
    expect(sentence).toBe(
      "Issuer freeze: clawbacks or authorization revocations above 5% of supply within 7 days, or more than 2 revocations in that window.",
    );
  });

  it("says 'any authorization revocation' instead of 'more than 0 revocations' when the threshold is 0", async () => {
    const { sentenceFor } = await import("./asset-data");
    const sentence = sentenceFor("IssuerFreeze", fakeIssuerFreezeDef({ auth_revocation_threshold: 0 }));
    expect(sentence).toBe(
      "Issuer freeze: clawbacks or authorization revocations above 5% of supply within 7 days, or any authorization revocation in that window.",
    );
    expect(sentence).not.toMatch(/more than 0/i);
  });
});

describe("fetchHeader (via fetchAssetPageData)", () => {
  it("reports eventInProgress/homeDomain straight from RiskOracle's own event_in_progress/asset_config reads", async () => {
    eventInProgressFn.mockResolvedValue({ result: true });
    assetConfigFn.mockResolvedValue({ result: { home_domain: "usdc.example.com" } });
    oracleScore.mockResolvedValue(fakeRiskScore({ epoch: BigInt(10), score: 90, band: "Normal" }));
    oracleRing.mockResolvedValue(Array.from({ length: RING_SLOTS }, () => effectiveSlot(emptySlot(), "Empty")));
    oracleLive.mockResolvedValue(null);
    eventsCoverGate.mockResolvedValue({ tag: "Clear", values: undefined } as CoverGate);

    const { fetchAssetPageData } = await import("./asset-data");
    const data = await fetchAssetPageData("ASSET");

    expect(data.header.status).toBe("ok");
    if (data.header.status !== "ok") throw new Error("expected 'ok'");
    expect(data.header.value.eventInProgress).toBe(true);
    expect(data.header.value.homeDomain).toBe("usdc.example.com");
    expect(data.header.value.score).toBe(90);
  });

  it("reshapes a thrown SyloxError from al.oracle.score into a plain section error message, never crashing the page", async () => {
    oracleScore.mockRejectedValue(new Error("This asset isn't tracked by the oracle."));

    const { fetchAssetPageData } = await import("./asset-data");
    const data = await fetchAssetPageData("ASSET");

    expect(data.header.status).toBe("error");
    if (data.header.status !== "error") throw new Error("expected 'error'");
    expect(data.header.message).toBe("This asset isn't tracked by the oracle.");
  });
});
