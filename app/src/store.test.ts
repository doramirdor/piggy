import { describe, expect, it, vi, beforeEach } from "vitest";
import type { SaverRow, SaversState } from "./types";

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
const advisorSavers = vi.fn();

vi.mock("./ipc", () => ({
  api: {
    statsOverview: (...a: unknown[]) => stats(...a),
    sourcesOverview: (...a: unknown[]) => sources(...a),
    usageSeries: (...a: unknown[]) => series(...a),
    ledgerOverview: (...a: unknown[]) => ledger(...a),
    ledgerInsights: (...a: unknown[]) => insights(...a),
    advisorSavers: (...a: unknown[]) => advisorSavers(...a),
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

/** One saver row, with only the fields the notes key reads spelled out. */
const row = (id: string, delta: number | null, kind = "measured", enabled = true): SaverRow =>
  ({ id, enabled, badge: { kind, delta, n: 20, nOn: 10, nOff: 10 } }) as unknown as SaverRow;

const saversState = (...savers: SaverRow[]): SaversState => ({ masterOn: true, savers });

describe("loadSaverNotes", () => {
  const NOTE = [{ insightId: "saver:a", headline: "h", why: "w", model: "m" }];

  beforeEach(() => {
    useStore.setState({ savers: null, saverNotes: [], saverNotesFor: null, banner: null });
    advisorSavers.mockResolvedValue(NOTE);
  });

  it("asks once while the reading it annotates holds still", async () => {
    useStore.setState({ savers: saversState(row("a", -0.22)) });

    await useStore.getState().loadSaverNotes();
    // A refresh that re-fetches the same measurement: new object, same reading.
    useStore.setState({ savers: saversState(row("a", -0.22)) });
    await useStore.getState().loadSaverNotes();

    expect(advisorSavers).toHaveBeenCalledTimes(1);
    expect(useStore.getState().saverNotes).toEqual(NOTE);
  });

  it("re-asks when the finding under the note moves", async () => {
    // Still measuring, so the prose is written about a saver with no result.
    useStore.setState({ savers: saversState(row("a", null, "measuring")) });
    await useStore.getState().loadSaverNotes();
    expect(advisorSavers).toHaveBeenCalledTimes(1);

    // It settles. The row now prints a delta the old prose knows nothing about.
    useStore.setState({ savers: saversState(row("a", -0.22)) });
    await useStore.getState().loadSaverNotes();
    expect(advisorSavers).toHaveBeenCalledTimes(2);

    // And when the user switches it off, which changes the comparison itself.
    useStore.setState({ savers: saversState(row("a", -0.22, "measured", false)) });
    await useStore.getState().loadSaverNotes();
    expect(advisorSavers).toHaveBeenCalledTimes(3);
  });

  it("does not re-ask for a delta that only moved inside the printed digit", async () => {
    // A new session nudges the median. The row still prints "22%", so the prose
    // beside it still describes what the reader sees, and a re-ask would mean
    // loading a ~3GB model for a change nothing on screen shows.
    useStore.setState({ savers: saversState(row("a", -0.2201)) });
    await useStore.getState().loadSaverNotes();
    useStore.setState({ savers: saversState(row("a", -0.2204)) });
    await useStore.getState().loadSaverNotes();

    expect(advisorSavers).toHaveBeenCalledTimes(1);
  });

  it("re-arms after a failure without discarding the reading mid-flight", async () => {
    advisorSavers.mockRejectedValue(new Error("model would not load"));
    useStore.setState({ savers: saversState(row("a", -0.22)) });

    await useStore.getState().loadSaverNotes();

    // Silent: the deterministic summary on the row is already on screen.
    expect(useStore.getState().banner).toBeNull();
    expect(useStore.getState().saverNotesFor).toBeNull();
    // A later visit retries rather than being stuck at "asked".
    advisorSavers.mockResolvedValue(NOTE);
    await useStore.getState().loadSaverNotes();
    expect(useStore.getState().saverNotes).toEqual(NOTE);
  });

  it("stays silent when there is no saver to advise on", async () => {
    useStore.setState({ savers: saversState() });
    await useStore.getState().loadSaverNotes();
    expect(advisorSavers).not.toHaveBeenCalled();
  });
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
