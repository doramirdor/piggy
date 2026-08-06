//! Store round-trip and incremental-index tests using a temp PIGGY_HOME.

use std::fs;
use std::path::PathBuf;

use piggy_core::store::{advice_status, SCOPE_USER};
use piggy_core::{
    parse_file, run_index, AdviceRow, ClaudemdFile, McpManifest, Period, Pricing, Store,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn upsert_round_trip_and_pricing() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    let parse = parse_file(&fixture("basic.jsonl")).unwrap();
    store
        .upsert_session(&parse, &pricing, "/fake/basic.jsonl", 3242, 111)
        .unwrap();

    let totals = store.totals(Period::All).unwrap();
    // opus (120+55+40+200) + sonnet (200+80) tokens.
    assert_eq!(totals.input_tokens, 320);
    assert_eq!(totals.output_tokens, 135);
    assert_eq!(totals.cache_creation_tokens, 40);
    assert_eq!(totals.cache_read_tokens, 200);
    assert_eq!(totals.sessions, 1);
    assert!(totals.fully_priced());
    assert!(totals.cost_usd_est > 0.0);

    let by_model = store.by_model(Period::All).unwrap();
    assert_eq!(by_model.len(), 2);

    // Re-upserting the same session must not double-count (session_models
    // replaced, not appended).
    store
        .upsert_session(&parse, &pricing, "/fake/basic.jsonl", 3242, 111)
        .unwrap();
    let totals2 = store.totals(Period::All).unwrap();
    assert_eq!(totals2.input_tokens, 320);
    assert_eq!(totals2.sessions, 1);

    let (matched, total) = store.pricing_coverage().unwrap();
    assert_eq!(matched, total);
    assert!(total > 0);
}

#[test]
fn incremental_index_skips_unchanged_files() {
    let home = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();

    // A project subdirectory with one session file copied from a fixture.
    let proj_dir = projects.path().join("-Users-dev-proj");
    fs::create_dir_all(&proj_dir).unwrap();
    fs::copy(fixture("basic.jsonl"), proj_dir.join("basic.jsonl")).unwrap();

    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    let r1 = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(r1.scanned, 1);
    assert_eq!(r1.updated, 1);
    assert_eq!(r1.skipped, 0);
    assert_eq!(r1.sessions, 1);

    // Second run: file unchanged -> skipped.
    let r2 = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(r2.scanned, 1);
    assert_eq!(r2.updated, 0);
    assert_eq!(r2.skipped, 1);

    // --full forces a re-parse.
    let r3 = run_index(&mut store, &pricing, projects.path(), true).unwrap();
    assert_eq!(r3.updated, 1);
    assert_eq!(r3.skipped, 0);
}

// ---------------------------------------------------------------------------
// Schema 8: the three M5 advisor tables
// ---------------------------------------------------------------------------

/// Rewind an already-migrated database to what schema 7 left behind: the three
/// M5 tables did not exist, and the recorded version says 7.
fn rewind_to_v7(home: &std::path::Path) {
    let conn = rusqlite::Connection::open(home.join("piggy.db")).unwrap();
    conn.execute_batch(
        "DROP TABLE mcp_manifests;
         DROP TABLE claudemd_files;
         DROP TABLE advice;",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '7')",
        [],
    )
    .unwrap();
}

fn manifest(server_key: &str, scope: &str, config_hash: &str) -> McpManifest {
    McpManifest {
        server_key: server_key.to_string(),
        scope: scope.to_string(),
        config_hash: config_hash.to_string(),
        tool_count: 12,
        schema_bytes: 40_000,
        schema_tokens: 11_400,
        tokenizer: "gemma-3-4b".to_string(),
        measured_at: "2026-01-02T03:04:05Z".to_string(),
        ok: true,
        error: None,
    }
}

fn advice_row(id: &str, target: &str, est_tokens_month: i64) -> AdviceRow {
    AdviceRow {
        id: id.to_string(),
        kind: "ClaudemdTrim".to_string(),
        target: target.to_string(),
        created_at: "2026-01-02T03:04:05Z".to_string(),
        facts_hash: Some("facts-abc".to_string()),
        est_tokens_month,
        status: advice_status::OPEN.to_string(),
        payload_json: Some(r#"{"evidence":[]}"#.to_string()),
        applied_at: None,
        restore_ref: None,
        dismiss_note: None,
    }
}

#[test]
fn a_v7_database_migrates_in_place_and_keeps_its_rows() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();

    let parse = parse_file(&fixture("basic.jsonl")).unwrap();
    {
        let mut store = Store::open(home.path()).unwrap();
        store
            .upsert_session(&parse, &pricing, "/fake/basic.jsonl", 3242, 111)
            .unwrap();
    }
    rewind_to_v7(home.path());

    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), Some(8), "migrated in place");

    // Everything schema 7 held is still there: a migration that loses a
    // session loses the measurement it was indexed for.
    assert_eq!(store.session_count().unwrap(), 1);
    let totals = store.totals(Period::All).unwrap();
    assert_eq!(totals.input_tokens, 320);
    assert_eq!(totals.output_tokens, 135);
    assert_eq!(totals.cache_read_tokens, 200);

    // And the three new tables exist, empty and writable.
    assert!(store.mcp_manifests().unwrap().is_empty());
    assert!(store.claudemd_files().unwrap().is_empty());
    assert!(store
        .advice_by_status(advice_status::OPEN)
        .unwrap()
        .is_empty());

    store
        .upsert_mcp_manifest(&manifest("github", SCOPE_USER, "cfg-1"))
        .unwrap();
    store
        .upsert_claudemd_file(&ClaudemdFile {
            path: "/work/proj/CLAUDE.md".to_string(),
            project: Some("/work/proj".to_string()),
            bytes: 9_000,
            est_tokens: 2_400,
            hash: "sha-1".to_string(),
            mtime_ns: 1_700_000_000_000_000_000,
            last_scanned: "2026-01-02T03:04:05Z".to_string(),
        })
        .unwrap();
    assert!(store.insert_advice(&advice_row("a1", "/work/proj/CLAUDE.md", 5_000)).unwrap());
    assert_eq!(store.mcp_manifests().unwrap().len(), 1);
    assert_eq!(store.claudemd_files().unwrap().len(), 1);
    assert_eq!(store.advice_by_status(advice_status::OPEN).unwrap().len(), 1);
}

#[test]
fn a_manifest_is_keyed_by_server_and_scope() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();

    // The same server can be configured twice: once globally, once inside a
    // project. They are separate measurements, not a collision.
    store
        .upsert_mcp_manifest(&manifest("github", SCOPE_USER, "cfg-user"))
        .unwrap();
    store
        .upsert_mcp_manifest(&manifest("github", "/work/proj", "cfg-proj"))
        .unwrap();
    assert_eq!(store.mcp_manifests().unwrap().len(), 2);

    let got = store.mcp_manifest("github", SCOPE_USER).unwrap().unwrap();
    assert_eq!(got, manifest("github", SCOPE_USER, "cfg-user"));

    // Re-measuring the same key replaces it, and the config hash is how a
    // caller tells a current measurement from one taken before the user edited
    // the server's command.
    let mut remeasured = manifest("github", SCOPE_USER, "cfg-user-2");
    remeasured.tool_count = 3;
    store.upsert_mcp_manifest(&remeasured).unwrap();
    let got = store.mcp_manifest("github", SCOPE_USER).unwrap().unwrap();
    assert_eq!(got.config_hash, "cfg-user-2");
    assert_eq!(got.tool_count, 3);
    assert_eq!(store.mcp_manifests().unwrap().len(), 2, "still two rows");

    assert!(store.mcp_manifest("never-probed", SCOPE_USER).unwrap().is_none());
}

#[test]
fn a_claudemd_row_is_replaced_by_the_next_scan() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();

    let mut f = ClaudemdFile {
        path: "/work/proj/CLAUDE.md".to_string(),
        project: Some("/work/proj".to_string()),
        bytes: 9_000,
        est_tokens: 2_400,
        hash: "sha-1".to_string(),
        mtime_ns: 1_700_000_000_000_000_000,
        last_scanned: "2026-01-02T03:04:05Z".to_string(),
    };
    store.upsert_claudemd_file(&f).unwrap();
    // A global file has no project.
    store
        .upsert_claudemd_file(&ClaudemdFile {
            path: "/home/dev/.claude/CLAUDE.md".to_string(),
            project: None,
            ..f.clone()
        })
        .unwrap();

    f.bytes = 4_000;
    f.est_tokens = 1_050;
    f.hash = "sha-2".to_string();
    store.upsert_claudemd_file(&f).unwrap();

    let rows = store.claudemd_files().unwrap();
    assert_eq!(rows.len(), 2, "path is the key; the rescan replaced its row");
    assert_eq!(rows[0].path, "/home/dev/.claude/CLAUDE.md", "path order");
    assert_eq!(rows[0].project, None);
    assert_eq!(rows[1].est_tokens, 1_050);
    assert_eq!(rows[1].hash, "sha-2");
}

#[test]
fn advice_keeps_its_lifecycle_when_the_same_candidate_is_generated_again() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();

    assert!(store.insert_advice(&advice_row("a1", "/work/a/CLAUDE.md", 5_000)).unwrap());
    assert!(store.insert_advice(&advice_row("a2", "/work/b/CLAUDE.md", 9_000)).unwrap());

    // Biggest saving first, so the section's top-3 slice is the top 3.
    let open = store.advice_by_status(advice_status::OPEN).unwrap();
    assert_eq!(
        open.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["a2", "a1"]
    );

    // The user says "not for me".
    assert!(store
        .set_advice_status(
            "a1",
            advice_status::DISMISSED,
            None,
            None,
            Some("this file is deliberate")
        )
        .unwrap());
    let a1 = store.advice("a1").unwrap().unwrap();
    assert_eq!(a1.status, advice_status::DISMISSED);
    assert_eq!(a1.dismiss_note.as_deref(), Some("this file is deliberate"));

    // The next scan regenerates the identical candidate. Its id is a hash of
    // its own evidence, so it must find its dismissal, not overwrite it.
    assert!(!store.insert_advice(&advice_row("a1", "/work/a/CLAUDE.md", 5_000)).unwrap());
    let a1 = store.advice("a1").unwrap().unwrap();
    assert_eq!(a1.status, advice_status::DISMISSED);
    assert_eq!(a1.dismiss_note.as_deref(), Some("this file is deliberate"));
    assert_eq!(
        store.advice_by_status(advice_status::OPEN).unwrap().len(),
        1,
        "only a2 is still open"
    );

    // Apply stamps the row with the handle Undo needs.
    store
        .set_advice_status(
            "a2",
            advice_status::APPLIED,
            Some("2026-01-03T00:00:00Z"),
            Some("snapshot:/work/b/CLAUDE.md"),
            None,
        )
        .unwrap();
    let a2 = store.advice("a2").unwrap().unwrap();
    assert_eq!(a2.applied_at.as_deref(), Some("2026-01-03T00:00:00Z"));
    assert_eq!(a2.restore_ref.as_deref(), Some("snapshot:/work/b/CLAUDE.md"));

    // Undo puts it back to open and leaves no stamp from a state it has left.
    store
        .set_advice_status("a2", advice_status::OPEN, None, None, None)
        .unwrap();
    let a2 = store.advice("a2").unwrap().unwrap();
    assert_eq!(a2.status, advice_status::OPEN);
    assert_eq!(a2.applied_at, None);
    assert_eq!(a2.restore_ref, None);

    // An unknown id is reported, never a silent no-op.
    assert!(!store
        .set_advice_status("nope", advice_status::STALE, None, None, None)
        .unwrap());
    assert!(store.advice("nope").unwrap().is_none());
}
