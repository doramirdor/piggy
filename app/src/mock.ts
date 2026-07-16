// In-memory fixtures for `npm run dev:mock`. VITE_MOCK=1 serves a realistic
// populated state; VITE_MOCK=empty serves the fresh-install first-run state;
// VITE_MOCK=noclaude serves the "no Claude Code found" state. Toggles/sweep
// mutate module-level copies so the UI feels live during design QA.

import { MOCK_MODE } from "./ipc";
import type {
  ConfigOption,
  DiscoverDto,
  Doctor,
  Environment,
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
  UsageSeries,
} from "./types";

const EMPTY = MOCK_MODE === "empty";
const NO_CLAUDE = MOCK_MODE === "noclaude";

// ---------------------------------------------------------------------------
// savers
// ---------------------------------------------------------------------------

function populatedSavers(): SaverRow[] {
  return [
    {
      id: "sweep",
      name: "Sweep",
      plainLabel: "Clean unused extras",
      description: "Finds add-ons you never use that cost tokens on every request.",
      installType: "builtin",
      status: "curated_v1",
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
      badge: { kind: "measured", delta: -0.09, n: 18 },
    },
    {
      id: "rtk",
      name: "RTK",
      plainLabel: "Shrink terminal noise",
      description: "Compresses command output (git, tests, builds) before Claude sees it.",
      installType: "binary+hook",
      status: "curated_v1",
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
      badge: { kind: "measured", delta: -0.22, n: 41 },
    },
    {
      id: "token-optimizer",
      name: "Token Optimizer",
      plainLabel: "Smart file re-reads",
      description: "Sends Claude only what changed in files it already saw.",
      installType: "claude_plugin",
      status: "curated_v1",
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
      badge: { kind: "measuring", delta: null, n: 0 },
    },
    {
      id: "headroom",
      name: "Headroom",
      plainLabel: "Compress everything (Headroom)",
      description: "Piggy's default compressor. Wraps terminal shrinking and more in one proxy.",
      installType: "binary+proxy",
      status: "curated_v1",
      defaultOn: true,
      installed: true,
      // Off initially so rtk can be on - turning the master on flips this and
      // auto-disables the conflicting RTK.
      enabled: false,
      installable: true,
      behaviorChanging: false,
      warning: null,
      risk: "low",
      claimedSavings: "~80% on shell output, plus prompt compression",
      license: "MIT",
      licenseNote: null,
      ordering: 40,
      configurable: false,
      badge: { kind: "measuring", delta: null, n: 0 },
    },
    {
      id: "caveman",
      name: "Caveman",
      plainLabel: "Terse replies",
      description: "Claude answers in short caveman speak - fewer words, same meaning.",
      installType: "claude_plugin",
      status: "curated_v1",
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
      // Estimated: enough observational history to show a number, but no live
      // holdout yet - the gray-blue "≈ −X% estimated" badge.
      badge: { kind: "estimated", delta: -0.085, n: 15 },
    },
    {
      id: "ponytail",
      name: "Ponytail",
      plainLabel: "Write less code",
      description: "Nudges Claude to build only what you asked for - no gold-plating.",
      installType: "claude_plugin",
      status: "curated_v1",
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
      badge: { kind: "measuring", delta: null, n: 4 },
    },
  ];
}

function emptySavers(): SaverRow[] {
  return populatedSavers().map((s) => ({
    ...s,
    installed: false,
    enabled: false,
    badge: { kind: "measuring", delta: null, n: 0 },
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
  if (EMPTY) {
    return {
      period,
      periodLabel: periodLabel(period),
      streams: { input: 0, output: 0, cacheWrite: 0, cacheRead: 0 },
      totalTokens: 0,
      sessions: 0,
      costUsdEst: 0,
      costEstimated: true,
      fullyPriced: true,
      todayTokens: 0,
      headline: { value: null, label: "not_enough_data", nHoldout: 0 },
    };
  }
  const scale = { today: 0.12, week: 1, month: 4.2, all: 11 }[period];
  const streams = {
    input: Math.round(620_000 * scale),
    output: Math.round(240_000 * scale),
    cacheWrite: Math.round(380_000 * scale),
    cacheRead: Math.round(520_000 * scale),
  };
  const total = streams.input + streams.output + streams.cacheWrite + streams.cacheRead;
  return {
    period,
    periodLabel: periodLabel(period),
    streams,
    totalTokens: total,
    sessions: { today: 4, week: 31, month: 118, all: 143 }[period],
    costUsdEst: Math.round(42.18 * scale * 100) / 100,
    costEstimated: true,
    fullyPriced: true,
    todayTokens: 184_000,
    headline: { value: 1.7, label: "measured", nHoldout: 12 },
  };
}

function sourcesOverview(period: Period): SourcesOverview {
  if (EMPTY) {
    return {
      period,
      cells: [
        { source: "claude-code", interface: "gui", sessions: 0, totalTokens: 0, costUsdEst: 0, toolPresent: true },
        { source: "claude-code", interface: "tui", sessions: 0, totalTokens: 0, costUsdEst: 0, toolPresent: true },
        { source: "codex", interface: "gui", sessions: 0, totalTokens: 0, costUsdEst: 0, toolPresent: false },
        { source: "codex", interface: "tui", sessions: 0, totalTokens: 0, costUsdEst: 0, toolPresent: false },
      ],
      unknownTokens: 0,
      unknownSessions: 0,
    };
  }
  const scale = { today: 0.12, week: 1, month: 4.2, all: 11 }[period];
  const t = (n: number) => Math.round(n * scale);
  return {
    period,
    cells: [
      { source: "claude-code", interface: "gui", sessions: t(19), totalTokens: t(1_080_000), costUsdEst: Math.round(2540 * scale) / 100, toolPresent: true },
      { source: "claude-code", interface: "tui", sessions: t(8), totalTokens: t(410_000), costUsdEst: Math.round(980 * scale) / 100, toolPresent: true },
      { source: "codex", interface: "gui", sessions: t(2), totalTokens: t(90_000), costUsdEst: Math.round(160 * scale) / 100, toolPresent: true },
      { source: "codex", interface: "tui", sessions: t(2), totalTokens: t(180_000), costUsdEst: Math.round(320 * scale) / 100, toolPresent: true },
    ],
    unknownTokens: 0,
    unknownSessions: 0,
  };
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
        const notice = on ? conflictNotice(applyConflicts(id), id) : undefined;
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
        return saversState(replacer ? conflictNotice(turnedOff, replacer) : undefined);
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
