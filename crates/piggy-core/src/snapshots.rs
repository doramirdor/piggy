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
//!   and records it in `state.file_snapshots`, pruning copies no record points
//!   at any more to the same ceiling `settings.json`'s history keeps;
//! * [`backup_only`] copies a file the same way but records it in
//!   `state.file_backups`, the ledger of the *user's* bytes, which is a separate
//!   type precisely so that nothing can hand one to [`restore`];
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

/// How many `.bak` copies with no record left pointing at them to keep, matching
/// the settings engine's timestamped-backup ceiling.
const MAX_SPENT_FILE_BACKUPS: usize = 50;

/// One file Piggy backed up **before editing it**, and where the copy lives.
///
/// Every record here is a restore target: [`restore`] takes a slice of these and
/// writes each one back. That is the whole meaning of the type, and it is why the
/// copies a restore takes of the *user's* bytes are a [`FileBackup`] instead. The
/// two used to share this struct, told apart only by whether `advice_id` was set,
/// and three defects came out of that convention - an Undo destroying post-apply
/// edits, an out-of-order check that skipped id-less rows by hand, and a Restore
/// Defaults that re-applied Piggy's edits by restoring the safety copies. A
/// consumer added from here on cannot make that mistake: there is no id-less
/// `FileSnapshot` to forget about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSnapshot {
    /// The file that was snapshotted (absolute).
    pub path: String,
    /// The `.bak` copy under `<piggy_home>/backups/files` (absolute).
    pub backup: String,
    /// The advice row this snapshot belongs to, so an Undo can find its own
    /// records. Never empty: a snapshot with no row behind it is not a thing
    /// anything should restore, and so is not this type.
    pub advice_id: String,
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

/// A copy of what the **user** had on disk, taken just before a restore wrote
/// over it, and where the copy lives.
///
/// The counterpart to [`FileSnapshot`], and deliberately not the same type. This
/// is somebody's own writing, kept so a restore never destroys work Piggy did not
/// author; putting it back would hand them Piggy's edit again, on top of the
/// original the restore just gave them. Nothing here is a restore target, which
/// no consumer has to remember: [`restore`] cannot be called with one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileBackup {
    /// The file whose content was copied aside (absolute).
    pub path: String,
    /// The `.bak` copy under `<piggy_home>/backups/files` (absolute).
    pub backup: String,
    /// When the copy was taken (RFC3339). Not `applied_at`: nothing was applied,
    /// and this is the moment their bytes were rescued.
    pub taken_at: String,
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
pub fn snapshot(path: &Path, advice_id: &str, state: &mut PiggyState) -> Result<FileSnapshot> {
    record_snapshot(path, advice_id, None, state)
}

/// Copy `path`'s current content aside as the user's own bytes, with no restore
/// target attached.
///
/// The same copy on disk as [`snapshot`], recorded in the other ledger. This is
/// what [`restore_one`] calls before it overwrites a file somebody has edited
/// since the apply: their work has to survive, and it must not become something a
/// later Undo or Restore Defaults writes back out.
pub fn backup_only(path: &Path, state: &mut PiggyState) -> Result<FileBackup> {
    let (dir, backup) = copy_aside(path)?;
    let record = FileBackup {
        path: path.to_string_lossy().into_owned(),
        backup: backup.to_string_lossy().into_owned(),
        taken_at: chrono::Utc::now().to_rfc3339(),
    };
    state.file_backups.push(record.clone());
    prune_file_backups(&dir, state);
    Ok(record)
}

/// Back up `path`, then replace its content with `bytes`.
///
/// The pair is one call because the record has to carry the hash of what was
/// written ([`FileSnapshot::after_hash`]), and a caller that took the snapshot
/// and then wrote separately could forget the second half - at which point Undo
/// silently loses whatever the user did to the file in between.
/// A failed write leaves `state` untouched. The copy is taken first, because
/// there is no copying the original bytes back off a file that has already been
/// overwritten, but the **record** is pushed only once the write has landed. It
/// used to be pushed first, and a caller that applied several items through one
/// `PiggyState` then persisted the phantom on the next successful item: a
/// snapshot record with an `after_hash` of content that was never written,
/// claiming Piggy had edited a file it had failed to touch. Restore Defaults
/// would later write that backup back over whatever the file had since become.
pub fn snapshot_and_write(
    path: &Path,
    bytes: &[u8],
    advice_id: &str,
    state: &mut PiggyState,
) -> Result<FileSnapshot> {
    let (dir, backup) = copy_aside(path)?;
    if let Err(e) = write_atomic(path, bytes) {
        // Nothing points at the copy and the file it came from is unchanged, so
        // it is not a backup of anything. Removing it is best effort: pruning
        // would get it eventually either way.
        let _ = std::fs::remove_file(&backup);
        return Err(e);
    }
    let record = FileSnapshot {
        path: path.to_string_lossy().into_owned(),
        backup: backup.to_string_lossy().into_owned(),
        advice_id: advice_id.to_string(),
        after_hash: Some(hash_bytes(bytes)),
        applied_at: chrono::Utc::now().to_rfc3339(),
    };
    state.file_snapshots.push(record.clone());
    prune_file_backups(&dir, state);
    Ok(record)
}

fn record_snapshot(
    path: &Path,
    advice_id: &str,
    after_hash: Option<String>,
    state: &mut PiggyState,
) -> Result<FileSnapshot> {
    let (dir, backup) = copy_aside(path)?;
    let record = FileSnapshot {
        path: path.to_string_lossy().into_owned(),
        backup: backup.to_string_lossy().into_owned(),
        advice_id: advice_id.to_string(),
        after_hash,
        applied_at: chrono::Utc::now().to_rfc3339(),
    };
    state.file_snapshots.push(record.clone());
    prune_file_backups(&dir, state);
    Ok(record)
}

/// Copy `path`'s current bytes into the snapshot directory, returning that
/// directory and the copy. Shared by both ledgers: the bytes on disk are the same
/// move either way, and only the record that names them differs.
fn copy_aside(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let dir = files_backup_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let backup = unique_backup_path(&dir, &hash_bytes(&bytes));
    std::fs::write(&backup, &bytes).with_context(|| format!("writing {}", backup.display()))?;
    Ok((dir, backup))
}

/// Keep at most [`MAX_SPENT_FILE_BACKUPS`] `.bak` copies that no record points
/// at any more.
///
/// This directory had no bound at all: it grows by one copy per apply, and now
/// by one more per restore that finds the file edited. The settings engine keeps
/// its `settings-*.json` history to the same 50, so the discipline is the same
/// one, applied to the other ledger.
///
/// A copy either ledger still names is never a candidate - a [`FileSnapshot`]'s
/// copy is the only thing standing between an Undo and the original bytes
/// (exactly as `settings::prune_backups` protects a saver's `pre_install_backup`),
/// and a [`FileBackup`]'s copy is the only copy of the user's own work. Both
/// ledgers are swept for live names, so adding one could not quietly leave its
/// copies prunable. What is left over is the spent copies of records already
/// restored and dropped, kept as a recovery trail for somebody who has to go
/// digging by hand.
fn prune_file_backups(dir: &Path, state: &PiggyState) {
    let live: std::collections::HashSet<&std::ffi::OsStr> = state
        .file_snapshots
        .iter()
        .map(|s| s.backup.as_str())
        .chain(state.file_backups.iter().map(|b| b.backup.as_str()))
        .filter_map(|b| Path::new(b).file_name())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut spent: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "bak"))
        .filter(|p| !p.file_name().is_some_and(|n| live.contains(n)))
        .collect();
    if spent.len() <= MAX_SPENT_FILE_BACKUPS {
        return;
    }
    // Sorting the whole name would sort by content hash. The timestamp is what
    // orders these, and it is everything after the first hyphen.
    spent.sort_by(|a, b| backup_stamp(a).cmp(backup_stamp(b)));
    let remove_n = spent.len() - MAX_SPENT_FILE_BACKUPS;
    for old in spent.into_iter().take(remove_n) {
        let _ = std::fs::remove_file(old);
    }
}

/// The timestamp half of a `<sha>-<timestamp>.bak` name (a hash holds no hyphen,
/// so the first one is the separator). Empty for anything else, which sorts
/// oldest and so goes first.
fn backup_stamp(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split_once('-'))
        .map_or("", |(_, stamp)| stamp)
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

    // Every step names the file the caller asked for, not the temp file it goes
    // through. A per-item apply failure is shown to the user verbatim, and
    // `Permission denied at path ".tmp0KNcfU"` names something they have never
    // seen and which no longer exists by the time they read it.
    let target = || format!("writing {}", write_path.display());
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).with_context(target)?;
    tmp.write_all(bytes).with_context(target)?;
    tmp.as_file().sync_all().with_context(target)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // metadata() follows the symlink, so this is the target's mode.
        let mode = std::fs::metadata(write_path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o600);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))
            .with_context(target)?;
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
        // The other ledger: this is a plain copy of the user's own content, not
        // an edit of Piggy's that anything should try to reverse later.
        backup_only(path, state)
            .with_context(|| format!("backing up the current {} before restoring it", rec.path))?;
    }
    write_atomic(path, &bytes).with_context(|| format!("restoring {}", rec.path))
}

/// Whether what is on disk is something other than the bytes the apply wrote,
/// i.e. whether putting the original back would throw work away. A file with no
/// recorded [`FileSnapshot::after_hash`] is answered "edited", which costs one
/// spare copy and never costs the user their work.
///
/// The read error has to be discriminated, not swallowed. Only `NotFound` means
/// there is nothing on disk to lose; `EACCES`, `EPERM` and `EIO` mean there *is*
/// content and we could not copy it. Answering "not edited" for those would send
/// [`restore_one`] straight to [`write_atomic`], which needs only the parent
/// directory to be writable and would destroy unreadable content with no backup
/// anywhere.
fn edited_since_apply(path: &Path, rec: &FileSnapshot) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => rec.after_hash.as_deref() != Some(hash_bytes(&bytes).as_str()),
        // Nothing on disk to lose: the restore is what puts the file back.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        // Something is there and it is unreadable. Say "edited" so the snapshot
        // below fails on the same read and the item is reported as a per-item
        // failure rather than quietly overwritten.
        Err(_) => true,
    }
}
