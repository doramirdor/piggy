import { create } from "zustand";
import { api } from "./ipc";
import { errorBanner, infoBanner, toApiError, type Banner } from "./lib/errors";
import type {
  AdviceApplyResult,
  AdviceReport,
  AdviceUndoResult,
  Annotation,
  Environment,
  Insight,
  LedgerOverview,
  Period,
  SaversState,
  SourcesOverview,
  StatsOverview,
  TaskTable,
  UsageSeries,
} from "./types";

// Four destinations, one per question a user actually arrives with: where did
// my tokens go, what do I turn on, did it work, and how do I control Piggy.
// Dashboard and Reports were folded into Spend (they answered the same
// question in different cuts), and Discovery and Sweep became sections of
// Savers, which is the tab that installs things.
export type Tab = "spend" | "savers" | "proof" | "settings" | "about";

/**
 * The identity of the measurement per-saver advice is written against.
 *
 * The prose sits directly beside the row's own finding and caveat, and those
 * refresh on every `refresh()`. Pinning the prose to the reading it explains is
 * what stops a note written about "still measuring" surviving into a settled
 * delta, or a note about one delta surviving into another.
 *
 * Coarse on purpose. Re-asking means loading a ~3GB model, so the key must not
 * move with every indexed session: the delta enters it at the precision the row
 * actually prints (whole percent, `pctMagnitude`), and the medians behind it do
 * not enter at all. What does move it is what the reader can see move: a saver
 * appearing, being switched on or off, settling, or changing its printed number.
 *
 * `null` when there is no saver to advise on, which is also what keeps the
 * fetch from firing at all.
 */
export function saverNotesKey(savers: SaversState | null): string | null {
  const rows = savers?.savers ?? [];
  if (rows.length === 0) return null;
  const rowKeys = rows.map((s) => {
    const pct = s.badge.delta == null ? "-" : Math.round(s.badge.delta * 100);
    return `${s.id}:${s.enabled ? 1 : 0}:${s.badge.kind}:${pct}`;
  });
  return `${savers?.masterOn ? "on" : "off"}|${rowKeys.join("|")}`;
}

interface AppState {
  tab: Tab;
  period: Period;
  env: Environment | null;
  stats: StatsOverview | null;
  ledger: LedgerOverview | null;
  insights: Insight[];
  /** Optional prose from a local model, keyed to `insights` by id. Always empty
   *  unless the user opted in, and never required for the findings to render. */
  annotations: Annotation[];
  /** The period whose findings have already been sent to the advisor, so a tab
   *  switch or a background refresh cannot re-trigger inference for the same
   *  data. `null` means "not asked yet for the current period". */
  annotatedPeriod: Period | null;
  /** Per-saver advice from the same local model, keyed by `saver:<id>`. Not
   *  period-scoped: the comparison behind it counts every session ever run. */
  saverNotes: Annotation[];
  /** The reading `saverNotes` was written against (see `saverNotesKey`), or
   *  `null` for "not asked yet". One load is ~3GB resident, so Proof asks once
   *  per reading and never on a refresh. */
  saverNotesFor: string | null;
  sources: SourcesOverview | null;
  series: UsageSeries | null;
  /** The task table, fetched only when the Tasks view is opened. `null` means
   *  "not asked yet", which is why it has its own period marker below. */
  tasks: TaskTable | null;
  /** The period `tasks` holds. A background refresh must not serve last week's
   *  table beside this week's header, and a period switch clears both. */
  tasksPeriod: Period | null;
  savers: SaversState | null;
  /** The advice list. `null` means "not asked yet" and renders a skeleton, not
   *  an empty state: "Piggy has nothing to suggest" is a claim, and a loading
   *  list must not make it.
   *
   *  It lives here rather than in screen-local state because both entry points
   *  - Spend's section and the Savers hint - have to show the same list. */
  advice: AdviceReport | null;
  /** A generate, apply, undo or dismiss is in flight. The sheet disables its
   *  controls while it is. */
  adviceBusy: boolean;
  banner: Banner | null;
  booting: boolean;
  /** A period switch is in flight. The screens keep the old slice on screen and
   *  dim it rather than blanking, so the swap reads as the same page changing. */
  periodBusy: boolean;
  busySavers: string[];
  masterBusy: boolean;

  setTab: (t: Tab) => void;
  setPeriod: (p: Period) => Promise<void>;
  loadTasks: () => Promise<void>;
  boot: () => Promise<void>;
  loadStats: () => Promise<void>;
  /** Ask the local advisor for prose about the current findings. Cheap to call
   *  repeatedly: it no-ops unless the period's findings are un-annotated. */
  loadAnnotations: () => Promise<void>;
  loadSaverNotes: () => Promise<void>;
  loadSavers: () => Promise<void>;
  /** Ask the engine what to do next. Pull, never push: this is deliberately NOT
   *  part of `refresh()`, because a generate re-scans every CLAUDE.md and every
   *  MCP config and the watcher fires every couple of seconds while Claude is
   *  running. Asked once per app run; `force` re-asks. */
  loadAdvice: (force?: boolean) => Promise<void>;
  applyAdvice: (ids: string[]) => Promise<AdviceApplyResult>;
  undoAdvice: (id: string) => Promise<AdviceUndoResult>;
  dismissAdvice: (id: string) => Promise<void>;
  refresh: () => Promise<void>;
  toggleSaver: (id: string, on: boolean) => Promise<void>;
  unpinSaver: (id: string) => Promise<void>;
  toggleMaster: (on: boolean) => Promise<void>;
  showError: (e: unknown) => void;
  dismissBanner: () => void;
}

export const useStore = create<AppState>((set, get) => {
  // Refresh coordination (non-reactive). The daemon emits `stats-updated` on
  // every session write - roughly once every couple seconds while Claude is
  // active - and a window refocus fires another. Each refresh recomputes the
  // measurement model, so without this a burst of events stacks overlapping
  // recomputes. We coalesce bursts (trailing debounce), never run two at once
  // (in-flight guard, re-running once if events arrived mid-flight), and skip
  // entirely while the window is hidden.
  let refreshInFlight = false;
  let refreshQueued = false;
  let refreshTimer: ReturnType<typeof setTimeout> | null = null;

  // One generate at a time, whoever asks. `advice::generate` is expensive - it
  // re-scans every CLAUDE.md and every MCP config - and it also WRITES: new rows
  // land open, vanished ones go stale, spent dismissals retire. Two screens
  // mounting at once must produce one pass, not two reconciling the same table.
  let adviceInFlight: Promise<void> | null = null;

  const refreshNow = async () => {
    if (refreshInFlight) {
      refreshQueued = true;
      return;
    }
    refreshInFlight = true;
    try {
      do {
        refreshQueued = false;
        await Promise.all([get().loadStats(), get().loadSavers()]);
      } while (refreshQueued);
    } finally {
      refreshInFlight = false;
    }
  };

  return {
  tab: "spend",
  period: "week",
  env: null,
  stats: null,
  ledger: null,
  insights: [],
  annotations: [],
  saverNotes: [],
  saverNotesFor: null,
  annotatedPeriod: null,
  sources: null,
  series: null,
  tasks: null,
  tasksPeriod: null,
  savers: null,
  advice: null,
  adviceBusy: false,
  banner: null,
  booting: true,
  periodBusy: false,
  busySavers: [],
  masterBusy: false,

  setTab: (tab) => set({ tab }),

  setPeriod: async (period) => {
    // Last period's prose next to this period's numbers is exactly the mismatch
    // the whole design avoids, so a period change is the one thing that discards
    // annotations and re-arms inference.
    // The task table goes with them: it is windowed and compared against the
    // window before it, so it is wrong the instant the window changes.
    set({
      period,
      annotations: [],
      annotatedPeriod: null,
      tasks: null,
      tasksPeriod: null,
      periodBusy: true,
    });
    try {
      await get().loadStats();
    } finally {
      // Only the newest switch clears the flag: two fast clicks leave the
      // loader up until the second one's data lands.
      if (get().period === period) set({ periodBusy: false });
    }
  },

  /** Fetched on demand: the Tasks view is one of three subviews and may never
   *  be opened, and `loadStats` runs on the watcher's 400ms debounce. Putting
   *  this in that batch would run a two-query join on every write to a session
   *  log for a table nobody is looking at. */
  loadTasks: async () => {
    const period = get().period;
    if (get().tasksPeriod === period) return;
    try {
      const tasks = await api.taskTable(period);
      // A period change mid-flight makes this answer the wrong one.
      if (get().period === period) set({ tasks, tasksPeriod: period });
    } catch (e) {
      get().showError(e);
    }
  },

  loadAnnotations: async () => {
    const { period, annotatedPeriod, insights } = get();
    // Already asked for this period, or there is nothing to annotate. Either way
    // this must be cheap, because the Ledger calls it on every render pass.
    if (annotatedPeriod === period || insights.length === 0) return;
    // Claim the period BEFORE awaiting, so two quick tab switches cannot start
    // two models at once. Each load is ~3GB resident; two is a swapped machine.
    set({ annotatedPeriod: period });
    try {
      const annotations = await api.advisorAnnotate(period);
      // The period may have changed while the model was thinking.
      if (get().period === period) set({ annotations });
    } catch {
      // Silent: the findings are already correct and complete without prose.
      // Re-arm so a later visit can retry.
      if (get().period === period) set({ annotatedPeriod: null });
    }
  },

  /** Ask the local model for per-saver advice. Called by Proof, once per
   *  reading.
   *
   *  Deliberately not part of `refresh`: without the guard of `saverNotesFor` a
   *  4B would load on every session-log write, whichever tab is open. But
   *  "asked once, ever" was the other error: the row's finding and caveat move
   *  under the note on every refresh, and in a menu bar app left open for days
   *  the prose ends up describing a reading the row no longer shows. The key is
   *  what makes once mean once per reading. */
  loadSaverNotes: async () => {
    const key = saverNotesKey(get().savers);
    if (key === null || get().saverNotesFor === key) return;
    // Claim the reading BEFORE awaiting, so two visits cannot start two models
    // at once. Each load is ~3GB resident. The previous prose goes with the
    // claim: it was written about a reading that has since moved, which is the
    // mismatch `setPeriod` discards annotations to avoid.
    set({ saverNotesFor: key, saverNotes: [] });
    try {
      const saverNotes = await api.advisorSavers();
      // The reading may have moved while the model was thinking.
      if (get().saverNotesFor === key) set({ saverNotes });
    } catch {
      // Silent: the deterministic summary on each row is the product, and it is
      // already rendered. Re-arm so a later visit can retry.
      if (get().saverNotesFor === key) set({ saverNotesFor: null });
    }
  },

  boot: async () => {
    try {
      const env = await api.environment();
      set({ env, booting: false });
      if (env.claudeInstalled && env.hasData) {
        await refreshNow(); // first load: run immediately, don't debounce
      }
    } catch (e) {
      set({ booting: false });
      get().showError(e);
    }
  },

  loadStats: async () => {
    const period = get().period;

    // Each query commits on its own. These five are not equally cheap: the
    // ledger lands in a second or two, while `stats_overview` and `savers_list`
    // both wait on the attribution bundle, which rescans every session and
    // bootstraps a CI per saver. Awaiting them together as one Promise.all meant
    // the SLOWEST call gated all five, so the Ledger - the default tab - held
    // its "Reading your sessions…" placeholder over a blank pane for as long as
    // attribution took, with its own data already in hand. A screen should wait
    // for its own query and nobody else's.
    //
    // Annotations are deliberately left alone here, and inference is
    // deliberately NOT kicked off here.
    //
    // This runs on the session watcher's 400ms debounce. Starting a 4B model
    // from it meant loading and generating on every write to the session log,
    // in the background, whichever tab was open. Clearing the prose here would
    // be just as wrong in the other direction: the notes would blink out on
    // every refresh. `setPeriod` owns invalidation, and the screen that
    // displays the prose owns fetching it.
    let failed: unknown;
    const commit = <T>(p: Promise<T>, apply: (v: T) => void): Promise<void> =>
      p.then(
        (v) => {
          // A period change mid-flight makes this answer the wrong one.
          if (get().period === period) apply(v);
        },
        (e) => {
          // One banner for the batch, not five: the first failure is the one
          // worth naming, and the screens that did load stay usable.
          failed ??= e;
        },
      );

    await Promise.all([
      commit(api.ledgerOverview(period), (ledger) => set({ ledger })),
      commit(api.ledgerInsights(period), (insights) => set({ insights })),
      commit(api.statsOverview(period), (stats) => set({ stats })),
      commit(api.sourcesOverview(period), (sources) => set({ sources })),
      commit(api.usageSeries(period), (series) => set({ series })),
    ]);

    if (failed !== undefined) get().showError(failed);
  },

  loadSavers: async () => {
    try {
      const savers = await api.saversList();
      set({ savers });
    } catch (e) {
      get().showError(e);
    }
  },

  loadAdvice: async (force = false) => {
    if (adviceInFlight) return adviceInFlight;
    if (get().advice !== null && !force) return;
    const ask = async () => {
      try {
        set({ advice: await api.adviceReport() });
      } catch {
        // Silent, and re-armed by the `finally` below. Spend is complete and
        // correct without a suggestion list, so a `Store::open` hiccup must not
        // raise the global banner over a working ledger. Matches
        // `loadAnnotations`.
      }
    };
    const pass = ask().finally(() => {
      adviceInFlight = null;
    });
    adviceInFlight = pass;
    return pass;
  },

  /** Apply a bundle. Per-item failures come back in the result rather than as a
   *  throw; a throw here is the whole call failing, and the sheet banners it. */
  applyAdvice: async (ids) => {
    set({ adviceBusy: true });
    // Which kinds are going, read BEFORE the call: the response replaces the
    // list, and a saver toggle moves that saver's switch and badge on the
    // Savers screen. Not `refresh()` - that is the debounced watcher path.
    const touchesSavers = (get().advice?.items ?? []).some(
      (a) => ids.includes(a.id) && a.kind === "saver-mix",
    );
    try {
      const res = await api.adviceApply(ids);
      set({ advice: res.report });
      if (touchesSavers) await get().loadSavers();
      return res;
    } finally {
      set({ adviceBusy: false });
    }
  },

  undoAdvice: async (id) => {
    set({ adviceBusy: true });
    const touchesSavers = (get().advice?.applied ?? []).some(
      (a) => a.id === id && a.kind === "saver-mix",
    );
    try {
      const res = await api.adviceUndo(id);
      set({ advice: res.report });
      if (touchesSavers) await get().loadSavers();
      return res;
    } finally {
      set({ adviceBusy: false });
    }
  },

  dismissAdvice: async (id) => {
    set({ adviceBusy: true });
    try {
      set({ advice: await api.adviceDismiss(id) });
    } finally {
      set({ adviceBusy: false });
    }
  },

  refresh: async () => {
    // Event/refocus-driven: coalesce a burst into one trailing refresh, and
    // don't waste a recompute on a hidden window (it refreshes on re-show).
    if (typeof document !== "undefined" && document.hidden) return;
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => void refreshNow(), 400);
  },

  toggleSaver: async (id, on) => {
    set({ busySavers: [...get().busySavers, id], banner: null });
    try {
      const savers = await api.saverToggle(id, on);
      set({ savers, banner: savers.notice ? infoBanner(savers.notice) : null });
    } catch (e) {
      get().showError(e);
      await get().loadSavers(); // reflect the true post-failure state
    } finally {
      set({ busySavers: get().busySavers.filter((x) => x !== id) });
    }
  },

  unpinSaver: async (id) => {
    set({ busySavers: [...get().busySavers, id], banner: null });
    try {
      const savers = await api.saverUnpin(id);
      set({ savers, banner: savers.notice ? infoBanner(savers.notice) : null });
    } catch (e) {
      get().showError(e);
      await get().loadSavers();
    } finally {
      set({ busySavers: get().busySavers.filter((x) => x !== id) });
    }
  },

  toggleMaster: async (on) => {
    set({ masterBusy: true, banner: null });
    // Hold the "turning on/off" loader for a beat even if the IPC call returns
    // instantly, so the animation reads as a deliberate moment, not a flicker.
    const minShow = new Promise((r) => setTimeout(r, 550));
    try {
      const savers = await api.masterToggle(on);
      await minShow;
      set({ savers, banner: savers.notice ? infoBanner(savers.notice) : null });
    } catch (e) {
      get().showError(e);
      await get().loadSavers();
    } finally {
      set({ masterBusy: false });
    }
  },

  showError: (e) => set({ banner: errorBanner(toApiError(e)) }),
  dismissBanner: () => set({ banner: null }),
  };
});
