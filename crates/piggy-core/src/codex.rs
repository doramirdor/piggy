//! Streaming JSONL parser for a single Codex rollout (session) file.
//!
//! Codex (the OpenAI coding agent) writes one rollout file per session under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (plus
//! `archived_sessions`). Every line is `{"timestamp": …, "type": …,
//! "payload": {…}}`:
//!
//! * `session_meta` — session id, `cwd`, `originator` (the GUI/TUI
//!   discriminator: `codex_desktop` / `codex_vscode` vs `codex_cli_rs` /
//!   `codex_exec` / …), git info.
//! * `turn_context` — carries the active `model` for subsequent turns.
//! * `event_msg` with `payload.type == "token_count"` — **cumulative**
//!   `info.total_token_usage` counters for the whole session. Per-turn usage
//!   is recovered by subtracting the previous cumulative total (the same
//!   delta scheme ccusage uses). A counter that goes *down* means the
//!   cumulative baseline reset (e.g. compaction started a fresh count), so
//!   the new value is taken whole rather than producing a negative delta.
//!
//! Codex reports `input_tokens` (which already includes
//! `cached_input_tokens`), `cached_input_tokens`, `output_tokens` (which
//! includes `reasoning_output_tokens`). Mapping onto Piggy's four streams:
//! input = `input − cached`, cache_read = `cached`, output = `output`,
//! cache_write = 0 (Codex has no cache-write concept in its logs).
//!
//! Same leniency contract as the Claude Code parser: a malformed line counts
//! in `parse_errors` and never aborts the file.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde::Deserialize;

use crate::parser::{ModelTokens, SessionParse, UNKNOWN_MODEL};
use crate::sources::{classify_codex_originator, Interface, SourceKind};

// ---------------------------------------------------------------------------
// Wire types (permissive)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct RawLine {
    #[serde(default, rename = "type")]
    line_type: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl TokenUsage {
    /// The per-turn delta from `prev` to `self`, cumulative-counter style: any
    /// counter that decreased means the cumulative baseline reset, so that
    /// counter's new value is taken whole.
    fn delta_from(&self, prev: &TokenUsage) -> TokenUsage {
        fn d(cur: u64, prev: u64) -> u64 {
            if cur >= prev {
                cur - prev
            } else {
                cur
            }
        }
        TokenUsage {
            input_tokens: d(self.input_tokens, prev.input_tokens),
            cached_input_tokens: d(self.cached_input_tokens, prev.cached_input_tokens),
            output_tokens: d(self.output_tokens, prev.output_tokens),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0 && self.cached_input_tokens == 0 && self.output_tokens == 0
    }
}

/// Fold a per-turn usage delta into a [`ModelTokens`] bucket using the
/// stream mapping documented at the top of this module.
fn add_delta(tok: &mut ModelTokens, d: &TokenUsage) {
    tok.input_tokens += d.input_tokens.saturating_sub(d.cached_input_tokens);
    tok.cache_read_tokens += d.cached_input_tokens;
    tok.output_tokens += d.output_tokens;
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a single Codex rollout `.jsonl` file into a [`SessionParse`].
///
/// Returns an `io::Error` only if the file cannot be opened; malformed lines
/// are counted in `parse_errors` and skipped.
pub fn parse_codex_file(path: &Path) -> io::Result<SessionParse> {
    let stem_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let reader = BufReader::new(File::open(path)?);

    let mut session_id: Option<String> = None;
    let mut originator: Option<String> = None;
    let mut project_path: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut parse_errors: u64 = 0;
    let mut n_user_msgs: u64 = 0;
    let mut n_turns: u64 = 0;

    let mut current_model: Option<String> = None;
    let mut prev_total = TokenUsage::default();
    let mut models: BTreeMap<String, ModelTokens> = BTreeMap::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };

        if let Some(ts) = &raw.timestamp {
            if first_ts.as_deref().map(|f| ts.as_str() < f).unwrap_or(true) {
                first_ts = Some(ts.clone());
            }
            if last_ts.as_deref().map(|l| ts.as_str() > l).unwrap_or(true) {
                last_ts = Some(ts.clone());
            }
        }

        let payload = raw.payload.as_ref();
        match raw.line_type.as_deref() {
            Some("session_meta") => {
                let Some(p) = payload else { continue };
                if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        session_id = Some(id.to_string());
                    }
                }
                if let Some(o) = p.get("originator").and_then(|v| v.as_str()) {
                    if !o.is_empty() {
                        originator = Some(o.to_string());
                    }
                }
                if let Some(c) = p.get("cwd").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        project_path = Some(c.to_string());
                    }
                }
                if let Some(b) = p
                    .get("git")
                    .and_then(|g| g.get("branch"))
                    .and_then(|v| v.as_str())
                {
                    if !b.is_empty() {
                        git_branch = Some(b.to_string());
                    }
                }
            }
            Some("turn_context") => {
                if let Some(m) = payload
                    .and_then(|p| p.get("model"))
                    .and_then(|v| v.as_str())
                {
                    if !m.is_empty() {
                        current_model = Some(m.to_string());
                    }
                }
            }
            Some("event_msg") => {
                let Some(p) = payload else { continue };
                match p.get("type").and_then(|v| v.as_str()) {
                    Some("token_count") => {
                        // `info` can be null on housekeeping events — skip those.
                        let Some(total) = p
                            .get("info")
                            .and_then(|i| i.get("total_token_usage"))
                            .and_then(|u| serde_json::from_value::<TokenUsage>(u.clone()).ok())
                        else {
                            continue;
                        };
                        let delta = total.delta_from(&prev_total);
                        prev_total = total;
                        if delta.is_zero() {
                            continue;
                        }
                        n_turns += 1;
                        let model = current_model
                            .clone()
                            .unwrap_or_else(|| UNKNOWN_MODEL.to_string());
                        add_delta(models.entry(model).or_default(), &delta);
                    }
                    Some("user_message") => n_user_msgs += 1,
                    _ => {}
                }
            }
            _ => { /* response_item / compacted / unknown: ignore */ }
        }
    }

    let interface = originator
        .as_deref()
        .map(classify_codex_originator)
        .unwrap_or(Interface::Unknown);

    Ok(SessionParse {
        session_id: session_id.unwrap_or(stem_id),
        source: SourceKind::Codex.as_str().to_string(),
        interface: interface.as_str().to_string(),
        client: originator,
        project_path,
        git_branch,
        first_ts,
        last_ts,
        models,
        // Each positive token_count delta is one model turn — the closest
        // Codex analogue of a deduplicated assistant message.
        n_assistant_msgs: n_turns,
        n_user_msgs,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    const META: &str = r#"{"timestamp":"2026-07-14T10:00:00.000Z","type":"session_meta","payload":{"id":"0197-abc","timestamp":"2026-07-14T10:00:00.000Z","cwd":"/Users/x/proj","originator":"codex_cli_rs","cli_version":"0.46.0","git":{"branch":"main"}}}"#;
    const TURN_GPT: &str = r#"{"timestamp":"2026-07-14T10:00:01.000Z","type":"turn_context","payload":{"cwd":"/Users/x/proj","model":"gpt-5.3-codex","effort":"medium"}}"#;

    fn count(total_in: u64, cached: u64, out: u64) -> String {
        format!(
            r#"{{"timestamp":"2026-07-14T10:00:02.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total_in},"cached_input_tokens":{cached},"output_tokens":{out},"reasoning_output_tokens":0,"total_tokens":{}}},"last_token_usage":null,"model_context_window":272000}},"rate_limits":null}}}}"#,
            total_in + out
        )
    }

    #[test]
    fn cumulative_deltas_sum_to_final_total() {
        let c1 = count(1000, 800, 50);
        let c2 = count(2500, 2100, 130);
        let c3 = count(6000, 5200, 400);
        let f = write_file(&[META, TURN_GPT, &c1, &c2, &c3]);
        let p = parse_codex_file(f.path()).unwrap();

        assert_eq!(p.source, "codex");
        assert_eq!(p.interface, "tui");
        assert_eq!(p.client.as_deref(), Some("codex_cli_rs"));
        assert_eq!(p.session_id, "0197-abc");
        assert_eq!(p.project_path.as_deref(), Some("/Users/x/proj"));
        assert_eq!(p.git_branch.as_deref(), Some("main"));
        assert_eq!(p.n_assistant_msgs, 3);

        let tok = &p.models["gpt-5.3-codex"];
        // Deltas telescope back to the final cumulative totals.
        assert_eq!(tok.input_tokens + tok.cache_read_tokens, 6000);
        assert_eq!(tok.cache_read_tokens, 5200);
        assert_eq!(tok.input_tokens, 800);
        assert_eq!(tok.output_tokens, 400);
        assert_eq!(tok.cache_creation_tokens, 0);
    }

    #[test]
    fn counter_reset_taken_whole_not_negative() {
        let c1 = count(5000, 4000, 300);
        let c2 = count(1200, 900, 40); // cumulative reset (compaction)
        let f = write_file(&[META, TURN_GPT, &c1, &c2]);
        let p = parse_codex_file(f.path()).unwrap();
        let tok = &p.models["gpt-5.3-codex"];
        // 5000+1200 input-incl-cache, 4000+900 cached, 300+40 output.
        assert_eq!(tok.input_tokens, (5000 - 4000) + (1200 - 900));
        assert_eq!(tok.cache_read_tokens, 4000 + 900);
        assert_eq!(tok.output_tokens, 340);
        assert_eq!(p.parse_errors, 0);
    }

    #[test]
    fn model_switch_attributes_deltas_to_current_model() {
        let c1 = count(1000, 0, 100);
        let turn2 = r#"{"timestamp":"2026-07-14T10:05:00.000Z","type":"turn_context","payload":{"model":"gpt-5.4"}}"#;
        let c2 = count(3000, 0, 250);
        let f = write_file(&[META, TURN_GPT, &c1, turn2, &c2]);
        let p = parse_codex_file(f.path()).unwrap();
        assert_eq!(p.models["gpt-5.3-codex"].input_tokens, 1000);
        assert_eq!(p.models["gpt-5.3-codex"].output_tokens, 100);
        assert_eq!(p.models["gpt-5.4"].input_tokens, 2000);
        assert_eq!(p.models["gpt-5.4"].output_tokens, 150);
    }

    #[test]
    fn gui_originator_and_missing_meta_defaults() {
        let meta_gui = META.replace("codex_cli_rs", "codex_desktop");
        let c1 = count(10, 0, 5);
        let f = write_file(&[&meta_gui, &c1]);
        let p = parse_codex_file(f.path()).unwrap();
        assert_eq!(p.interface, "gui");
        // No turn_context → unknown model bucket, still counted.
        assert_eq!(p.models[UNKNOWN_MODEL].input_tokens, 10);

        // No session_meta at all → stem id, unknown interface.
        let f2 = write_file(&[&c1]);
        let p2 = parse_codex_file(f2.path()).unwrap();
        assert_eq!(p2.interface, "unknown");
        assert!(!p2.session_id.is_empty());
    }

    #[test]
    fn malformed_and_null_info_lines_are_lenient() {
        let bad = "{not json";
        let null_info = r#"{"timestamp":"2026-07-14T10:00:03.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        let user = r#"{"timestamp":"2026-07-14T10:00:04.000Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#;
        let c1 = count(100, 0, 10);
        let f = write_file(&[META, TURN_GPT, bad, null_info, user, &c1]);
        let p = parse_codex_file(f.path()).unwrap();
        assert_eq!(p.parse_errors, 1);
        assert_eq!(p.n_user_msgs, 1);
        assert_eq!(p.models["gpt-5.3-codex"].output_tokens, 10);
        assert_eq!(p.first_ts.as_deref(), Some("2026-07-14T10:00:00.000Z"));
        assert_eq!(p.last_ts.as_deref(), Some("2026-07-14T10:00:04.000Z"));
    }
}
