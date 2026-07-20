# M4 spec - Tauri menu bar app (head decisions)

> **Historical. Superseded in part; kept for the decisions it records.**
>
> This is the M4 spec as written, when Piggy was a tray-only popover. The shipped app is a
> normal desktop window instead (940×660, resizable, decorated, Dock icon, six tabs). See
> **DESIGN.md**, the "GUI (M4)" section on the Apple-native desktop window, for the current
> design; where the two disagree, DESIGN.md and the code win.
>
> Still accurate and still binding: the command surface, background/daemon behavior, share
> card, empty/degraded states, installer, and icons. The **Shell** and **Screens** sections
> below are the ones the redesign overtook; each carries a correction inline.

Design reference: `docs/mockups/panel.html` (panel), `docs/mockups/sharecard.html`
(share card), `docs/mockups/icon.svg` (icon). The icon and share card still bind. The panel
mockup is the superseded popover layout - the sidebar layout replaced it. Vocabulary rules
from DESIGN.md apply everywhere (savers, never hooks).

## Shell

> **Superseded.** Shipped: `ActivationPolicy::Regular` (`app/src-tauri/src/lib.rs`), a Dock
> icon, and a 940×660 resizable window with decorations and a macOS Overlay title bar
> (`tauri.conf.json`). No HudWindow vibrancy, no hide-on-blur: closing the window hides it and
> the tray glyph shows it again, while the background indexer keeps running. The rest of this
> section (frontend stack, piggy-core linkage) is unchanged.

- Tauri v2, tray-only app (`ActivationPolicy::Accessory` - no dock icon).
- Tray icon: monochrome template piggy glyph + optional short text (today's saved %) via
  tray title. Left-click toggles a popover-style window anchored to the tray icon
  (`tauri-plugin-positioner`, tray-center). Window: 360×600, no decorations, rounded 14px,
  transparent + `NSVisualEffectMaterial::HudWindow` vibrancy (window-vibrancy crate),
  hides on blur (focus loss), Esc hides.
- Frontend: React 18 + Tailwind v4 + Vite. Dark default; respects system light mode (the
  mockup palette has CSS-var equivalents both ways). No UI kit; components hand-rolled to
  match mockup. Zustand for state. No localStorage - all state via Tauri commands.
- The Rust side links `piggy-core` directly (workspace member `app/src-tauri`).

## Background behavior (the daemon lives here)

- On app start: spawn watcher (piggy-core, notify) on `~/.claude/projects` → new/changed
  JSONL triggers incremental index (debounced 2s) + session-saver snapshot + rotation step
  (M3 API). Emit `piggy://stats-updated` event to frontend.
- Menu bar title refresh on the same event. Frontend re-queries on window-show + on event.
- Launch-at-login toggle in Settings (tauri-plugin-autostart).

## Tauri command surface (all return serde JSON; thin wrappers over piggy-core)

- `stats_overview(period)` → totals per stream + est cost + headline multiplier {value|null,
  label: "measured"|"estimated"|"not_enough_data", n_holdout}
- `savers_list()` → registry entries joined with install state + per-saver badge
  {kind: measured|measuring|claimed, delta, n}. Rows carry `launchCommand` for wrapper-model
  savers (the command that starts a session through the saver, e.g. Headroom's `piggy-claude`;
  null otherwise).
- `saver_toggle(id, on)` / `master_toggle(on)` → engine install/enable/disable; returns new
  state or plain-language error {title, detail, rolledBack: bool}. On success the returned state
  may carry a one-line `notice` (conflict auto-disable, and the launch instruction for
  wrapper-model savers) that the UI shows as an info banner.
- `sweep_report()` / `sweep_apply(item_ids)` / `sweep_restore(item_ids)`
- `discovered_list()` → discovery module results (cached, refresh ≤1/day)
- `share_card_data(period)` → numbers for the card; `save_share_card(png_bytes)` → writes
  ~/Desktop/piggy-savings.png + reveals in Finder; copy handled in JS via clipboard API.
- `settings_get()/settings_set()` → holdout fraction, launch at login, rotation on/off
- `restore_defaults()` - confirmation UI first ("puts Claude's settings back exactly as
  before Piggy"), then engine call.
- `doctor()` → checks for Settings > Health section.

## Screens (tabs per mockup: Home, Dashboard, Discover, Settings)

> **Superseded.** Shipped tabs are six, not four: `overview` ("Dashboard"), `savers`,
> `discover` ("Discovery"), `proof`, `reports`, `settings`. The old "Home" split into the
> Overview screen and the Savers screen; "Dashboard" here is roughly today's Proof screen;
> Reports is new. The per-screen content below still describes what each surface owes the
> user, and the Settings line about "never phones home" has since been corrected: Piggy's own
> calls are GitHub-only, but a saver's installer fetches from its own home.

- **Home** = mockup panel exactly: master card, saver rows (toggle, plain label, badge,
  amber warning dot with popover text for behaviorChanging), sweep hint card, headline strip.
- **Dashboard**: big multiplier headline + "measured against N holdout sessions" line,
  4-stream breakdown bars, per-saver attribution table (delta, CI, n, latency if present),
  period picker (7d/30d/all), Share button → share sheet modal with card preview + Copy PNG /
  Save buttons.
- **Discover**: discovery feed rows (name, stars, one-liner, "author claims X" gray label,
  View on GitHub link). Listed-only entries (token-optimizer-mcp) show their exclusionReason
  in plain language. Nothing here is installable.
- **Settings**: holdout %, rotation toggle, launch at login, Restore Defaults (destructive
  style), Health (doctor output), version + "Piggy never phones home" line.

## Share card

- Rendered in-app on a hidden `<canvas>` at 1200×630@2x by drawing directly (no html2canvas
  dep): bg gradients + grain, texts per sharecard.html. Copy = clipboard PNG; Save = command
  above. Numbers only from `share_card_data`; if headline is estimated, the card must say
  "estimated"; if not enough data, share button disabled with "measuring" tooltip.

## Empty/degraded states (non-developers - every state must say what to do)

- No Claude Code found → friendly setup card ("Piggy needs Claude Code - install it first").
- Fresh install, no data yet → "Piggy is reading your history…" progress, then first stats.
- No holdout data yet → headline shows "measuring… N of 10 sessions" instead of multiplier.
- Engine error → plain sentence + "Everything was rolled back" when true. Never show JSON.

## npx installer (`installer/` dir, npm package `piggybank`)

- Tiny Node script: detect arch, download latest .dmg from GitHub releases (URL from
  package.json config), verify sha256 from checksums file, open the dmg. Graceful message if
  releases don't exist yet (pre-release repo state). No deps beyond node stdlib.

## Icons / assets

- `docs/mockups/icon.svg` → rasterize 1024 PNG → `tauri icon` pipeline for .icns.
- Tray template icon: simplified pig-head glyph, black w/ alpha, 22×22@1x/44×44@2x PNG,
  `is_template=true` so macOS tints it.

## Acceptance (from build prompt M4)

Fresh Mac path: `npx @amirdor/piggybank` → app opens → master toggle → run one Claude Code session →
panel shows it counted. Plus: `npm run tauri build` produces a .app; `npm run tauri dev`
works for development; unit tests for share-card data mapping and badge-state logic.
