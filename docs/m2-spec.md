# M2 spec - registry, install engine, config merge (head decisions)

Locked decisions for the install engine. The catalog lives in `registry/catalog.json`
(already authored, live-verified 2026-07-12 - see docs/research/optimizers.md).

## Ownership model

- Piggy state lives in `~/.piggy/state.json`: installed savers {id, version, installed_at,
  enabled}, the **exact hook JSON objects Piggy injected** (per saver), sweep-disabled items
  with their original config snippets, backup ledger, content hash of settings.json after our
  last write.
- "Piggy-owned" hook entries are identified by **structural equality against state.json
  records**, never by guessing. Removal deletes only exact matches; user hooks (e.g. this
  machine's openbar wildcard hooks) are untouchable.

## Merge engine (crates/piggy-core::config)

1. Every write: read current bytes → timestamped backup `~/.piggy/backups/settings-<RFC3339>.json`
   (plus first-ever backup preserved as `pre-piggy.json`, the Restore Defaults target; keep last 50 others).
2. Atomic write: temp file in same dir, fsync, rename; preserve permissions.
3. External-change detection: compare stored content hash before writing; on mismatch,
   re-read and re-merge additions onto the *current* content. Never clobber.
4. Serialization: 2-space indent (matches Claude Code's own writer) to minimize diff noise.
5. **Byte-identical uninstall**: after structural removal, if resulting structure ==
   pre-install backup structure, write the backup's exact bytes. If user made unrelated edits
   since, keep them (structural removal only). Document this in the uninstall result.
6. Hook merge is additive, ordered by registry `ordering` among Piggy-owned entries;
   pre-existing user hooks always stay first in their arrays.
7. Handle: missing file (create minimal), empty file, BOM (strip + warn - real bug seen in
   the wild from token-optimizer-mcp), trailing newline preservation, unknown top-level keys
   (preserve verbatim - settings.json evolves).

## Install step DSL (interpret registry/catalog.json steps)

- `download_release_asset` - GitHub release, arch-mapped asset, sha256 verified from the
  release's checksum file (fetched from same tag). reqwest, no redirects to non-github hosts.
- `extract_binary` → `~/.piggy/bin/`, chmod 755.
- `merge_hooks` - via merge engine; record injected objects in state.json.
- `claude_cli` - run `claude <args>` non-interactively (locate binary: `which claude`,
  fallback known paths). Backup settings.json before AND after (plugin installs write to it).
  If `claude` CLI absent → saver shows "needs Claude Code CLI" and install is refused cleanly.
- `require_binary` (soft) - warn-only gate.
- `run_plugin_script`, `verify_no_setting`, `remove_hooks`, `delete_file`, `builtin_enable/disable`.
- Unknown step in catalog → refuse install of that saver ("catalog newer than app"), never guess.

## Toggle semantics (fast path for A/B rotation)

- OFF ≠ uninstall. Hook savers: remove owned hooks (binary stays). Plugin savers:
  `claude plugin disable <p>@<m>` (stays installed). ON re-adds/enables. Rotation (M3) uses
  toggles only, so it's cheap and safe.

## Health checks + fail-safe

- `binary_runs`, `hook_present`, `plugin_enabled` (parse `claude plugin list` or settings
  enabledPlugins). Run post-install; ANY failure → automatic rollback to the pre-install
  backup + plain-language error. Also run on `piggy doctor`.

## Sweep module (built-in)

- Data sources: `~/.claude/settings.json` (enabledPlugins, hooks), `~/.claude.json`
  `projects.*.mcpServers`, `~/.claude/plugins/installed_plugins.json`, `~/.claude/skills/`.
- Usage cross-ref from session DB: MCP tool invocations appear in logs as `mcp__<server>__*`
  tool_use names; skills as Skill tool invocations - count per source over last N=50 sessions.
- Unused item → offer disable: MCP server: remove entry but snapshot the exact JSON in
  state.json for restore; plugin: enabledPlugins=false; skill: move dir to
  `~/.piggy/disabled/skills/` (restore = move back). Cost numbers are **estimated** (schema/
  description size heuristic) and always labeled so.
- v1 scans and reports via `piggy sweep`; disable/restore via `piggy sweep --apply <n>` and GUI later.

## CLI additions (piggy-cli)

`piggy install|remove|on|off <saver>`, `piggy list` (with measured/claimed labels),
`piggy sweep [--apply N]`, `piggy restore-defaults`, `piggy backups`.

## Test fixtures (tests/fixtures/settings/) - the hardest-tested code in the repo

- `openbar.json` - replica of this machine's real settings.json (wildcard matchers on 6 events).
- `empty.json`, `missing` (no file), `bom.json`, `minimal.json` (`{}`),
- `already-has-rtk.json` (user manually installed rtk before Piggy),
- `unknown-keys.json` (future settings fields must round-trip verbatim),
- `hostile.json` (duplicate-looking hooks, deep nesting, unicode).
- Acceptance test: install rtk → health-check → uninstall → **byte-identical** to backup, on
  the openbar fixture. Property-style loop over all fixtures: merge∘remove == identity.
