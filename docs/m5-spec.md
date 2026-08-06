# M5 spec - advisor actions: the advisor suggests, the engine applies (head decisions)

Locked decisions for turning the local advisor from narration into product. Today the
advisor (crates/piggy-core/src/advisor/) re-words findings the deterministic engine already
produced, is forbidden from inventing numbers, has no apply path, and ships dark
(`local-llm` off in release builds). M5 keeps the first two properties, adds the third,
and flips the fourth: feed the local model everything Piggy knows, let it propose and
prioritize actions from a deterministic candidate menu, and let the user apply each one
through the same reversible plumbing savers and Sweep already use.

Core principle, non-negotiable: **the LLM proposes, the engine disposes.** The model never
mutates anything. It ranks, bundles, parameterizes, and explains candidates that pure code
computed, and it authors content only for diff-reviewed file edits. Every number shown
traces to the DB, the probe, or the scanner; guard.rs remains the enforcement point.

## Why now

- `floor-dominates` is a shipped insight, but the two biggest floor components are invisible
  and unactionable in-product: MCP tool-schema manifests (sweep.rs admits its cost figures
  are "a deliberately rough, clearly-labelled heuristic") and CLAUDE.md (zero parsing or
  size tracking anywhere in the repo; the `cto` saver sits blocked on a missing per-project UI).
- Sweep stops at "globally unused"; its re-scope finding stops short of applying.
- The advisor's whole runtime (pinned .gguf download, llama.cpp in-process, guard, tests)
  is built and idle. M5 is the payoff for that investment.

## Ship the advisor (release flip)

- Enable the `local-llm` cargo feature in `app/src-tauri` and `.github/workflows/release.yml`.
  Model download stays opt-in in Settings (AdvisorSettings.tsx flow unchanged).
- Measure and document the .dmg size delta in About. If static llama.cpp adds > 25 MB,
  note it in the release notes; do not gate the flip on it.
- Everything in M5 has a deterministic fallback: advisor off or model absent means the same
  candidates, ranked by estimated savings, with house copy instead of model prose, and no
  CLAUDE.md drafts. No feature disappears; only prioritization and drafting degrade.

## Facts v2 (advisor/facts.rs)

- Expand the payload from allow-listed snippets to the full structured picture: ledger
  buckets, all insights, sweep report, per-project MCP usage matrix (from `session_tools`),
  probe results, CLAUDE.md inventory, saver states with badges and StreamStats, floor
  trend per project, holdout status.
- CLAUDE.md file *contents* enter the payload only for files targeted by a drafting call,
  capped (see LLM pass). Contents are read at call time, never stored in the DB.
- All of it stays on-device. The only network in this milestone is the existing pinned
  model download and the user's own MCP server processes under the probe.

## Manifest probe (new: crates/piggy-core/src/probe.rs)

Turns Sweep's schema-size heuristic into a measured number.

- Opt-in and user-initiated only: never runs from the watcher/daemon. Per-server button in
  the app, `piggy probe` in the CLI.
- v1 scope: stdio servers from `~/.claude.json` (`mcpServers` and `projects.*.mcpServers`).
  Spawn the configured command with its configured env, speak MCP `initialize` +
  `tools/list`, capture the schema JSON. 10 s timeout, no retries, stdout capped at 2 MB,
  env values redacted from all logs and errors.
- Token count: advisor tokenizer when the model is downloaded; otherwise bytes/3.5 labelled
  "estimated". Store per server in `mcp_manifests` keyed by a config hash so a changed
  command/args invalidates the measurement.
- These are commands the user already configured Claude Code to run in every session, so
  the probe adds no new trust grant; the consent gate exists because Piggy still refuses to
  execute anything without an explicit click.
- HTTP/SSE servers: deferred (auth complexity). They keep the heuristic and its label.
- Sweep and all advice evidence prefer probe numbers when present; labels flip from
  "rough estimate" to "measured manifest".

## CLAUDE.md scanner (new: crates/piggy-core/src/claudemd.rs)

- Inventory: `~/.claude/CLAUDE.md`, `~/.claude/rules/*.md`, and for every project in the
  sessions table: `<project>/CLAUDE.md`, `<project>/CLAUDE.local.md`,
  `<project>/.claude/rules/*.md`. Record path, project, bytes, estimated tokens, content
  hash, mtime into `claudemd_files`. Contents are never stored in the DB.
- Read-only awareness of `<project>/.mcp.json` so server advice never calls a
  project-configured server "unused"; apply never targets `.mcp.json` in v1.
- Deterministic detectors (no LLM required, each unit-tested):
  - dead references: mentioned repo paths that no longer exist;
  - duplicate blocks: normalized-paragraph hash match across global and project files;
  - oversize: file > 2,000 estimated tokens (same spirit as the `floor-component` insight);
  - cost: estimated tokens x that project's sessions in the period = tokens/month burden.

## Candidate actions (new: crates/piggy-core/src/advice.rs)

- `ActionKind` v1, five kinds in three families:
  - **Server combinations**: `ServerDisable` (globally unused; today's Sweep),
    `ServerScope` (user-scope server used by few projects: pin it to those projects).
  - **CLAUDE.md cleanup**: `ClaudemdFix` (deterministic dead-reference and exact-duplicate
    removal), `ClaudemdTrim` (LLM-drafted rewrite of an oversized file).
  - **Saver mix**: `SaverMix` (toggle a saver on or off, grounded in attribution: e.g. a
    `behaviorChanging` saver with `NoChange` after >= 30 randomized sessions per side
    proposes off; a Measured saver currently off proposes on; respects `conflictsWith`).
- A candidate is pure data: stable id (hash of kind + target + evidence inputs), kind,
  target, evidence rows (computed numbers with their badge labels), estimated tokens/month,
  risk tier, prerequisites (e.g. ClaudemdTrim requires the model). Generators are pure
  functions over DB + configs; no LLM anywhere in generation.
- Sweep refactor: sweep.rs becomes the `ServerDisable`/`ServerScope` generator; SweepSheet
  and `piggy sweep` read from the advice engine. One source of truth, two entry points.
- Supersedes m2-spec's "will not move config it did not write" for `ServerScope` only:
  moving an entry between user scope and a project entry happens inside the one
  `~/.claude.json` file (secrets never change files), with the exact before-JSON of both
  scopes snapshotted in state.json and a one-click restore. Per-item consent required.

## LLM pass (advisor/mod.rs)

- New `suggest()` beside `annotate()`. Input: facts v2 + the candidate list. Output: JSON
  with ordered candidate ids, optional per-project bundles, a rationale string per pick,
  and for `ClaudemdTrim` targets a drafted replacement.
  - **No GBNF grammar (M5.4 clarification of the line this replaces).** The grammar API
    exists in the pinned llama-cpp-2, but `llama_sampler_sample` accepts the sampled token
    inside C++ (`llama.cpp/src/llama-sampler.cpp:870`) before Rust regains control, and
    that internal accept is not the try/catch-wrapped one the sys crate exposes. So the
    end-of-generation check that would make a grammar safe cannot be placed anywhere
    useful, and `llama-grammar.cpp:1428-1435` remains an uncatchable `GGML_ABORT` that
    takes the menu bar process down. What ships instead: constrained prompting, a strict
    parser, and one bounded retry. The grammar text and the manual sampling protocol that
    would be required are kept in `advisor/prompts.rs` behind `const GRAMMAR: bool = false`.
  - The drafting call returns the file between two sentinels rather than inside JSON: a
    markdown file in a JSON string makes escaping the failure mode rather than the writing.
- Guard v2 (guard.rs + advisor/draft.rs), all deterministic:
  - any candidate id not in the input list is dropped;
  - the existing numbers allow-list applies to rationales; rationale > 280 chars is
    truncated at a whitespace boundary (before the allow-list runs, so a cut cannot invent
    a number). **A rationale carrying a number that is not in facts drops the whole pick**
    rather than stripping the number: a sentence with a numeral surgically removed no
    longer says what it said, and the house copy beside it is a complete answer;
  - drafts must shrink the file >= 10%, introduce no path or URL absent from the source,
    keep headings a subset of the original (merges allowed), and **introduce no number
    absent from the source** (so a rewrite cannot edit "cut this to 2,000 tokens" into
    "3,000" inside the user's own guidance). A failed check demotes the candidate to
    deterministic presentation ("turn on the advisor for a drafted cleanup" / findings list
    only). Nothing invalid ever reaches the UI.
- Budget: runs after indexing goes idle, on a detached worker at the macOS `utility` QoS
  class with half the cores; n_ctx 16384; max 1,024 generated tokens for the rank call.
  Drafting is one call per file, larger files drafted section-by-section by `##` heading.
  - **The drafting input cap is 6k tokens, not 12k (M5.4 correction).** A rewrite is the
    same file again, so a 12,000-token input needs nearly 12,000 tokens of output and
    24,000 does not fit a 16,384-token window at all. The drafting call's token ceiling is
    likewise the source's own length rather than 1,024, which could not emit a whole file.
- Caching: results keyed by facts hash; recompute on facts change or manual "Refresh
  advice". Same facts, same advice. **In memory only, one entry deep**: a draft is derived
  from a CLAUDE.md's contents, and contents never enter the DB (see Facts v2), so an app
  restart legitimately re-runs the pass. What persists is provenance: `advice.facts_hash`
  records which fact sheet the advisor was shown.

## Advice surface (app)

- `AdviceSection` at the top of Spend (Ledger.tsx): top 3 open suggestions by estimated
  tokens/month, each a plain-language claim in the registry `plainLabel` voice
  ("Trim this project's CLAUDE.md", "Pin the github server to 2 projects",
  "Honey has not moved the needle; turn it off"), plus "Review all".
- `AdviceSheet` (SweepSheet pattern): evidence table with badge labels, per-item checkboxes
  for bundles, side-by-side diff for CLAUDEmd drafts, Apply, inline Undo after apply.
  Per-item apply and restore failures surface with reasons (the 7425c66 pattern), never
  silently swallowed.
- Server suggestions are the same engine behind the existing Sweep row in Savers; both
  entry points open the same sheet.
- Lifecycle: `open -> applied | dismissed | stale`. Dismiss ("Not for me") suppresses that
  target until its evidence roughly doubles. A target whose file hash/mtime changed since
  drafting goes stale and is never applyable; re-scan regenerates.
- Advice is pull, not push: no tray badge, no notifications, no auto-refresh nags.

## Apply + restore

- `ServerDisable`: existing `sweep::apply()` path unchanged.
- `ServerScope`: new engine op on `~/.claude.json` through the settings.rs machinery
  (timestamped backup, atomic write, external-change re-merge), snapshots in
  `state.sweep_disabled`-style records.
- `Claudemd*`: generalize `ByteRestore` into a file snapshot store:
  `~/.piggy/backups/files/<sha>-<RFC3339>` + a state.json record
  {path, backup, advice_id, applied_at}. Atomic write; mtime conflict check at apply time;
  `piggy backups` lists these; Restore Defaults restores them; per-item failures reported.
- `SaverMix`: existing `engine::set_enabled()`; the applied advice records the toggle so
  Undo knows the prior state.
- Every apply stamps the advice row `applied` with a `restore_ref`. Undo is one click and
  one IPC call.

## Measurement + honesty (M3 rules unchanged)

- Floor-reducing actions get an observational readout: per-project floor tokens/session
  before vs after apply, with n-counts, badged **Estimated**. Never pooled into the
  Measured headline; promotion rules are untouched by M5.
- Probe figures are labelled "measured manifest, estimated session impact" (schema tokens
  are real; how the client charges them stays an estimate).
- `SaverMix` evidence quotes existing StreamStats verbatim, badge included.
- No number in any advice UI or rationale may originate in the model. Dev fixtures stay
  generated from the real DB (`app/scripts/snapshot-dev-data.mjs`); never hand-author one.

## Data model (store.rs, SCHEMA_VERSION = 8)

- `mcp_manifests(server_key, scope, config_hash, tool_count, schema_bytes, schema_tokens,
  tokenizer, measured_at, ok, error)` PK (server_key, scope).
- `claudemd_files(path PK, project, bytes, est_tokens, hash, mtime_ns, last_scanned)`.
- `advice(id PK, kind, target, created_at, facts_hash, est_tokens_month, status,
  payload_json, applied_at, restore_ref, dismiss_note)`.

## CLI (piggy-cli)

- `piggy advise [--json]` - deterministic candidates with evidence and status. Model
  ranking and drafts are app-only in v1; the CLI says so rather than pretending.
- `piggy probe [--server <key> | --all] [--json]` - `--all` requires `--yes`.
- `piggy claudemd [--json]` - inventory + detector findings.
- `piggy backups` - now includes file snapshots.

## Non-goals (v1)

- No auto-apply of anything, ever, under any setting.
- No cloud LLM, no fallback API, no telemetry. Advice quality is bounded by the local 4B
  models already pinned in download.rs; that is the deal.
- No HTTP/SSE probing; no writes to `.mcp.json`; no authoring new CLAUDE.md guidance
  (trims and fixes only).
- `cto` stays blocked; ClaudemdTrim covers the need first-party (note it in catalog `notes`).
- No new tabs; the five-tab layout stands.

## Acceptance (fresh-install journey)

1. Fresh Mac, release .dmg: advisor section present in Settings (feature no longer dark);
   model download opt-in works offline-tolerant as today.
2. `piggy probe --all --yes` measures every stdio server; Sweep evidence flips to
   "measured manifest".
3. Spend shows at least one suggestion with evidence rows; a ClaudemdTrim opens a diff;
   Apply writes the file; the project's next session shows a lower floor; Undo restores
   byte-identical content; `piggy backups` lists the snapshot.
4. Kill write permission on a target (chmod 000): apply reports that item's failure by
   name, other items in the bundle succeed, nothing is silently lost.
5. Advisor off: same candidates in the same order fallback (estimated savings), house
   copy, no drafts, no dead UI.
6. Same facts hash twice: byte-identical advice list (caching + guard determinism).

## Test fixtures

- `tests/fixtures/claudemd/`: oversized.md (> 2,000 est tokens), dead-refs.md,
  dup-pair/{global,project}.md, bom.md, empty.md.
- `tests/fixtures/mcp/`: `ok-server.mjs` (deterministic tools/list), `slow-server.mjs`
  (exceeds timeout), `garbage-server.mjs` (non-JSON output) - probe must classify all
  three correctly and never hang.
- Guard: draft containing a path/URL absent from source is rejected; candidate id outside
  the allow-list is dropped; rationale number not in facts drops the pick (see LLM pass);
  draft growing the file is rejected.
- Engine: ClaudemdTrim apply then restore leaves the file byte-identical; ServerScope
  apply then restore leaves `~/.claude.json` structurally identical with user edits
  preserved.

## Build plan (agent milestones)

Dependency graph, not a strict sequence; pairs marked parallel run in isolated worktrees.

- **M5.0 foundation** (solo): store.rs v8 (three tables + typed accessors + v7 migration),
  generalize ByteRestore into the file snapshot store, flip `local-llm` in app +
  release.yml. Everything else builds on this.
- **M5.1 probe** (parallel with M5.2): probe.rs, `piggy probe`, the three server fixtures,
  sweep evidence prefers probe numbers when present.
- **M5.2 claudemd** (parallel with M5.1): claudemd.rs inventory + detectors,
  `piggy claudemd`, claudemd fixtures.
- **M5.3 advice engine** (solo, after M5.1 + M5.2): advice.rs kinds + generators, sweep
  refactored behind it, apply/restore for all five kinds, `piggy advise`.
- **M5.4 llm pass** (parallel with M5.5): facts v2, `suggest()`, GBNF grammar, guard v2.
- **M5.5 app surface** (parallel with M5.4): AdviceSection, AdviceSheet, IPC commands,
  probe controls in Settings; builds against deterministic advice, model prose is an
  optional field.
- **M5.6 acceptance** (solo): journey tests, per-item failure test, DESIGN.md M5 line,
  catalog note on `cto`.

## DESIGN.md addition

- **M5** advisor actions: advice reviewed, applied, undone leaves configs byte-identical;
  every number on an advice card traces to DB, probe, or scanner. Checkmark = merge.
