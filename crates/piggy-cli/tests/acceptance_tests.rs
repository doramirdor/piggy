//! The M5 acceptance journeys, driven through the compiled `piggy` binary.
//!
//! `docs/m5-spec.md` states its acceptance as things a person does on a fresh
//! Mac, and most of it is reachable from the CLI. What is not is named as a
//! manual step in `docs/releasing.md` rather than approximated here.
//!
//! Everything runs inside [`common::Sandbox`], which points every one of Piggy's
//! path overrides at a temp dir and asserts that none of them escaped. Read the
//! module docs there before adding a test: the advice engine writes to
//! `~/.claude.json`, and a partly sandboxed test edits the developer's own MCP
//! configuration.
//!
//! Criterion coverage, so the map is in the code rather than in a report:
//!
//! * 1 release ships the advisor: `app/src-tauri/src/advisor.rs`
//!   (`the_shipped_bundle_compiles_the_advisor_in_and_the_test_path_does_not`),
//!   plus the fresh-Mac half as a checklist item in `docs/releasing.md`.
//! * 2 probe measures every stdio server, sweep evidence flips: here.
//! * 3 the full journey: here, and the apply/undo halves in
//!   `piggy-core/tests/advice_tests.rs`.
//! * 4 per-item failure: `piggy-core/tests/advice_tests.rs`.
//! * 5 advisor off is a complete product: `piggy-core/tests/advice_tests.rs`
//!   and `app/src/lib/advice.test.ts`.
//! * 6 determinism: `piggy-core/tests/advice_llm_tests.rs`.

mod common;

use std::path::Path;

use common::{mcp_fixture, node_bin, Sandbox};
use piggy_core::advice::{self, ActionKind, GenerateOptions};
use piggy_core::store::advice_status;
use piggy_core::{sweep, Catalog, PiggyState, Pricing};
use serde_json::{json, Value};

/// Resolve `node` or leave the test, saying so.
macro_rules! node_or_skip {
    ($name:expr) => {
        match node_bin() {
            Some(n) => n,
            None => {
                println!("SKIP {}: no `node` on PATH, and the MCP fixtures are node scripts", $name);
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// The harness itself
// ---------------------------------------------------------------------------

/// The harness is only worth having if it is true, so it is checked against the
/// binary's own idea of where things are rather than against the table that set
/// it. `piggy doctor` prints the three paths it touches; all three must be
/// inside the sandbox.
#[test]
fn every_path_the_binary_resolves_stays_inside_the_sandbox() {
    let sb = Sandbox::new();
    // Not `run`: doctor exits non-zero when a check fails, and in an empty
    // sandbox some of them do. The output is what is being read here.
    let out = sb.output(&["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let root = sb.root().display().to_string();

    let mut seen = 0;
    for line in text.lines() {
        // Every doctor line that names a path names it after a colon.
        let Some((_, tail)) = line.split_once(": ") else {
            continue;
        };
        let path = tail.split(" (").next().unwrap_or(tail).trim();
        if !path.starts_with('/') {
            continue;
        }
        seen += 1;
        assert!(
            path.starts_with(&root),
            "the binary resolved a path outside the sandbox: {path}\n{text}"
        );
    }
    assert!(seen >= 3, "doctor named no paths, so nothing was checked:\n{text}");
}

// ---------------------------------------------------------------------------
// Criterion 2: `piggy probe --all --yes` measures every stdio server, and the
// sweep evidence flips to the measurement.
// ---------------------------------------------------------------------------

/// Probing launches programs. Piggy will not do that on its own say-so, even
/// though these are programs Claude Code already starts every session.
#[test]
fn probe_all_refuses_to_launch_anything_without_the_consent_flag() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({
        "mcpServers": { "ok": { "command": "/bin/echo", "args": ["hi"] } }
    }));
    let out = sb.output(&["probe", "--all"]);
    assert!(!out.status.success(), "`probe --all` must refuse without --yes");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--yes"), "the refusal has to say what to do: {err}");
    // And nothing was measured.
    let listed = sb.json(&["probe", "--json"]);
    assert_eq!(listed["probed"], json!(false));
    assert_eq!(listed["servers"][0]["status"], json!("never"));
}

#[test]
fn probe_all_measures_every_stdio_server_and_sweep_reads_the_measurement() {
    let node = node_or_skip!("probe_all_measures_every_stdio_server_and_sweep_reads_the_measurement");
    let sb = Sandbox::new();
    let ok = mcp_fixture("ok-server.mjs").display().to_string();
    let garbage = mcp_fixture("garbage-server.mjs").display().to_string();
    let project = sb.project("app").display().to_string();

    // A user-scope server, the same server in a project scope, one that answers
    // with rubbish, and one remote. The slow server is deliberately absent: its
    // timeout is covered in `probe_tests.rs` and paying ten seconds for it here
    // would buy nothing.
    sb.write_claude_json(&json!({
        "mcpServers": {
            "docs": { "command": node, "args": [ok, "user"] },
            "broken": { "command": node, "args": [garbage] },
            "remote": { "type": "http", "url": "https://example.com/mcp" }
        },
        "projects": {
            project: { "mcpServers": { "scoped": { "command": node, "args": [ok, "project"] } } }
        }
    }));

    let report = sb.json(&["probe", "--all", "--yes", "--json"]);
    assert_eq!(report["probed"], json!(true));
    let servers = report["servers"].as_array().expect("a servers array");
    assert_eq!(servers.len(), 4, "every configured server is reported: {servers:#?}");

    let by_key = |key: &str| -> Value {
        servers
            .iter()
            .find(|s| s["server"] == json!(key))
            .unwrap_or_else(|| panic!("no row for {key}"))
            .clone()
    };

    // Every stdio server was measured, in both scopes.
    for key in ["docs", "scoped"] {
        let row = by_key(key);
        assert_eq!(row["status"], json!("measured"), "{key}: {row:#?}");
        assert!(row["toolCount"].as_i64().unwrap_or(0) > 0, "{key} has tools");
        assert!(row["schemaBytes"].as_i64().unwrap_or(0) > 0, "{key} has bytes");
        // The row says which configuration it describes, not just that it
        // measured something once.
        assert_eq!(row["measuredConfigHash"], row["configHash"], "{key}");
        // THE regression the foundation review caught. The schema bytes are
        // real; bytes/3.5 is not a token count, and the payload must keep
        // saying so for as long as that is the tokenizer.
        assert_eq!(
            row["estimated"],
            json!(true),
            "{key} publishes a bytes/3.5 count as if it were tokenized"
        );
        assert_eq!(row["tokenizer"], json!(piggy_core::probe::TOKENIZER_BYTES_ESTIMATE));
    }

    // A server that answers with rubbish is a stored failure, not a hang and
    // not a silent gap.
    let broken = by_key("broken");
    assert_eq!(broken["status"], json!("failed"), "{broken:#?}");
    assert!(broken["error"].is_string(), "a failure keeps its reason");
    assert!(broken["toolCount"].is_null(), "a failed row publishes no numbers");

    // Remote transports are deferred in v1 and keep the heuristic.
    let remote = by_key("remote");
    assert_eq!(remote["status"], json!("deferred"));
    assert!(remote["schemaTokens"].is_null());

    // And the sweep's evidence now rests on the measurement.
    let swept = sb.json(&["sweep", "--json"]);
    let items = swept["items"].as_array().expect("sweep items");
    let docs = items
        .iter()
        .find(|i| i["id"] == json!("docs"))
        .unwrap_or_else(|| panic!("sweep lost the probed server: {items:#?}"));
    assert_eq!(docs["costBasis"], json!(sweep::COST_BASIS_MEASURED));
    // The same honesty, one surface over: the basis flipped, the token count
    // did not. `sweep --json` said estimated:false while `probe --json` said
    // estimated:true for this row, in the same second.
    assert_eq!(
        docs["estimated"],
        json!(true),
        "a measured manifest is still counted with bytes/3.5"
    );
    let remote_item = items.iter().find(|i| i["id"] == json!("remote"));
    if let Some(remote_item) = remote_item {
        assert_eq!(remote_item["costBasis"], json!(sweep::COST_BASIS_ESTIMATE));
    }
}

// ---------------------------------------------------------------------------
// Criterion 3: suggestion, diff, apply, lower floor, undo, backup listed.
// ---------------------------------------------------------------------------

/// A project CLAUDE.md with references that point at nothing, plus enough size
/// to be worth reporting.
fn fat_claudemd() -> String {
    let mut s = String::from("# Project rules\n\n## Layout\n\n");
    s.push_str("- the parser used to live in src/gone.rs\n");
    s.push_str("- the old notes moved to ./docs/removed.md\n");
    s.push_str("- the build script was scripts/old-build.sh\n\n");
    s.push_str("## Style\n\n");
    // Bulk, so the file is genuinely oversized rather than a toy.
    for i in 0..200 {
        s.push_str(&format!(
            "- rule {i}: keep functions small and name things for what they are.\n"
        ));
    }
    s
}

#[test]
fn the_journey_from_a_suggestion_to_a_byte_identical_undo() {
    let sb = Sandbox::new();
    let project = sb.project("app");
    let claudemd = project.join("CLAUDE.md");
    sb.write(&claudemd, &fat_claudemd());
    let before = std::fs::read(&claudemd).unwrap();

    // Sessions, so the file is inventoried and its monthly burden is real
    // arithmetic rather than a placeholder.
    for i in 0..6 {
        sb.seed_session(&format!("pre-{i}"), &project, 10, 30_000, 4_000);
    }
    sb.run(&["index"]);

    // The scanner sees it, and says what it costs.
    let inventory = sb.json(&["claudemd", "--json"]);
    let files = inventory["files"].as_array().expect("an inventory");
    let row = files
        .iter()
        .find(|f| f["path"] == json!(claudemd.display().to_string()))
        .unwrap_or_else(|| panic!("the scanner missed the file: {files:#?}"));
    assert!(row["estTokens"].as_i64().unwrap_or(0) > 2_000, "{row:#?}");

    // The advice engine proposes the deterministic cleanup, with its evidence.
    let advised = sb.json(&["advise", "--json"]);
    let candidates = advised["candidates"].as_array().expect("candidates");
    let fix = candidates
        .iter()
        .find(|c| c["kind"] == json!("claudemd-fix"))
        .unwrap_or_else(|| panic!("no cleanup was proposed: {candidates:#?}"));
    assert!(
        !fix["evidence"].as_array().map(|e| e.is_empty()).unwrap_or(true),
        "a suggestion with no evidence is not a suggestion"
    );
    assert_eq!(fix["blocked"], json!(false), "the deterministic fix needs no model");

    // Apply. The CLI deliberately has no apply verb (`piggy advise` is listing
    // only; applying is the app's, behind a diff), so this half drives the same
    // engine call the app's IPC does.
    let id = fix["id"].as_str().expect("an id").to_string();
    let catalog = Catalog::embedded();
    let pricing = Pricing::load(&sb.root().join("piggy"));
    let mut store = sb.store();
    let mut state = PiggyState::load().unwrap();
    let opts = GenerateOptions::new(&catalog, &pricing, &state);
    let generated = advice::generate(&mut store, &opts).unwrap();
    let candidate = generated
        .iter()
        .find(|c| c.id == id)
        .expect("the same candidate the CLI just printed");
    assert_eq!(candidate.kind, ActionKind::ClaudemdFix);
    let applied = advice::apply(&mut store, &mut state, &catalog, candidate).unwrap();
    assert_eq!(applied.id, id);

    // The file changed, and the change is the one that was proposed.
    let after = std::fs::read(&claudemd).unwrap();
    assert_ne!(after, before, "apply wrote nothing");
    let after_text = String::from_utf8(after).unwrap();
    assert!(!after_text.contains("src/gone.rs"), "the dead reference survived");
    assert!(after_text.contains("rule 199"), "apply took more than it said");

    // The snapshot exists, on disk and in the ledger `piggy backups` reads.
    let state = PiggyState::load().unwrap();
    assert_eq!(state.file_snapshots.len(), 1, "no snapshot was taken");
    let snapshot = Path::new(&state.file_snapshots[0].backup).to_path_buf();
    assert!(snapshot.exists(), "the snapshot path does not exist: {snapshot:?}");

    let listed = sb.run(&["backups"]);
    assert!(
        listed.contains("Files Piggy edited (1 restorable"),
        "`piggy backups` did not list the snapshot:\n{listed}"
    );
    assert!(
        listed.contains(&claudemd.display().to_string()),
        "`piggy backups` did not name the file:\n{listed}"
    );

    // Undo puts the original bytes back, to the byte.
    let mut store = sb.store();
    let mut state = PiggyState::load().unwrap();
    let undone = advice::undo(&mut store, &mut state, &catalog, &id).unwrap();
    assert!(undone.complete(), "undo reported failures: {:?}", undone.failures);
    assert_eq!(
        std::fs::read(&claudemd).unwrap(),
        before,
        "undo did not restore the file byte for byte"
    );

    // And the suggestion is open again, because the thing it suggests is true
    // again.
    let rows = store.advice_by_status(advice_status::OPEN).unwrap();
    assert!(rows.iter().any(|r| r.id == id), "the candidate did not reopen");
}

/// The other half of criterion 3: the next session shows a lower floor, and
/// that reading is an observation, never a measurement.
#[test]
fn a_lower_floor_after_the_edit_is_reported_and_never_pooled_into_the_headline() {
    let sb = Sandbox::new();
    let project = sb.project("app");

    // Before: six sessions carrying a heavy floor. After: six carrying a
    // lighter one, in a later window.
    for i in 0..6 {
        sb.seed_session(&format!("before-{i}"), &project, 20, 40_000, 4_000);
    }
    for i in 0..6 {
        sb.seed_session(&format!("after-{i}"), &project, 1, 24_000, 4_000);
    }
    sb.run(&["index"]);

    // Two adjacent windows, which is what `ledger_between` exists for: a 7-day
    // window nested inside a 30-day one damps the very change it is meant to
    // show.
    let pricing = Pricing::load(&sb.root().join("piggy"));
    let store = sb.store();
    let days_ago = |n: i64| (chrono::Utc::now() - chrono::Duration::days(n)).to_rfc3339();
    let recent = store
        .ledger_between(Some(&days_ago(7)), None, &pricing)
        .unwrap();
    let prior = store
        .ledger_between(Some(&days_ago(30)), Some(&days_ago(7)), &pricing)
        .unwrap();

    let floor_per_session = |l: &piggy_core::ledger::Ledger| -> f64 {
        let p = l.projects.first().expect("a project in the window");
        assert!(p.sessions > 0);
        p.floor_tokens as f64 / p.sessions as f64
    };
    let after = floor_per_session(&recent);
    let before = floor_per_session(&prior);
    assert!(
        after < before,
        "the floor did not move: {before} before, {after} after"
    );

    // Every floor figure the CLI publishes is badged as an estimate.
    let ledger = sb.json(&["ledger", "--json"]);
    let sources = ledger["sources"].as_array().expect("ledger sources");
    let floors: Vec<&Value> = sources.iter().filter(|r| r["is_floor"] == json!(true)).collect();
    assert!(!floors.is_empty(), "no floor rows: {sources:#?}");

    // And none of it reaches the measured headline. There is no randomized
    // holdout behind any of these sessions, so the headline must not claim one
    // however far the floor moved.
    let report = sb.json(&["report", "--json"]);
    assert_eq!(
        report["headline"]["observational"],
        json!(true),
        "a floor that moved was pooled into a measured headline: {:#?}",
        report["headline"]
    );
}
