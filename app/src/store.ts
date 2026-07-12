import { create } from "zustand";
import { api } from "./ipc";
import { errorBanner, toApiError, type Banner } from "./lib/errors";
import type { Environment, Period, SaversState, StatsOverview } from "./types";

export type Tab = "home" | "dashboard" | "discover" | "settings";

interface AppState {
  tab: Tab;
  period: Period;
  env: Environment | null;
  stats: StatsOverview | null;
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

export const useStore = create<AppState>((set, get) => ({
  tab: "home",
  period: "week",
  env: null,
  stats: null,
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
        await get().refresh();
      }
    } catch (e) {
      set({ booting: false });
      get().showError(e);
    }
  },

  loadStats: async () => {
    try {
      const stats = await api.statsOverview(get().period);
      set({ stats });
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
    await Promise.all([get().loadStats(), get().loadSavers()]);
  },

  toggleSaver: async (id, on) => {
    set({ busySavers: [...get().busySavers, id], banner: null });
    try {
      const savers = await api.saverToggle(id, on);
      set({ savers });
    } catch (e) {
      get().showError(e);
      await get().loadSavers(); // reflect the true post-failure state
    } finally {
      set({ busySavers: get().busySavers.filter((x) => x !== id) });
    }
  },

  toggleMaster: async (on) => {
    set({ masterBusy: true, banner: null });
    try {
      const savers = await api.masterToggle(on);
      set({ savers });
    } catch (e) {
      get().showError(e);
      await get().loadSavers();
    } finally {
      set({ masterBusy: false });
    }
  },

  showError: (e) => set({ banner: errorBanner(toApiError(e)) }),
  dismissBanner: () => set({ banner: null }),
}));
