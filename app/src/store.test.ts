import { describe, expect, it, vi, beforeEach } from "vitest";

// The store talks to the backend through `api`, so the whole IPC surface is
// stubbed here. Only the five calls `loadStats` makes are interesting; the rest
// exist so `boot`/`refresh` can run without exploding.
const deferred = <T>() => {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
};

const stats = vi.fn();
const sources = vi.fn();
const series = vi.fn();
const ledger = vi.fn();
const insights = vi.fn();

vi.mock("./ipc", () => ({
  api: {
    statsOverview: (...a: unknown[]) => stats(...a),
    sourcesOverview: (...a: unknown[]) => sources(...a),
    usageSeries: (...a: unknown[]) => series(...a),
    ledgerOverview: (...a: unknown[]) => ledger(...a),
    ledgerInsights: (...a: unknown[]) => insights(...a),
  },
}));

const { useStore } = await import("./store");

const LEDGER = { periodLabel: "Last 7 days", sources: [], projects: [], empty: false };

beforeEach(() => {
  vi.clearAllMocks();
  useStore.setState({ ledger: null, insights: [], stats: null, sources: null, series: null });
  sources.mockResolvedValue({});
  series.mockResolvedValue({});
  insights.mockResolvedValue([]);
});

describe("loadStats", () => {
  it("renders the ledger without waiting on attribution", async () => {
    // `stats_overview` waits on the attribution bundle: a full session rescan
    // plus a bootstrapped CI per saver. On a large history that is tens of
    // seconds, while the ledger query is a couple. These used to share one
    // Promise.all, so the Ledger - the default tab - sat on its loading
    // placeholder over a blank pane until attribution finished.
    const slow = deferred<unknown>();
    stats.mockReturnValue(slow.promise);
    ledger.mockResolvedValue(LEDGER);

    const done = useStore.getState().loadStats();
    // Let the resolved queries commit while `stats_overview` is still pending.
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    expect(useStore.getState().ledger).toEqual(LEDGER);
    expect(useStore.getState().stats).toBeNull();

    slow.resolve({ ok: true });
    await done;
    expect(useStore.getState().stats).toEqual({ ok: true });
  });

  it("keeps the screens that loaded when one query fails", async () => {
    stats.mockRejectedValue(new Error("attribution blew up"));
    ledger.mockResolvedValue(LEDGER);

    await useStore.getState().loadStats();

    expect(useStore.getState().ledger).toEqual(LEDGER);
    expect(useStore.getState().banner).not.toBeNull();
  });
});
