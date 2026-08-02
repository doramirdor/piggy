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

// ---------------------------------------------------------------------------
// Context ledger: every cache-write token is charged to what caused it.
// ---------------------------------------------------------------------------

#[test]
fn context_ledger_attributes_and_reconciles() {
    use piggy_core::parser::{CTX_CONVERSATION, CTX_FLOOR};

    let p = parse_file(&fixture("context.jsonl")).unwrap();
    let ctx = &p.context;

    // req_A is the first assistant message: 100 input + 40 cache write = 140,
    // the session floor. The skill_listing that preceded it is part of what the
    // session opens with, so it is charged as a NAMED FLOOR COMPONENT bounded
    // by its own size, and the unexplained rest stays on the floor residual.
    // Floor total is still exactly the measured write.
    let floor_component = ctx["floor:skill_listing"].tokens;
    assert!(
        floor_component > 0 && floor_component < 40,
        "bounded by its own ~43-byte payload, got {floor_component}"
    );
    assert_eq!(
        ctx[CTX_FLOOR].tokens + floor_component,
        140,
        "floor residual + components must equal the first write"
    );
    assert!(
        !ctx.contains_key("skill_listing"),
        "a pre-floor injection is a floor component, not a per-turn injection"
    );

    // req_B's 300-token write is NOT all caused by the two injections before
    // it: that write also carries the user's turn. Each injection is charged at
    // most what it contains, and the residual is conversation growth.
    //
    // Regression: charging the whole write to whatever preceded it let a
    // 480-byte date_change notice absorb 1.1M tokens on a real tree.
    let hook = ctx["hook_success"].tokens;
    let date = ctx["date_change"].tokens;
    assert!(
        hook > date,
        "hook_success has the larger payload: {hook} vs {date}"
    );
    assert!(
        hook < 120,
        "hook_success's payload is ~200 bytes, so it cannot cost {hook} tokens"
    );
    assert!(
        date < 40,
        "date_change is a few dozen bytes, so it cannot cost {date} tokens"
    );

    // req_C has nothing pending, so its whole 50-token write is conversation,
    // plus the residual of req_B's write that the injections did not explain.
    // Its streaming rewrite is the same message and must not be charged twice.
    let conv = ctx[CTX_CONVERSATION].tokens;
    assert_eq!(
        conv,
        350 - hook - date,
        "conversation takes every token the injections did not"
    );
    assert_eq!(ctx[CTX_CONVERSATION].n, 2, "req_B's residual and req_C");

    // The whole point: the ledger reconciles against the token totals rather
    // than estimating alongside them.
    let ledger: u64 = ctx.values().map(|c| c.tokens).sum();
    let models: u64 = p
        .models
        .values()
        .map(|m| m.input_tokens + m.cache_creation_tokens)
        .sum();
    assert_eq!(
        ledger, models,
        "ledger must sum to input + cache_creation exactly"
    );
}
