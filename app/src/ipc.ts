// The single IPC boundary. In a real build every call goes to a Tauri command;
// when VITE_MOCK is set, calls are served from in-memory fixtures instead so the
// whole UI (populated *and* empty first-run) can be designed and QA'd in a plain
// browser with `npm run dev:mock`.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AdvisorProgress,
  AdvisorStatus,
  Annotation,
  ConfigOption,
  DiscoverDto,
  Doctor,
  Environment,
  Insight,
  LedgerOverview,
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
  ledgerOverview: (period: Period) => call<LedgerOverview>("ledger_overview", { period }),
  ledgerInsights: (period: Period) => call<Insight[]>("ledger_insights", { period }),
  advisorStatus: () => call<AdvisorStatus>("advisor_status"),
  /** `null` switches the advisor off. Weights stay on disk; use `advisorRemove`
   *  to reclaim the space. */
  advisorSelect: (modelId: string | null) => call<AdvisorStatus>("advisor_select", { modelId }),
  /** Returns as soon as the transfer starts. Follow `onAdvisorProgress` for the
   *  rest: awaiting a multi-gigabyte download would block the invoke channel. */
  advisorDownload: (modelId: string) => call<void>("advisor_download", { modelId }),
  advisorCancel: () => call<void>("advisor_cancel"),
  advisorRemove: (modelId: string) => call<AdvisorStatus>("advisor_remove", { modelId }),
  /** Empty whenever the advisor is off, not downloaded, or produced nothing that
   *  survived the guard. None of those is an error. */
  advisorAnnotate: (period: Period) => call<Annotation[]>("advisor_annotate", { period }),
  saversList: () => call<SaversState>("savers_list"),
  saverConfigGet: (id: string) => call<ConfigOption[]>("saver_config_get", { id }),
  saverConfigSet: (id: string, key: string, value: string) =>
    call<ConfigOption[]>("saver_config_set", { id, key, value }),
  saverToggle: (id: string, on: boolean) => call<SaversState>("saver_toggle", { id, on }),
  saverUnpin: (id: string) => call<SaversState>("saver_unpin", { id }),
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

/** Subscribe to advisor download progress (no-op in mock). */
export async function onAdvisorProgress(
  cb: (p: AdvisorProgress) => void,
): Promise<() => void> {
  if (IS_MOCK) return () => {};
  return listen<AdvisorProgress>("advisor://download", (e) => cb(e.payload));
}

/** Hide the panel window (Esc / click-away). No-op in the browser mock. */
export async function hidePanel(): Promise<void> {
  if (IS_MOCK) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().hide();
}
