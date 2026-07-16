//! Multi-source ingestion: one index pass over a Claude Code projects root
//! plus a Codex sessions root lands both in the store with the right
//! `(source, interface)` split, and `by_source` reports them separately.

use std::fs;

use piggy_core::index::{run_index_roots, SourceRoot};
use piggy_core::sources::SourceKind;
use piggy_core::stats::Period;
use piggy_core::{Pricing, Store};

const CLAUDE_GUI_SESSION: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-07-14T10:00:00.000Z","cwd":"/p/one","entrypoint":"claude-desktop","message":{"content":"hi"}}
{"type":"assistant","uuid":"a1","requestId":"req_1","timestamp":"2026-07-14T10:00:05.000Z","cwd":"/p/one","entrypoint":"claude-desktop","message":{"id":"m1","model":"claude-sonnet-5","usage":{"input_tokens":100,"output_tokens":40,"cache_creation_input_tokens":10,"cache_read_input_tokens":900}}}
"#;

const CLAUDE_TUI_SESSION: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-07-14T11:00:00.000Z","cwd":"/p/two","entrypoint":"cli","message":{"content":"hi"}}
{"type":"assistant","uuid":"a1","requestId":"req_2","timestamp":"2026-07-14T11:00:05.000Z","cwd":"/p/two","entrypoint":"cli","message":{"id":"m2","model":"claude-sonnet-5","usage":{"input_tokens":50,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
"#;

const CODEX_TUI_SESSION: &str = r#"{"timestamp":"2026-07-14T12:00:00.000Z","type":"session_meta","payload":{"id":"cx-1","cwd":"/p/three","originator":"codex_cli_rs","cli_version":"0.46.0"}}
{"timestamp":"2026-07-14T12:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.1-codex"}}
{"timestamp":"2026-07-14T12:00:09.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":80,"reasoning_output_tokens":20,"total_tokens":1080},"last_token_usage":null}}}
{"timestamp":"2026-07-14T12:00:30.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2400,"cached_input_tokens":1900,"output_tokens":200,"reasoning_output_tokens":60,"total_tokens":2600},"last_token_usage":null}}}
"#;

const CODEX_GUI_SESSION: &str = r#"{"timestamp":"2026-07-14T13:00:00.000Z","type":"session_meta","payload":{"id":"cx-2","cwd":"/p/four","originator":"codex_desktop","cli_version":"0.46.0"}}
{"timestamp":"2026-07-14T13:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.1-codex"}}
{"timestamp":"2026-07-14T13:00:09.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":30,"reasoning_output_tokens":0,"total_tokens":530},"last_token_usage":null}}}
"#;

#[test]
fn indexes_claude_and_codex_roots_with_source_split() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_root = tmp.path().join("projects");
    let codex_root = tmp.path().join("codex-sessions/2026/07/14");
    fs::create_dir_all(claude_root.join("-p-one")).unwrap();
    fs::create_dir_all(&codex_root).unwrap();

    fs::write(
        claude_root.join("-p-one/aaaa-gui.jsonl"),
        CLAUDE_GUI_SESSION,
    )
    .unwrap();
    fs::write(
        claude_root.join("-p-one/bbbb-tui.jsonl"),
        CLAUDE_TUI_SESSION,
    )
    .unwrap();
    fs::write(
        codex_root.join("rollout-2026-07-14T12-00-00-cx-1.jsonl"),
        CODEX_TUI_SESSION,
    )
    .unwrap();
    fs::write(
        codex_root.join("rollout-2026-07-14T13-00-00-cx-2.jsonl"),
        CODEX_GUI_SESSION,
    )
    .unwrap();

    let home = tmp.path().join("piggy");
    let mut store = Store::open(&home).unwrap();
    let pricing = Pricing::embedded();
    let roots = vec![
        SourceRoot::new(claude_root, SourceKind::ClaudeCode),
        SourceRoot::new(tmp.path().join("codex-sessions"), SourceKind::Codex),
    ];
    let rep = run_index_roots(&mut store, &pricing, &roots, false).unwrap();
    assert_eq!(rep.scanned, 4);
    assert_eq!(rep.updated, 4);
    assert_eq!(rep.sessions, 4);
    assert_eq!(rep.parse_errors, 0);

    let rows = store.by_source(Period::All).unwrap();
    let get = |src: &str, iface: &str| {
        rows.iter()
            .find(|r| r.source == src && r.interface == iface)
            .unwrap_or_else(|| panic!("missing {src}/{iface} row"))
    };

    let claude_gui = get("claude-code", "gui");
    assert_eq!(claude_gui.totals.sessions, 1);
    assert_eq!(claude_gui.totals.input_tokens, 100);
    assert_eq!(claude_gui.totals.cache_read_tokens, 900);

    let claude_tui = get("claude-code", "tui");
    assert_eq!(claude_tui.totals.sessions, 1);
    assert_eq!(claude_tui.totals.total_tokens(), 70);

    // Codex TUI: cumulative counters telescope to the final totals.
    let codex_tui = get("codex", "tui");
    assert_eq!(codex_tui.totals.sessions, 1);
    assert_eq!(codex_tui.totals.input_tokens, 2400 - 1900);
    assert_eq!(codex_tui.totals.cache_read_tokens, 1900);
    assert_eq!(codex_tui.totals.output_tokens, 200);
    assert_eq!(codex_tui.totals.cache_creation_tokens, 0);

    let codex_gui = get("codex", "gui");
    assert_eq!(codex_gui.totals.sessions, 1);
    assert_eq!(codex_gui.totals.total_tokens(), 530);

    // Codex models are priced (gpt-5.1-codex is in the embedded table), so
    // nothing lands in the unpriced bucket.
    assert_eq!(codex_tui.totals.unpriced_tokens, 0);
    assert!(codex_tui.totals.cost_usd_est > 0.0);

    // Overall totals include every source.
    let all = store.totals(Period::All).unwrap();
    assert_eq!(all.sessions, 4);
    assert_eq!(
        all.total_tokens(),
        claude_gui.totals.total_tokens()
            + claude_tui.totals.total_tokens()
            + codex_tui.totals.total_tokens()
            + codex_gui.totals.total_tokens()
    );
}

#[test]
fn reindex_is_incremental_across_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_root = tmp.path().join("projects/-p-one");
    let codex_root = tmp.path().join("codex/2026/07/14");
    fs::create_dir_all(&claude_root).unwrap();
    fs::create_dir_all(&codex_root).unwrap();
    fs::write(claude_root.join("s1.jsonl"), CLAUDE_GUI_SESSION).unwrap();
    fs::write(codex_root.join("r1.jsonl"), CODEX_GUI_SESSION).unwrap();

    let home = tmp.path().join("piggy");
    let mut store = Store::open(&home).unwrap();
    let pricing = Pricing::embedded();
    let roots = vec![
        SourceRoot::new(tmp.path().join("projects"), SourceKind::ClaudeCode),
        SourceRoot::new(tmp.path().join("codex"), SourceKind::Codex),
    ];
    let first = run_index_roots(&mut store, &pricing, &roots, false).unwrap();
    assert_eq!(first.updated, 2);
    let second = run_index_roots(&mut store, &pricing, &roots, false).unwrap();
    assert_eq!(second.updated, 0, "unchanged files are skipped");
    assert_eq!(second.skipped, 2);
}
