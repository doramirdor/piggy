//! Parser unit tests over synthesized fixtures.

use std::path::PathBuf;

use piggy_core::parse_file;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn basic_dedup_mixed_models_sidechain_and_unknown_lines() {
    let p = parse_file(&fixture("basic.jsonl")).unwrap();

    // session id comes from the file stem.
    assert_eq!(p.session_id, "basic");
    assert_eq!(p.project_path.as_deref(), Some("/Users/dev/proj"));
    assert_eq!(p.git_branch.as_deref(), Some("main"));
    assert_eq!(p.first_ts.as_deref(), Some("2026-07-10T10:00:00.000Z"));
    assert_eq!(p.last_ts.as_deref(), Some("2026-07-10T10:06:08.000Z"));

    // req_A (deduped, last-wins), req_B, req_C. Synthetic req_D excluded.
    assert_eq!(p.n_assistant_msgs, 3);
    assert_eq!(p.n_user_msgs, 3);
    assert_eq!(p.n_tool_results, 2);
    assert_eq!(p.parse_errors, 0);

    let opus = p.models.get("claude-opus-4-8").expect("opus present");
    // req_A last-wins output = 50 (not the streaming intermediate 10) + req_C 5.
    assert_eq!(opus.input_tokens, 120);
    assert_eq!(opus.output_tokens, 55);
    assert_eq!(opus.cache_creation_tokens, 40);
    assert_eq!(opus.cache_creation_1h_tokens, 30);
    assert_eq!(opus.cache_read_tokens, 200);

    let sonnet = p.models.get("claude-sonnet-5").expect("sonnet present");
    assert_eq!(sonnet.input_tokens, 200);
    assert_eq!(sonnet.output_tokens, 80);

    assert!(!p.models.contains_key("<synthetic>"));

    // sidechain subtotal is exactly req_C.
    assert_eq!(p.sidechain.input_tokens, 20);
    assert_eq!(p.sidechain.output_tokens, 5);
    assert_eq!(p.sidechain.cache_creation_tokens, 0);
}

#[test]
fn truncated_final_line_is_counted_not_fatal() {
    let p = parse_file(&fixture("truncated.jsonl")).unwrap();
    assert_eq!(p.n_assistant_msgs, 1);
    assert_eq!(p.n_user_msgs, 1);
    assert_eq!(p.parse_errors, 1);
    let opus = p.models.get("claude-opus-4-8").unwrap();
    assert_eq!(opus.input_tokens, 10);
    assert_eq!(opus.output_tokens, 5);
}

#[test]
fn synthetic_lines_are_skipped() {
    let p = parse_file(&fixture("synthetic.jsonl")).unwrap();
    assert_eq!(p.n_assistant_msgs, 1);
    assert_eq!(p.parse_errors, 0);
    assert!(!p.models.contains_key("<synthetic>"));
    let opus = p.models.get("claude-opus-4-8").unwrap();
    assert_eq!(opus.input_tokens, 5);
    assert_eq!(opus.output_tokens, 5);
}

#[test]
fn empty_file_is_empty_parse() {
    let p = parse_file(&fixture("empty.jsonl")).unwrap();
    assert_eq!(p.session_id, "empty");
    assert_eq!(p.n_assistant_msgs, 0);
    assert_eq!(p.n_user_msgs, 0);
    assert_eq!(p.parse_errors, 0);
    assert!(p.models.is_empty());
    assert!(p.first_ts.is_none());
    assert!(p.last_ts.is_none());
}
