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

export type BaselineKind = "holdout" | "pre_install" | "none";

/** One stream's side of the headline comparison: the two medians the delta is a
 *  ratio of, so Proof can draw the comparison rather than assert its result. */
export interface HeadlineStream {
  /** "input" | "output" | "cache write" | "cache read". */
  stream: string;
  /** This stream's own badge, which is not always the headline's. */
  kind: BadgeKind;
  nOn: number;
  nOff: number;
  /** Median tokens per assistant turn on each arm. Zero when the arm is empty,
   *  which the UI must render as "no sessions", never as a zero measurement. */
  medianOn: number;
  medianOff: number;
  /** The change as a fraction, **negative = a saving** - same convention as
   *  `Badge.delta`. Already gated on the badge by the backend, so a non-null
   *  delta is always safe to print; `kind` says whether it may be called
   *  measured. */
  delta: number | null;
}

export interface Headline {
  value: number | null;
  label: HeadlineLabel;
  nHoldout: number;
  /**
   * Why the figure is only estimated, in the user's terms. Null unless the label
   * is "estimated". There is more than one reason (no holdout yet, savers pinned
   * on by hand, a pinned saver ran through the holdout) and they are not
   * distinguishable from `label`, so the backend names the right one.
   */
  note: string | null;
  /** Sessions on the ON arm: your current saver set, running. */
  nFullOn: number;
  /** Sessions on the OFF arm, whichever baseline won. Not interchangeable with
   *  `nHoldout`, which is 0 unless the baseline IS the holdout. */
  nBaseline: number;
  baselineKind: BaselineKind;
  /** False once the ON arm leans on hand-pinned sessions. This is the blocker
   *  the user can actually undo, so the UI leads with it. */
  onRandomized: boolean;
  /** False when a pinned saver rode through the holdout. */
  baselineClean: boolean;
  /** Sessions needed on EACH arm before a measured claim (MIN_GROUP). */
  minGroup: number;
  streams: HeadlineStream[];
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
  /** The two arms behind `n`. Promotion needs 10 sessions on BOTH sides, so the
   * sum alone cannot say how close a saver is: 14 on / 0 off is `n = 14` and
   * never settles. The status chip's bar fills on the weaker arm. */
  nOn: number;
  nOff: number;
  /** Why an enabled saver is stuck at "measuring" (a required binary missing,
   * rotation off, or pinned on by hand), in the user's terms. Absent for settled
   * badges, off savers, or the ordinary warm-up. Mirrors `Headline.note`. */
  note?: string | null;
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
  /** True when hand-toggled (source "manual"), which pauses it from rotation.
   * The UI offers "let Piggy measure this" to hand it back to the scheduler. */
  pinned: boolean;
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

/** One row of the context ledger: where cache-write tokens came from. */
export interface LedgerSource {
  /** Stable key: `__floor`, `__conversation`, or an attachment type. */
  kind: string;
  label: string;
  tokens: number;
  /** Share of all cache-write tokens, 0-1. */
  share: number;
  /** An injection the user can configure away (not the floor, not the work). */
  removable: boolean;
  /** Part of session startup: the floor residual or a named component of it. */
  isFloor: boolean;
  /** Bounded by content size rather than a measured write. */
  estimated: boolean;
}

/** One project's split between opening sessions and doing work. */
export interface LedgerProject {
  project: string;
  name: string;
  sessions: number;
  msgsPerSession: number;
  floorTokens: number;
  workTokens: number;
  /** Floor share of this project's tokens, 0-1. */
  overhead: number;
}

export interface LedgerOverview {
  period: Period;
  periodLabel: string;
  totalTokens: number;
  removableTokens: number;
  overhead: number;
  /** How much further the plan goes with configurable context removed. This is
   *  AVAILABLE headroom, not savings already achieved. Null when trivial. */
  headroom: number | null;
  /** Removable share behind `headroom`, as a fraction of total COST (0-1). */
  removableShare: number;
  sessions: number;
  sources: LedgerSource[];
  projects: LedgerProject[];
  empty: boolean;
}

/** One finding derived from the ledger: what it cost and the lever for it. */
export interface Insight {
  id: string;
  severity: "high" | "notable" | "info";
  title: string;
  detail: string;
  tokens: number;
  action: string;
}

// --- the local advisor (opt-in, off by default) ---------------------------

/** One downloadable model the picker may offer. Entries that cannot run on this
 *  machine never reach the UI, so every model here is a real choice. */
export interface AdvisorModel {
  id: string;
  name: string;
  blurb: string;
  /** Download size. */
  bytes: number;
  /** What it costs to *run*: weights plus KV cache plus compute buffers. This is
   *  the number the picker shows, because it is the one that decides whether the
   *  machine copes. */
  peakBytes: number;
  context: number;
  downloaded: boolean;
}

export interface AdvisorStatus {
  /** False when the build has no inference compiled in. */
  compiledIn: boolean;
  hostRamBytes: number | null;
  budgetBytes: number | null;
  state: "unsupported" | "off" | "needsDownload" | "ready";
  models: AdvisorModel[];
  selectedId: string | null;
  recommendedId: string | null;
}

/** Prose a local model wrote about a finding Piggy already measured. Never a
 *  number: anything the model states that is not already a fact is rejected
 *  before it gets here. */
export interface Annotation {
  insightId: string;
  headline: string;
  why: string;
  /** Shown in the UI. Locally generated text must not look like it came from the
   *  same place as the receipt. */
  model: string;
}

export interface AdvisorProgress {
  modelId: string;
  received: number;
  total: number;
  done: boolean;
  error: string | null;
}
