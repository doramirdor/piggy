# Piggy — Design Document

*The token piggy bank for Claude Code.*

## Name & brand

**Piggy** 🐷 — a menu bar piggy bank that makes your Claude plan last longer.

- Why: the ICP (vibe coders on Claude Code / Codex, mostly non-developers) doesn't think in
  "optimizers" or "hooks" — they think *"my plan ran out again."* Piggy = savings, instantly.
  Warm, tweetable ("Piggy saved me 1.2M tokens this week 🐷"), provider-neutral so Codex
  support can come later without a rename.
- CLI binary: `piggy` · data dir: `~/.piggy/` · db: `~/.piggy/piggy.db`
- npm installer: `npx piggybank` (verified available) · repo working dir: this one.
- Tagline: **"Your Claude plan, but longer."** Sub: *The App Store — and the referee — for
  Claude Code token savers.*

## Positioning rules (non-negotiable, from build prompt)

1. Never vendor/fork optimizer code — download official release artifacts, pin versions, verify checksums.
2. Never blend measured and claimed numbers. Dashboard = measured only; README claims appear
   only on install cards labeled "claimed".
3. Everything reversible; Restore Defaults always works.
4. No telemetry, no accounts. Network = GitHub only (registry refresh, releases, discovery).
5. Fail safe: failed health check ⇒ auto-rollback + plain-English error.
6. The user never sees a terminal, a JSON file, or the word "hook". UI vocabulary:
   optimizer = **"saver"**, install = **"turn on"**, hook chain = invisible, settings.json = **"Claude's settings (backed up)"**.

## Architecture

```
crates/
  piggy-core     # parser, sqlite store, pricing, stats, registry, merge engine, holdout
  piggy-cli      # `piggy` binary: index | stats | doctor | (later: install|remove|sweep)
app/             # Tauri v2 menu bar app (React + Tailwind), links piggy-core
registry/        # versioned JSON catalog (data, not code)
docs/research/   # live-verified research on optimizer repos
scripts/         # verify-against-jq.sh etc.
```

### Measurement (M1 + M3)
- Parse `~/.claude/projects/**/*.jsonl`; assistant lines only; **dedupe by requestId last-wins**
  (verified: streaming rewrites duplicate lines); skip `<synthetic>`.
- Four streams tracked separately: input / output / cache_create (5m vs 1h split) / cache_read.
- Cost is always **estimated** (pricing table data file); tokens are **measured**. Label every
  number in UI as one or the other. Never blend.
- Holdout: default 10% of sessions run with all savers off (or use pre-install history as
  observational baseline). Savings = delta vs holdout, normalized per-turn, with n-counts and
  a "not enough data yet" state below minimum n.
- Attribution: round-robin A/B rotation of saver sets across sessions.

### Install engine (M2)
- Registry entries are declarative data (see `registry/catalog.json`): id, plainLabel, layer,
  installType, source, install/uninstall steps, healthCheck, conflictsWith, ordering,
  behaviorChanging, claimedSavings, risk.
- Merge engine owns writes to `~/.claude/settings.json`: timestamped backup to
  `~/.piggy/backups/` before every write, atomic temp+rename, additive hook merge preserving
  user hooks, `ordering` controls chain position, re-read+re-merge if externally modified.
  **This is the most heavily tested code in the repo.** Real-world fixture: this machine's
  settings.json already has wildcard PreToolUse/PostToolUse/Stop hooks (openbar) — merges must
  keep them byte-perfect.
- Built-in **Sweep** saver (only optimization logic we write): find skills/MCP servers/plugins
  never used in last N sessions (cross-ref logs), estimate their per-session context cost,
  one-click disable with restore. Data sources verified on this machine:
  `~/.claude/settings.json` (hooks/plugins), `~/.claude.json` → `projects.<path>.mcpServers`,
  `~/.claude/plugins/installed_plugins.json`.

### GUI (M4) — Apple-native direction
- Tauri v2 tray app, no dock icon, NSPanel-style popover feel. React + Tailwind.
- **Design language:** macOS native, not web-app. SF Pro system font stack
  (`-apple-system`), translucent vibrancy background (Tauri window effects: `hudWindow` /
  `popover` material), 13px base type, native-feeling toggle switches (44×26 green pills),
  hairline separators (0.5px), SF Symbols-style icons, dark mode default + light support,
  spring animations ≤200ms, no scrollbars visible until scroll.
- Menu bar item: piggy glyph + today's savings % (template image, adapts to menu bar theme).
- Panel layout (360×~560): header (Piggy + today's tokens) → master switch card ("Save
  everything") → saver rows (toggle · plain label · measured badge `measured 22% · 41
  sessions` or `not enough data yet` · behavior-change warning dot) → footer tabs:
  Dashboard / Discovered / Settings.
- Dashboard: headline "Your plan lasts **N.N× longer**" (measured), stream breakdown bars,
  attribution table, sweep recommendations.
- Share card: 1200×630 PNG, dark, big number, "measured with holdout · Piggy" footer,
  Copy/Save buttons. Growth loop — must look great.

## Dependency policy (head-approved)

Beyond the prompt's allowlist (Tauri, React, Tailwind, rusqlite, serde, notify, reqwest):
clap, anyhow, thiserror, chrono, walkdir, dirs, tempfile (dev), sha2 (checksums).
Frontend: zustand or none, no UI kit — hand-rolled Apple-style components.
Rationale: standard, small, audited crates; hand-rolling arg parsing/error types buys nothing.

## Milestone acceptance (from build prompt)

- **M1** daemon: per-session totals match independent jq computation on real files. ✅ = merge.
- **M2** engine: install→verify→uninstall leaves settings.json byte-identical to backup, with
  pre-existing hooks present. ✅ = merge.
- **M3** measurement: dashboard-ready measured deltas with n-counts.
- **M4** GUI: fresh Mac → `npx piggybank` → toggle master switch → run a session → see it counted.
