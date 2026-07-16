//! Incremental indexing: walk `projects_dir` for `*.jsonl`, parse changed
//! files, and upsert their aggregates.
//!
//! A file is skipped when its `(size, mtime_ns)` match what was last recorded.
//! If a file grew or changed, the whole file is re-parsed (correct because
//! deduplication is per-file); `--full` forces re-parsing everything. The
//! stored byte offset is unused today and reserved for a future resume
//! optimization.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use walkdir::WalkDir;

use crate::codex::parse_codex_file;
use crate::parser::parse_file;
use crate::pricing::Pricing;
use crate::sources::SourceKind;
use crate::store::Store;

/// One directory tree of session logs plus the tool that writes it — the unit
/// [`run_index_roots`] and the watcher operate over.
#[derive(Debug, Clone)]
pub struct SourceRoot {
    pub dir: PathBuf,
    pub kind: SourceKind,
}

impl SourceRoot {
    pub fn new(dir: PathBuf, kind: SourceKind) -> Self {
        SourceRoot { dir, kind }
    }
}

/// Every source root present on this machine: Claude Code's projects dir
/// (always, when it exists) plus Codex's `sessions`/`archived_sessions`.
pub fn default_roots() -> Vec<SourceRoot> {
    let mut roots = Vec::new();
    let projects = crate::config::claude_projects_dir();
    if projects.is_dir() {
        roots.push(SourceRoot::new(projects, SourceKind::ClaudeCode));
    }
    for dir in crate::config::codex_sessions_dirs() {
        roots.push(SourceRoot::new(dir, SourceKind::Codex));
    }
    roots
}

/// Summary of an indexing run.
#[derive(Debug, Default, Clone)]
pub struct IndexReport {
    pub scanned: u64,
    pub updated: u64,
    pub skipped: u64,
    pub unreadable: u64,
    /// `parse_errors` summed across files parsed this run.
    pub parse_errors: u64,
    /// Total sessions in the database after the run.
    pub sessions: u64,
}

fn mtime_ns(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Index every `*.jsonl` under `projects_dir` (Claude Code logs) into `store`.
///
/// Kept as the single-root Claude Code entry point; multi-source callers use
/// [`run_index_roots`].
pub fn run_index(
    store: &mut Store,
    pricing: &Pricing,
    projects_dir: &Path,
    full: bool,
) -> Result<IndexReport> {
    run_index_roots(
        store,
        pricing,
        &[SourceRoot::new(
            projects_dir.to_path_buf(),
            SourceKind::ClaudeCode,
        )],
        full,
    )
}

/// Index every `*.jsonl` under each root into `store`, parsing with the
/// root's source-specific parser. One [`IndexReport`] sums across roots.
pub fn run_index_roots(
    store: &mut Store,
    pricing: &Pricing,
    roots: &[SourceRoot],
    full: bool,
) -> Result<IndexReport> {
    let mut rep = IndexReport::default();
    for root in roots {
        index_root(store, pricing, root, full, &mut rep)?;
    }
    rep.sessions = store.session_count()?;
    Ok(rep)
}

fn index_root(
    store: &mut Store,
    pricing: &Pricing,
    root: &SourceRoot,
    full: bool,
    rep: &mut IndexReport,
) -> Result<()> {
    for entry in WalkDir::new(&root.dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        rep.scanned += 1;

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                rep.unreadable += 1;
                continue;
            }
        };
        let size = meta.len() as i64;
        let mtime = mtime_ns(&meta);
        let path_str = path.to_string_lossy().into_owned();

        if !full {
            if let Ok(Some((s, m))) = store.file_state(&path_str) {
                if s == size && m == mtime {
                    rep.skipped += 1;
                    continue;
                }
            }
        }

        let parsed = match root.kind {
            SourceKind::ClaudeCode => parse_file(path),
            SourceKind::Codex => parse_codex_file(path),
        };
        match parsed {
            Ok(parse) => {
                rep.parse_errors += parse.parse_errors;
                store.upsert_session(&parse, pricing, &path_str, size, mtime)?;
                rep.updated += 1;
            }
            Err(_) => {
                rep.unreadable += 1;
            }
        }
    }
    Ok(())
}
