// Shared payload types — mirror the `#[derive(Serialize)]` structs in
// app/src-tauri/src/backend.rs (all camelCase over the IPC boundary).

export type Period = "today" | "week" | "month" | "all";

export type HeadlineLabel = "measured" | "estimated" | "not_enough_data";
export type BadgeKind = "measured" | "measuring" | "claimed";

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
}

export interface SaversState {
  masterOn: boolean;
  savers: SaverRow[];
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
