// The single IPC boundary. In a real build every call goes to a Tauri command;
// when VITE_MOCK is set, calls are served from in-memory fixtures instead so the
// whole UI (populated *and* empty first-run) can be designed and QA'd in a plain
// browser with `npm run dev:mock`.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  ConfigOption,
  DiscoverDto,
  Doctor,
  Environment,
  Period,
  ReindexResult,
  RestoreResult,
  SaversState,
  Settings,
  ShareCardData,
  SourcesOverview,
  StatsOverview,
  SweepReport,
  UpdateInfo,
  UsageSeries,
} from "./types";

/** "1" | "empty" | undefined - set by `dev:mock` / `VITE_MOCK=… vite build`. */
export const MOCK_MODE: string | undefined = import.meta.env.VITE_MOCK;
export const IS_MOCK = Boolean(MOCK_MODE);

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (IS_MOCK) {
    const { mockInvoke } = await import("./mock");
    return mockInvoke<T>(cmd, args);
  }
  return invoke<T>(cmd, args);
}

export const api = {
  environment: () => call<Environment>("environment"),
  statsOverview: (period: Period) => call<StatsOverview>("stats_overview", { period }),
  sourcesOverview: (period: Period) => call<SourcesOverview>("sources_overview", { period }),
  usageSeries: (period: Period) => call<UsageSeries>("usage_series", { period }),
  saversList: () => call<SaversState>("savers_list"),
  saverConfigGet: (id: string) => call<ConfigOption[]>("saver_config_get", { id }),
  saverConfigSet: (id: string, key: string, value: string) =>
    call<ConfigOption[]>("saver_config_set", { id, key, value }),
  saverToggle: (id: string, on: boolean) => call<SaversState>("saver_toggle", { id, on }),
  masterToggle: (on: boolean) => call<SaversState>("master_toggle", { on }),
  sweepReport: () => call<SweepReport>("sweep_report"),
  sweepApply: (itemIds: string[]) => call<SweepReport>("sweep_apply", { itemIds }),
  sweepRestore: (itemIds: string[]) => call<SweepReport>("sweep_restore", { itemIds }),
  discoveredList: () => call<DiscoverDto>("discovered_list"),
  refreshDiscovered: () => call<DiscoverDto>("refresh_discovered"),
  shareCardData: (period: Period) => call<ShareCardData>("share_card_data", { period }),
  saveShareCard: (pngBase64: string) => call<{ path: string }>("save_share_card", { pngBase64 }),
  settingsGet: () => call<Settings>("settings_get"),
  settingsSet: (settings: Settings) => call<Settings>("settings_set", { settings }),
  restoreDefaults: () => call<RestoreResult>("restore_defaults"),
  doctor: () => call<Doctor>("doctor"),
  reindex: () => call<ReindexResult>("reindex"),
  openExternal: (url: string) => call<void>("open_external", { url }),
  checkForUpdate: () => call<UpdateInfo | null>("check_for_update"),
  installUpdate: () => call<void>("install_update"),
};

/** Subscribe to the background `piggy://stats-updated` event (no-op in mock). */
export async function onStatsUpdated(cb: () => void): Promise<() => void> {
  if (IS_MOCK) return () => {};
  return listen("piggy://stats-updated", () => cb());
}

/** Hide the panel window (Esc / click-away). No-op in the browser mock. */
export async function hidePanel(): Promise<void> {
  if (IS_MOCK) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().hide();
}
