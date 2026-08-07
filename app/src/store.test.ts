import { describe, expect, it, vi, beforeEach } from "vitest";
import type { AdviceItem, AdviceReport, SaverRow, SaversState } from "./types";

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
const adviceReport = vi.fn();
const adviceApply = vi.fn();
const adviceUndo = vi.fn();
const adviceDismiss = vi.fn();
const saversList = vi.fn();

vi.mock("./ipc", () => ({
  api: {
    statsOverview: (...a: unknown[]) => stats(...a),
    sourcesOverview: (...a: unknown[]) => sources(...a),
    usageSeries: (...a: unknown[]) => series(...a),
    ledgerOverview: (...a: unknown[]) => ledger(...a),
    ledgerInsights: (...a: unknown[]) => insights(...a),
    advisorSavers: (...a: unknown[]) => advisorSavers(...a),
    adviceReport: (...a: unknown[]) => adviceReport(...a),
    adviceApply: (...a: unknown[]) => adviceApply(...a),
    adviceUndo: (...a: unknown[]) => adviceUndo(...a),
    adviceDismiss: (...a: unknown[]) => adviceDismiss(...a),
    saversList: (...a: unknown[]) => saversList(...a),
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

// ---------------------------------------------------------------------------
// advice
// ---------------------------------------------------------------------------

/** One suggestion, with only the fields the store reads spelled out. */
const suggestion = (id: string, kind: string) =>
  ({ id, kind, status: "open", applyable: true }) as unknown as AdviceItem;

const adviceOf = (...items: AdviceItem[]): AdviceReport =>
  ({
    items,
    applied: [],
    estTokensMonth: 0,
    estTokensMonthBurden: 0,
    generatedAt: "2026-08-06T10:00:00Z",
    advisorRanked: false,
  }) as AdviceReport;

describe("loadAdvice", () => {
  beforeEach(() => {
    useStore.setState({ advice: null, adviceBusy: false, savers: null, banner: null });
    adviceReport.mockResolvedValue(adviceOf());
    saversList.mockResolvedValue({ masterOn: true, savers: [] });
  });

  it("asks once, not once per screen", async () => {
    await useStore.getState().loadAdvice();
    await useStore.getState().loadAdvice();
    expect(adviceReport).toHaveBeenCalledTimes(1);
  });

  // The regression this guards: `advice::generate` WRITES the advice table -
  // new rows open, vanished ones stale, spent dismissals retired - so two
  // concurrent passes reconcile the same rows against each other.
  it("produces one generate when two screens mount at once", async () => {
    const slow = deferred<AdviceReport>();
    adviceReport.mockReturnValue(slow.promise);

    const first = useStore.getState().loadAdvice();
    const second = useStore.getState().loadAdvice();
    slow.resolve(adviceOf(suggestion("a", "server-disable")));
    await Promise.all([first, second]);

    expect(adviceReport).toHaveBeenCalledTimes(1);
    expect(useStore.getState().advice?.items).toHaveLength(1);
  });

  it("asks again when the caller forces it", async () => {
    await useStore.getState().loadAdvice();
    await useStore.getState().loadAdvice(true);
    expect(adviceReport).toHaveBeenCalledTimes(2);
  });

  // The regression: a generate re-scans every CLAUDE.md and every MCP config,
  // and the watcher fires roughly every couple of seconds while Claude runs.
  it("never generates advice on a background refresh", async () => {
    vi.useFakeTimers();
    try {
      ledger.mockResolvedValue(LEDGER);
      stats.mockResolvedValue({});
      await useStore.getState().refresh();
      await vi.advanceTimersByTimeAsync(1000);
    } finally {
      vi.useRealTimers();
    }
    expect(adviceReport).not.toHaveBeenCalled();
  });

  // Silent on failure: the Spend screen is complete and correct without a
  // suggestion list, and a store hiccup must not banner over a working ledger.
  it("stays silent after a failed load and re-arms", async () => {
    adviceReport.mockRejectedValueOnce(new Error("could not open the store"));
    await useStore.getState().loadAdvice();

    expect(useStore.getState().banner).toBeNull();
    expect(useStore.getState().advice).toBeNull();

    adviceReport.mockResolvedValue(adviceOf(suggestion("a", "saver-mix")));
    await useStore.getState().loadAdvice();
    expect(useStore.getState().advice?.items).toHaveLength(1);
  });
});

describe("applying and undoing advice", () => {
  beforeEach(() => {
    useStore.setState({ advice: null, adviceBusy: false, savers: null, banner: null });
    saversList.mockResolvedValue({ masterOn: true, savers: [] });
  });

  it("replaces the list from the response, with no second read", async () => {
    useStore.setState({ advice: adviceOf(suggestion("a", "claudemd-fix")) });
    const after = adviceOf();
    adviceApply.mockResolvedValue({ report: after, applied: ["a"], failures: [], warnings: [] });

    const res = await useStore.getState().applyAdvice(["a"]);

    expect(res.applied).toEqual(["a"]);
    expect(useStore.getState().advice).toBe(after);
    expect(adviceReport).not.toHaveBeenCalled();
    expect(useStore.getState().adviceBusy).toBe(false);
  });

  it("reloads the saver list after a saver mix, because its switch moved", async () => {
    useStore.setState({ advice: adviceOf(suggestion("a", "saver-mix")) });
    adviceApply.mockResolvedValue({
      report: adviceOf(),
      applied: ["a"],
      failures: [],
      warnings: [],
    });
    await useStore.getState().applyAdvice(["a"]);
    expect(saversList).toHaveBeenCalledTimes(1);
  });

  it("leaves the saver list alone after a CLAUDE.md fix", async () => {
    useStore.setState({ advice: adviceOf(suggestion("a", "claudemd-fix")) });
    adviceApply.mockResolvedValue({
      report: adviceOf(),
      applied: ["a"],
      failures: [],
      warnings: [],
    });
    await useStore.getState().applyAdvice(["a"]);
    expect(saversList).not.toHaveBeenCalled();
  });

  // The store rethrows rather than swallowing: the sheet is what banners, and a
  // list quietly replaced by nothing would read as "there is nothing to do".
  it("rethrows a failed apply and leaves the list alone", async () => {
    const before = adviceOf(suggestion("a", "claudemd-fix"));
    useStore.setState({ advice: before });
    adviceApply.mockRejectedValue(new Error("could not write the file"));

    await expect(useStore.getState().applyAdvice(["a"])).rejects.toThrow();
    expect(useStore.getState().advice).toBe(before);
    expect(useStore.getState().adviceBusy).toBe(false);
  });

  it("replaces the list from an undo response and reloads savers for a toggle", async () => {
    const applied = { ...suggestion("a", "saver-mix"), status: "applied" } as AdviceItem;
    useStore.setState({ advice: { ...adviceOf(), applied: [applied] } });
    const after = adviceOf();
    adviceUndo.mockResolvedValue({ report: after, restored: 1, failures: [], message: "back" });

    const res = await useStore.getState().undoAdvice("a");

    expect(res.restored).toBe(1);
    expect(useStore.getState().advice).toBe(after);
    expect(saversList).toHaveBeenCalledTimes(1);
  });

  it("replaces the list when a suggestion is set aside", async () => {
    useStore.setState({ advice: adviceOf(suggestion("a", "server-disable")) });
    const after = adviceOf();
    adviceDismiss.mockResolvedValue(after);

    await useStore.getState().dismissAdvice("a");

    expect(useStore.getState().advice).toBe(after);
    expect(adviceReport).not.toHaveBeenCalled();
  });
});
