// In-memory fixtures for `npm run dev:mock`. VITE_MOCK=1 serves a realistic
// populated state; VITE_MOCK=empty serves the fresh-install first-run state;
// VITE_MOCK=noclaude serves the "no Claude Code found" state. Toggles/sweep
// mutate module-level copies so the UI feels live during design QA.

import { MOCK_MODE } from "./ipc";
import snapshot from "./dev-snapshot.json";
import type {
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
  SaverRow,
  SaversState,
  Settings,
  ShareCardData,
  SourcesOverview,
  StatsOverview,
  SweepItem,
  SweepReport,
  SystemInfo,
  TaskTable,
  UsageSeries,
} from "./types";

const EMPTY = MOCK_MODE === "empty";
const NO_CLAUDE = MOCK_MODE === "noclaude";

// ---------------------------------------------------------------------------
// savers
// ---------------------------------------------------------------------------

function populatedSavers(): SaverRow[] {
  const rows: SaverRow[] = [
    {
      id: "sweep",
      name: "Sweep",
      plainLabel: "Clean unused extras",
      description: "Finds add-ons you never use that cost tokens on every request.",
      installType: "builtin",
      status: "curated_v1",
      pinned: false,
      defaultOn: true,
      installed: true,
      enabled: true,
      installable: true,
      behaviorChanging: false,
      warning: null,
      risk: "low",
      claimedSavings: "depends on your setup (Piggy measures it)",
      license: "MIT",
      licenseNote: null,
      ordering: 5,
      configurable: false,
      launchCommand: null,
      badge: { kind: "measured", delta: -0.09, n: 18, nOn: 12, nOff: 6 },
    },
    {
      id: "rtk",
      name: "RTK",
      plainLabel: "Shrink terminal noise",
      description: "Compresses command output (git, tests, builds) before Claude sees it.",
      installType: "binary+hook",
      status: "curated_v1",
      pinned: false,
      defaultOn: false,
      installed: true,
      enabled: true,
      installable: true,
      behaviorChanging: false,
      warning: null,
      risk: "low",
      claimedSavings: "~80% on shell output (author estimate)",
      license: "Apache-2.0",
      licenseNote: null,
      ordering: 10,
      configurable: false,
      launchCommand: null,
      badge: { kind: "measured", delta: -0.22, n: 41, nOn: 30, nOff: 11 },
    },
    {
      id: "token-optimizer",
      name: "Token Optimizer",
      plainLabel: "Smart file re-reads",
      description: "Sends Claude only what changed in files it already saw.",
      installType: "claude_plugin",
      status: "curated_v1",
      pinned: false,
      defaultOn: false,
      installed: false,
      enabled: false,
      installable: true,
      behaviorChanging: false,
      warning: null,
      risk: "low",
      claimedSavings: "~18% overall (author, 684-session counterfactual)",
      license: "PolyForm-Noncommercial-1.0.0",
      licenseNote:
        "Source-available, NOT open source. Free for individuals and small teams (<5 people or <$20k/mo).",
      ordering: 30,
      configurable: false,
      launchCommand: null,
      badge: { kind: "measuring", delta: null, n: 0, nOn: 0, nOff: 0 },
    },
    {
      id: "headroom",
      name: "Headroom",
      plainLabel: "Deep compression engine",
      description: "Compresses everything Claude reads. Works in sessions you start with piggy-claude.",
      installType: "venv+wrapper",
      status: "curated_v1",
      pinned: false,
      defaultOn: true,
      installed: true,
      // Off initially so rtk can be on - turning the master on flips this and
      // auto-disables the conflicting RTK.
      enabled: false,
      installable: true,
      behaviorChanging: false,
      warning: null,
      risk: "low",
      claimedSavings: "47–92% by workload (author, reproducible eval suite)",
      license: "Apache-2.0",
      licenseNote: null,
      ordering: 40,
      configurable: false,
      launchCommand: "piggy-claude",
      badge: { kind: "measuring", delta: null, n: 0, nOn: 0, nOff: 0 },
    },
    {
      id: "caveman",
      name: "Caveman",
      plainLabel: "Terse replies",
      description: "Claude answers in short caveman speak - fewer words, same meaning.",
      installType: "claude_plugin",
      status: "curated_v1",
      pinned: false,
      defaultOn: true,
      installed: true,
      enabled: true,
      installable: true,
      behaviorChanging: true,
      warning:
        "Independent JetBrains A/B test measured ~8.5% real-world savings vs the 65% claim - no quality loss. Modest but real.",
      risk: "low",
      claimedSavings: "65% fewer output tokens (author, chat-only benchmark)",
      license: "MIT",
      licenseNote: null,
      ordering: 50,
      configurable: true,
      launchCommand: null,
      // Estimated: enough observational history to show a number, but no live
      // holdout yet - the gray-blue "≈ −X% estimated" badge.
      badge: { kind: "estimated", delta: -0.085, n: 15, nOn: 10, nOff: 5 },
    },
    {
      id: "ponytail",
      name: "Ponytail",
      plainLabel: "Write less code",
      description: "Nudges Claude to build only what you asked for - no gold-plating.",
      installType: "claude_plugin",
      status: "curated_v1",
      pinned: false,
      defaultOn: false,
      installed: false,
      enabled: false,
      installable: true,
      behaviorChanging: true,
      warning:
        "Changes how Claude writes code (less of it). Authors self-corrected their early claim; honest benchmark: −22% tokens.",
      risk: "low",
      claimedSavings: "−22% tokens / −20% cost (author agentic benchmark)",
      license: "MIT",
      licenseNote: null,
      ordering: 60,
      configurable: false,
      launchCommand: null,
      badge: { kind: "measuring", delta: null, n: 4, nOn: 4, nOff: 0 },
    },
    // The per-saver stream breakdown is grafted on from the real report, so the
    // rows that expand in dev:mock expand onto the same comparison the product
    // shows - and a saver with no attribution yet stays a plain row, as it does
    // in the app.
  ];
  return rows.map((s) => ({ ...s, ...(saverStreams[s.id] ?? {}) }));
}

const saverStreams = snapshot.saverStreams as Record<
  string,
  Pick<SaverRow, "streams" | "turns" | "summary" | "caveat">
>;

function emptySavers(): SaverRow[] {
  return populatedSavers().map((s) => ({
    ...s,
    installed: false,
    enabled: false,
    streams: [],
    turns: null,
    badge: { kind: "measuring", delta: null, n: 0, nOn: 0, nOff: 0 },
  }));
}

let savers: SaverRow[] = EMPTY ? emptySavers() : populatedSavers();

// Mutual-exclusion pairs, mirroring the real catalog's `conflictsWith`. Turning
// one on auto-disables the other (they can't both own the same channel).
const CONFLICTS: Record<string, string[]> = {
  headroom: ["rtk"],
  rtk: ["headroom"],
};

function friendlyName(id: string): string {
  const s = savers.find((x) => x.id === id);
  return s?.name ?? id;
}

/** Disable any enabled saver that conflicts with `enabledId`; return their ids. */
function applyConflicts(enabledId: string): string[] {
  const conflictIds = new Set<string>([
    ...(CONFLICTS[enabledId] ?? []),
    ...savers.filter((s) => (CONFLICTS[s.id] ?? []).includes(enabledId)).map((s) => s.id),
  ]);
  const turnedOff: string[] = [];
  savers = savers.map((s) => {
    if (s.id !== enabledId && conflictIds.has(s.id) && s.enabled) {
      turnedOff.push(s.id);
      return { ...s, enabled: false };
    }
    return s;
  });
  return turnedOff;
}

/** The plain-language heads-up for savers auto-disabled in favor of `replacerId`. */
function conflictNotice(turnedOff: string[], replacerId: string): string | undefined {
  if (turnedOff.length === 0) return undefined;
  return turnedOff
    .map(
      (id) => `${friendlyName(id)} turned off - ${friendlyName(replacerId)} does the same job and is now on.`,
    )
    .join(" ");
}

/** Mirror of the real backend's wrapper-saver heads-up (backend.rs launch_notice). */
function launchNotice(id: string): string | undefined {
  const s = savers.find((x) => x.id === id);
  if (!s?.launchCommand || !s.enabled) return undefined;
  return `${s.name} is on. It saves only in sessions you start with ${s.launchCommand}. Plain claude sessions are untouched.`;
}

// The master switch is a system-level flag, independent of individual savers -
// disabling any one saver leaves Piggy ON. Seeded from "is anything running" so
// the demo opens in a sensible state; only master_toggle writes it thereafter.
let masterOnFlag = savers.some((s) => s.enabled);
function masterOn(): boolean {
  return masterOnFlag;
}

function saversState(notice?: string): SaversState {
  return { masterOn: masterOn(), savers: savers.map((s) => ({ ...s })), notice: notice ?? null };
}

// ---------------------------------------------------------------------------
// sweep
// ---------------------------------------------------------------------------

function initialSweepItems(): SweepItem[] {
  return [
    {
      idx: 1,
      stableId: "mcp|playwright|/Users/you/code/app",
      kind: "mcp",
      id: "playwright",
      source: "/Users/you/code/app",
      used: 0,
      usedScope: "window",
      estTokens: 2100,
      estimated: true,
      recommendDisable: true,
      reason: "no tool calls in the look-back window",
    },
    {
      idx: 2,
      stableId: "mcp|supabase|/Users/you/code/api",
      kind: "mcp",
      id: "supabase",
      source: "/Users/you/code/api",
      used: 0,
      usedScope: "window",
      estTokens: 1300,
      estimated: true,
      recommendDisable: true,
      reason: "no tool calls in the look-back window",
    },
    {
      idx: 3,
      stableId: "skill|legacy-migrator|/Users/you/.claude/skills/legacy-migrator",
      kind: "skill",
      id: "legacy-migrator",
      source: "/Users/you/.claude/skills/legacy-migrator",
      used: 0,
      usedScope: "lifetime",
      estTokens: 900,
      estimated: true,
      recommendDisable: true,
      reason: "installed but never invoked (lifetime)",
    },
    {
      idx: 4,
      stableId: "plugin|formatter@marketplace|",
      kind: "plugin",
      id: "formatter@marketplace",
      source: null,
      used: 12,
      usedScope: "lifetime",
      estTokens: 800,
      estimated: true,
      recommendDisable: false,
      reason: "used 12 time(s) (lifetime)",
    },
    {
      idx: 5,
      stableId: "hook|PreToolUse#1|PreToolUse",
      kind: "hook",
      id: "PreToolUse#1",
      source: "PreToolUse",
      used: 0,
      usedScope: "n/a",
      estTokens: 0,
      estimated: true,
      recommendDisable: false,
      reason: "hook - fires on events, not usage-measurable and costs no context tokens",
    },
  ];
}

let sweepItems: SweepItem[] = EMPTY ? [] : initialSweepItems();

function reindexSweep(): void {
  sweepItems = sweepItems.map((it, i) => ({ ...it, idx: i + 1 }));
}

function sweepReport(): SweepReport {
  const recoverable = sweepItems
    .filter((i) => i.recommendDisable)
    .reduce((a, i) => a + i.estTokens, 0);
  return {
    sessionsConsidered: EMPTY ? 0 : 50,
    estRecoverableTokens: recoverable,
    estimated: true,
    items: sweepItems.map((i) => ({ ...i })),
  };
}

// ---------------------------------------------------------------------------
// stats / share / discover / settings / doctor
// ---------------------------------------------------------------------------

function periodLabel(p: Period): string {
  return { today: "Today", week: "Last 7 days", month: "Last 30 days", all: "All time" }[p];
}

function statsOverview(period: Period): StatsOverview {
  const label =
    period === "today" ? "Today" : period === "week" ? "Last 7 days" : period === "month" ? "Last 30 days" : "All time";
  if (EMPTY) {
    return {
      period, periodLabel: label,
      streams: { input: 0, output: 0, cacheWrite: 0, cacheRead: 0 },
      totalTokens: 0, sessions: 0, costUsdEst: 0, costEstimated: true, fullyPriced: false,
      todayTokens: 0,
      headline: {
        value: null,
        label: "not_enough_data",
        nHoldout: 0,
        note: null,
        nFullOn: 0,
        nBaseline: 0,
        baselineKind: "none",
        onRandomized: false,
        nFullOnRandomized: 0,
        baselineClean: false,
        multiplierState: "no_data",
        minGroup: 10,
        streams: [],
        turns: null,
        waiting: null,
        nCarried: 0,
        carriedSavers: [],
      },
    };
  }
  const st = snapshot.stats as Omit<StatsOverview, "period" | "todayTokens">;
  return { ...st, period, periodLabel: label, todayTokens: 0 };
}

function sourcesOverview(period: Period): SourcesOverview {
  if (EMPTY) return { period, cells: [], unknownTokens: 0, unknownSessions: 0 };
  return { period, ...(snapshot.sources as Omit<SourcesOverview, "period">) };
}

function usageSeries(period: Period): UsageSeries {
  const days = { today: 1, week: 7, month: 30, all: 120 }[period];
  const today = new Date();
  const points = [];
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(today);
    d.setDate(today.getDate() - i);
    const iso = d.toISOString().slice(0, 10);
    // Weekends quiet, a couple of true zero days for gap realism; otherwise a
    // deterministic weekday-shaped ramp so the chart reads like real work.
    const dow = d.getDay();
    const idle = EMPTY || dow === 0 || i === 4 || i === 11;
    if (idle) {
      points.push({ date: iso, totalTokens: 0, input: 0, output: 0, cacheWrite: 0, cacheRead: 0, costUsdEst: 0, sessions: 0 });
      continue;
    }
    const wobble = 0.6 + ((i * 37) % 100) / 125; // 0.6..1.4, stable per day
    const base = dow === 6 ? 0.45 : 1;
    const input = Math.round(180_000 * base * wobble);
    const output = Math.round(70_000 * base * wobble);
    const cacheWrite = Math.round(120_000 * base * wobble);
    const cacheRead = Math.round(210_000 * base * wobble);
    points.push({
      date: iso,
      totalTokens: input + output + cacheWrite + cacheRead,
      input,
      output,
      cacheWrite,
      cacheRead,
      costUsdEst: Math.round((input + output + cacheWrite + cacheRead) * 0.0000075 * 100) / 100,
      sessions: Math.max(1, Math.round(4 * base * wobble)),
    });
  }
  return { period, periodLabel: periodLabel(period), points };
}

// Per-saver options (Caveman's intensity), mutable so the mock feels live.
let cavemanMode = "full";

function cavemanConfig(): ConfigOption[] {
  return [
    {
      key: "defaultMode",
      label: "Intensity",
      description: "How compressed Claude's replies are. Applies from the next session.",
      choices: [
        { value: "lite", label: "Lite", description: "Trims filler, keeps normal sentences" },
        { value: "full", label: "Full", description: "Classic caveman: drops articles, fragments OK" },
        { value: "ultra", label: "Ultra", description: "Maximum compression, telegram style" },
      ],
      default: "full",
      current: cavemanMode,
    },
  ];
}

function saverConfig(id: string): ConfigOption[] {
  return id === "caveman" ? cavemanConfig() : [];
}

function shareCardData(period: Period): ShareCardData {
  if (EMPTY) {
    return {
      period,
      weekLabel: periodLabel(period),
      tokensSaved: null,
      multiplier: null,
      headlineLabel: "not_enough_data",
      nHoldout: 0,
      shareable: false,
    };
  }
  const weekLabel = {
    today: "Jul 12",
    week: "Jul 6 – Jul 12",
    month: "Jun 13 – Jul 12",
    all: "All time",
  }[period];
  const tokensSaved = { today: 180_000, week: 1_200_000, month: 4_800_000, all: 12_000_000 }[period];
  return {
    period,
    weekLabel,
    tokensSaved,
    multiplier: 1.7,
    headlineLabel: "measured",
    nHoldout: 12,
    shareable: true,
  };
}

function discover(): DiscoverDto {
  const feed = EMPTY
    ? []
    : [
        {
          name: "llm-context-pruner",
          description: "Trims stale file context before each turn using a local heuristic.",
          stars: 214,
          authorClaims: "author claims ~30% fewer input tokens",
          repoUrl: "https://github.com/example/llm-context-pruner",
        },
        {
          name: "promptdiet",
          description: "Rewrites verbose system prompts into compact equivalents.",
          stars: 89,
          authorClaims: "author claims 15% overall",
          repoUrl: "https://github.com/example/promptdiet",
        },
      ];
  const listedOnly = [
    {
      id: "boost",
      name: "Boost",
      description: "JFrog CLI that compacts terminal output before Claude reads it (shell-wrapper, like RTK)",
      claimedSavings: "up to 89.6% claimed (JFrog); ~12% measured in Boost's own README",
      license: "Proprietary-Beta",
      licenseNote:
        "JFrog product in public beta under a BETA_AGREEMENT (not an OSI open-source license). Source-available at github.com/jfrog/boost.",
      exclusionReason:
        "Proprietary beta under the JFrog Online Preview Agreement - its terms forbid a third party from distributing, proxying, or installing a plug-in to the Beta Service without JFrog's prior written approval, so Piggy cannot ship or wire it (install it yourself if you want it). Its Claude hook also auto-allows every Bash command and turns on telemetry with no opt-out.",
      note: "Listed for transparency - not installable.",
      repoUrl: "https://github.com/jfrog/boost",
      risk: "high",
    },
    {
      id: "token-optimizer-mcp",
      name: "token-optimizer-mcp",
      description: "MCP server with 65 tools + hook pipeline",
      claimedSavings: "60–90% (author; GPT-4 tokenizer approximation)",
      license: "MIT",
      licenseNote: null,
      exclusionReason:
        "No documented uninstall path (violates Piggy's reversibility principle); npm postinstall auto-edits settings.json without opt-in; self-documented settings.json corruption bug; releases frozen ~8 months.",
      note: "Listed for transparency - not installable.",
      repoUrl: null,
      risk: "high",
    },
    {
      id: "token-optimizer",
      name: "Token Optimizer",
      description: "Sends Claude only what changed in files it already saw",
      claimedSavings: "~18% overall (author, 684-session counterfactual)",
      license: "PolyForm-Noncommercial-1.0.0",
      licenseNote:
        "Source-available, NOT open source. Free for individuals and small teams. Piggy shows this label before install.",
      exclusionReason: null,
      note: "Coming in a later Piggy update - it needs a license-acknowledge step we haven't built yet.",
      repoUrl: "https://github.com/alexgreensh/token-optimizer",
      risk: "low",
    },
    {
      id: "headroom",
      name: "Headroom",
      description: "Proxy-level compression for everything Claude reads",
      claimedSavings: "47–92% by workload (author, reproducible eval suite)",
      license: "Apache-2.0",
      licenseNote: null,
      exclusionReason: null,
      note: "Piggy's intended default compressor (turns on ahead of RTK), but the proxy install engine isn't built yet - planned for a future version.",
      repoUrl: null,
      risk: "medium",
    },
    {
      id: "nadirclaw",
      name: "NadirClaw",
      description: "Routes simple prompts to cheaper/local models, hard ones to Claude",
      claimedSavings: "40–70% by routing to cheaper models (author)",
      license: "PolyForm-Noncommercial-1.0.0",
      licenseNote:
        "Source-available, NOT open source. Free for noncommercial use; commercial use needs a license. Piggy shows this label before install.",
      exclusionReason: null,
      note: "Router/proxy - conflicts with Headroom (both own ANTHROPIC_BASE_URL). Needs the same proxy install engine, planned for a future version.",
      repoUrl: "https://github.com/NadirRouter/NadirClaw",
      risk: "medium",
    },
  ];
  return { feed, listedOnly };
}

let settings: Settings = {
  holdoutFraction: 0.1,
  rotationEnabled: true,
  launchAtLogin: false,
  cliTool: false,
};

function doctor(): Doctor {
  const checks = [
    { label: "Claude Code history", ok: true, detail: "Piggy can read your sessions." },
    { label: "Claude's settings", ok: true, detail: "Backed up and readable." },
    { label: "Piggy's database", ok: true, detail: "Writable and healthy." },
    {
      label: "Cost estimates",
      ok: true,
      detail: EMPTY
        ? "Pricing table loaded (28 models)."
        : "99% of tokens matched a known price (28 models).",
    },
  ];
  return { ok: true, checks };
}

function environment(): Environment {
  if (NO_CLAUDE) return { claudeInstalled: false, codexInstalled: false, hasData: false, sessions: 0 };
  if (EMPTY) return { claudeInstalled: true, codexInstalled: false, hasData: false, sessions: 0 };
  return { claudeInstalled: true, codexInstalled: true, hasData: true, sessions: 143 };
}

// ---------------------------------------------------------------------------
// dispatch
/** Ledger + insights come from a SNAPSHOT OF THE REAL DATABASE, never from
 *  numbers typed here. Regenerate with:
 *
 *      node scripts/snapshot-dev-data.mjs [--since YYYY-MM-DD]
 *
 *  Hand-written figures drift from the product and hide bugs — one did exactly
 *  that, disagreeing with the real ledger while a broken headroom multiplier
 *  went unnoticed. If these look wrong on screen, they are wrong for real. */
function ledgerOverview(period: Period): LedgerOverview {
  const label =
    period === "today" ? "Today" : period === "week" ? "Last 7 days" : period === "month" ? "Last 30 days" : "All time";
  if (EMPTY) {
    return {
      period, periodLabel: label, totalTokens: 0, removableTokens: 0, overhead: 0,
      headroom: null, removableShare: 0,
      sessions: 0, sources: [], projects: [], empty: true,
    };
  }
  return { ...(snapshot.ledger as Omit<LedgerOverview, "period">), period };
}

function ledgerInsights(): Insight[] {
  return EMPTY ? [] : (snapshot.insights as Insight[]);
}

/** Same rule as the ledger: a snapshot of the real database, never authored.
 *  The task columns in particular must not be invented: a fabricated failure
 *  rate is exactly the kind of number this product exists to refuse. */
function taskTable(period: Period): TaskTable {
  const label =
    period === "today" ? "Today" : period === "week" ? "Last 7 days" : period === "month" ? "Last 30 days" : "All time";
  if (EMPTY) {
    return { period, periodLabel: label, rows: [], tasksUnrecorded: false, empty: true };
  }
  return { ...(snapshot.tasks as Omit<TaskTable, "period">), period };
}

// --- the local advisor ------------------------------------------------------

/** An 8GB Mac: only the models that fit are listed, which is the real gate. */
let advisorStatus: AdvisorStatus = {
  compiledIn: true,
  hostRamBytes: 8 * 1024 * 1024 * 1024,
  budgetBytes: 4.8 * 1024 * 1024 * 1024,
  state: "ready",
  selectedId: "qwen3-4b-instruct-2507",
  recommendedId: "qwen3-4b-instruct-2507",
  models: [
    {
      id: "qwen3-4b-instruct-2507",
      name: "Qwen3 4B Instruct",
      blurb: "Best at reading your config. The default when it fits.",
      bytes: 2_497_281_120,
      peakBytes: 3_073_000_000,
      context: 4096,
      downloaded: true,
    },
    {
      id: "gemma-3-4b-it",
      name: "Gemma 3 4B",
      blurb: "Same size as Qwen 4B but far cheaper at long context.",
      bytes: 2_489_894_016,
      peakBytes: 2_900_000_000,
      context: 8192,
      downloaded: false,
    },
  ],
};

/**
 * Sample notes, written the way real ones must read: they name a cause and a
 * specific item, and they contain **no figures**. Anything numeric here would be
 * a fixture that the real guard would have rejected, which would make the mock
 * lie about what the feature does.
 */
function advisorAnnotations(): Annotation[] {
  if (EMPTY) return [];
  const ids = new Set(ledgerInsights().map((i) => i.id));
  return [
    {
      insightId: "floor-dominates",
      headline: "Your skill listing is the largest thing loaded before you type",
      why: "Every project loads the full catalogue at startup, so short sessions pay for it without using it. Scoping it to the projects that invoke skills would shrink the floor.",
      model: "Qwen3 4B Instruct",
    },
    {
      insightId: "per-turn-injections",
      headline: "A hook is firing on every tool call, not just once",
      why: "The largest injection re-enters context on each turn rather than at startup. Narrowing its matcher would keep it out of turns that do not need it.",
      model: "Qwen3 4B Instruct",
    },
  ].filter((n) => ids.has(n.insightId));
}

// ---------------------------------------------------------------------------

export async function mockInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const a = args ?? {};
  const out = ((): unknown => {
    switch (cmd) {
      case "environment":
        return environment();
      case "stats_overview":
        return statsOverview((a.period as Period) ?? "week");
      case "sources_overview":
        return sourcesOverview((a.period as Period) ?? "week");
      case "usage_series":
        return usageSeries((a.period as Period) ?? "week");
      case "ledger_overview":
        return ledgerOverview((a.period as Period) ?? "week");
      case "ledger_insights":
        return ledgerInsights();
      case "task_table":
        return taskTable((a.period as Period) ?? "week");
      case "savers_list":
        return saversState();
      case "saver_config_get":
        return saverConfig(a.id as string);
      case "saver_config_set": {
        if (a.id === "caveman" && a.key === "defaultMode") {
          cavemanMode = a.value as string;
        }
        return saverConfig(a.id as string);
      }
      case "saver_toggle": {
        const id = a.id as string;
        const on = a.on as boolean;
        savers = savers.map((s) =>
          s.id === id ? { ...s, installed: on ? true : s.installed, enabled: on } : s,
        );
        const notice = on
          ? [conflictNotice(applyConflicts(id), id), launchNotice(id)].filter(Boolean).join(" ") || undefined
          : undefined;
        return saversState(notice);
      }
      case "master_toggle": {
        const on = a.on as boolean;
        masterOnFlag = on;
        if (!on) {
          // Turning the master off pauses every enabled saver (matches the real
          // backend), so nothing conflicts and there's no notice.
          savers = savers.map((s) => ({ ...s, enabled: false }));
          return saversState();
        }
        // Enable the curated default-on set in order; each may auto-disable a
        // conflicting saver (e.g. Headroom replaces Shrink terminal noise).
        const turnedOff: string[] = [];
        let replacer = "";
        for (const d of savers.filter((s) => s.defaultOn)) {
          savers = savers.map((s) =>
            s.id === d.id ? { ...s, installed: true, enabled: true } : s,
          );
          const off = applyConflicts(d.id);
          if (off.length > 0) replacer = d.id;
          turnedOff.push(...off);
        }
        const parts = [
          replacer ? conflictNotice(turnedOff, replacer) : undefined,
          ...savers
            .filter((s) => s.defaultOn && s.enabled && s.launchCommand)
            .map((s) => launchNotice(s.id)),
        ].filter(Boolean);
        return saversState(parts.length > 0 ? parts.join(" ") : undefined);
      }
      case "sweep_report":
        return sweepReport();
      case "sweep_apply": {
        const ids = new Set((a.itemIds as string[]) ?? []);
        sweepItems = sweepItems.filter((i) => !(ids.has(i.stableId) && i.kind !== "hook"));
        reindexSweep();
        return sweepReport();
      }
      case "sweep_restore": {
        sweepItems = EMPTY ? [] : initialSweepItems();
        return sweepReport();
      }
      case "discovered_list":
      case "refresh_discovered":
        return discover();
      case "share_card_data":
        return shareCardData((a.period as Period) ?? "week");
      case "save_share_card":
        return { path: "~/Desktop/piggy-savings.png" };
      case "settings_get":
        return settings;
      case "settings_set":
        settings = a.settings as Settings;
        return settings;
      // The advisor, as an 8GB Mac with the model already downloaded, so the
      // "ready" state and the annotated findings can be designed without a
      // 2.5 GB fetch. Progress events do not exist in mock (no Tauri event bus),
      // so the download flow is the one state this cannot show.
      case "advisor_status":
        return advisorStatus;
      case "advisor_select":
        advisorStatus = { ...advisorStatus, selectedId: (a.modelId as string) ?? null };
        advisorStatus.state = advisorStatus.selectedId ? "ready" : "off";
        return advisorStatus;
      case "advisor_download":
      case "advisor_cancel":
        return undefined;
      case "advisor_remove":
        advisorStatus = { ...advisorStatus, selectedId: null, state: "off" };
        return advisorStatus;
      case "advisor_annotate":
        return advisorStatus.state === "ready" ? advisorAnnotations() : [];
      // No inference in a browser. The per-saver panel renders its deterministic
      // summary and caveat either way, which is exactly what a user without the
      // advisor sees.
      case "advisor_savers":
        return [];
      case "restore_defaults":
        savers = EMPTY ? emptySavers() : populatedSavers();
        sweepItems = EMPTY ? [] : initialSweepItems();
        return {
          byteRestored: true,
          saversRemoved: 2,
          sweptRestored: 0,
          filesRemoved: 1,
          messages: ["settings.json restored to its exact pre-Piggy contents"],
        } satisfies RestoreResult;
      case "doctor":
        return doctor();
      case "system_info":
        return {
          version: "0.1.0",
          arch: "Apple Silicon",
          dataDir: "~/.piggy",
          // No database in a browser build, and the About table says so rather
          // than inventing a size.
          database: null,
        } satisfies SystemInfo;
      case "open_data_folder":
        return undefined;
      case "reindex":
        return {
          ran: !NO_CLAUDE,
          sessions: EMPTY ? 0 : 143,
          updated: 0,
          scanned: EMPTY ? 0 : 143,
        } satisfies ReindexResult;
      case "open_external":
        if (typeof window !== "undefined") window.open(String(a.url), "_blank");
        return undefined;
      // The mock build is always "up to date": there is no update endpoint to
      // ask, and pretending otherwise would put a fake Install button in the UI.
      case "check_for_update":
        return null;
      case "install_update":
        return undefined;
      default:
        throw { title: "Unknown command", detail: cmd, rolledBack: false };
    }
  })();
  // Mimic real IPC latency so busy/progress states are visible in mock mode.
  if (cmd === "saver_toggle" || cmd === "master_toggle") {
    await new Promise((r) => setTimeout(r, 700));
  }
  return out as T;
}
