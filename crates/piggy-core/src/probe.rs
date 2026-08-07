//! The **manifest probe**: what an MCP server's tool schemas actually cost.
//!
//! [`crate::sweep`] can only guess an MCP server's context cost from the size of
//! its config, and says so. The real number is the tool-schema manifest the
//! server hands the client at startup, which is invisible until something
//! connects. This module connects: it spawns the configured command, speaks
//! JSON-RPC 2.0 over stdio (`initialize`, `notifications/initialized`,
//! `tools/list`), measures the serialized tool array, and records it in
//! `mcp_manifests`.
//!
//! The rules it lives by:
//!
//! * **User-initiated only.** Nothing here runs from the watcher or the daemon.
//!   These are commands Claude Code already launches in every session, so the
//!   probe adds no new trust grant, but Piggy still refuses to execute anything
//!   without an explicit click (or `piggy probe --all --yes`).
//! * **Bounded.** One [`PROBE_TIMEOUT`] wall-clock budget, [`STDOUT_CAP_BYTES`]
//!   of output, a ceiling on the server-to-client requests Piggy will answer
//!   (the budget covers reads, so what Piggy *writes* has to be bounded by
//!   count), no retries, and the child is killed and reaped on every path out of
//!   the probe.
//! * **Quiet about secrets.** Configured env values go into
//!   [`ConfiguredServer::config_hash`] and nowhere else: error strings and
//!   captured stderr are scrubbed to `KEY=<redacted>` before anything is
//!   returned, printed, or stored.
//! * **http/sse deferred** (an M5 non-goal, auth complexity): those servers are
//!   listed and skipped, never measured, and never given a row - a row would be
//!   a measurement Piggy did not make.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config;
use crate::store::{McpManifest, Store, SCOPE_USER};

/// Wall-clock budget for one probe: spawn, `initialize`, and every `tools/list`
/// page together. There are no retries - a server that cannot answer in ten
/// seconds is reported, not nagged.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How much stdout Piggy will read from one server before giving up on it. A
/// tool manifest is kilobytes; anything past this is a server streaming
/// something else at us.
pub const STDOUT_CAP_BYTES: usize = 2 * 1024 * 1024;

/// How much stderr is kept for error context (scrubbed before it is used).
pub const STDERR_CAP_BYTES: usize = 8 * 1024;

/// The MCP revision Piggy asks for. Servers answer with whatever revision they
/// speak and Piggy accepts it: the payload being measured (`tools/list`) has
/// been stable across revisions, so refusing a mismatch would measure fewer
/// servers and learn nothing.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Bytes per token for the default [`BytesEstimate`] count.
pub const BYTES_PER_TOKEN: f64 = 3.5;

/// `mcp_manifests.tokenizer` value written by [`BytesEstimate`]. It names the
/// method so the UI can badge the count as an estimate rather than a measurement.
pub const TOKENIZER_BYTES_ESTIMATE: &str = "est-bytes/3.5";

/// Ceiling on `tools/list` pages followed, so a server whose `nextCursor` never
/// settles fails with a clear reason instead of burning the whole budget.
const MAX_TOOL_PAGES: u32 = 100;

/// Ceiling on server-to-client requests Piggy will refuse before it gives up on
/// the server, counted across the whole [`Session`] rather than per request.
///
/// Every refusal is a line Piggy *writes*, and a server that stops reading its
/// stdin cannot be made to drain them. Once the pipe fills, `write_all` parks in
/// the kernel with no deadline (the budget is only checked while reading), so an
/// unbounded refusal loop is an unbounded hang with an orphaned server process
/// on the end of it. 32 refusals is about 3.8 KB; adding `initialize` and
/// [`MAX_TOOL_PAGES`] pages (each drained before the next is written, since a
/// server has to read a request to answer it) keeps everything Piggy can have
/// outstanding at once under 9 KB, inside the smallest 16 KiB pipe buffer on any
/// platform Piggy runs on. A server needing a 33rd answer is not going to
/// produce a tool list.
const MAX_SERVER_REQUESTS: u32 = 32;

/// Ceiling on a `nextCursor` Piggy will echo back in the next `tools/list`.
///
/// A cursor is an opaque resumption token of a few dozen bytes; this is three
/// orders of magnitude of headroom. It is a bound rather than a courtesy because
/// the cursor is the one part of a request Piggy writes that the *server* sizes,
/// and a megabyte of it (a line under [`STDOUT_CAP_BYTES`] is allowed to be that
/// big) would overflow the pipe on its own and hang the same write
/// [`MAX_SERVER_REQUESTS`] exists to keep bounded.
const MAX_CURSOR_BYTES: usize = 4 * 1024;

/// Shortest env value scrubbed from free text. Below this a value is
/// indistinguishable from an ordinary word (`"1"`, `"dev"`), and scrubbing it
/// would shred unrelated output; Piggy never prints env values itself, so the
/// only text at issue is what the server chose to print.
const MIN_REDACT_LEN: usize = 4;

/// Domain separator so a config hash can never collide with another sha256 use.
const CONFIG_HASH_DOMAIN: &[u8] = b"piggy/mcp-config/v1\n";

// ---------------------------------------------------------------------------
// The tokenizer seam
// ---------------------------------------------------------------------------

/// Turns the measured schema JSON into a token count.
///
/// The shipped default is [`BytesEstimate`], which divides by
/// [`BYTES_PER_TOKEN`] and labels itself an estimate. M5.4 injects the advisor's
/// real tokenizer by implementing this trait over the loaded model and passing
/// it in [`ProbeOptions::tokenizer`] - which is why nothing in this module (or
/// in the default build) links llama.
pub trait SchemaTokenizer {
    /// Tokens in `text`.
    fn count(&self, text: &str) -> i64;
    /// The `mcp_manifests.tokenizer` value that labels this count: a method name
    /// for an approximation, a model id for a real tokenizer.
    fn label(&self) -> String;
}

/// The dependency-free default: bytes / [`BYTES_PER_TOKEN`], rounded.
#[derive(Debug, Clone, Copy, Default)]
pub struct BytesEstimate;

impl SchemaTokenizer for BytesEstimate {
    fn count(&self, text: &str) -> i64 {
        (text.len() as f64 / BYTES_PER_TOKEN).round() as i64
    }
    fn label(&self) -> String {
        TOKENIZER_BYTES_ESTIMATE.to_string()
    }
}

/// Knobs for one probe run. [`Default`] is the shipped configuration; tests
/// shorten the timeout and M5.4 swaps the tokenizer.
pub struct ProbeOptions<'a> {
    /// Wall-clock budget for the whole exchange.
    pub timeout: Duration,
    /// Cumulative stdout cap.
    pub stdout_cap: usize,
    /// How `schema_bytes` becomes `schema_tokens`.
    pub tokenizer: &'a dyn SchemaTokenizer,
}

impl Default for ProbeOptions<'_> {
    fn default() -> Self {
        ProbeOptions {
            timeout: PROBE_TIMEOUT,
            stdout_cap: STDOUT_CAP_BYTES,
            tokenizer: &BytesEstimate,
        }
    }
}

// ---------------------------------------------------------------------------
// What is configured
// ---------------------------------------------------------------------------

/// How Claude Code reaches a configured server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A `command` Piggy can spawn and talk to over stdin/stdout: what the probe
    /// measures.
    Stdio,
    /// A `url` server (`type: "http"` or `"sse"`). Deferred in v1: authenticating
    /// as the user is a different problem, so these keep sweep's heuristic and
    /// its label.
    Remote,
}

impl Transport {
    /// Lowercase label for output.
    pub fn label(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Remote => "remote",
        }
    }
}

/// One MCP server exactly as `~/.claude.json` configures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredServer {
    /// The key it appears under in an `mcpServers` map.
    pub key: String,
    /// The project it is configured under, or `None` at user scope - the copy
    /// every session loads. Mirrors [`crate::sweep::SweepItem::source`].
    pub project: Option<String>,
    pub transport: Transport,
    /// The raw config object, untouched.
    pub config: Value,
}

impl ConfiguredServer {
    /// The `mcp_manifests.scope` value for this server: the project path, or
    /// [`SCOPE_USER`] at user scope.
    pub fn scope(&self) -> &str {
        self.project.as_deref().unwrap_or(SCOPE_USER)
    }

    /// The executable to spawn, when there is one.
    pub fn command(&self) -> Option<&str> {
        self.config.get("command").and_then(Value::as_str)
    }

    /// Configured arguments, in order. Non-string scalars are rendered as their
    /// JSON text (an unquoted port number is still an argument); arrays and
    /// objects cannot be arguments and are dropped.
    pub fn args(&self) -> Vec<String> {
        self.config
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(scalar_arg).collect())
            .unwrap_or_default()
    }

    /// Stable fingerprint of what this server *runs*: command, args, and env
    /// pairs.
    ///
    /// Env **values** are hashed (a rotated secret is a different server, and a
    /// measurement taken with the old one should not be trusted) but are stored
    /// nowhere - the hash is one-way, and every other path out of this module is
    /// redacted. Env is hashed in sorted-key order, so re-ordering the JSON map
    /// does not invalidate a good measurement. Nothing else in the config counts:
    /// an unrelated key cannot change what the server answers.
    pub fn config_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(CONFIG_HASH_DOMAIN);
        h.update(self.command().unwrap_or_default().as_bytes());
        h.update([0]);
        for a in self.args() {
            h.update(a.as_bytes());
            h.update([0]);
        }
        h.update([1]);
        for (k, v) in self.env_pairs() {
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
            h.update([0]);
        }
        format!("{:x}", h.finalize())
    }

    /// Configured env pairs, sorted by key. Private on purpose: values leave this
    /// module only as a hash.
    fn env_pairs(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(env) = self.config.get("env").and_then(Value::as_object) {
            for (k, v) in env {
                if let Some(s) = scalar_arg(v) {
                    out.insert(k.clone(), s);
                }
            }
        }
        out
    }
}

/// A config value usable as an argument or an env value: a string as-is, another
/// scalar as its JSON text, anything structured not at all.
fn scalar_arg(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    }
}

/// Every server configured in `root` (a parsed `~/.claude.json`): the top-level
/// `mcpServers` first, then each `projects.<path>.mcpServers`, in file order.
///
/// Shared with [`crate::sweep`] so the probe and the sweep can never disagree
/// about what is configured. A key configured in several scopes is returned once
/// per scope - they are distinct rows (`mcp_manifests` is keyed by server *and*
/// scope) and distinct costs. Folding them is the caller's call; sweep reports
/// the first.
pub fn servers_from_root(root: &Value) -> Vec<ConfiguredServer> {
    let mut out = Vec::new();
    for (key, cfg) in root
        .get("mcpServers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        out.push(configured(key, cfg, None));
    }
    if let Some(projects) = root.get("projects").and_then(Value::as_object) {
        for (proj_path, proj) in projects {
            let Some(servers) = proj.get("mcpServers").and_then(Value::as_object) else {
                continue;
            };
            for (key, cfg) in servers {
                out.push(configured(key, cfg, Some(proj_path.clone())));
            }
        }
    }
    out
}

/// Every server configured in `~/.claude.json`. A missing file is not an error:
/// it means no servers.
pub fn configured_servers() -> Result<Vec<ConfiguredServer>> {
    let path = config::claude_json_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let root: Value = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(servers_from_root(&root))
}

fn configured(key: &str, cfg: &Value, project: Option<String>) -> ConfiguredServer {
    ConfiguredServer {
        key: key.to_string(),
        project,
        transport: transport_of(cfg),
        config: cfg.clone(),
    }
}

/// Classify a server config. An explicit `type`/`transport` of `http` or `sse`
/// wins, then the presence of a `url`; everything else is treated as stdio, so a
/// config with neither `command` nor `url` fails loudly (with a row saying so)
/// rather than being silently filed under a transport it does not use.
fn transport_of(cfg: &Value) -> Transport {
    let declared = cfg
        .get("type")
        .or_else(|| cfg.get("transport"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match declared.as_str() {
        "http" | "sse" | "streamable-http" | "websocket" | "ws" => Transport::Remote,
        "stdio" => Transport::Stdio,
        _ if cfg.get("url").is_some() => Transport::Remote,
        _ => Transport::Stdio,
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// What one successful probe measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    pub tool_count: i64,
    /// UTF-8 length of the serialized `tools` array: the payload the client puts
    /// in the context window.
    pub schema_bytes: i64,
    pub schema_tokens: i64,
    /// Which [`SchemaTokenizer`] produced `schema_tokens`.
    pub tokenizer: String,
}

/// Why a probe produced no measurement.
///
/// Every variant's text is safe to store and print: env values are scrubbed out
/// before the error leaves [`measure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// Nothing to spawn: an http/sse server, or a config with no `command`.
    NotSpawnable(String),
    /// The command exists in the config but would not start.
    Spawn(String),
    /// The budget ran out; the child was killed.
    Timeout(Duration),
    /// The server wrote more than the cap without finishing; the child was killed.
    StdoutCap(usize),
    /// Something on stdout was not JSON. MCP's stdio framing is newline-delimited
    /// JSON, so a banner line is a protocol violation, not noise to skip.
    Parse(String),
    /// Well-formed JSON-RPC that does not answer the question: an error reply, a
    /// result with no tool list, an endless cursor.
    Protocol(String),
    /// The server stopped talking before answering.
    Exited(String),
}

impl ProbeError {
    /// Stable machine-readable tag for `--json` output and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            ProbeError::NotSpawnable(_) => "not-spawnable",
            ProbeError::Spawn(_) => "spawn",
            ProbeError::Timeout(_) => "timeout",
            ProbeError::StdoutCap(_) => "stdout-cap",
            ProbeError::Parse(_) => "parse",
            ProbeError::Protocol(_) => "protocol",
            ProbeError::Exited(_) => "exited",
        }
    }

    /// The same error with every configured env value replaced. Applied once, at
    /// the boundary, so no unredacted `ProbeError` can escape the module.
    fn scrubbed(self, red: &Redactor) -> ProbeError {
        match self {
            ProbeError::NotSpawnable(m) => ProbeError::NotSpawnable(red.scrub(&m)),
            ProbeError::Spawn(m) => ProbeError::Spawn(red.scrub(&m)),
            ProbeError::Parse(m) => ProbeError::Parse(red.scrub(&m)),
            ProbeError::Protocol(m) => ProbeError::Protocol(red.scrub(&m)),
            ProbeError::Exited(m) => ProbeError::Exited(red.scrub(&m)),
            other => other,
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::NotSpawnable(m) => write!(f, "nothing to launch: {m}"),
            ProbeError::Spawn(m) => write!(f, "could not start the server: {m}"),
            ProbeError::Timeout(d) => write!(
                f,
                "timed out after {}s with no answer; the server was stopped",
                d.as_secs_f64().round() as u64
            ),
            ProbeError::StdoutCap(n) => write!(
                f,
                "the server wrote more than {n} bytes without finishing; it was stopped"
            ),
            ProbeError::Parse(m) => write!(f, "the server wrote something that is not JSON: {m}"),
            ProbeError::Protocol(m) => write!(f, "the server answered, but not usefully: {m}"),
            ProbeError::Exited(m) => write!(f, "the server stopped before answering{m}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Where one configured server stands: what the CLI listing and the app's
/// per-server button read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeasurementStatus {
    /// http/sse: not probed in v1, so there is deliberately nothing stored.
    Deferred,
    /// Never probed.
    Never,
    /// Probed, but against a different command/args/env. Not a measurement of
    /// what runs today, so consumers treat it as unmeasured.
    Stale(McpManifest),
    /// Measured against the current config.
    Measured(McpManifest),
    /// Probed against the current config and could not be measured; the row says
    /// why.
    Failed(McpManifest),
}

impl MeasurementStatus {
    /// Stable machine-readable tag for `--json` output.
    pub fn tag(&self) -> &'static str {
        match self {
            MeasurementStatus::Deferred => "deferred",
            MeasurementStatus::Never => "never",
            MeasurementStatus::Stale(_) => "stale",
            MeasurementStatus::Measured(_) => "measured",
            MeasurementStatus::Failed(_) => "failed",
        }
    }

    /// The stored row behind this status, if there is one.
    pub fn manifest(&self) -> Option<&McpManifest> {
        match self {
            MeasurementStatus::Deferred | MeasurementStatus::Never => None,
            MeasurementStatus::Stale(m)
            | MeasurementStatus::Measured(m)
            | MeasurementStatus::Failed(m) => Some(m),
        }
    }
}

/// Where `server` stands, given every stored row (load them once with
/// [`Store::mcp_manifests`]: a listing wants one query, not one per server).
pub fn status(manifests: &[McpManifest], server: &ConfiguredServer) -> MeasurementStatus {
    if server.transport == Transport::Remote {
        return MeasurementStatus::Deferred;
    }
    let Some(row) = stored_row(manifests, server) else {
        return MeasurementStatus::Never;
    };
    if row.config_hash != server.config_hash() {
        return MeasurementStatus::Stale(row.clone());
    }
    if row.ok {
        MeasurementStatus::Measured(row.clone())
    } else {
        MeasurementStatus::Failed(row.clone())
    }
}

/// The stored row for `server`, whatever state it is in.
fn stored_row<'a>(
    manifests: &'a [McpManifest],
    server: &ConfiguredServer,
) -> Option<&'a McpManifest> {
    let scope = server.scope();
    manifests
        .iter()
        .find(|m| m.server_key == server.key && m.scope == scope)
}

/// The stored row that measured *this* server's current config successfully.
/// `None` means "not measured" - a stale or failed row is not a number anyone
/// may quote, and the caller keeps its own estimate.
///
/// The whole row, not just `schema_tokens`, and deliberately so: `tokenizer`
/// says whether the count is a real tokenization or the [`BytesEstimate`]
/// divisor, and a caller that took the number alone had no way to tell. Handing
/// back only the figure is what let sweep print an estimate without its tilde.
pub fn measured_manifest<'a>(
    manifests: &'a [McpManifest],
    server: &ConfiguredServer,
) -> Option<&'a McpManifest> {
    if server.transport == Transport::Remote {
        return None;
    }
    let row = stored_row(manifests, server)?;
    (row.ok && row.config_hash == server.config_hash()).then_some(row)
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

/// Probe one server and record the result in `mcp_manifests`.
///
/// `Ok(None)` means the server was deferred (http/sse) and nothing was launched
/// or written. A server that fails to answer *is* written, with `ok = false` and
/// a redacted reason, so the UI can say what went wrong instead of showing a
/// blank where a number should be.
pub fn probe(
    store: &mut Store,
    server: &ConfiguredServer,
    opts: &ProbeOptions,
) -> Result<Option<McpManifest>> {
    if server.transport == Transport::Remote {
        return Ok(None);
    }
    let row = manifest_row(server, measure(server, opts), opts);
    store.upsert_mcp_manifest(&row)?;
    Ok(Some(row))
}

/// Probe every server in `servers`, in order, recording each result.
///
/// The returned vector is index-aligned with `servers`; a `None` entry is a
/// deferred (http/sse) server. One server failing never stops the run: a failure
/// is a stored `ok = false` row like any other.
pub fn probe_all(
    store: &mut Store,
    servers: &[ConfiguredServer],
    opts: &ProbeOptions,
) -> Result<Vec<Option<McpManifest>>> {
    let mut out = Vec::with_capacity(servers.len());
    for server in servers {
        out.push(probe(store, server, opts)?);
    }
    Ok(out)
}

/// Launch `server`, speak MCP, and measure its tool manifest. Stores nothing.
///
/// Always kills and reaps the child, on every path out including a panic in the
/// caller ([`Session`] does it from `Drop`).
pub fn measure(server: &ConfiguredServer, opts: &ProbeOptions) -> Result<Measurement, ProbeError> {
    let red = Redactor::new(server);
    run(server, opts).map_err(|e| e.scrubbed(&red))
}

/// The measurement, before redaction. Every `?` in here can carry an env value
/// (a server is free to print one), which is why [`measure`] is the only public
/// door.
fn run(server: &ConfiguredServer, opts: &ProbeOptions) -> Result<Measurement, ProbeError> {
    if server.transport == Transport::Remote {
        return Err(ProbeError::NotSpawnable(format!(
            "'{}' is an http/sse server; those are not probed in v1",
            server.key
        )));
    }
    let Some(command) = server.command().filter(|c| !c.is_empty()) else {
        return Err(ProbeError::NotSpawnable(format!(
            "'{}' has no `command` in ~/.claude.json",
            server.key
        )));
    };

    let mut session = Session::start(server, command, opts)?;
    // The server answers with whatever revision it speaks and we take it: the
    // tool list is what we are here for, and it has outlived every revision.
    session.request(
        1,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "piggy", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    session.notify("notifications/initialized")?;

    let mut tools: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0u32;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let result = session.request(2 + page as u64, "tools/list", params)?;
        let Some(arr) = result.get("tools").and_then(Value::as_array) else {
            return Err(ProbeError::Protocol(
                "tools/list answered without a `tools` array".to_string(),
            ));
        };
        tools.extend(arr.iter().cloned());
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        if let Some(c) = &cursor {
            if c.len() > MAX_CURSOR_BYTES {
                return Err(ProbeError::Protocol(format!(
                    "tools/list answered with a {} byte nextCursor; \
                     piggy will not echo back more than {MAX_CURSOR_BYTES}",
                    c.len()
                )));
            }
        }
        if cursor.is_none() {
            break;
        }
        page += 1;
        if page >= MAX_TOOL_PAGES {
            return Err(ProbeError::Protocol(format!(
                "tools/list kept paginating past {MAX_TOOL_PAGES} pages"
            )));
        }
    }

    // The measured payload is the tool array itself: what the client actually
    // carries, not the transport frame around it.
    let tool_count = tools.len() as i64;
    let payload = serde_json::to_string(&Value::Array(tools))
        .map_err(|e| ProbeError::Protocol(format!("re-serializing the tool list: {e}")))?;
    Ok(Measurement {
        tool_count,
        schema_bytes: payload.len() as i64,
        schema_tokens: opts.tokenizer.count(&payload),
        tokenizer: opts.tokenizer.label(),
    })
}

/// Build the row for one probe outcome. A failure is a row too: `ok = false`
/// plus the (already redacted) reason.
fn manifest_row(
    server: &ConfiguredServer,
    outcome: Result<Measurement, ProbeError>,
    opts: &ProbeOptions,
) -> McpManifest {
    let base = McpManifest {
        server_key: server.key.clone(),
        scope: server.scope().to_string(),
        config_hash: server.config_hash(),
        tool_count: 0,
        schema_bytes: 0,
        schema_tokens: 0,
        tokenizer: opts.tokenizer.label(),
        measured_at: chrono::Utc::now().to_rfc3339(),
        ok: false,
        error: None,
    };
    match outcome {
        Ok(m) => McpManifest {
            tool_count: m.tool_count,
            schema_bytes: m.schema_bytes,
            schema_tokens: m.schema_tokens,
            tokenizer: m.tokenizer,
            ok: true,
            ..base
        },
        Err(e) => McpManifest {
            error: Some(e.to_string()),
            ..base
        },
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Replaces configured env **values** with `KEY=<redacted>` wherever they turn
/// up in text Piggy is about to return, print, or store.
///
/// The values themselves reach only [`ConfiguredServer::config_hash`]. This
/// exists for the other direction: a server that prints its own token to stderr,
/// or quotes it back in an error, must not get it written into `mcp_manifests`.
struct Redactor {
    /// (key, value), longest value first so a value containing another is
    /// replaced whole rather than in pieces.
    pairs: Vec<(String, String)>,
}

impl Redactor {
    fn new(server: &ConfiguredServer) -> Self {
        let mut pairs: Vec<(String, String)> = server
            .env_pairs()
            .into_iter()
            .filter(|(_, v)| v.len() >= MIN_REDACT_LEN)
            .collect();
        pairs.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
        Redactor { pairs }
    }

    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (key, value) in &self.pairs {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("{key}=<redacted>"));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The stdio conversation
// ---------------------------------------------------------------------------

/// What the stdout reader thread hands back.
enum ReadEvent {
    /// One newline-delimited message (MCP's stdio framing).
    Line(String),
    /// The cumulative stdout cap was reached.
    Cap,
    /// The server closed stdout.
    Eof,
}

/// A live server process plus the plumbing to talk to it.
///
/// Dropping it closes stdin, kills the child, and waits for it - so no path out
/// of [`run`], including an early `?` or a panic, can leave a zombie or an
/// orphaned MCP server running.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<ReadEvent>,
    stderr: Arc<Mutex<String>>,
    deadline: Instant,
    timeout: Duration,
    stdout_cap: usize,
    /// Server-to-client requests answered so far, over the whole session.
    ///
    /// On the session and not on [`Session::request`] deliberately: a per-call
    /// counter would hand each of the up-to-[`MAX_TOOL_PAGES`] pages a fresh
    /// budget, which is 100 times the ceiling and back to unbounded for any
    /// practical purpose.
    server_requests: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Close stdin first: a well-behaved server exits on EOF, so the kill is
        // usually a no-op on an already-dead process.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start(
        server: &ConfiguredServer,
        command: &str,
        opts: &ProbeOptions,
    ) -> Result<Session, ProbeError> {
        let mut cmd = Command::new(command);
        cmd.args(server.args());
        // The parent environment is inherited and the configured pairs are layered
        // on top, which is exactly how Claude Code launches these: clearing it
        // would break servers that need PATH or HOME and would measure a process
        // the user never runs.
        for (k, v) in server.env_pairs() {
            cmd.env(k, v);
        }
        // A project-scoped server runs from its project, when that still exists.
        if let Some(dir) = server.project.as_deref().filter(|p| Path::new(p).is_dir()) {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProbeError::Spawn(format!("{command}: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProbeError::Spawn("no stdout pipe".to_string()))?;
        let stderr = child.stderr.take();

        // Stdout reader: bounded by construction (`take`), so a server emitting
        // one endless line cannot grow our memory past the cap.
        let cap = opts.stdout_cap;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout.take(cap as u64 + 1));
            let mut total = 0usize;
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => {
                        let _ = tx.send(if total > cap {
                            ReadEvent::Cap
                        } else {
                            ReadEvent::Eof
                        });
                        return;
                    }
                    Ok(n) => {
                        total += n;
                        if total > cap {
                            let _ = tx.send(ReadEvent::Cap);
                            return;
                        }
                        let line = String::from_utf8_lossy(&buf).trim().to_string();
                        if tx.send(ReadEvent::Line(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(ReadEvent::Eof);
                        return;
                    }
                }
            }
        });

        // Stderr reader: keeps the first STDERR_CAP_BYTES for error context but
        // keeps draining, so a chatty server never blocks on a full pipe.
        let kept = Arc::new(Mutex::new(String::new()));
        if let Some(mut stderr) = stderr {
            let sink = Arc::clone(&kept);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if acc.len() < STDERR_CAP_BYTES {
                                let room = STDERR_CAP_BYTES - acc.len();
                                acc.extend_from_slice(&buf[..n.min(room)]);
                                if let Ok(mut g) = sink.lock() {
                                    *g = String::from_utf8_lossy(&acc).into_owned();
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(Session {
            child,
            stdin,
            lines,
            stderr: kept,
            deadline: Instant::now() + opts.timeout,
            timeout: opts.timeout,
            stdout_cap: opts.stdout_cap,
            server_requests: 0,
        })
    }

    /// Send a request and read until its response arrives, returning `result`.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value, ProbeError> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let msg = self.next_message()?;
            // A server may talk first. A notification (no id) is ignored; a
            // server-to-client *request* gets a refusal, so a server that waits
            // on a client capability Piggy does not have gets an answer instead
            // of deadlocking against our timeout.
            if msg.get("method").is_some() {
                if let Some(rid) = msg.get("id").cloned() {
                    // Bounded, because each refusal is a write and the budget
                    // does not cover writes. See [`MAX_SERVER_REQUESTS`].
                    self.server_requests += 1;
                    if self.server_requests > MAX_SERVER_REQUESTS {
                        return Err(ProbeError::Protocol(format!(
                            "the server asked piggy to do {MAX_SERVER_REQUESTS} things \
                             before answering {method}; piggy is a probe and implements no \
                             client methods, so it stopped replying"
                        )));
                    }
                    self.send(&json!({
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": { "code": -32601, "message": "piggy is a probe and implements no client methods" },
                    }))?;
                }
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // a response to something else; not ours to read
            }
            if let Some(err) = msg.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message");
                return Err(ProbeError::Protocol(format!(
                    "{method} returned error {code}: {}",
                    snippet(text)
                )));
            }
            return match msg.get("result") {
                Some(r) => Ok(r.clone()),
                None => Err(ProbeError::Protocol(format!(
                    "{method} answered with neither a result nor an error"
                ))),
            };
        }
    }

    /// Send a notification (no id, no answer expected).
    fn notify(&mut self, method: &str) -> Result<(), ProbeError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": {} }))
    }

    fn send(&mut self, msg: &Value) -> Result<(), ProbeError> {
        let mut line = serde_json::to_string(msg).unwrap_or_default();
        line.push('\n');
        let write = match self.stdin.as_mut() {
            Some(w) => w.write_all(line.as_bytes()).and_then(|()| w.flush()),
            None => return Err(self.exit_error(" (its input was already closed)")),
        };
        match write {
            Ok(()) => Ok(()),
            // A closed pipe means the child is gone; its stderr says more than
            // the io error does.
            Err(_) => Err(self.exit_error(" (it closed its input)")),
        }
    }

    /// The next parsed message, or the reason there will not be one.
    fn next_message(&mut self) -> Result<Value, ProbeError> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProbeError::Timeout(self.timeout));
            }
            match self.lines.recv_timeout(remaining) {
                Ok(ReadEvent::Line(line)) => {
                    if line.is_empty() {
                        continue;
                    }
                    return serde_json::from_str(&line).map_err(|e| {
                        ProbeError::Parse(format!("{e}, reading: {}", snippet(&line)))
                    });
                }
                Ok(ReadEvent::Cap) => return Err(ProbeError::StdoutCap(self.stdout_cap)),
                Ok(ReadEvent::Eof) => return Err(self.exit_error("")),
                Err(RecvTimeoutError::Timeout) => return Err(ProbeError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => return Err(self.exit_error("")),
            }
        }
    }

    /// "It stopped talking", with whatever the exit status and stderr add.
    fn exit_error(&mut self, context: &str) -> ProbeError {
        let mut msg = context.to_string();
        if let Ok(Some(status)) = self.child.try_wait() {
            match status.code() {
                Some(c) => msg.push_str(&format!(" (exit {c})")),
                None => msg.push_str(" (killed by a signal)"),
            }
        }
        let tail = self
            .stderr
            .lock()
            .map(|g| g.trim().to_string())
            .unwrap_or_default();
        if !tail.is_empty() {
            msg.push_str(&format!(": {}", snippet(&tail)));
        }
        ProbeError::Exited(msg)
    }
}

/// At most 240 characters of `text` on one line, for an error string that has to
/// stay readable in a table cell.
fn snippet(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    match flat.char_indices().nth(240) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat.to_string(),
    }
}
