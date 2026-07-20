# Piggy - Design Document

*The token piggy bank for Claude Code.*

## Name & brand

**Piggy** 🐷, a menu bar piggy bank that makes your Claude plan last longer.

- Why: the ICP (vibe coders on Claude Code / Codex, mostly non-developers) doesn't think in
  "optimizers" or "hooks". They think *"my plan ran out again."* Piggy = savings, instantly.
  Warm, tweetable ("Piggy saved me 1.2M tokens this week 🐷"), provider-neutral with no rename
  needed: the indexer already reads Codex rollout logs from `~/.codex` (read-only) alongside
  Claude Code's, and pricing covers `gpt-*`. The savers themselves are still Claude Code only.
- CLI binary: `piggy` · data dir: `~/.piggy/` · db: `~/.piggy/piggy.db`
- npm installer: `npx @amirdor/piggybank` (verified available) · repo working dir: this one.
- Tagline: **"Your Claude plan, but longer."** Sub: *The App Store - and the referee - for
  Claude Code token savers.*

## Positioning rules (non-negotiable, from build prompt)

1. Never vendor/fork optimizer code: install from the author's official source (GitHub release
   artifacts, PyPI, the Claude plugin marketplace) and pin versions. Verify checksums where the
   source offers them - today that is the GitHub release path only (`rtk`), since pip installs
   pass no hashes.
2. Never blend measured and claimed numbers. Dashboard = measured only; README claims appear
   only on install cards labeled "claimed".
3. Everything reversible; Restore Defaults always works.
4. No telemetry, no accounts; usage data never leaves the Mac. *Piggy's own* network calls are
   GitHub only (releases, discovery; registry refresh is designed but not built - the catalog is
   embedded at build time). Turning a saver on runs that saver's official installer, which
   fetches from its own home: PyPI for headroom, the plugin marketplace (via the user's `claude`
   binary) for the plugin savers. Say this plainly in docs and UI rather than claiming
   GitHub-only end to end - the claim has to survive a packet capture.
5. Fail safe: failed health check ⇒ auto-rollback + plain-English error.
6. The user never sees a terminal, a JSON file, or the word "hook". UI vocabulary:
   optimizer = **"saver"**, install = **"turn on"**, hook chain = invisible, settings.json = **"Claude's settings (backed up)"**.

## Architecture

```
crates/
  piggy-core     # parser, sqlite store, pricing, stats, registry, merge engine, holdout
  piggy-cli      # `piggy` binary: index | stats | doctor | parse | list | install | remove
                 #   | on | off | sweep | report | holdout | discover | watch | backups
                 #   | restore-defaults
app/             # Tauri v2 menu bar app (React + Tailwind), links piggy-core
registry/        # versioned JSON catalog (data, not code)
docs/research/   # live-verified research on optimizer repos
scripts/         # verify-against-jq.sh etc.
```

### Measurement (M1 + M3)
- Parse `~/.claude/projects/**/*.jsonl`; assistant lines only; **dedupe by requestId last-wins**
  (verified: streaming rewrites duplicate lines); skip `<synthetic>`.
- Codex is a second source, same store: `~/.codex/{sessions,archived_sessions}` rollout logs
  (`sources.rs` + `codex.rs`), read-only, skipped when the dir is absent.
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
  settings.json already has wildcard PreToolUse/PostToolUse/Stop hooks (openbar). Merges must
  keep them byte-perfect.
- Built-in **Sweep** saver (only optimization logic we write): find skills/MCP servers/plugins
  never used in last N sessions (cross-ref logs), estimate their per-session context cost,
  one-click disable with restore. Data sources verified on this machine:
  `~/.claude/settings.json` (hooks/plugins), `~/.claude.json` → `projects.<path>.mcpServers`,
  `~/.claude/plugins/installed_plugins.json`.

### GUI (M4) - Apple-native desktop window
- Tauri v2 **desktop window** (940×660, resizable, macOS Overlay title bar, Dock icon) with a
  companion menu-bar tray glyph that shows/hides it. Closing the window hides it and keeps the
  background daemon running. React + Tailwind. *(Superseded the original 360×600 tray popover.
  See the app icon + product mockups; the sidebar layout carries the brand.)*
- **Design language:** macOS native, not web-app. SF Pro system font stack (`-apple-system`),
  solid dark surface (`--bg #0f151b`) with the blueprint/brand personality in the piggy mark and
  hero cards, native-feeling toggle switches, hairline separators (0.5px), SF-Symbols-style line
  icons, dark mode default + light support, spring animations ≤200ms, no scrollbars until scroll.
- Layout: left **sidebar** (Piggy mark + wordmark → six nav tabs → master switch + version) and
  a scrolling **content** column (max-width ~720). Tab ids `overview | savers | discover | proof
  | reports | settings`; two render under a different label, `overview` as "Dashboard" and
  `discover` as "Discovery".
- **Overview:** greeting + "Your plan lasts **N.N× longer**" hero (measured, radial-green glow +
  stream bars) → Tokens-saved / Money-avoided metric grid → sweep hint → recent-proof feed.
- **Savers:** master "Save everything" card → saver rows (icon · plain label · measured/estimated
  badge · behavior-change warn dot · toggle).
- **Proof:** period picker → hero → totals/cost metric grid → per-saver attribution → Share.
- **Discover:** two-column card grid; author claims labelled, never Piggy's measurements.
- Share card: 2400×1260 PNG, dark, vector piggy mark + big number, "measured with holdout · Piggy"
  footer, Copy/Save buttons. Growth loop - must look great.

## Dependency policy (head-approved)

Beyond the prompt's allowlist (Tauri, React, Tailwind, rusqlite, serde, notify, reqwest):
clap, anyhow, thiserror, chrono, walkdir, dirs, tempfile (dev), sha2 (checksums).
Frontend: zustand or none, no UI kit - hand-rolled Apple-style components.
Rationale: standard, small, audited crates; hand-rolling arg parsing/error types buys nothing.

## Milestone acceptance (from build prompt)

- **M1** daemon: per-session totals match independent jq computation on real files. ✅ = merge.
- **M2** engine: install→verify→uninstall leaves settings.json byte-identical to backup, with
  pre-existing hooks present. ✅ = merge.
- **M3** measurement: dashboard-ready measured deltas with n-counts.
- **M4** GUI: fresh Mac → `npx @amirdor/piggybank` → toggle master switch → run a session → see it counted.
