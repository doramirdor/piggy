//! The task unit: one row per user prompt, with its spend and its outcome.
//!
//! These cover the two things that make the table worth having and that nothing
//! else in the schema can express: attributing assistant work to the prompt that
//! caused it, and counting tool failures against it.

use std::io::Write;

use piggy_core::parse_file;

/// Write a session log and parse it.
fn parse(lines: &[&str]) -> piggy_core::parser::SessionParse {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sess.jsonl");
    let mut f = std::fs::File::create(&path).expect("create");
    for l in lines {
        writeln!(f, "{l}").expect("write");
    }
    f.flush().expect("flush");
    parse_file(&path).expect("parse")
}

fn assistant(req: &str, input: u64, output: u64, cache_w: u64, cache_r: u64) -> String {
    format!(
        r#"{{"type":"assistant","requestId":"{req}","timestamp":"2026-08-01T10:00:00Z","message":{{"model":"claude-opus-5","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_creation_input_tokens":{cache_w},"cache_read_input_tokens":{cache_r}}}}}}}"#
    )
}

/// An assistant turn whose content is `names` worth of `tool_use` blocks.
fn assistant_calling(req: &str, names: &[&str]) -> String {
    let blocks: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"type":"tool_use","name":"{n}","input":{{}}}}"#))
        .collect();
    format!(
        r#"{{"type":"assistant","requestId":"{req}","timestamp":"2026-08-01T10:00:00Z","cwd":"/work/proj","message":{{"model":"claude-opus-5","usage":{{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":10}},"content":[{}]}}}}"#,
        blocks.join(",")
    )
}

fn prompt(pid: &str) -> String {
    format!(
        r#"{{"type":"user","promptId":"{pid}","timestamp":"2026-08-01T10:00:00Z","message":{{"content":"do a thing"}}}}"#
    )
}

fn tool_results(pid: &str, ok: usize, errs: usize) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for _ in 0..ok {
        blocks.push(r#"{"type":"tool_result","is_error":false,"content":"fine"}"#.to_string());
    }
    for _ in 0..errs {
        blocks.push(r#"{"type":"tool_result","is_error":true,"content":"boom"}"#.to_string());
    }
    format!(
        r#"{{"type":"user","promptId":"{pid}","timestamp":"2026-08-01T10:01:00Z","message":{{"content":[{}]}}}}"#,
        blocks.join(",")
    )
}

#[test]
fn assistant_work_is_charged_to_the_prompt_that_caused_it() {
    // Assistant lines carry no promptId of their own, so the only way to bill
    // them is the cursor: whichever user prompt came last owns them.
    let p = parse(&[
        &prompt("task-a"),
        &assistant("r1", 10, 20, 30, 500),
        &assistant("r2", 1, 2, 3, 4),
        &prompt("task-b"),
        &assistant("r3", 100, 200, 300, 0),
    ]);

    assert_eq!(p.tasks.len(), 2, "one row per prompt");
    let a = &p.tasks["task-a"];
    let b = &p.tasks["task-b"];

    // Plan-metered spend only: input + output + cache write. Cache reads are
    // tracked separately so this stays comparable to the headline.
    assert_eq!(a.spend_tokens, 10 + 20 + 30 + 1 + 2 + 3);
    assert_eq!(a.cache_read_tokens, 504);
    assert_eq!(a.n_turns, 2);

    assert_eq!(b.spend_tokens, 600);
    assert_eq!(b.n_turns, 1);
}

#[test]
fn a_streamed_message_is_billed_once_not_once_per_rewrite() {
    // Dedup is last-wins on requestId. A task that counted every streaming
    // rewrite would inflate exactly the expensive tasks people care about most.
    let p = parse(&[
        &prompt("t"),
        &assistant("same", 5, 5, 5, 0),
        &assistant("same", 50, 50, 50, 0),
    ]);
    let t = &p.tasks["t"];
    assert_eq!(t.n_turns, 1, "one message, however many rewrites");
    assert_eq!(t.spend_tokens, 150, "the final record wins outright");
}

#[test]
fn tool_errors_are_counted_against_their_task() {
    let p = parse(&[
        &prompt("t"),
        &assistant("r1", 1, 1, 1, 0),
        &tool_results("t", 3, 2),
    ]);
    assert_eq!(p.tasks["t"].n_tool_errors, 2);
}

#[test]
fn an_absent_error_flag_is_not_an_error() {
    // Older logs and some clients omit `is_error` entirely. Counting absence as
    // failure would invent a regression out of a schema change, so the error
    // count is deliberately a floor.
    let line = r#"{"type":"user","promptId":"t","timestamp":"2026-08-01T10:00:00Z","message":{"content":[{"type":"tool_result","content":"no flag"}]}}"#;
    let p = parse(&[&prompt("t"), &assistant("r1", 1, 1, 1, 0), line]);
    assert_eq!(p.tasks["t"].n_tool_errors, 0);
}

#[test]
fn work_before_any_prompt_is_dropped_rather_than_misattributed() {
    // A file can open mid-conversation (resumed sessions, rotated logs). Those
    // messages belong to a prompt this file never saw, and guessing would
    // silently bill them to the next unrelated task.
    let p = parse(&[&assistant("orphan", 999, 999, 999, 0), &prompt("t"), &assistant("r1", 1, 1, 1, 0)]);
    assert_eq!(p.tasks.len(), 1);
    assert_eq!(p.tasks["t"].spend_tokens, 3, "the orphan is not folded in");
}

#[test]
fn a_log_with_no_prompt_ids_records_no_tasks() {
    // Logs predating promptId, and Codex. "No tasks" has to mean "not
    // recorded", never "no work happened", so the session totals still stand.
    let p = parse(&[&assistant("r1", 10, 10, 10, 0)]);
    assert!(p.tasks.is_empty());
    assert_eq!(p.n_assistant_msgs, 1, "session-level counting is unaffected");
}

#[test]
fn every_tool_call_counts_not_just_the_ones_sweep_tracks() {
    // The call count used to reuse Sweep's filter (MCP tools and `Skill`), while
    // the error count counted every flagged result. A task that ran forty Reads
    // and failed twice persisted as 0 calls and 2 errors: two columns with
    // different denominators, sitting next to each other.
    let p = parse(&[
        &prompt("t"),
        &assistant_calling("r1", &["Read", "Bash", "Edit", "mcp__db__query", "Skill"]),
        &tool_results("t", 3, 2),
    ]);

    let t = &p.tasks["t"];
    assert_eq!(t.n_tool_calls, 5, "Read/Bash/Edit are tool calls too");
    assert_eq!(t.n_tool_results, 5, "the denominator the error count needs");
    assert_eq!(t.n_tool_errors, 2);
    // The Sweep table keeps its filter: it answers a different question.
    assert_eq!(p.tool_use_counts.len(), 2);
    assert_eq!(p.tool_use_counts["mcp__db__query"], 1);
}

#[test]
fn the_call_and_result_counts_survive_the_store() {
    // The columns are only worth counting if they come back out. `n_tool_results`
    // is new in schema 7, so this is also the guard on that migration.
    let home = tempfile::tempdir().unwrap();
    let pricing = piggy_core::Pricing::embedded();
    let mut store = piggy_core::Store::open(home.path()).unwrap();

    let p = parse(&[
        &prompt("t"),
        &assistant_calling("r1", &["Read", "Bash", "mcp__db__query"]),
        &tool_results("t", 2, 1),
    ]);
    store
        .upsert_session(&p, &pricing, "/logs/sess.jsonl", 1, 1)
        .unwrap();

    let rows = store.task_table(piggy_core::Period::All).unwrap();
    let r = rows
        .iter()
        .find(|r| r.project == "/work/proj")
        .expect("the session's project");
    assert_eq!(r.tasks, 1);
    assert_eq!(r.tool_calls, 3);
    assert_eq!(r.tool_results, 3);
    assert_eq!(r.tool_errors, 1);
}

#[test]
fn task_timestamps_span_the_whole_task() {
    let p = parse(&[&prompt("t"), &assistant("r1", 1, 1, 1, 0), &tool_results("t", 1, 0)]);
    let t = &p.tasks["t"];
    assert_eq!(t.first_ts.as_deref(), Some("2026-08-01T10:00:00Z"));
    assert_eq!(t.last_ts.as_deref(), Some("2026-08-01T10:01:00Z"));
}
