# Build Prompt: Token Optimizer Stack Manager (working name: "stacked")

You are building a **free, open-source macOS menu bar app** (Tauri v2 + Rust backend + React frontend) that lets non-technical Claude Code users ("vibe coders") install, manage, and honestly measure token-saving optimizers — with one toggle each, no terminal, no settings.json editing.

**One-line pitch:** The App Store plus the referee for Claude Code token optimizers. Install is the hook, holdout-based measurement is the moat.

**Target user:** Uses Claude Code daily (CLI or VS Code extension), hits usage limits, is NOT comfortable editing JSON configs or running init commands. Everything must be reversible with one click.

---

## Architecture (3 components, build in this order)

### 1. Measurement daemon (Rust, standalone — build FIRST, works without GUI)

Ground-truth token accounting. Never trust optimizer self-reported savings.

- Watch Claude Code session logs: JSONL files under `~/.claude/projects/**/*.jsonl`. Parse per-message usage blocks: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, model, timestamp. Verify the current JSONL schema against real files on this machine before writing the parser — do not assume field names.
- Store per-session aggregates in local SQLite (`~/.stacked/stacked.db`): session_id, project, start/end, token counts per stream, estimated cost (per-model pricing table, keep in a data file), active optimizer set at session start (snapshot from config state).
- **Holdout system:** a configurable fraction of sessions (default 10%) run with ALL optimizers disabled (daemon temporarily removes hooks before session start where feasible; where not feasible, mark holdout sessions as "observational baseline" using pre-install historical sessions). Savings are computed as measured delta vs holdout/baseline, normalized per-turn and per-tool-call, never as raw before/after totals. Label every number in the UI as `measured` or `estimated` — never blend.
- **Per-optimizer attribution:** rotate optimizer sets across sessions (simple round-robin A/B) so over time each optimizer accumulates on/off comparison data. Show per-optimizer: measured token delta, added latency (if measurable), confidence (n sessions).
- Also expose a CLI (`stacked stats`, `stacked doctor`) so the daemon is useful standalone.

### 2. Optimizer registry + install engine (Rust)

**Registry is data, not code.** A versioned JSON catalog (bundled + refreshable from the app's GitHub repo) where each entry declares:

```json
{
  "id": "rtk",
  "name": "RTK",
  "plainLabel": "Shrink terminal noise",
  "description": "Compresses command output before Claude sees it",
  "layer": "tool_output",        // tool_output | output_style | context_input | static_config | proxy
  "installType": "binary+hook",  // binary+hook | claude_plugin | skill | claude_md | mcp_server | proxy
  "source": { "type": "github_release", "repo": "rtk-ai/rtk" },
  "install": { ... },            // declarative steps: download, verify checksum, hook JSON to merge
  "uninstall": { ... },
  "healthCheck": { ... },        // e.g. run `rtk --version`, verify hook present in settings.json
  "conflictsWith": [],
  "ordering": 10,                // lower = earlier in hook chain
  "behaviorChanging": false,     // true => extra warning label in UI
  "claimedSavings": "60-90% on shell output",
  "risk": "low"
}
```

**Seed catalog (research each repo's CURRENT install mechanism before writing its entry — do not trust this prompt's summaries, verify against the live READMEs):**

| id | repo | layer | v1? |
|---|---|---|---|
| rtk | rtk-ai/rtk | tool_output | ✅ default-on in master switch |
| ponytail | DietrichGebert/ponytail | output_style | ✅ (label: changes coding style) |
| sweep | (built-in, see below) | static_config | ✅ |
| caveman | juliusbrussee/caveman | output_style | ⚠️ ship but show measured-savings warning (JetBrains A/B measured 8.5% ceiling vs 65% claim) |
| token-optimizer | alexgreensh/token-optimizer | context_input | v1.1 |
| context-mode | mksglu/context-mode | context_input | v1.1 |
| cto | nadimtuhin/claude-token-optimizer | static_config | v1.1 |
| headroom | headroomlabs-ai/headroom | proxy | v2 (Python runtime dep; also a paid desktop competitor exists — do not bundle in v1) |
| token-optimizer-mcp | ooples/token-optimizer-mcp | context_input | v2 (its tool manifest costs 4-6k tokens/session — surface that in UI) |

**Built-in "sweep" optimizer (our own code, the only optimization logic we write):** scan `~/.claude/settings.json`, installed skills, and MCP server configs; cross-reference against session logs to find skills/MCP servers never invoked in the last N sessions; estimate their per-session context cost (tool schemas + skill descriptions loaded every request); offer one-click disable with restore. This is static-config cleanup — zero runtime risk.

**Discovery module:** a background job that queries the GitHub API (topics: `token-optimization` + `claude-code`, search: "claude code token" sorted by stars/recency) and surfaces new candidate tools in a "Discovered" tab — NOT auto-installed, just listed with stars/recency/description and a link. New tools only become installable via a registry update (curated), never automatically. This keeps the app evergreen without executing unvetted code.

**Config merge engine — this is the hard, important part:**
- Own all writes to `~/.claude/settings.json`. Before ANY write: timestamped backup to `~/.stacked/backups/`. Atomic write (temp file + rename).
- Merge hooks additively, preserving user's pre-existing hooks untouched. Respect `ordering` for hook chain sequence (e.g., RTK's PreToolUse rewrite must run before anything that reads the command).
- One-click **Restore Defaults**: revert to the pre-stacked backup.
- If settings.json was modified externally since our last write, re-read and re-merge — never clobber.
- Test the merge engine hardest of everything. Include unit tests with real-world messy settings.json fixtures (existing hooks, comments-stripped JSON, missing files).

### 3. Tauri GUI (menu bar app)

- **Menu bar icon** with today's tokens + savings %. Click opens the panel.
- **Main panel:** master switch ("Optimize everything" = curated v1 set in correct order), then per-optimizer toggles with plain-language labels, measured savings badge (`measured 22% · 41 sessions` or `not enough data yet`), and behavior-change warnings where flagged.
- **Dashboard tab:** headline "Your Claude plan lasts N.Nx longer" (measured), token breakdown by stream (input/output/cache_create/cache_read), per-optimizer attribution table, sweep recommendations ("3 unused MCP servers cost you ~4,100 tokens/session — clean up").
- **Discovered tab:** new tools feed from the discovery module.
- **Share card:** generate a PNG stat card ("My stack saved 1.2M tokens this week — measured with holdout · stacked") with a copy/save button. This is the growth loop; make it look good.
- Frontend: React + Tailwind, dark mode default, minimal. No Electron, no localStorage (use Tauri store / SQLite via commands).
- Distribution: `npx stacked-app` installer that downloads the notarized .dmg/.app and launches it, plus plain GitHub releases. Set up the Tauri updater config (signing keys as a documented manual step, don't block the build on it).

---

## Principles (non-negotiable)

1. **Never vendor or fork optimizer code.** Download official release artifacts at toggle-time, pin known-good versions in the registry, verify checksums. We are an orchestrator, not a reimplementation.
2. **Never blend measured and claimed numbers.** README claims appear only in the install card, clearly labeled "claimed". Dashboard shows measured only.
3. **Everything reversible.** Every install has a tested uninstall. Restore Defaults always works.
4. **No telemetry, no accounts, no network calls except:** GitHub (registry refresh, releases, discovery) — and state this in the README.
5. **Fail safe:** if a health check fails post-install, auto-rollback that optimizer and show the error plainly.
6. **The user never sees a terminal, a JSON file, or the word "hook".**

## Milestones

1. **M1 — Daemon:** JSONL parser + SQLite + `stacked stats` CLI. Acceptance: correct per-session token totals matching `/cost` output on real sessions from this machine.
2. **M2 — Install engine:** registry format, merge engine with backup/rollback, RTK + ponytail + sweep adapters, health checks, full uninstall. Acceptance: install→verify→uninstall→settings.json byte-identical to backup, on a machine with pre-existing hooks.
3. **M3 — Measurement:** holdout scheduling, per-optimizer A/B rotation, attribution math. Acceptance: dashboard-ready measured deltas with n-counts.
4. **M4 — GUI + share card + npx installer.** Acceptance: fresh Mac, `npx stacked-app`, toggle master switch, run one Claude Code session, see it counted.

Ship M1+M2 as a working CLI before touching Tauri. Ask before adding any dependency beyond Tauri, React, Tailwind, rusqlite/sqlx, serde, notify (fs watching), and reqwest.

## Before writing code

1. Inspect real `~/.claude/projects` JSONL files and `~/.claude/settings.json` on this machine to confirm schemas.
2. Fetch and read the live READMEs of rtk-ai/rtk, DietrichGebert/ponytail, juliusbrussee/caveman to confirm current install/uninstall mechanics and hook shapes.
3. Write the registry entries for the v1 set and show them to me for review before implementing adapters.
