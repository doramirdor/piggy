# M5 spec - advisor actions: the advisor suggests, the engine applies (head decisions)

> **Superseded in part. M5 is built; this is the spec as written, plus what the build
> learned.**
>
> Every section below that the implementation deviated from carries an inline
> `> **Superseded.**` note in the house style `m4-spec.md` uses. Where the two disagree,
> the note and the code win. The notes were accumulated across two adversarial reviews,
> two fix rounds and the M5.4 grammar decision record; each one is a thing the shipped
> build does differently, with the reason it does.
>
> The line below about the advisor shipping dark is the first of them: `local-llm` is on
> in `.github/workflows/release.yml` and in `app/src-tauri/Cargo.toml` as of M5.0, and
> `app/src-tauri/src/advisor.rs` has a test that fails if either stops being true.

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

> **Superseded** as done, with one correction to the last line. The flip landed in M5.0
> (`app/src-tauri/Cargo.toml`, `.github/workflows/release.yml:194`) and a test fails if either
> stops being true. The fallback is real, and it ranks by **estimated tokens a month**, not by
> estimated savings: a `ClaudemdTrim`'s figure is a burden, so a surface calling that ranking a
> savings ranking claims something nobody computed. What degrades with no model is
> prioritization and drafting, exactly as written.

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

> **Superseded** on three points, all of them honesty or safety.
>
> * **A measured manifest is not a tokenized one.** Sweep still flips `cost_basis` to
>   "measured manifest" (the manifest genuinely was measured), but it now carries
>   `tokens_estimated` as its own field, because the count itself is bytes/3.5 until the
>   advisor's tokenizer is loaded. Before the split, `sweep --json` said
>   `estimated: false` while `probe --json` said `estimated: true` for the same row in
>   the same second.
> * **`probe --json` publishes numbers only from a row matching the current config**, and
>   names the config it measured (`measuredConfigHash`). A row can be `ok` and still
>   describe a command that no longer exists.
> * **The probe bounds what a server can make it do**: `MAX_SERVER_REQUESTS` and
>   `MAX_CURSOR_BYTES`, on top of the timeout and the stdout cap. There is deliberately
>   no watchdog thread: it would need the child behind a lock, which undermines the
>   Drop-based guarantee that no process is ever leaked.

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

> **Superseded** on three points.
>
> * **`ClaudemdFile.project` is `Option<String>`, not an empty string.** `None` is a
>   global file, which is a different thing from a project whose name nobody filled in.
> * **Dead-reference DELETION is much narrower than dead-reference DETECTION.** Two
>   confirmed data-loss defects sit behind this. The final rule (`claudemd::deletable_ref`)
>   is: the token must carry a known file extension; a leading `/` is never a reason to
>   delete a line, because a root-anchored token is an HTTP route at least as often as a
>   path (`/openapi.json`, `/sw.js`, `/robots.txt`); and in a global file, whose base
>   falls back to `$HOME`, only a `~/`-anchored reference is deletable, since every
>   unanchored reference in a global file would otherwise be dead by construction.
>   Everything else is still reported, and reporting is allowed to be wrong about a
>   token in a way deleting is not.
> * **The monthly burden counts Claude Code sessions only** (`session_counts_since`).
>   Codex rollouts share the sessions table and never load CLAUDE.md, so counting them
>   inflated every burden figure and invented one outright for a Codex-only project.
>   `session_projects` stays unfiltered, so such a file is still inventoried, at zero.

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

> **Superseded** on what each kind covers and on what a candidate may carry.
>
> * **`ServerDisable` covers plugins and skills, not only MCP servers.** Sweep's
>   recommendations always included all three, and splitting them would have left the
>   sheet describing a third of what Sweep found.
> * **`ClaudemdTrim.est_tokens_month` is the file's monthly BURDEN, not a promised
>   saving.** How much a rewrite removes is not known until it is drafted. The CLI
>   headline never sums burdens with savings (roughly a 10x overstatement in the shape a
>   reader is most likely to believe), `--json` carries them as separate fields, and the
>   app renders `figureKind` rather than guessing.
> * **`Params::ServerScope` carries no `config`.** It was serializing the moved entry,
>   whose `env` is where people keep API tokens, into `advice.payload_json` for every
>   candidate `generate` produced: out of a 0600 `~/.claude.json` and into a 0644
>   `~/.piggy/piggy.db`, with no apply, no consent and no expiry. Apply re-reads the
>   entry from the file it is moving it inside, and checks it against the fingerprint.
> * **`ServerScope` folds project paths for the DECISION and writes to the raw session
>   keys.** `~/.claude.json` is keyed by exact cwd, so folding the write lost the server
>   in the subdirectories that had made half the calls.
> * **`SaverMix` never proposes turning off a saver whose layer is routing or proxy.**
>   Flat token streams are the documented expected outcome there, and Piggy has no cost
>   arm to judge one by.
> * **`stale` is not terminal.** A candidate that regenerates is live again: evidence
>   values oscillate as session windows move, and a retired id could otherwise never be
>   applied.
> * **`dismiss()` refuses an APPLIED row.** It was nulling `restore_ref`, which for
>   `SaverMix` is the only record of the user's prior toggle state.

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

> **Superseded** on where the results live and on one more guard rule.
>
> * **The advice overlay is memory only**, one entry deep, and there is no
>   `SCHEMA_VERSION 9`. Drafts derive from CLAUDE.md contents, which this spec forbids
>   from reaching the database, so an app restart legitimately re-runs the pass. What
>   persists is provenance: `advice.facts_hash` records which fact sheet the advisor was
>   shown, and it is stamped only once a model has actually read one.
> * **The draft guard also rejects any number in a draft that is absent from the source
>   file**, so a rewrite cannot silently edit "cut this to 2,000 tokens" in the user's
>   own guidance into "3,000".
> * The guard's numbers allow-list harvests every number reachable in the facts payload.
>   Candidate ids are hex, so their digit runs enter the allow-list too. Judged and left:
>   single digits are already admitted by tool counts and risk tiers, so the widening is
>   marginal. `advisor/guard.rs` carries the note.
> * **ClaudemdTrim ships as a burden report in v1.** Drafting is best effort and gated on
>   a quality bar the pinned 4B usually will not clear: run live against a real oversized
>   CLAUDE.md the model cut 3.7%, and `accept_draft`'s 10% shrink rule refused it. The
>   rule stays. A "trim" that removes 3.7% of a file is not worth asking someone to review
>   a diff for, and lowering a guard so a demo succeeds is the failure mode this milestone
>   exists to fight: the guard doing its job is the feature working. This is the honest
>   version of "advice quality is bounded by the local 4B models already pinned".

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

> **Superseded** on what a card may claim, and on what the app is made of.
>
> * **A `ClaudemdTrim` card distinguishes three states and never claims the wrong one.**
>   One string ("Turn on the local advisor in Settings for a drafted rewrite") covered all
>   of them, and it was a false statement to the user it mattered most to: someone who had
>   already switched the advisor on and whose draft the guard had refused was told to
>   switch the advisor on. The DTO carries `draftState`
>   (`unavailable | pending | refused | ready`, from `advisor::DraftState`) and the app
>   writes the sentence from it, the way it writes the figure line from `figureKind`. The
>   burden figure stays on the card in all three states: knowing a file costs ~135k tokens
>   a month is the insight, and it is honest with no draft behind it.
> * **`advice_report` goes through `advisor::advice_sheet`.** M5.4 and M5.5 were built in
>   parallel and the join was never made: the command surface called
>   `advice::generate` directly, so no advice pass ever started, no draft ever reached a
>   card, and no ranking ever left the model. Fixed in M5.6, along with the other end of the
>   loop: a landed pass emits `advice://updated` (its own channel, not `stats-updated`,
>   which fires on the watcher's 400ms debounce), and only when something landed, so a model
>   that cannot load does not spin on its own failure. Still pull, not push: nothing here
>   badges a tray or raises a notification.
> * **The list says how it was ordered.** `advisorRanked` is true only when a finished pass
>   supplied picks the guard accepted, and the surfaces read "ranked by estimated tokens a
>   month" or "ordered by the local advisor" from it. The old wording, "ranked by estimated
>   saving", was wrong either way: the sort includes burdens.
> * **`app/src/screens/Overview.tsx` and `SweepSheet.tsx` are deleted.** Both were
>   imported nowhere once `AdviceSheet` replaced them.
> * **`app/src-tauri` takes a direct `libc` dependency** for a real macOS QoS class on the
>   advisor thread. Already in `Cargo.lock` transitively, so it compiles nothing new.

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

> **Superseded** on what undo is allowed to overwrite, and on one thing deliberately
> left as a stopgap.
>
> * **Restore snapshots the CURRENT bytes before overwriting them**, so undo cannot
>   silently destroy edits the user made since the apply. Undo is never refused for that
>   reason; the copy is kept, and `piggy backups` lists it under its own heading as
>   something nothing puts back.
> * **Out-of-order undo across two edits to one file IS refused**, naming the later edit,
>   so an orphan record can never be written back by a later Restore Defaults.
> * **`state.file_snapshots` conflates two opposite kinds of record**, and the guard
>   against that is a filter rather than a type. Snapshots of files Piggy EDITED
>   (`advice_id` Some, which undo and Restore Defaults must write back) live in the same
>   list as safety backups of the USER's bytes taken just before a restore (`advice_id`
>   None, which nothing may ever write back). Three separate confirmed defects came out of
>   that single conflation, each revealed by fixing the last: undo silently destroying
>   edits made after an apply (fixed by taking the safety backup at all); out-of-order
>   undo needing to hand-ignore id-less records (fixed by scoping the guard); and Restore
>   Defaults re-applying Piggy's edits, losing idempotency entirely, because it restored
>   the safety backups (fixed by filtering on `advice_id.is_some()`).
>
>   The structural fix is to give the safety backups their own field, so they are
>   incapable of being restore targets by TYPE rather than by filter, making the invalid
>   state unrepresentable. Deliberately NOT done at closeout: refactoring the undo path
>   under time pressure, in the exact subsystem that has produced a defect per change,
>   would trade a known-tested state for an unknown one. Filed as a follow-up. The next
>   person should know the guard is a stopgap and why it was left as one.

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

> **Superseded** by one addition: `Store::ledger_between(since, until)`, so the floor
> trend compares two adjacent windows rather than a 7-day window nested inside a 30-day
> one, which damps the very change it is there to show. Eight is still the schema
> version: nothing the advisor produces is persisted (see the LLM pass note).

- `mcp_manifests(server_key, scope, config_hash, tool_count, schema_bytes, schema_tokens,
  tokenizer, measured_at, ok, error)` PK (server_key, scope).
- `claudemd_files(path PK, project, bytes, est_tokens, hash, mtime_ns, last_scanned)`.
- `advice(id PK, kind, target, created_at, facts_hash, est_tokens_month, status,
  payload_json, applied_at, restore_ref, dismiss_note)`.

## CLI (piggy-cli)

> **Superseded** by one added flag and one split figure.
>
> * **`piggy advise --json --diff`** exists so the app's dev fixture can be GENERATED
>   from the real database (hand-authored fixtures are forbidden). It prints deterministic
>   claudemd-fix diffs only, never a drafted file, and it says in its own help that it
>   prints lines of your own CLAUDE.md.
> * **`piggy advise` reports savings and burden as two figures**, never one total. See the
>   Candidate actions note.

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

> **Superseded** by where each of these is actually checked, as of M5.6. One of them is a
> person with a Mac, and it is written down as one rather than approximated in a test.
>
> 1. `app/src-tauri/src/advisor.rs`
>    (`the_shipped_bundle_compiles_the_advisor_in_and_the_test_path_does_not`,
>    `a_default_build_cannot_run_a_model`). The fresh-Mac half (download the .dmg, opt
>    into a model, watch it work offline-tolerant) is **manual**, and is step 6 of the
>    release checklist in `docs/releasing.md`.
> 2. `crates/piggy-cli/tests/acceptance_tests.rs`
>    (`probe_all_measures_every_stdio_server_and_sweep_reads_the_measurement`,
>    `probe_all_refuses_to_launch_anything_without_the_consent_flag`).
> 3. `crates/piggy-cli/tests/acceptance_tests.rs`
>    (`the_journey_from_a_suggestion_to_a_byte_identical_undo`,
>    `a_lower_floor_after_the_edit_is_reported_and_never_pooled_into_the_headline`), with
>    the apply/undo halves also in `advice_tests.rs`. The diff-then-apply half of the
>    journey is exercised on a `ClaudemdFix`: a `ClaudemdTrim` cannot be applied in a test
>    build, because there is no model in it and the guard is not lowered to pretend
>    otherwise (see the LLM pass note).
> 4. `crates/piggy-core/tests/advice_tests.rs`
>    (`one_unwritable_target_in_a_bundle_fails_by_name_and_the_others_still_apply` for the
>    apply half, `restore_defaults_puts_edited_files_back_and_names_the_one_it_could_not`
>    for the restore half).
> 5. `crates/piggy-core/tests/advice_tests.rs`
>    (`with_no_advisor_every_candidate_still_carries_its_own_copy_and_evidence`) and
>    `app/src/lib/advice.test.ts` ("the advice surfaces with no model in the build").
> 6. `crates/piggy-core/tests/advice_llm_tests.rs`
>    (`the_same_facts_produce_a_byte_identical_advice_list`,
>    `the_guard_refuses_the_same_things_every_time`, and the cache-key tests).

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

> **Superseded** by two additions. `tests/fixtures/mcp/` has a fourth server,
> `flood-server.mjs`, which bursts server-to-client requests and never reads stdin: it is what
> `MAX_SERVER_REQUESTS` is tested against. `tests/fixtures/claudemd/` has `routes.md`, which is
> the root-anchored-token case the deletion rule refuses to touch. The fixtures are node
> scripts, so a machine with no `node` on PATH skips those tests, loudly.

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

> **Superseded** in one line: M5.4 shipped constrained prompting, a strict parser and one
> bounded retry, **not** a GBNF grammar. The reasons are in the LLM pass section above.

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
