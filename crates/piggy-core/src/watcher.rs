//! Filesystem watcher over the Claude projects directory (M3).
//!
//! When a new session `.jsonl` appears we must snapshot the enabled-saver set
//! *at file-creation time* (the config cannot be changed once a session starts).
//! On any create/modify we run an incremental index so the DB stays current.
//!
//! The watcher is exposed as a [`SessionWatcher`] the GUI can drive, plus the
//! `piggy watch` CLI. Production uses the platform's native kernel-push watcher
//! (FSEvents on macOS) for near-zero idle CPU; tests opt into a deterministic
//! poll backend via [`WatchBackend::Poll`]. Events are debounced so a streaming
//! write burst collapses into one index pass.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Config, PollWatcher, RecursiveMode, Watcher};

use crate::index::{run_index_roots, SourceRoot};
use crate::pricing::Pricing;
use crate::sources::SourceKind;
use crate::state::PiggyState;
use crate::store::Store;
use crate::tagging::snapshot_new_session;

/// Default poll interval for the poll backend (tests, CLI fallback).
pub const DEFAULT_POLL: Duration = Duration::from_secs(2);
/// After the first event, wait this long for the burst to settle before acting.
const SETTLE: Duration = Duration::from_millis(120);

/// Which OS mechanism backs the watcher.
///
/// Production uses [`WatchBackend::Native`] — the platform's kernel-push watcher
/// (FSEvents on macOS), which does no work while idle. [`WatchBackend::Poll`]
/// re-`stat`s every watched path each interval, so its CPU cost is O(paths)
/// *forever*; on a large Claude history (10k+ session files) that is a steady
/// slice of a core even with nothing happening. Poll is therefore test-only,
/// where a fixed interval buys deterministic, cross-platform timing.
pub enum WatchBackend {
    /// Kernel-push notifications (FSEvents/inotify/…). Near-zero idle cost.
    Native,
    /// Fixed-interval polling. Deterministic, but O(paths) CPU every tick.
    Poll(Duration),
}

/// What a [`SessionWatcher::tick`] did for one session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    pub path: PathBuf,
    pub session_id: String,
    /// True if this tick wrote the session's first saver-set snapshot (i.e. it
    /// looked like a brand-new session).
    pub newly_tagged: bool,
}

/// A live watcher over one or more session-log roots (Claude Code projects,
/// Codex sessions). Owns its own [`Store`] handle.
pub struct SessionWatcher {
    // Field order matters for drop: the watcher must drop before the receiver.
    _watcher: Box<dyn Watcher + Send>,
    rx: Receiver<notify::Result<notify::Event>>,
    roots: Vec<SourceRoot>,
    store: Store,
}

impl SessionWatcher {
    /// Watch `projects_dir` (Claude Code logs), persisting into the DB under
    /// `home`. Production constructor: uses the native kernel-push backend.
    pub fn new(projects_dir: PathBuf, home: &Path) -> Result<Self> {
        Self::for_single_root(projects_dir, home, WatchBackend::Native)
    }

    /// Watch a Claude Code projects dir with an explicit poll interval. Test-only:
    /// polling gives deterministic, cross-platform timing at the cost of steady
    /// CPU, which production avoids via [`WatchBackend::Native`].
    pub fn with_poll_interval(
        projects_dir: PathBuf,
        home: &Path,
        interval: Duration,
    ) -> Result<Self> {
        Self::for_single_root(projects_dir, home, WatchBackend::Poll(interval))
    }

    /// Shared single-root setup: create the Claude root if missing (Piggy has
    /// always done so for its primary source), then watch it with `backend`.
    fn for_single_root(projects_dir: PathBuf, home: &Path, backend: WatchBackend) -> Result<Self> {
        std::fs::create_dir_all(&projects_dir)
            .with_context(|| format!("creating {}", projects_dir.display()))?;
        Self::with_roots(
            vec![SourceRoot::new(projects_dir, SourceKind::ClaudeCode)],
            home,
            backend,
        )
    }

    /// Watch several source roots at once (e.g. Claude Code projects + Codex
    /// sessions). Roots must exist; each is watched recursively.
    pub fn with_roots(roots: Vec<SourceRoot>, home: &Path, backend: WatchBackend) -> Result<Self> {
        // The native (FSEvents) backend reports canonical, symlink-resolved paths.
        // `claude_owns` routes each event by prefix-matching against these root
        // dirs, so canonicalize them once up front — otherwise a symlinked
        // ancestor (macOS's /var → /private/var, or a symlinked home) makes every
        // event path fail `starts_with`, and new sessions never get snapshot-
        // tagged. Fall back to the original path if it can't be resolved yet.
        let roots: Vec<SourceRoot> = roots
            .into_iter()
            .map(|r| SourceRoot::new(r.dir.canonicalize().unwrap_or(r.dir), r.kind))
            .collect();
        let (tx, rx) = mpsc::channel();
        // `tx` is moved into whichever arm runs — a value may be moved in several
        // mutually-exclusive match arms, since only one ever executes.
        let mut watcher: Box<dyn Watcher + Send> = match backend {
            WatchBackend::Native => Box::new(
                notify::recommended_watcher(move |res| {
                    let _ = tx.send(res);
                })
                .context("starting the filesystem watcher")?,
            ),
            WatchBackend::Poll(interval) => Box::new(
                PollWatcher::new(
                    move |res| {
                        let _ = tx.send(res);
                    },
                    Config::default().with_poll_interval(interval),
                )
                .context("starting the filesystem poll watcher")?,
            ),
        };
        for root in &roots {
            watcher
                .watch(&root.dir, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", root.dir.display()))?;
        }
        let store = Store::open(home)?;
        Ok(SessionWatcher {
            _watcher: watcher,
            rx,
            roots,
            store,
        })
    }

    /// Block up to `max_wait` for filesystem activity, then snapshot-tag any new
    /// sessions and run an incremental index. Returns one [`WatchEvent`] per
    /// touched `.jsonl` (empty if nothing happened within `max_wait`).
    pub fn tick(&mut self, max_wait: Duration, pricing: &Pricing) -> Result<Vec<WatchEvent>> {
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();

        // Wait for the first event (or give up after max_wait).
        match self.rx.recv_timeout(max_wait) {
            Ok(res) => collect(res, &mut paths),
            Err(RecvTimeoutError::Timeout) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => return Ok(Vec::new()),
        }
        // Drain the rest of the burst.
        while let Ok(res) = self.rx.recv_timeout(SETTLE) {
            collect(res, &mut paths);
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        // Snapshot-tag new sessions from the *current* saver set, before indexing
        // (the snapshot is a session-start fact, independent of parse results).
        // Only Claude Code sessions are tagged: savers act on Claude Code, so a
        // Codex session carries no saver set and must stay out of attribution.
        let state = PiggyState::load()?;
        let mut events = Vec::new();
        for path in &paths {
            let session_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if session_id.is_empty() {
                continue;
            }
            let newly_tagged = if self.claude_owns(path) {
                snapshot_new_session(&mut self.store, &state, &session_id)?
            } else {
                false
            };
            events.push(WatchEvent {
                path: path.clone(),
                session_id,
                newly_tagged,
            });
        }

        // Incremental index so token aggregates reflect the new/changed files.
        run_index_roots(&mut self.store, pricing, &self.roots, false)?;
        Ok(events)
    }

    /// Whether `path` lives under a Claude Code root (vs Codex).
    fn claude_owns(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|r| r.kind == SourceKind::ClaudeCode && path.starts_with(&r.dir))
    }

    /// Read-only access to the underlying store (e.g. to query after a tick).
    pub fn store(&self) -> &Store {
        &self.store
    }
}

/// Fold a notify result into the set of touched `.jsonl` paths.
fn collect(res: notify::Result<notify::Event>, paths: &mut BTreeSet<PathBuf>) {
    let Ok(event) = res else { return };
    if !matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Any
    ) {
        return;
    }
    for p in event.paths {
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.insert(p);
        }
    }
}
