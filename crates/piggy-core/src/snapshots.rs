//! File snapshots: back up any file, put it back byte for byte.
//!
//! [`crate::settings`] already does this for one file (`settings.json`), where
//! the byte-identical restore target is a parsed [`crate::settings::ByteRestore`]
//! held in memory for the length of one commit. M5 edits files Piggy did not
//! write and cannot re-derive - a trimmed CLAUDE.md is prose, not config - so the
//! restore target has to be the original bytes on disk, recorded durably.
//!
//! The shape here is deliberately small:
//!
//! * [`snapshot`] copies a file to `<piggy_home>/backups/files/<sha>-<ts>.bak`
//!   and records it in `state.file_snapshots`;
//! * [`write_atomic`] replaces a file with new bytes under the same discipline
//!   as the settings engine (temp file in the same directory, fsync, rename,
//!   preserved permissions, written *through* a symlink);
//! * [`check_unchanged`] is the apply-time gate: a file that moved since a draft
//!   was made against it is refused with a typed [`Conflict`], never overwritten;
//! * [`restore`] puts a batch back and reports **per item**, so one unwritable
//!   file can never make the others disappear quietly, and snapshots anything
//!   the user wrote over Piggy's edit before it goes.
//!
//! `settings.json` keeps its own machinery for now; this module is additive.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::settings::hash_bytes;
use crate::state::PiggyState;

/// One file Piggy backed up before editing it, and where the copy lives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSnapshot {
    /// The file that was snapshotted (absolute).
    pub path: String,
    /// The `.bak` copy under `<piggy_home>/backups/files` (absolute).
    pub backup: String,
    /// The advice row this snapshot belongs to, so an Undo can find its own
    /// records. `None` for a snapshot taken outside the advice engine.
    #[serde(default)]
    pub advice_id: Option<String>,
    /// sha256 of the bytes the apply *wrote*, for a snapshot taken by
    /// [`snapshot_and_write`]. It is how [`restore`] tells "the file is still
    /// exactly as Piggy left it" from "the user has edited it since", and the
    /// second case gets backed up before the original goes back.
    ///
    /// `None` for a snapshot taken on its own and for every record written
    /// before this field existed. Those are treated as edited, which costs one
    /// extra copy of a file and never costs the user their work.
    #[serde(default)]
    pub after_hash: Option<String>,
    pub applied_at: String,
}

/// An apply refused because the file is not what the draft was made against.
///
/// Typed rather than a string so callers can tell "someone edited it" from "it
/// is gone" without matching on prose - the two want different UI (re-scan vs.
/// drop the suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// The file no longer exists.
    Missing { path: String },
    /// The file exists but its content moved.
    Changed {
        path: String,
        expected: String,
        actual: String,
    },
    /// The file could not be read at all (permissions, I/O).
    Unreadable { path: String, reason: String },
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Conflict::Missing { path } => write!(f, "{path} no longer exists"),
            Conflict::Changed { path, .. } => {
                write!(f, "{path} has changed since Piggy read it")
            }
            Conflict::Unreadable { path, reason } => {
                write!(f, "{path} could not be read: {reason}")
            }
        }
    }
}

impl std::error::Error for Conflict {}

/// A file that could not be restored, and why.
#[derive(Debug, Clone)]
pub struct RestoreFailure {
    /// The file that is still not back to its snapshotted content.
    pub path: String,
    pub reason: String,
}

/// Per-item result of a [`restore`] batch. Never a bare count: a caller that
/// only learns "3 of 4" cannot tell the user *which* file it failed to put back.
#[derive(Debug, Clone, Default)]
pub struct RestoreOutcome {
    pub restored: usize,
    pub failures: Vec<RestoreFailure>,
}

/// Directory holding file snapshots. `<piggy_home>/backups/files`.
pub fn files_backup_dir() -> PathBuf {
    config::backups_dir().join("files")
}

/// Copy `path`'s current content into the snapshot directory and record it in
/// `state`, returning the record.
///
/// The caller persists `state` afterwards, exactly as with the settings engine's
/// backup ledger. A missing file is an error: there is nothing to put back, and
/// an apply that believes it has a restore target when it does not is worse than
/// one that refuses.
pub fn snapshot(
    path: &Path,
    advice_id: Option<&str>,
    state: &mut PiggyState,
) -> Result<FileSnapshot> {
    record_snapshot(path, advice_id, None, state)
}

/// Back up `path`, then replace its content with `bytes`.
///
/// The pair is one call because the record has to carry the hash of what was
/// written ([`FileSnapshot::after_hash`]), and a caller that took the snapshot
/// and then wrote separately could forget the second half - at which point Undo
/// silently loses whatever the user did to the file in between.
pub fn snapshot_and_write(
    path: &Path,
    bytes: &[u8],
    advice_id: Option<&str>,
    state: &mut PiggyState,
) -> Result<FileSnapshot> {
    let record = record_snapshot(path, advice_id, Some(hash_bytes(bytes)), state)?;
    write_atomic(path, bytes)?;
    Ok(record)
}

fn record_snapshot(
    path: &Path,
    advice_id: Option<&str>,
    after_hash: Option<String>,
    state: &mut PiggyState,
) -> Result<FileSnapshot> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let dir = files_backup_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let backup = unique_backup_path(&dir, &hash_bytes(&bytes));
    std::fs::write(&backup, &bytes).with_context(|| format!("writing {}", backup.display()))?;

    let record = FileSnapshot {
        path: path.to_string_lossy().into_owned(),
        backup: backup.to_string_lossy().into_owned(),
        advice_id: advice_id.map(str::to_string),
        after_hash,
        applied_at: chrono::Utc::now().to_rfc3339(),
    };
    state.file_snapshots.push(record.clone());
    Ok(record)
}

/// `<sha>-<timestamp>.bak` under `dir`, guaranteed not to exist yet.
///
/// Nanosecond precision plus an existence-checked suffix, so two snapshots taken
/// in the same instant cannot overwrite each other (the settings engine learned
/// this the hard way). Colons become hyphens: RFC3339 spells the offset with
/// them, and macOS renders a `:` in a filename as `/`.
fn unique_backup_path(dir: &Path, sha: &str) -> PathBuf {
    let ts = chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        .replace(':', "-");
    let base = dir.join(format!("{sha}-{ts}.bak"));
    if !base.exists() {
        return base;
    }
    for i in 1.. {
        let p = dir.join(format!("{sha}-{ts}-{i}.bak"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// Confirm `path` still holds the content a draft was made against.
///
/// The content hash rather than the mtime: a file that was touched but not
/// edited is not a conflict, and a file rewritten within the same mtime tick
/// still is. (`claudemd_files.mtime_ns` remains the cheap rescan filter; this is
/// the gate that actually guards a write.)
pub fn check_unchanged(path: &Path, expected_hash: &str) -> std::result::Result<(), Conflict> {
    let display = path.to_string_lossy().into_owned();
    if !path.exists() {
        return Err(Conflict::Missing { path: display });
    }
    let bytes = std::fs::read(path).map_err(|e| Conflict::Unreadable {
        path: display.clone(),
        reason: e.to_string(),
    })?;
    let actual = hash_bytes(&bytes);
    if actual != expected_hash {
        return Err(Conflict::Changed {
            path: display,
            expected: expected_hash.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Replace `path`'s content with `bytes`, atomically.
///
/// Temp file in the same directory, fsync, rename, then the original file's
/// permissions (0600 for a file that did not exist, matching the settings
/// engine). A symlinked target is written *through*: CLAUDE.md is commonly a
/// link into a dotfiles repo, and replacing the link with a regular file would
/// leave the tracked source stale while the edit looked applied.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let resolved = resolve_symlink_target(path);
    let write_path: &Path = resolved.as_deref().unwrap_or(path);

    let dir = write_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // metadata() follows the symlink, so this is the target's mode.
        let mode = std::fs::metadata(write_path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o600);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
    }

    tmp.persist(write_path)
        .map_err(|e| anyhow::anyhow!("persisting {}: {e}", write_path.display()))?;
    Ok(())
}

/// If `path` is a symlink, the concrete file it points at; `None` for a regular
/// file, a missing file, or a broken link (the caller writes `path` itself).
fn resolve_symlink_target(path: &Path) -> Option<PathBuf> {
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        std::fs::canonicalize(path).ok()
    } else {
        None
    }
}

/// Put every file in `records` back to its snapshotted content.
///
/// Applied **oldest last**: records accumulate in the order they were taken, so
/// a file edited twice has two snapshots, and restoring the whole list forward
/// would stop at the state before the *second* edit. Walking backwards lets the
/// earliest snapshot win, which is what "undo everything Piggy did" means.
///
/// Every item is attempted; a failure is reported by path and reason rather than
/// aborting the batch, so one file with permissions revoked cannot hide the
/// others' outcome.
///
/// `state` is taken mutably because a restore that would overwrite the user's
/// own work snapshots that work first (see [`restore_one`]); pass a clone of the
/// records rather than a borrow of the same field.
pub fn restore(records: &[FileSnapshot], state: &mut PiggyState) -> RestoreOutcome {
    let mut outcome = RestoreOutcome::default();
    for rec in records.iter().rev() {
        match restore_one(rec, state) {
            Ok(()) => outcome.restored += 1,
            Err(e) => {
                eprintln!("warning: {e:#}");
                outcome.failures.push(RestoreFailure {
                    path: rec.path.clone(),
                    reason: format!("{e:#}"),
                });
            }
        }
    }
    outcome
}

/// Put one file back, backing up whatever is on disk now if that is not what the
/// apply left behind.
///
/// Undo must not be a weaker gate than apply. Apply refuses to touch a file
/// whose hash moved ([`check_unchanged`]); a restore cannot refuse - somebody
/// who fixed a typo the week after applying still has to be able to undo - so it
/// keeps their bytes instead of guarding against them. The current content goes
/// into `backups/files` with a record of its own first, which is the same move
/// `settings::backup_only` makes before Restore Defaults overwrites
/// `settings.json`.
fn restore_one(rec: &FileSnapshot, state: &mut PiggyState) -> Result<()> {
    let backup = Path::new(&rec.backup);
    // The backup is the only copy of the original content. Missing means the
    // restore is impossible, not that it succeeded with nothing to do. Read
    // before anything else so an impossible restore does not leave a stray copy
    // behind.
    let bytes = std::fs::read(backup)
        .with_context(|| format!("reading backup {} for {}", rec.backup, rec.path))?;
    let path = Path::new(&rec.path);
    if edited_since_apply(path, rec) {
        // No `advice_id`: this is a plain backup of the user's own content, not
        // an edit of Piggy's that anything should try to reverse later.
        snapshot(path, None, state)
            .with_context(|| format!("backing up the current {} before restoring it", rec.path))?;
    }
    write_atomic(path, &bytes).with_context(|| format!("restoring {}", rec.path))
}

/// Whether what is on disk is something other than the bytes the apply wrote,
/// i.e. whether putting the original back would throw work away. A file with no
/// recorded [`FileSnapshot::after_hash`], or one that has since been deleted, is
/// answered on the safe side.
fn edited_since_apply(path: &Path, rec: &FileSnapshot) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => rec.after_hash.as_deref() != Some(hash_bytes(&bytes).as_str()),
        // Nothing on disk to lose: the restore is what puts the file back.
        Err(_) => false,
    }
}
