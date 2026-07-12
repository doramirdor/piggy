// In-memory fixtures for `npm run dev:mock`. VITE_MOCK=1 serves a realistic
// populated state; VITE_MOCK=empty serves the fresh-install first-run state;
// VITE_MOCK=noclaude serves the "no Claude Code found" state. Toggles/sweep
// mutate module-level copies so the UI feels live during design QA.

import { MOCK_MODE } from "./ipc";
import type {
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
  StatsOverview,
  SweepItem,
  SweepReport,
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
      badge: { kind: "measured", delta: -0.09, n: 18 },
    },
    {
      id: "rtk",
      name: "RTK",
      plainLabel: "Shrink terminal noise",
      description: "Compresses command output (git, tests, builds) before Claude sees it.",
      installType: "binary+hook",
      status: "curated_v1",
      defaultOn: true,
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
      badge: { kind: "measured", delta: -0.22, n: 41 },
    },
    {
      id: "caveman",
      name: "Caveman",
      plainLabel: "Terse replies",
      description: "Claude answers in short caveman speak — fewer words, same meaning.",
      installType: "claude_plugin",
      status: "curated_v1",
      defaultOn: false,
      installed: true,
      enabled: true,
      installable: true,
      behaviorChanging: true,
      warning:
        "Independent JetBrains A/B test measured ~8.5% real-world savings vs the 65% claim — no quality loss. Modest but real.",
      risk: "low",
      claimedSavings: "65% fewer output tokens (author, chat-only benchmark)",
      license: "MIT",
      licenseNote: null,
      ordering: 50,
      // Estimated: enough observational history to show a number, but no live
      // holdout yet — the gray-blue "≈ −X% estimated" badge.
      badge: { kind: "estimated", delta: -0.085, n: 15 },
    },
    {
      id: "ponytail",
      name: "Ponytail",
      plainLabel: "Write less code",
      description: "Nudges Claude to build only what you asked for — no gold-plating.",
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
      badge: { kind: "measuring", delta: null, n: 0 },
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

function masterOn(): boolean {
  const defaults = savers.filter((s) => s.defaultOn);
  return defaults.length > 0 && defaults.every((s) => s.enabled);
}

function saversState(): SaversState {
  return { masterOn: masterOn(), savers: savers.map((s) => ({ ...s })) };
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
      reason: "hook — fires on events, not usage-measurable and costs no context tokens",
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
      note: "Listed for transparency — not installable.",
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
      note: "Coming in a later Piggy update — it needs a license-acknowledge step we haven't built yet.",
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
      note: "Planned for a future version of Piggy.",
      repoUrl: null,
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
  if (NO_CLAUDE) return { claudeInstalled: false, hasData: false, sessions: 0 };
  if (EMPTY) return { claudeInstalled: true, hasData: false, sessions: 0 };
  return { claudeInstalled: true, hasData: true, sessions: 143 };
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
      case "savers_list":
        return saversState();
      case "saver_toggle": {
        const id = a.id as string;
        const on = a.on as boolean;
        savers = savers.map((s) =>
          s.id === id ? { ...s, installed: on ? true : s.installed, enabled: on } : s,
        );
        return saversState();
      }
      case "master_toggle": {
        const on = a.on as boolean;
        savers = savers.map((s) =>
          s.defaultOn ? { ...s, installed: on ? true : s.installed, enabled: on } : s,
        );
        return saversState();
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
  return out as T;
}
