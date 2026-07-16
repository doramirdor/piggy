import { create } from "zustand";
import { api } from "./ipc";
import { errorBanner, infoBanner, toApiError, type Banner } from "./lib/errors";
import type {
  Environment,
  Period,
  SaversState,
  SourcesOverview,
  StatsOverview,
  UsageSeries,
} from "./types";

export type Tab = "overview" | "savers" | "discover" | "proof" | "reports" | "settings";

interface AppState {
  tab: Tab;
  period: Period;
  env: Environment | null;
  stats: StatsOverview | null;
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
  loadSavers: () => Promise<void>;
  refresh: () => Promise<void>;
  toggleSaver: (id: string, on: boolean) => Promise<void>;
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
  tab: "overview",
  period: "week",
  env: null,
  stats: null,
  sources: null,
  series: null,
  savers: null,
  banner: null,
  booting: true,
  busySavers: [],
  masterBusy: false,

  setTab: (tab) => set({ tab }),

  setPeriod: async (period) => {
    set({ period });
    await get().loadStats();
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
    try {
      const period = get().period;
      const [stats, sources, series] = await Promise.all([
        api.statsOverview(period),
        api.sourcesOverview(period),
        api.usageSeries(period),
      ]);
      set({ stats, sources, series });
    } catch (e) {
      get().showError(e);
    }
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
