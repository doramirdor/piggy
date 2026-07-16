// Shared payload types - mirror the `#[derive(Serialize)]` structs in
// app/src-tauri/src/backend.rs (all camelCase over the IPC boundary).

export type Period = "today" | "week" | "month" | "all";

export type HeadlineLabel = "measured" | "estimated" | "not_enough_data";
export type BadgeKind = "measured" | "estimated" | "measuring" | "claimed";

export interface Streams {
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
}

export interface Headline {
  value: number | null;
  label: HeadlineLabel;
  nHoldout: number;
}

export interface StatsOverview {
  period: Period;
  periodLabel: string;
  streams: Streams;
  totalTokens: number;
  sessions: number;
  costUsdEst: number;
  costEstimated: boolean;
  fullyPriced: boolean;
  todayTokens: number;
  headline: Headline;
}

/** One (tool, surface) cell of the observability grid. */
export interface SourceCell {
  source: "claude-code" | "codex";
  interface: "gui" | "tui";
  sessions: number;
  totalTokens: number;
  costUsdEst: number;
  toolPresent: boolean;
}

export interface SourcesOverview {
  period: Period;
  cells: SourceCell[];
  unknownTokens: number;
  unknownSessions: number;
}

/** One UTC calendar day of usage (day-over-day analytics series). */
export interface DailyPoint {
  date: string; // YYYY-MM-DD (UTC)
  totalTokens: number;
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
  costUsdEst: number;
  sessions: number;
}

export interface UsageSeries {
  period: Period;
  periodLabel: string;
  /** Oldest day first, zero-filled so the series is continuous. */
  points: DailyPoint[];
}

export interface ConfigChoice {
  value: string;
  label: string;
  description: string;
}

/** One user-tunable saver option, resolved to its current value. */
export interface ConfigOption {
  key: string;
  label: string;
  description: string;
  choices: ConfigChoice[];
  default: string;
  current: string;
}

export interface Badge {
  kind: BadgeKind;
  delta: number | null;
  n: number;
}

export interface SaverRow {
  id: string;
  name: string;
  plainLabel: string | null;
  description: string;
  installType: string;
  status: string;
  defaultOn: boolean;
  installed: boolean;
  enabled: boolean;
  installable: boolean;
  behaviorChanging: boolean;
  warning: string | null;
  risk: string | null;
  claimedSavings: string | null;
  license: string;
  licenseNote: string | null;
  ordering: number;
  badge: Badge;
  /** True when the saver exposes user-tunable options (shows Configure). */
  configurable: boolean;
  /** Wrapper-model savers only: the command that starts a session through this
   * saver (e.g. Headroom's piggy-claude). Null when the saver applies to every
   * session. */
  launchCommand: string | null;
}

export interface SaversState {
  masterOn: boolean;
  savers: SaverRow[];
  /** A one-line heads-up from the last mutation (e.g. a conflicting saver was
   * auto-disabled). Absent on plain reads. */
  notice?: string | null;
}

export interface SweepItem {
  idx: number;
  stableId: string;
  kind: string;
  id: string;
  source: string | null;
  used: number;
  usedScope: string;
  estTokens: number;
  estimated: boolean;
  recommendDisable: boolean;
  reason: string;
}

export interface SweepReport {
  sessionsConsidered: number;
  estRecoverableTokens: number;
  estimated: boolean;
  items: SweepItem[];
}

export interface DiscoverEntry {
  id: string;
  name: string;
  description: string;
  claimedSavings: string | null;
  license: string;
  licenseNote: string | null;
  exclusionReason: string | null;
  note: string;
  repoUrl: string | null;
  risk: string | null;
}

export interface DiscoverFeedItem {
  name: string;
  description: string;
  stars: number | null;
  authorClaims: string | null;
  repoUrl: string | null;
}

export interface DiscoverDto {
  feed: DiscoverFeedItem[];
  listedOnly: DiscoverEntry[];
}

export interface ShareCardData {
  period: Period;
  weekLabel: string;
  tokensSaved: number | null;
  multiplier: number | null;
  headlineLabel: HeadlineLabel;
  nHoldout: number;
  shareable: boolean;
}

export interface Settings {
  holdoutFraction: number;
  rotationEnabled: boolean;
  launchAtLogin: boolean;
  /** Whether the `piggy` CLI is linked onto the user's PATH. */
  cliTool: boolean;
}

/** A release newer than the running build. */
export interface UpdateInfo {
  version: string;
  currentVersion: string;
  notes: string | null;
}

export interface DoctorCheck {
  label: string;
  ok: boolean;
  detail: string;
}

export interface Doctor {
  ok: boolean;
  checks: DoctorCheck[];
}

export interface Environment {
  claudeInstalled: boolean;
  codexInstalled: boolean;
  hasData: boolean;
  sessions: number;
}

export interface RestoreResult {
  byteRestored: boolean;
  saversRemoved: number;
  sweptRestored: number;
  filesRemoved: number;
  messages: string[];
}

export interface ReindexResult {
  ran: boolean;
  sessions: number;
  updated: number;
  scanned: number;
}

/** Plain-language error payload; the UI renders it as a red inline banner. */
export interface ApiError {
  title: string;
  detail: string;
  rolledBack: boolean;
}
