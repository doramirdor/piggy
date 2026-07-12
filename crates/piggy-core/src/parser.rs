//! Streaming JSONL parser for a single Claude Code session file.
//!
//! One file == one session (the filename stem is the session id). Files are
//! append-only JSONL where each line is an independent JSON object. Only
//! `type == "assistant"` lines carry token usage, and the *same* assistant
//! message is rewritten across multiple lines during streaming — so usage is
//! deduplicated by `requestId` (fallback `message.id`, fallback line `uuid`),
//! last-wins. `model == "<synthetic>"` lines are skipped.
//!
//! Parsing is deliberately lenient: unknown line types are ignored, and a
//! malformed line (including a truncated final line from an in-progress write)
//! is counted in `parse_errors` and skipped — it never aborts the file.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde::{Deserialize, Serialize};

const SYNTHETIC_MODEL: &str = "<synthetic>";
/// Bucket used when an assistant line carries usage but no model id. Kept so
/// its tokens are still counted (and reported as unpriced), matching a lenient
/// `jq` reduction over the same lines.
pub const UNKNOWN_MODEL: &str = "unknown";

// ---------------------------------------------------------------------------
// Wire types (permissive: every field optional / defaulted, unknown ignored)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    #[serde(default, rename = "type")]
    line_type: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    git_branch: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
    /// `user` lines carry content (array of blocks, or a bare string).
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// The four token streams plus the ephemeral cache-write split. All fields are
/// `Option` so an explicit `null` in the JSON is treated as 0 rather than a
/// parse failure.
#[derive(Debug, Default, Clone, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct CacheCreation {
    // `ephemeral_5m_input_tokens` also appears here; the 5m write subset is
    // derived as (cache_creation_input_tokens - ephemeral_1h_input_tokens), so
    // only the 1h field is captured.
    #[serde(default)]
    ephemeral_1h_input_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Public aggregate types
// ---------------------------------------------------------------------------

/// Deduplicated token totals for one model within a session.
///
/// `cache_creation_tokens` is the total cache write; `cache_creation_1h_tokens`
/// is the 1-hour-TTL subset of it (the 5-minute subset is the difference).
/// Keeping the split is required for pricing (5m write = 1.25x input rate, 1h
/// write = 2x input rate, cache read = 0.1x input rate).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ModelTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_read_tokens: u64,
}

impl ModelTokens {
    fn add_usage(&mut self, u: &Usage) {
        self.input_tokens += u.input_tokens.unwrap_or(0);
        self.output_tokens += u.output_tokens.unwrap_or(0);
        self.cache_creation_tokens += u.cache_creation_input_tokens.unwrap_or(0);
        self.cache_creation_1h_tokens += u
            .cache_creation
            .as_ref()
            .and_then(|c| c.ephemeral_1h_input_tokens)
            .unwrap_or(0);
        self.cache_read_tokens += u.cache_read_input_tokens.unwrap_or(0);
    }

    /// Sum of all four token streams (input + output + cache write + cache read).
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }
}

/// Result of parsing one session `.jsonl` file.
#[derive(Debug, Clone, Serialize)]
pub struct SessionParse {
    pub session_id: String,
    /// Most common `cwd` seen in the file.
    pub project_path: Option<String>,
    /// Most common non-empty `gitBranch` seen in the file.
    pub git_branch: Option<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    /// Deduplicated per-model token aggregates.
    pub models: BTreeMap<String, ModelTokens>,
    /// Deduplicated count of assistant messages (unique request ids).
    pub n_assistant_msgs: u64,
    pub n_user_msgs: u64,
    /// Count of user lines that contain at least one `tool_result` block.
    pub n_tool_results: u64,
    /// Token subtotal across assistant messages flagged `isSidechain`.
    pub sidechain: ModelTokens,
    pub parse_errors: u64,
}

struct AssistantRec {
    model: String,
    usage: Usage,
    is_sidechain: bool,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a single session `.jsonl` file into a [`SessionParse`].
///
/// Returns an `io::Error` only if the file cannot be opened; malformed *lines*
/// never fail the call (they are counted in `parse_errors`). Empty files yield
/// an empty parse with zero counts.
pub fn parse_file(path: &Path) -> io::Result<SessionParse> {
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let reader = BufReader::new(File::open(path)?);

    let mut dedup: HashMap<String, AssistantRec> = HashMap::new();
    let mut nokey_counter: u64 = 0;
    let mut n_user_msgs: u64 = 0;
    let mut n_tool_results: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut cwd_counts: HashMap<String, u64> = HashMap::new();
    let mut branch_counts: HashMap<String, u64> = HashMap::new();
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line_res in reader.lines() {
        // A read error (e.g. invalid UTF-8) is treated like a malformed line.
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
        if let Some(c) = &raw.cwd {
            if !c.is_empty() {
                *cwd_counts.entry(c.clone()).or_insert(0) += 1;
            }
        }
        if let Some(b) = &raw.git_branch {
            if !b.is_empty() {
                *branch_counts.entry(b.clone()).or_insert(0) += 1;
            }
        }

        match raw.line_type.as_deref() {
            Some("assistant") => {
                let model = raw.message.as_ref().and_then(|m| m.model.clone());
                if model.as_deref() == Some(SYNTHETIC_MODEL) {
                    continue;
                }
                let key = raw
                    .request_id
                    .clone()
                    .or_else(|| raw.message.as_ref().and_then(|m| m.id.clone()))
                    .or_else(|| raw.uuid.clone())
                    .unwrap_or_else(|| {
                        nokey_counter += 1;
                        format!("__nokey_{nokey_counter}")
                    });
                let usage = raw
                    .message
                    .as_ref()
                    .and_then(|m| m.usage.clone())
                    .unwrap_or_default();
                let model_key = model.unwrap_or_else(|| UNKNOWN_MODEL.to_string());
                // Last-wins: a later streaming rewrite of the same requestId
                // replaces the earlier record.
                dedup.insert(
                    key,
                    AssistantRec {
                        model: model_key,
                        usage,
                        is_sidechain: raw.is_sidechain,
                    },
                );
            }
            Some("user") => {
                n_user_msgs += 1;
                if raw
                    .message
                    .as_ref()
                    .map(|m| content_has_tool_result(&m.content))
                    .unwrap_or(false)
                {
                    n_tool_results += 1;
                }
            }
            _ => { /* summary / queue-operation / attachment / unknown: ignore */ }
        }
    }

    let mut models: BTreeMap<String, ModelTokens> = BTreeMap::new();
    let mut sidechain = ModelTokens::default();
    for rec in dedup.values() {
        models
            .entry(rec.model.clone())
            .or_default()
            .add_usage(&rec.usage);
        if rec.is_sidechain {
            sidechain.add_usage(&rec.usage);
        }
    }

    Ok(SessionParse {
        session_id,
        project_path: most_common(&cwd_counts),
        git_branch: most_common(&branch_counts),
        first_ts,
        last_ts,
        n_assistant_msgs: dedup.len() as u64,
        models,
        n_user_msgs,
        n_tool_results,
        sidechain,
        parse_errors,
    })
}

/// True if `content` is an array containing at least one `tool_result` block.
fn content_has_tool_result(content: &Option<serde_json::Value>) -> bool {
    match content {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .any(|el| el.get("type").and_then(|t| t.as_str()) == Some("tool_result")),
        _ => false,
    }
}

/// Pick the key with the highest count; ties broken by lexicographically
/// smallest key for determinism.
fn most_common(counts: &HashMap<String, u64>) -> Option<String> {
    counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
}
