//! Manifest-probe tests: the three fixture servers, the hard limits, redaction,
//! and the sweep label flip.
//!
//! The fixture servers are node scripts, so every test that launches one looks
//! for `node` first and skips loudly when there is none. Everything that does
//! not need a live server (hashing, deferral, the sweep flip) runs everywhere.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use piggy_core::probe::{self, MeasurementStatus, ProbeOptions, Transport};
use piggy_core::store::SCOPE_USER;
use piggy_core::{sweep, McpManifest, Store};
use serde_json::{json, Value};

/// The compact serialization of the three tools `ok-server.mjs` returns (two on
/// the first page, one after the cursor). Cross-checked against the fixture with
/// `node -e 'JSON.stringify(TOOLS).length'`, which is the same array the probe
/// re-serializes.
const OK_SCHEMA_BYTES: i64 = 717;
/// 717 / 3.5, rounded: what [`probe::BytesEstimate`] makes of it.
const OK_SCHEMA_TOKENS: i64 = 205;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// `which node`, or `None`.
fn node_bin() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("node")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Resolve `node` or print a skip line and leave the test.
macro_rules! node_or_skip {
    ($name:expr) => {
        match node_bin() {
            Some(n) => n,
            None => {
                println!(
                    "SKIP {}: no `node` on PATH, and the MCP fixture servers are node scripts",
                    $name
                );
                return;
            }
        }
    };
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// One user-scope stdio server pointed at a fixture script, built the way the
/// probe will see it in the wild: through `servers_from_root` over a
/// `~/.claude.json` shape.
fn fixture_server(key: &str, node: &str, script: &str, env: Value) -> probe::ConfiguredServer {
    let root = json!({
        "mcpServers": {
            key: { "command": node, "args": [fixture(script), run_marker()], "env": env }
        }
    });
    probe::servers_from_root(&root)
        .into_iter()
        .next()
        .expect("one configured server")
}

/// A short-fused options set, so a test never waits out the shipped budget.
fn fast_opts() -> ProbeOptions<'static> {
    ProbeOptions {
        timeout: Duration::from_millis(2_500),
        ..ProbeOptions::default()
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    (home, store)
}

// ---------------------------------------------------------------------------
// The three fixture servers
// ---------------------------------------------------------------------------

#[test]
fn ok_server_is_measured_across_both_pages() {
    let node = node_or_skip!("ok_server_is_measured_across_both_pages");
    let (_home, mut db) = store();
    let server = fixture_server("ok", &node, "ok-server.mjs", json!({}));

    let row = probe::probe(&mut db, &server, &fast_opts())
        .unwrap()
        .expect("stdio servers are measured, not deferred");

    assert!(row.ok, "probe failed: {:?}", row.error);
    assert_eq!(row.error, None);
    // Three tools: two from the first page, one after following `nextCursor`.
    assert_eq!(row.tool_count, 3);
    assert_eq!(row.schema_bytes, OK_SCHEMA_BYTES);
    assert_eq!(row.schema_tokens, OK_SCHEMA_TOKENS);
    assert_eq!(row.tokenizer, probe::TOKENIZER_BYTES_ESTIMATE);
    assert_eq!(row.server_key, "ok");
    assert_eq!(row.scope, SCOPE_USER);
    assert_eq!(row.config_hash, server.config_hash());

    // And it is readable as a measurement of *this* config.
    let manifests = db.mcp_manifests().unwrap();
    assert_eq!(
        probe::status(&manifests, &server),
        MeasurementStatus::Measured(row)
    );
    // The whole row, so the caller can still see which tokenizer produced the
    // count rather than quoting a bare number.
    let measured = probe::measured_manifest(&manifests, &server).expect("a measurement");
    assert_eq!(measured.schema_tokens, OK_SCHEMA_TOKENS);
    assert_eq!(measured.tokenizer, probe::TOKENIZER_BYTES_ESTIMATE);
}

#[test]
fn slow_server_times_out_and_does_not_hang() {
    let node = node_or_skip!("slow_server_times_out_and_does_not_hang");
    let (_home, mut db) = store();
    let server = fixture_server("slow", &node, "slow-server.mjs", json!({}));

    let started = Instant::now();
    let row = probe::probe(&mut db, &server, &fast_opts())
        .unwrap()
        .expect("a row is written even when the probe fails");
    let elapsed = started.elapsed();

    assert!(!row.ok);
    let err = row.error.clone().unwrap_or_default();
    assert!(err.contains("timed out"), "unexpected reason: {err}");
    assert_eq!(row.tool_count, 0);
    assert_eq!(row.schema_bytes, 0);
    // The whole point: the budget bounds the call. Generous ceiling so a loaded
    // CI box does not flake, but nowhere near a hang.
    assert!(
        elapsed < Duration::from_secs(20),
        "probe took {elapsed:?}, which means the timeout did not stop it"
    );
}

#[test]
fn a_flood_of_server_requests_is_refused_a_bounded_number_of_times() {
    let node = node_or_skip!("a_flood_of_server_requests_is_refused_a_bounded_number_of_times");
    let (_home, mut db) = store();
    let server = fixture_server("flood", &node, "flood-server.mjs", json!({}));

    // The shipped budget on purpose: the point is that the probe stops without
    // needing a clock at all. Before the ceiling existed it never stopped, since
    // the timeout only guards reads and this hang is a blocked write.
    let started = Instant::now();
    let row = probe::probe(&mut db, &server, &probe::ProbeOptions::default())
        .unwrap()
        .expect("a row is written even when the probe fails");
    let elapsed = started.elapsed();

    assert!(!row.ok);
    let err = row.error.clone().unwrap_or_default();
    assert!(err.contains("stopped replying"), "unexpected reason: {err}");
    assert!(
        !err.contains("timed out"),
        "the ceiling should classify this, not the clock: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "probe took {elapsed:?}; it answered the flood instead of cutting it off"
    );

    // And the server it started is gone: an orphaned MCP process is half of what
    // made the hang expensive.
    assert!(
        !fixture_process_is_running("flood-server.mjs"),
        "the probe left its server running"
    );
}

/// A token unique to this test binary's run, passed to every fixture server as
/// a trailing argument it ignores.
///
/// `pgrep -f` matches every process on the machine, so searching for the script
/// name alone finds fixture servers started by a *different* concurrent run
/// (another worktree, another agent, a second `cargo test`) and fails a leak
/// assertion that has nothing to do with this run. Seen twice in practice.
fn run_marker() -> String {
    format!("--piggy-test-run={}", std::process::id())
}

/// Whether a fixture server started by *this* run is still going. `false` when
/// `pgrep` is unavailable, so the assertion above never fails for want of a
/// tool.
fn fixture_process_is_running(script: &str) -> bool {
    // Both patterns, so a concurrent run's copy of the same script does not
    // count: `pgrep -f` ANDs repeated patterns only with `-a` on some platforms,
    // so match the marker and confirm the script name in the matched line.
    std::process::Command::new("pgrep")
        .args(["-fl", &run_marker()])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.contains(script))
        })
        .unwrap_or(false)
}

#[test]
fn garbage_server_is_a_parse_error() {
    let node = node_or_skip!("garbage_server_is_a_parse_error");
    let (_home, mut db) = store();
    let server = fixture_server("garbage", &node, "garbage-server.mjs", json!({}));

    let row = probe::probe(&mut db, &server, &fast_opts()).unwrap().unwrap();

    assert!(!row.ok);
    let err = row.error.clone().unwrap_or_default();
    assert!(err.contains("not JSON"), "unexpected reason: {err}");
    // Classified from the first bad line, not by running out the clock.
    assert!(!err.contains("timed out"), "unexpected reason: {err}");
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[test]
fn configured_env_values_never_reach_an_error_or_a_row() {
    let node = node_or_skip!("configured_env_values_never_reach_an_error_or_a_row");
    let (_home, mut db) = store();
    // The garbage server prints this value on both streams, which is precisely
    // how a real server leaks a credential into an error string.
    let secret = "sk-fake-do-not-log-8f2c41";
    let server = fixture_server(
        "leaky",
        &node,
        "garbage-server.mjs",
        json!({ "PIGGY_FAKE_TOKEN": secret }),
    );

    let err = probe::measure(&server, &fast_opts()).unwrap_err();
    assert_eq!(err.kind(), "parse");
    let text = err.to_string();
    assert!(!text.contains(secret), "the error leaked the value: {text}");
    // Redacted, not merely absent: the probe read the line that carried it.
    assert!(
        text.contains("PIGGY_FAKE_TOKEN=<redacted>"),
        "the value was never in the error to begin with, so this proves nothing: {text}"
    );

    let row = probe::probe(&mut db, &server, &fast_opts()).unwrap().unwrap();
    assert!(!row.ok);
    let stored = format!("{row:?}");
    assert!(
        !stored.contains(secret),
        "the stored row leaked the value: {stored}"
    );
    // The key name is not a secret, and naming it is what makes the redaction
    // legible ("that is your token, not a config typo").
    assert!(
        stored.contains("PIGGY_FAKE_TOKEN"),
        "the redaction dropped the key name too: {stored}"
    );
    // Nothing that reads the DB back can see it either.
    let reread = db.mcp_manifest("leaky", SCOPE_USER).unwrap().unwrap();
    assert!(!format!("{reread:?}").contains(secret));
}

#[test]
fn config_hash_tracks_command_args_and_env_values() {
    let base = json!({
        "mcpServers": {
            "s": { "command": "node", "args": ["a.mjs"], "env": { "A": "1", "TOKEN": "first" } }
        }
    });
    let hash = |v: &Value| probe::servers_from_root(v)[0].config_hash();
    let baseline = hash(&base);

    // Args are part of what runs, so changing one invalidates the measurement.
    let other_args = json!({
        "mcpServers": {
            "s": { "command": "node", "args": ["b.mjs"], "env": { "A": "1", "TOKEN": "first" } }
        }
    });
    assert_ne!(baseline, hash(&other_args));

    // So is the command.
    let other_cmd = json!({
        "mcpServers": {
            "s": { "command": "bun", "args": ["a.mjs"], "env": { "A": "1", "TOKEN": "first" } }
        }
    });
    assert_ne!(baseline, hash(&other_cmd));

    // A rotated secret is a different server: the value is hashed (and only
    // hashed) so a stale measurement cannot survive it.
    let other_env = json!({
        "mcpServers": {
            "s": { "command": "node", "args": ["a.mjs"], "env": { "A": "1", "TOKEN": "second" } }
        }
    });
    assert_ne!(baseline, hash(&other_env));

    // Re-ordering the env map is not a change; it must not throw away a good
    // measurement.
    let reordered = json!({
        "mcpServers": {
            "s": { "command": "node", "args": ["a.mjs"], "env": { "TOKEN": "first", "A": "1" } }
        }
    });
    assert_eq!(baseline, hash(&reordered));

    // Neither is an unrelated key: it cannot change what the server answers.
    let extra_key = json!({
        "mcpServers": {
            "s": { "command": "node", "args": ["a.mjs"], "env": { "A": "1", "TOKEN": "first" },
                   "description": "notes the user typed" }
        }
    });
    assert_eq!(baseline, hash(&extra_key));

    // The value itself is nowhere in the hash output.
    assert!(!baseline.contains("first"));
}

// ---------------------------------------------------------------------------
// Enumeration and deferral
// ---------------------------------------------------------------------------

#[test]
fn http_servers_are_deferred_and_get_no_row() {
    let (_home, mut db) = store();
    let root = json!({
        "mcpServers": {
            "remote-typed": { "type": "http", "url": "https://example.test/mcp" },
            "remote-untyped": { "url": "https://example.test/sse" }
        }
    });
    for server in probe::servers_from_root(&root) {
        assert_eq!(server.transport, Transport::Remote, "{}", server.key);
        assert!(probe::probe(&mut db, &server, &fast_opts()).unwrap().is_none());
        assert_eq!(probe::status(&[], &server), MeasurementStatus::Deferred);
    }
    // A deferred server leaves no measurement behind, because none was made.
    assert!(db.mcp_manifests().unwrap().is_empty());
}

#[test]
fn enumeration_covers_user_scope_and_every_project() {
    let root = json!({
        "mcpServers": { "global": { "command": "node", "args": [] } },
        "projects": {
            "/Users/dev/one": { "mcpServers": { "scoped": { "command": "node", "args": [] } } },
            "/Users/dev/two": { "history": [] }
        }
    });
    let servers = probe::servers_from_root(&root);
    assert_eq!(servers.len(), 2);
    // User scope first: it is the copy every session pays for.
    assert_eq!(servers[0].key, "global");
    assert_eq!(servers[0].project, None);
    assert_eq!(servers[0].scope(), SCOPE_USER);
    assert_eq!(servers[0].transport, Transport::Stdio);
    assert_eq!(servers[1].key, "scoped");
    assert_eq!(servers[1].scope(), "/Users/dev/one");
}

// ---------------------------------------------------------------------------
// Sweep evidence
// ---------------------------------------------------------------------------

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn sweep_prefers_a_measured_manifest_over_its_estimate() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("PIGGY_HOME", dir.path().join("piggy"));
    std::env::set_var("PIGGY_CLAUDE_DIR", dir.path().join("claude"));
    std::env::set_var("PIGGY_CLAUDE_JSON", dir.path().join("claude.json"));

    let config = json!({
        "mcpServers": {
            "atlas": { "command": "node", "args": ["atlas.mjs"], "env": { "TOKEN": "abcdef" } }
        }
    });
    std::fs::write(
        dir.path().join("claude.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let mut db = Store::open(dir.path().join("db").as_path()).unwrap();
    let server = probe::servers_from_root(&config).remove(0);

    // Unprobed: the config-size heuristic, labelled as the guess it is.
    let item = mcp_item(&sweep::scan(&db, 50).unwrap());
    assert_eq!(item.cost_basis, sweep::COST_BASIS_ESTIMATE);
    assert!(item.tokens_estimated);
    let heuristic = item.est_tokens;

    // Measured: the probe's number and the probe's label.
    db.upsert_mcp_manifest(&measured(&server, 12_345)).unwrap();
    let item = mcp_item(&sweep::scan(&db, 50).unwrap());
    assert_eq!(item.cost_basis, sweep::COST_BASIS_MEASURED);
    assert_eq!(item.est_tokens, 12_345);
    // The manifest was measured, the token count was not: the shipped tokenizer
    // divides bytes by 3.5, and dropping that on the way to the row is what let
    // sweep print an estimate as an exact figure.
    assert!(
        item.tokens_estimated,
        "a bytes/3.5 count is an estimate however real the bytes are"
    );

    // A row a real tokenizer wrote is the one case the count is exact.
    db.upsert_mcp_manifest(&measured_with(&server, 12_345, "qwen3-4b"))
        .unwrap();
    let item = mcp_item(&sweep::scan(&db, 50).unwrap());
    assert_eq!(item.cost_basis, sweep::COST_BASIS_MEASURED);
    assert!(!item.tokens_estimated);
    // Back to the shipped tokenizer for the rest of the walk.
    db.upsert_mcp_manifest(&measured(&server, 12_345)).unwrap();

    // Changed config: the stored row measured something else, so sweep falls
    // back rather than quoting a number for a server that no longer exists.
    let moved = json!({
        "mcpServers": {
            "atlas": { "command": "node", "args": ["atlas.mjs", "--fast"], "env": { "TOKEN": "abcdef" } }
        }
    });
    std::fs::write(
        dir.path().join("claude.json"),
        serde_json::to_string_pretty(&moved).unwrap(),
    )
    .unwrap();
    let item = mcp_item(&sweep::scan(&db, 50).unwrap());
    assert_eq!(item.cost_basis, sweep::COST_BASIS_ESTIMATE);
    assert_ne!(item.est_tokens, 12_345);
    // The heuristic reads the config, so a longer arg list moves it: what
    // matters is that it is the heuristic again, not the stale measurement.
    assert!(item.est_tokens >= heuristic);
    let heuristic_after = item.est_tokens;

    // A failed probe is not a measurement either.
    let server = probe::servers_from_root(&moved).remove(0);
    let mut failed = measured(&server, 999);
    failed.ok = false;
    failed.error = Some("the server stopped before answering".into());
    db.upsert_mcp_manifest(&failed).unwrap();
    let item = mcp_item(&sweep::scan(&db, 50).unwrap());
    assert_eq!(item.cost_basis, sweep::COST_BASIS_ESTIMATE);
    assert_eq!(item.est_tokens, heuristic_after);
}

/// The one MCP row in a sweep report.
fn mcp_item(report: &sweep::SweepReport) -> sweep::SweepItem {
    report
        .items
        .iter()
        .find(|i| i.kind == "mcp")
        .expect("the configured MCP server is in the report")
        .clone()
}

/// A successful manifest row for `server`, as the probe would have written it
/// with the shipped bytes/3.5 tokenizer.
fn measured(server: &probe::ConfiguredServer, tokens: i64) -> McpManifest {
    measured_with(server, tokens, probe::TOKENIZER_BYTES_ESTIMATE)
}

/// The same, with an explicit `tokenizer` label: what M5.4 writes once the
/// advisor's real tokenizer counts the schemas.
fn measured_with(
    server: &probe::ConfiguredServer,
    tokens: i64,
    tokenizer: &str,
) -> McpManifest {
    McpManifest {
        server_key: server.key.clone(),
        scope: server.scope().to_string(),
        config_hash: server.config_hash(),
        tool_count: 24,
        schema_bytes: tokens * 7 / 2,
        schema_tokens: tokens,
        tokenizer: tokenizer.to_string(),
        measured_at: "2026-08-06T10:00:00Z".to_string(),
        ok: true,
        error: None,
    }
}
