//! The CLAUDE.md inventory and its deterministic detectors.
//!
//! Claude Code loads a stack of markdown into the front of every session:
//! `~/.claude/CLAUDE.md`, `~/.claude/rules/*.md`, and per project
//! `CLAUDE.md`, `CLAUDE.local.md`, `<project>/.claude/rules/*.md`. None of it is
//! named in the ledger and none of it is measured anywhere else in Piggy, so
//! this module counts it: one row per file, plus findings about what is in them.
//!
//! Two rules shape everything here:
//!
//! * **Contents never enter the database.** A file is read at scan time, sized,
//!   hashed, handed to the detectors, and dropped. `claudemd_files` holds
//!   `(path, project, bytes, est_tokens, hash, mtime_ns, last_scanned)` and
//!   nothing else, so an inventory row cannot leak prose.
//! * **Every token figure is an estimate and says so.** There is no tokenizer on
//!   this path (the advisor's is optional and may not be downloaded), so tokens
//!   are `bytes / 3.5` and the monthly burden multiplies that by *observed*
//!   sessions. Both are labelled estimated wherever they surface.
//!
//! The three detectors are pure functions over [`FileText`], so a fixture drives
//! them with no database in sight: [`dead_refs`], [`duplicate_blocks`],
//! [`oversize`]. [`scan`] is what wires them to the store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config;
use crate::insights::commas;
use crate::settings::hash_bytes;
use crate::stats::Period;
use crate::store::{ClaudemdFile, Store};

/// A UTF-8 byte order mark, stripped before any detector sees the text (the
/// [`crate::settings`] precedent: it is a real cause of Claude Code parse
/// failures, and it is never content).
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Bytes per token, the same rough divisor Sweep uses when no tokenizer is
/// available. Every figure derived from it is labelled estimated.
const BYTES_PER_TOKEN: f64 = 3.5;

/// A single file above this estimated size is a finding: it is paid on every
/// session that loads it, whether or not the session needs any of it. Same
/// spirit as the `floor-component` insight threshold.
pub const OVERSIZE_EST_TOKENS: i64 = 2_000;

/// A paragraph shorter than this (normalized) is not worth reporting as a
/// duplicate: "Run the tests before you commit." appearing in two files is
/// shared boilerplate, not a copy worth deleting.
const DUP_MIN_BYTES: usize = 80;

/// Dead references reported per file. Past this the list stops being a finding
/// and starts being the file, so the rest is reported as a count.
const MAX_DEAD_REFS: usize = 10;

/// Characters that end a path token in prose or markdown rather than belonging
/// to it. `.` is handled separately: it opens `./foo` but closes a sentence.
const TRIM_CHARS: &str = "`'\"*_,;:!?()[]{}<>|";

/// Characters that mean the token is a pattern, a placeholder or a shell
/// fragment, none of which resolves to one file.
const NOT_A_PATH_CHARS: &str = "*?<>{}$|";

/// Extensions that make a separator-less token a file reference. Without this
/// list, `and/or` is a path and every second sentence is a finding.
const PATH_EXTS: &[&str] = &[
    "cjs", "css", "go", "html", "js", "json", "jsonl", "lock", "md", "mjs", "png", "py", "rs", "sh",
    "sql", "svg", "toml", "ts", "tsx", "txt", "yaml", "yml",
];

/// First-segment suffixes that make a token a scheme-less URL (`example.com/x`)
/// rather than a relative path.
const HOST_SUFFIXES: &[&str] = &[".ai", ".com", ".dev", ".io", ".net", ".org", ".sh"];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One file as the detectors see it. Contents live here for the length of a
/// scan and nowhere else.
#[derive(Debug, Clone)]
pub struct FileText {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// The project root this file belongs to, or `None` for a global file under
    /// `~/.claude`. Doubles as the base a relative reference resolves against.
    pub project: Option<String>,
    /// Length of the file on disk, BOM included: it is what `ls` reports and
    /// what [`Self::hash`] covers.
    pub bytes: i64,
    /// sha256 of the bytes exactly as they sit on disk.
    pub hash: String,
    pub mtime_ns: i64,
    /// The text with any BOM stripped: what the model actually reads.
    pub text: String,
    /// Whether the bytes on disk began with a BOM. [`Self::text`] has it
    /// stripped, so anything rebuilding the file from the text (M5.3's
    /// deterministic edits) has to put it back or the rewrite silently changes
    /// the first three bytes.
    pub had_bom: bool,
}

impl FileText {
    /// Estimated tokens for the whole file.
    pub fn est_tokens(&self) -> i64 {
        est_tokens(self.bytes)
    }

    /// The directory a relative reference in this file resolves against: the
    /// project root, or the home directory for a global file. The second is a
    /// fallback rather than a meaning, which is why [`deletable_ref`] will not
    /// delete a line over a relative reference in a global file.
    fn base(&self) -> PathBuf {
        match &self.project {
            Some(p) => PathBuf::from(p),
            None => home_anchor(),
        }
    }
}

/// What a detector found, typed so a caller can act on it rather than parse
/// prose. The strings alongside are the presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// A path the file mentions that is not on disk.
    DeadRef {
        /// The reference exactly as the file writes it.
        reference: String,
        /// Where it resolved to, absolute.
        resolved: String,
        /// Dead references the [`MAX_DEAD_REFS`] cap dropped. Non-zero only on
        /// the last reported finding for a file, so a capped list never reads
        /// as the whole list.
        more: usize,
    },
    /// A normalized paragraph that also appears in another scanned file.
    DuplicateBlock {
        /// The other files carrying the same block, in path order.
        others: Vec<String>,
        /// First 60 characters of the normalized paragraph.
        label: String,
        /// Normalized length of the paragraph.
        bytes: usize,
    },
    /// The file on its own costs more than [`OVERSIZE_EST_TOKENS`] per load.
    Oversize { threshold: i64 },
}

impl FindingKind {
    /// Stable machine name (the `--json` `kind`).
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingKind::DeadRef { .. } => "dead-ref",
            FindingKind::DuplicateBlock { .. } => "duplicate-block",
            FindingKind::Oversize { .. } => "oversize",
        }
    }
}

/// One finding about one file: what it is, what it costs, and what to do.
///
/// Same shape as [`crate::insights::Insight`] - stable id, a claim a person can
/// read, the arithmetic behind it, and an imperative action - with the typed
/// [`FindingKind`] added so M5.3 can generate candidates without re-parsing
/// prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable id (`<kind>:<path>[:<detail>]`), for dismissing and linking.
    pub id: String,
    pub kind: FindingKind,
    /// The file this is about, absolute.
    pub path: String,
    /// Plain-language claim.
    pub claim: String,
    /// The arithmetic, stated so the user can check it.
    pub detail: String,
    /// Estimated tokens this finding accounts for on every load of the file.
    /// Zero for a dead reference: it misdirects the model, it does not cost.
    pub est_tokens: i64,
    /// `est_tokens` x the file's sessions over the last 30 days, filled in by
    /// [`scan`]. Zero from a bare detector call, which has no session data.
    pub est_tokens_month: i64,
    /// The lever. Imperative, specific, and never "consider".
    pub action: String,
}

/// One inventoried file: its row, what it costs per month, and its findings.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// Exactly what was written to `claudemd_files`.
    pub file: ClaudemdFile,
    /// Claude Code sessions over the last 30 days that load this file: the
    /// project's own for a project file, every project's for a global one.
    /// Codex sessions do not load CLAUDE.md, so they are not counted.
    pub sessions_30d: i64,
    /// `est_tokens` x [`Self::sessions_30d`]. Estimated, always.
    pub est_tokens_month: i64,
    pub findings: Vec<Finding>,
}

impl ScannedFile {
    /// `"global"` or `"project"`.
    pub fn scope(&self) -> &'static str {
        match self.file.project {
            Some(_) => "project",
            None => "global",
        }
    }
}

/// The whole inventory and everything the detectors said about it.
#[derive(Debug, Clone, Default)]
pub struct ClaudemdReport {
    /// One entry per file found on disk: the global files first, then each
    /// project's, every group in path order (the order [`candidates`] walks).
    pub files: Vec<ScannedFile>,
    /// Inventory rows dropped because the file is gone.
    pub removed: Vec<String>,
    /// Files that could not be read, with the reason. A scan never aborts on
    /// one bad file: the other twenty are still worth counting.
    pub warnings: Vec<String>,
}

impl ClaudemdReport {
    /// Estimated tokens across every inventoried file (one load of each).
    pub fn est_tokens(&self) -> i64 {
        self.files.iter().map(|f| f.file.est_tokens).sum()
    }

    /// Estimated tokens per month across every inventoried file.
    pub fn est_tokens_month(&self) -> i64 {
        self.files.iter().map(|f| f.est_tokens_month).sum()
    }

    /// Every finding, file order then finding order.
    pub fn findings(&self) -> impl Iterator<Item = &Finding> {
        self.files.iter().flat_map(|f| f.findings.iter())
    }
}

/// The MCP servers each project configures in its own `.mcp.json`.
#[derive(Debug, Clone, Default)]
pub struct ProjectMcpServers {
    /// Project path -> server names, sorted.
    pub by_project: BTreeMap<String, Vec<String>>,
    /// `.mcp.json` files that could not be read or parsed, with the reason.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

/// Inventory every CLAUDE.md-family file, refresh `claudemd_files`, and run the
/// detectors.
///
/// Writes exactly two kinds of change: an upsert per file found, and a delete
/// per row whose file is gone. Findings are returned, never stored - they are
/// derived from content, and content does not live in the database.
pub fn scan(store: &mut Store) -> Result<ClaudemdReport> {
    let mut report = ClaudemdReport::default();

    // Read every candidate first, so the cross-file duplicate detector sees the
    // whole set and the DB is touched once per file.
    let mut texts: Vec<FileText> = Vec::new();
    for (path, project) in candidates(store)? {
        match read_file_text(&path, project) {
            Ok(t) => texts.push(t),
            Err(e) => report.warnings.push(format!("{e:#}")),
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let counts = store.session_counts_since(Period::Month.cutoff().as_deref())?;

    // Detectors: per-file ones first, then the cross-file one, so each file's
    // findings arrive in a stable kind order.
    let mut by_path: BTreeMap<String, Vec<Finding>> = BTreeMap::new();
    for t in &texts {
        let key = t.path.to_string_lossy().into_owned();
        let entry = by_path.entry(key).or_default();
        entry.extend(oversize(t));
        entry.extend(dead_refs(t));
    }
    for f in duplicate_blocks(&texts) {
        by_path.entry(f.path.clone()).or_default().push(f);
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for t in &texts {
        let path = t.path.to_string_lossy().into_owned();
        let row = ClaudemdFile {
            path: path.clone(),
            project: t.project.clone(),
            bytes: t.bytes,
            est_tokens: t.est_tokens(),
            hash: t.hash.clone(),
            mtime_ns: t.mtime_ns,
            last_scanned: now.clone(),
        };
        store.upsert_claudemd_file(&row)?;
        seen.insert(path.clone());

        // A global file is loaded by every Claude Code session; a project file
        // only by that project's. Codex sessions are not in either count: they
        // never load a CLAUDE.md. Both counts are observed, only the
        // multiplication is an estimate.
        let sessions_30d = match &t.project {
            Some(p) => counts.by_project.get(p).copied().unwrap_or(0),
            None => counts.total,
        };
        let mut findings = by_path.remove(&path).unwrap_or_default();
        for f in &mut findings {
            f.est_tokens_month = f.est_tokens * sessions_30d;
        }
        report.files.push(ScannedFile {
            est_tokens_month: row.est_tokens * sessions_30d,
            file: row,
            sessions_30d,
            findings,
        });
    }

    // A row whose file is gone is dropped. A row for a file that still exists
    // but is no longer a candidate (its project left the sessions table) is
    // left alone: it is stale, not wrong, and deleting it would throw away the
    // last thing Piggy knew about a file that is still on disk.
    for row in store.claudemd_files()? {
        if !seen.contains(&row.path) && !Path::new(&row.path).exists() {
            store.delete_claudemd_file(&row.path)?;
            report.removed.push(row.path);
        }
    }
    Ok(report)
}

/// Every file the scan looks for, in (global, then project) path order.
///
/// Paths are deduplicated: a project rooted at the home directory would
/// otherwise claim a global file twice, and the first claim (global) is the
/// right one.
fn candidates(store: &Store) -> Result<Vec<(PathBuf, Option<String>)>> {
    let claude = config::claude_dir();
    let mut out: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    {
        let mut add = |path: PathBuf, project: Option<String>| {
            if path.is_file() && seen.insert(path.clone()) {
                out.push((path, project));
            }
        };
        add(claude.join("CLAUDE.md"), None);
        for p in md_files_in(&claude.join("rules")) {
            add(p, None);
        }
        for project in store.session_projects()? {
            let root = PathBuf::from(&project);
            add(root.join("CLAUDE.md"), Some(project.clone()));
            add(root.join("CLAUDE.local.md"), Some(project.clone()));
            for p in md_files_in(&root.join(".claude").join("rules")) {
                add(p, Some(project.clone()));
            }
        }
    }
    Ok(out)
}

/// The `*.md` files directly in `dir` (not recursive - the spec's glob is one
/// level), sorted. A missing or unreadable directory yields nothing.
fn md_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
        .collect();
    out.sort();
    out
}

/// Read one file as text: BOM stripped, contents kept in memory only.
///
/// A file that is not UTF-8 is an error rather than lossy text: a detector
/// running on replacement characters would report references nobody wrote.
pub fn read_file_text(path: &Path, project: Option<String>) -> Result<FileText> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let body = raw.strip_prefix(&BOM).unwrap_or(&raw);
    let had_bom = body.len() != raw.len();
    let text = std::str::from_utf8(body)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?
        .to_string();
    Ok(FileText {
        path: path.to_path_buf(),
        project,
        bytes: raw.len() as i64,
        // Hashed as it sits on disk, BOM and all, so the hash an inventory row
        // carries is the one `crate::snapshots::check_unchanged` computes when
        // an edit is applied against it.
        hash: hash_bytes(&raw),
        mtime_ns: mtime_ns(path),
        text,
        had_bom,
    })
}

/// Estimated tokens for `bytes`, the one place the divisor is applied.
pub fn est_tokens(bytes: i64) -> i64 {
    (bytes as f64 / BYTES_PER_TOKEN).round() as i64
}

fn mtime_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// The user's home directory, reached through the one path helper a test can
/// override: `claude_dir()` is `<home>/.claude`, so its parent is the home a
/// sandboxed test controls with `PIGGY_CLAUDE_DIR`.
fn home_anchor() -> PathBuf {
    let claude = config::claude_dir();
    match claude.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

// ---------------------------------------------------------------------------
// Detector: dead references
// ---------------------------------------------------------------------------

/// One reference that does not resolve, and where in the file it sits.
///
/// The uncapped, per-occurrence view behind [`dead_refs`]: the finding list is
/// for reading (deduplicated, capped at [`MAX_DEAD_REFS`]), this is for acting
/// on ([`crate::advice`] deletes the lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadRef {
    /// 0-based line the reference was written on.
    pub line: usize,
    /// The reference exactly as the file writes it.
    pub reference: String,
    /// Where it resolved to, absolute.
    pub resolved: PathBuf,
}

/// Every dead reference in `f`, one entry per occurrence, in file order.
pub fn dead_refs_located(f: &FileText) -> Vec<DeadRef> {
    let base = f.base();
    path_tokens_located(&f.text)
        .into_iter()
        .map(|(line, reference)| {
            let resolved = resolve(&reference, &base);
            DeadRef {
                line,
                reference,
                resolved,
            }
        })
        .filter(names_a_path)
        .filter(|d| !d.resolved.exists())
        .collect()
}

/// Whether a token that resolves to nothing was a path reference at all.
///
/// [`path_token`] accepts anything anchored, which is right for `/opt/thing` and
/// wrong for the HTTP routes a project's own guidance is full of. "The API
/// exposes GET /v1/sessions and POST /users/:id/refresh" is three sentences
/// about a web service and zero claims about the disk, and reporting it as three
/// broken references is both wrong and loud enough to discredit the real ones.
///
/// A name and an extension (`docs/x.md`) or a trailing slash (`vendor/`) say
/// "file" on their own. Without either, the evidence has to come from the
/// neighbourhood: a reference to something that used to be there sits next to a
/// directory that still is. `~/.claude/rules` keeps flagging because
/// `~/.claude` exists; `/v1/sessions` does not, because `/v1` never did, and
/// `/login` does not because the filesystem root is not evidence of anything.
fn names_a_path(d: &DeadRef) -> bool {
    has_path_ext(&d.reference) || d.reference.ends_with('/') || parent_is_a_real_dir(&d.resolved)
}

/// Whether `resolved` sits inside a directory that exists and is not the
/// filesystem root. The root is excluded on purpose: it exists everywhere, so a
/// one-segment path (`/login`, `/openapi.json`) would otherwise clear a bare
/// "does its parent exist" test, which is the whole question being asked.
fn parent_is_a_real_dir(resolved: &Path) -> bool {
    resolved
        .parent()
        .is_some_and(|p| p.parent().is_some() && p.is_dir())
}

/// Paths the file points at that are not there.
///
/// Extraction is deliberately conservative (see [`path_token`]): a wrong flag
/// costs the user's trust in every other finding, a missed one costs nothing
/// but a missed finding.
pub fn dead_refs(f: &FileText) -> Vec<Finding> {
    let name = file_name(&f.path);
    let base = f.base();
    // One finding per distinct reference: a path written on five lines is one
    // thing to fix, not five.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let dead: Vec<(String, PathBuf)> = dead_refs_located(f)
        .into_iter()
        .filter(|d| seen.insert(d.reference.clone()))
        .map(|d| (d.reference, d.resolved))
        .collect();

    let more = dead.len().saturating_sub(MAX_DEAD_REFS);
    let path = f.path.to_string_lossy().into_owned();
    let mut out: Vec<Finding> = dead
        .into_iter()
        .take(MAX_DEAD_REFS)
        .map(|(reference, resolved)| {
            let resolved = resolved.to_string_lossy().into_owned();
            Finding {
                id: format!("dead-ref:{path}:{reference}"),
                claim: format!("{name} points at {reference}, which is not there"),
                detail: format!(
                    "Resolved against {} to {resolved}, and nothing exists at that path. \
                     Claude Code loads this file into every session and follows what it says.",
                    base.display()
                ),
                // A dead reference sends the model to a file that is not there.
                // That wastes a tool call and some trust, not context tokens,
                // and claiming a saving here would be inventing one.
                est_tokens: 0,
                est_tokens_month: 0,
                action: format!("Update or delete the reference to {reference} in {name}."),
                kind: FindingKind::DeadRef {
                    reference,
                    resolved,
                    more: 0,
                },
                path: path.clone(),
            }
        })
        .collect();

    // The overflow count rides the last reported finding, so a reader of the
    // capped list is told there is more rather than left to assume there is not.
    if more > 0 {
        if let Some(last) = out.last_mut() {
            if let FindingKind::DeadRef { more: m, .. } = &mut last.kind {
                *m = more;
            }
            last.detail
                .push_str(&format!(" {more} further dead reference(s) in this file are not listed."));
        }
    }
    out
}

/// Path-like tokens in `text` with the 0-based line each was written on, in
/// file order and **not** deduplicated.
///
/// Fenced code blocks are skipped: they are illustrations (`cargo run --config
/// /some/example`), and treating an example as a claim about the repository is
/// the single biggest source of false flags.
pub fn path_tokens_located(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        for raw in line.split_whitespace() {
            if let Some(token) = path_token(raw) {
                out.push((i, token));
            }
        }
    }
    out
}

/// One whitespace-delimited word as a path reference, or `None` when it is
/// prose.
///
/// A token counts when it is anchored (`/x`, `./x`, `../x`, `~/x`), carries a
/// known extension, or ends in `/`. That deliberately misses `app/src-tauri`
/// (relative, no extension) to avoid flagging `and/or` and `read/write`, which
/// look identical to a scanner and are far more common.
fn path_token(raw: &str) -> Option<String> {
    // A markdown link keeps its target after `](`; the label in front of it is
    // prose.
    let mut token = raw.rsplit("](").next().unwrap_or(raw);
    loop {
        let before = token;
        token = token.trim_matches(|c: char| TRIM_CHARS.contains(c));
        token = token.trim_end_matches('.');
        if token == before {
            break;
        }
    }
    // `docs/x.md#section` refers to `docs/x.md`.
    if let Some(i) = token.find('#') {
        token = &token[..i];
    }
    if token.len() < 2 || token.chars().any(|c| NOT_A_PATH_CHARS.contains(c)) {
        return None;
    }
    if token.contains("://") || token.starts_with("www.") || token.starts_with("mailto:") {
        return None;
    }
    let anchored = token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/");
    if !anchored {
        // `example.com/docs/x.md` is a link somebody wrote without a scheme.
        let head = token.split('/').next().unwrap_or(token).to_ascii_lowercase();
        if HOST_SUFFIXES.iter().any(|s| head.ends_with(s)) {
            return None;
        }
    }
    if anchored || token.ends_with('/') || has_path_ext(token) {
        return Some(token.to_string());
    }
    None
}

/// Whether Piggy is sure enough that a dead reference names a *file* to let
/// [`crate::advice`] delete the whole line carrying it.
///
/// Reporting is allowed to be wrong about a token; deleting is not, so this bar
/// sits above [`names_a_path`]'s, and it asks about the file the reference was
/// written in as much as about the reference. Three tests:
///
/// * it carries a known file extension. `/login` and `/healthz/live` never get
///   this far.
/// * it is not anchored at `/`. A root-anchored token is an HTTP route at least
///   as often as it is a path, and an extension does not tell the two apart:
///   `/openapi.json`, `/static/app.js`, `/sw.js`, `/manifest.json` and
///   `/robots.txt` are all routes that resolve against the filesystem root,
///   where they were never going to exist. Neither does the neighbourhood, so
///   the whole shape is report-only. The only thing given up is auto-deleting a
///   genuinely absolute dead path, which is still reported.
/// * and, in a global file, it is anchored at `~/`. A global file has no project
///   root, so a relative reference in it resolves against the home directory,
///   which is not where its author meant. A rule that says "reproduce with
///   `bench/src/report.js`" is about whatever repo the reader is in, and
///   `~/bench/src/report.js` being absent is not evidence that the line is
///   stale. Every unanchored reference in a global file would otherwise be dead
///   by construction and lose its line.
///
/// In a project file the base *is* that project, which is exactly the case this
/// transform exists for ("the `scripts/` directory is gone, so every line
/// pointing into it is stale"), so a relative reference there is deletable.
pub fn deletable_ref(dead: &DeadRef, file: &FileText) -> bool {
    if !has_path_ext(&dead.reference) {
        return false;
    }
    if dead.reference.starts_with('/') {
        return false;
    }
    file.project.is_some() || dead.reference.starts_with("~/")
}

fn has_path_ext(token: &str) -> bool {
    let last = token.rsplit('/').next().unwrap_or(token);
    match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            let ext = ext.to_ascii_lowercase();
            PATH_EXTS.contains(&ext.as_str())
        }
        _ => false,
    }
}

/// Where a reference points: `~/x` and `/x` speak for themselves, everything
/// else hangs off the file's own base (project root, or home for a global file).
fn resolve(token: &str, base: &Path) -> PathBuf {
    if let Some(rest) = token.strip_prefix("~/") {
        return home_anchor().join(rest);
    }
    if token.starts_with('/') {
        return PathBuf::from(token);
    }
    base.join(token.strip_prefix("./").unwrap_or(token))
}

// ---------------------------------------------------------------------------
// Detector: duplicate blocks
// ---------------------------------------------------------------------------

/// Paragraphs that appear in more than one of the scanned files.
///
/// Global-vs-project is the case worth catching (the same paragraph in
/// `~/.claude/CLAUDE.md` and a project file is paid twice in that project), but
/// any two files count: both copies are loaded, both are charged.
pub fn duplicate_blocks(files: &[FileText]) -> Vec<Finding> {
    // hash -> (normalized text, the files carrying it).
    let mut blocks: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
    for f in files {
        let path = f.path.to_string_lossy().into_owned();
        for para in paragraphs(&f.text) {
            let entry = blocks
                .entry(hash_bytes(para.as_bytes()))
                .or_insert_with(|| (para, BTreeSet::new()));
            entry.1.insert(path.clone());
        }
    }

    let mut out = Vec::new();
    for (hash, (para, paths)) in &blocks {
        if paths.len() < 2 {
            continue;
        }
        let label: String = para.chars().take(60).collect();
        let tokens = est_tokens(para.len() as i64);
        for path in paths {
            let others: Vec<String> = paths.iter().filter(|p| *p != path).cloned().collect();
            let others_list = others
                .iter()
                .map(|p| file_name(Path::new(p)).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Finding {
                id: format!("duplicate:{path}:{}", &hash[..12]),
                claim: format!(
                    "A {} character block here is also in {others_list}",
                    para.len()
                ),
                detail: format!(
                    "The same paragraph (whitespace normalized) is in {path} and in {}. \
                     It reads \"{label}...\". A session that loads both files pays for it twice.",
                    others.join(", ")
                ),
                est_tokens: tokens,
                est_tokens_month: 0,
                action: format!(
                    "Delete the copy in {} and keep the one in {others_list}.",
                    file_name(Path::new(path))
                ),
                kind: FindingKind::DuplicateBlock {
                    others,
                    label: label.clone(),
                    bytes: para.len(),
                },
                path: path.clone(),
            });
        }
    }
    out
}

/// One normalized paragraph and the lines it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    /// 0-based index of the paragraph's first line.
    pub start: usize,
    /// 0-based index one past its last line.
    pub end: usize,
    /// The normalized text (what [`duplicate_blocks`] hashes).
    pub text: String,
}

/// Normalized paragraphs with the line range each came from.
///
/// The positions are what makes a duplicate *removable*: the normalized text
/// alone says two files share a block, not which lines to delete.
pub fn paragraphs_located(text: &str) -> Vec<Paragraph> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            push_paragraph(&mut out, &current, start, i);
            current.clear();
            start = i + 1;
        } else {
            current.push(line);
        }
    }
    push_paragraph(&mut out, &current, start, text.lines().count());
    out
}

/// Normalized paragraphs: split on blank lines, trimmed, internal whitespace
/// collapsed, and anything under [`DUP_MIN_BYTES`] dropped.
///
/// Normalizing is what makes the match useful: the same block re-wrapped by an
/// editor is the same block, and a raw hash would miss every one of them.
fn paragraphs(text: &str) -> Vec<String> {
    paragraphs_located(text).into_iter().map(|p| p.text).collect()
}

fn push_paragraph(out: &mut Vec<Paragraph>, lines: &[&str], start: usize, end: usize) {
    if lines.is_empty() {
        return;
    }
    let joined = lines.join(" ");
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() >= DUP_MIN_BYTES {
        out.push(Paragraph {
            start,
            end,
            text: normalized,
        });
    }
}

// ---------------------------------------------------------------------------
// Detector: oversize
// ---------------------------------------------------------------------------

/// A file that is big enough to be worth trimming on its own.
pub fn oversize(f: &FileText) -> Option<Finding> {
    let tokens = f.est_tokens();
    if tokens <= OVERSIZE_EST_TOKENS {
        return None;
    }
    let path = f.path.to_string_lossy().into_owned();
    let name = file_name(&f.path);
    Some(Finding {
        id: format!("oversize:{path}"),
        kind: FindingKind::Oversize {
            threshold: OVERSIZE_EST_TOKENS,
        },
        claim: format!(
            "{name} is about {} tokens on its own",
            commas(tokens.max(0) as u64)
        ),
        detail: format!(
            "{} bytes at roughly {BYTES_PER_TOKEN} bytes per token, against a {} token line. \
             It is loaded before your first message, so every session pays all of it.",
            commas(f.bytes.max(0) as u64),
            commas(OVERSIZE_EST_TOKENS as u64)
        ),
        est_tokens: tokens,
        est_tokens_month: 0,
        action: format!("Cut {name} down to the guidance you actually rely on every session."),
        path,
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// .mcp.json (read-only)
// ---------------------------------------------------------------------------

/// The MCP servers each project configures in its own `.mcp.json`.
///
/// Read-only and never stored: M5.3 consumes it as evidence, so that a server a
/// project checked into its repo is never called "globally unused". Piggy does
/// not write `.mcp.json` in v1.
pub fn project_mcp_servers(store: &Store) -> Result<ProjectMcpServers> {
    let mut out = ProjectMcpServers::default();
    for project in store.session_projects()? {
        let path = Path::new(&project).join(".mcp.json");
        if !path.is_file() {
            continue;
        }
        match mcp_server_names(&path) {
            Ok(names) if !names.is_empty() => {
                out.by_project.insert(project, names);
            }
            Ok(_) => {}
            Err(e) => out.warnings.push(format!("{e:#}")),
        }
    }
    Ok(out)
}

/// The `mcpServers` keys in one `.mcp.json`, sorted. A file with no
/// `mcpServers` object yields an empty list rather than an error: it is a valid
/// file that configures nothing.
pub fn mcp_server_names(path: &Path) -> Result<Vec<String>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let mut names: Vec<String> = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Store reads the scanner needs
// ---------------------------------------------------------------------------

/// Claude Code sessions over a window, per project and in total.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCounts {
    pub by_project: BTreeMap<String, i64>,
    /// Every counted session in the window, including those with no recorded
    /// project: they still loaded the global files.
    pub total: i64,
}

impl Store {
    /// Every distinct project path in the sessions table, in path order.
    ///
    /// The scan's roots: a project CLAUDE.md is worth inventorying exactly when
    /// Piggy has seen a session run there. Deliberately unfiltered by source, so
    /// a project Piggy has only seen Codex run in is still inventoried (at zero
    /// sessions, see [`Store::session_counts_since`]).
    pub fn session_projects(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT project FROM sessions
             WHERE project IS NOT NULL AND project <> ''
             ORDER BY project",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Claude Code session counts with last activity on or after `cutoff` (all
    /// time when `None`), per project and in total.
    ///
    /// Codex sessions are excluded on purpose. Piggy indexes Codex rollouts into
    /// the same table, but CLAUDE.md is a Claude Code artifact and a Codex
    /// session never loads one, so counting those rows would inflate both
    /// `sessions_30d` and every monthly burden derived from it. The companion
    /// [`Store::session_projects`] stays unfiltered: a project Piggy has only
    /// ever seen Codex run in still gets its CLAUDE.md inventoried, now honestly
    /// at zero sessions and zero monthly burden.
    pub fn session_counts_since(&self, cutoff: Option<&str>) -> Result<SessionCounts> {
        let mut out = SessionCounts::default();
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(project, ''), COUNT(*) FROM sessions
             WHERE (?1 IS NULL OR ended_at >= ?1) AND source = ?2
             GROUP BY COALESCE(project, '')",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![cutoff, crate::sources::SourceKind::ClaudeCode.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?;
        for row in rows {
            let (project, n) = row?;
            out.total += n;
            if !project.is_empty() {
                out.by_project.insert(project, n);
            }
        }
        Ok(out)
    }
}
