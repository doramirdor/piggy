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

/// Ledger bucket for the context a session pays for before anyone types: system
/// prompt, tool definitions, memory files. Charged to the first assistant
/// message, which is where it is actually written to cache.
pub const CTX_FLOOR: &str = "__floor";
/// Ledger bucket for context that grew from the work itself — prompts, tool
/// results, file reads. The residual: what no injection accounts for.
pub const CTX_CONVERSATION: &str = "__conversation";

/// Prefix for a **named component of the session floor** (`floor:skill_listing`).
///
/// Injections logged before the first assistant message are not per-turn costs;
/// they are what the session opens with. Recording them by name turns "your
/// floor is 26k tokens" into "…of which 6.5k is the skill listing", which is
/// the difference between a number and something a user can act on. 99.9% of
/// real sessions log at least one.
///
/// `CTX_FLOOR` keeps whatever is left (system prompt, tool schemas, memory —
/// none of which are logged), so floor total = `CTX_FLOOR` + every `floor:*`.
pub const CTX_FLOOR_PREFIX: &str = "floor:";

/// Bytes per token when bounding an injection's cost by its own content.
///
/// An injection cannot cost more tokens than it contains, and a cache write is
/// never *only* the injection: the same write carries the user's prompt and the
/// previous turn's tool results. Charging an injection the whole write let a
/// 480-byte `date_change` notice absorb **1,144,283 tokens** across 4,000
/// sessions (7,152x its own size) and inflated the headline "removable by
/// configuration" figure with conversation growth.
///
/// 3 is a deliberate over-estimate — JSON-ish text runs nearer 4 bytes/token —
/// so the bound errs toward charging an injection slightly too much rather than
/// too little. A "you could remove this" number should not be flattered by its
/// own rounding.
const BYTES_PER_TOKEN: u64 = 3;

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
    /// Which surface wrote the line (`cli`, `claude-desktop`, `claude-vscode`,
    /// `sdk-cli`, …) — the GUI/TUI discriminator.
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
    /// Present on `type == "attachment"` lines. Its `type` field names what was
    /// injected (`hook_success`, `skill_listing`, `mcp_instructions_delta`, …) —
    /// the ledger's whole "from where".
    #[serde(default)]
    attachment: Option<serde_json::Value>,
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

/// One ledger bucket's cost within a session: the cache-write tokens charged to
/// it and how many times it was charged.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ContextTokens {
    /// Cache-write tokens (`input + cache_creation`) attributed to this bucket.
    /// Cache *reads* are excluded on purpose: a read is the discounted re-send
    /// of context already paid for, so charging it here would bill the same
    /// injection once per turn for the rest of the session.
    pub tokens: u64,
    /// How many assistant messages this bucket was charged on.
    pub n: u64,
}

/// Result of parsing one session `.jsonl` file.
#[derive(Debug, Clone, Serialize)]
pub struct SessionParse {
    pub session_id: String,
    /// Which tool wrote the log: `"claude-code"` or `"codex"` (see
    /// [`crate::sources::SourceKind`]).
    pub source: String,
    /// The surface it ran in: `"gui"`, `"tui"`, or `"unknown"` (see
    /// [`crate::sources::Interface`]).
    pub interface: String,
    /// The raw client marker the classification came from (Claude Code
    /// `entrypoint` / Codex `originator`), kept for diagnostics.
    pub client: Option<String>,
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
    /// Deduplicated counts of tool_use invocations, filtered to the names Sweep
    /// cares about: MCP tools (`mcp__<server>__<tool>`) and `Skill`. Counted from
    /// the last-wins assistant records so a streamed message is not double-counted.
    pub tool_use_counts: BTreeMap<String, u64>,
    /// Where this session's cache-write tokens came from, keyed by
    /// [`CTX_FLOOR`], [`CTX_CONVERSATION`], or an attachment type. Sums to the
    /// session's `input + cache_creation` across deduplicated assistant
    /// messages, so the ledger reconciles against `session_models` rather than
    /// estimating.
    pub context: BTreeMap<String, ContextTokens>,
    pub parse_errors: u64,
}

struct AssistantRec {
    model: String,
    usage: Usage,
    is_sidechain: bool,
    /// Sweep-relevant tool_use names in this (deduplicated) assistant message.
    tool_uses: Vec<String>,
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
    // The ledger needs ORDER, which `dedup` throws away. `order` records each
    // assistant message the first time its key is seen, paired with whatever was
    // injected since the previous one. A streaming rewrite of the same requestId
    // is not a new message and must not be charged again.
    let mut order: Vec<(String, Vec<(String, u64)>)> = Vec::new();
    let mut pending: Vec<(String, u64)> = Vec::new();
    let mut nokey_counter: u64 = 0;
    let mut n_user_msgs: u64 = 0;
    let mut n_tool_results: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut cwd_counts: HashMap<String, u64> = HashMap::new();
    let mut branch_counts: HashMap<String, u64> = HashMap::new();
    let mut entrypoint_counts: HashMap<String, u64> = HashMap::new();
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
        if let Some(e) = &raw.entrypoint {
            if !e.is_empty() {
                *entrypoint_counts.entry(e.clone()).or_insert(0) += 1;
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
                let tool_uses = raw
                    .message
                    .as_ref()
                    .map(|m| sweep_tool_use_names(&m.content))
                    .unwrap_or_default();
                let model_key = model.unwrap_or_else(|| UNKNOWN_MODEL.to_string());
                if !dedup.contains_key(&key) {
                    order.push((key.clone(), std::mem::take(&mut pending)));
                }
                // Last-wins: a later streaming rewrite of the same requestId
                // replaces the earlier record.
                dedup.insert(
                    key,
                    AssistantRec {
                        model: model_key,
                        usage,
                        is_sidechain: raw.is_sidechain,
                        tool_uses,
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
            Some("attachment") => {
                // Weight by line length: when several injections land between
                // the same pair of assistant messages their shared cache write
                // splits by byte share. It is the one approximation in the
                // ledger, and it only ever redistributes *within* a single
                // message's write — the session total stays exact.
                let kind = raw
                    .attachment
                    .as_ref()
                    .and_then(|a| a.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("attachment:unknown")
                    .to_string();
                // The attachment payload, NOT the whole line: every record
                // carries ~150 bytes of envelope (timestamp, sessionId, uuid)
                // that never reaches the model. Charging by line length gave a
                // 30-byte date_change 29% of a write it shared with a 200-byte
                // hook injection.
                let weight = raw
                    .attachment
                    .as_ref()
                    .map(|a| serde_json::to_string(a).map(|s| s.len()).unwrap_or(0))
                    .unwrap_or(0) as u64;
                pending.push((kind, weight));
            }
            _ => { /* summary / queue-operation / unknown: ignore */ }
        }
    }

    let mut models: BTreeMap<String, ModelTokens> = BTreeMap::new();
    let mut sidechain = ModelTokens::default();
    let mut tool_use_counts: BTreeMap<String, u64> = BTreeMap::new();
    for rec in dedup.values() {
        models
            .entry(rec.model.clone())
            .or_default()
            .add_usage(&rec.usage);
        if rec.is_sidechain {
            sidechain.add_usage(&rec.usage);
        }
        for name in &rec.tool_uses {
            *tool_use_counts.entry(name.clone()).or_insert(0) += 1;
        }
    }

    let context = attribute_context(&order, &dedup);
    let client = most_common(&entrypoint_counts);
    let interface = client
        .as_deref()
        .map(crate::sources::classify_claude_entrypoint)
        .unwrap_or(crate::sources::Interface::Unknown);

    Ok(SessionParse {
        session_id,
        source: crate::sources::SourceKind::ClaudeCode.as_str().to_string(),
        interface: interface.as_str().to_string(),
        client,
        project_path: most_common(&cwd_counts),
        git_branch: most_common(&branch_counts),
        first_ts,
        last_ts,
        n_assistant_msgs: dedup.len() as u64,
        models,
        n_user_msgs,
        n_tool_results,
        sidechain,
        tool_use_counts,
        context,
        parse_errors,
    })
}

/// Replay a session in order and charge every cache-write token to what caused
/// it.
///
/// Three buckets, in the order the rules fire:
///
/// 1. The **first** assistant message is [`CTX_FLOOR`] — system prompt, tool
///    definitions, memory. It is charged whole even when attachments preceded
///    it, because those attachments *are* part of what the session opens with.
/// 2. Each injection pending since the previous message is charged **at most
///    what it contains** ([`BYTES_PER_TOKEN`]), never the whole write.
/// 3. [`CTX_CONVERSATION`] takes the residual: the prompt, the tool results,
///    the file reads — everything in the same write that was not an injection.
///
/// Rule 2 is the load-bearing one. A cache write following an injection is not
/// caused by the injection alone; it carries the user's turn as well. Charging
/// the whole write to whatever happened to precede it made injections absorb
/// conversation growth, and the error was not subtle: a `date_change` notice
/// totalling 480 bytes was charged 1.1M tokens.
///
/// Precision differs per bucket, and the UI should not pretend otherwise. Floor
/// and conversation are **exact** (a measured write and an exact residual);
/// the injection figures are a **bounded estimate**. What stays exact either
/// way is the total: `sum(context) == sum(input + cache_creation)`, so the
/// ledger still reconciles against `session_models`.
fn attribute_context(
    order: &[(String, Vec<(String, u64)>)],
    dedup: &HashMap<String, AssistantRec>,
) -> BTreeMap<String, ContextTokens> {
    let mut out: BTreeMap<String, ContextTokens> = BTreeMap::new();
    let mut charge = |kind: &str, tokens: u64| {
        let e = out.entry(kind.to_string()).or_default();
        e.tokens += tokens;
        e.n += 1;
    };
    for (i, (key, pend)) in order.iter().enumerate() {
        let Some(rec) = dedup.get(key) else { continue };
        let written = rec.usage.input_tokens.unwrap_or(0)
            + rec.usage.cache_creation_input_tokens.unwrap_or(0);
        if i == 0 {
            // Same bounded-by-content rule as injections, but the residual is
            // the floor rather than the conversation: whatever the logged
            // components don't explain is the system prompt, the tool schemas
            // and memory, none of which appear in the log at all.
            let mut assigned = 0u64;
            for (kind, bytes) in pend {
                let remaining = written - assigned;
                if remaining == 0 {
                    break;
                }
                let est = (bytes / BYTES_PER_TOKEN).min(remaining);
                if est > 0 {
                    charge(&format!("{CTX_FLOOR_PREFIX}{kind}"), est);
                    assigned += est;
                }
            }
            charge(CTX_FLOOR, written - assigned);
            continue;
        }
        if written == 0 {
            continue;
        }
        // Each injection takes what it contains, capped by what is left of this
        // write (several large injections before one small write cannot invent
        // tokens). Order is file order, so the charge is deterministic.
        let mut assigned = 0u64;
        for (kind, bytes) in pend {
            let remaining = written - assigned;
            if remaining == 0 {
                break;
            }
            let est = (bytes / BYTES_PER_TOKEN).min(remaining);
            if est > 0 {
                charge(kind, est);
                assigned += est;
            }
        }
        // The residual is the work: the prompt and tool results that shared
        // this write. Also the whole write when nothing was pending.
        if written > assigned {
            charge(CTX_CONVERSATION, written - assigned);
        }
    }
    out
}

/// Extract Sweep-relevant `tool_use` names from an assistant message's content:
/// MCP tool invocations (`mcp__<server>__<tool>`) and `Skill`. Other tool names
/// are ignored so the per-session table stays tiny.
fn sweep_tool_use_names(content: &Option<serde_json::Value>) -> Vec<String> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
            if name.starts_with("mcp__") || name == "Skill" {
                names.push(name.to_string());
            }
        }
    }
    names
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
