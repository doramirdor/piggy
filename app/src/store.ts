import { create } from "zustand";
import { api } from "./ipc";
import { errorBanner, infoBanner, toApiError, type Banner } from "./lib/errors";
import type {
  Annotation,
  Environment,
  Insight,
  LedgerOverview,
  Period,
  SaversState,
  SourcesOverview,
  StatsOverview,
  UsageSeries,
} from "./types";

export type Tab = "ledger" | "overview" | "savers" | "discover" | "proof" | "reports" | "settings";

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
  sources: SourcesOverview | null;
  series: UsageSeries | null;
  savers: SaversState | null;
  banner: Banner | null;
  booting: boolean;
  busySavers: string[];
  masterBusy: boolean;

  setTab: (t: Tab) => void;
  setPeriod: (p: Period) => Promise<void>;
  boot: () => Promise<void>;
  loadStats: () => Promise<void>;
  /** Ask the local advisor for prose about the current findings. Cheap to call
   *  repeatedly: it no-ops unless the period's findings are un-annotated. */
  loadAnnotations: () => Promise<void>;
  loadSavers: () => Promise<void>;
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
  tab: "ledger",
  period: "week",
  env: null,
  stats: null,
  ledger: null,
  insights: [],
  annotations: [],
  annotatedPeriod: null,
  sources: null,
  series: null,
  savers: null,
  banner: null,
  booting: true,
  busySavers: [],
  masterBusy: false,

  setTab: (tab) => set({ tab }),

  setPeriod: async (period) => {
    // Last period's prose next to this period's numbers is exactly the mismatch
    // the whole design avoids, so a period change is the one thing that discards
    // annotations and re-arms inference.
    set({ period, annotations: [], annotatedPeriod: null });
    await get().loadStats();
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
