//! The `settings.json` merge engine — Piggy's single write path into Claude
//! Code's configuration.
//!
//! Design goals (from `docs/m2-spec.md`), in priority order:
//!
//! 1. **Never clobber.** Every write is preceded by a timestamped backup, and
//!    the mutation is always applied to *freshly re-read* on-disk bytes, so an
//!    edit made by the user (or Claude Code) since Piggy's last write is carried
//!    forward, never lost. A hash mismatch against Piggy's last write is
//!    surfaced as an external-change warning, and a write that races us *during*
//!    a commit is snapshotted to a backup before our write lands (so it is
//!    recoverable even though it cannot be merged after the fact).
//! 2. **Structural ownership.** Piggy removes only hook objects it can match
//!    structurally against what [`crate::state`] recorded it injected. Wildcard
//!    user hooks (this machine's `openbar` hooks) are invisible to removal.
//! 3. **Byte-identical restore.** After a structural removal, if the result is
//!    value-equal to the pre-install backup, Piggy writes the backup's *exact
//!    bytes* — so an install→uninstall round-trip leaves `settings.json`
//!    byte-for-byte as it started.
//! 4. **Low diff noise.** 2-space indent (Claude Code's own format), preserved
//!    trailing newline, preserved CRLF/LF line-ending style, preserved unknown
//!    top-level keys and their order, and exact round-tripping of unknown numeric
//!    config (via serde_json `arbitrary_precision`).
//! 5. **Robust I/O.** Missing file (treated as `{}`), empty file, a UTF-8 BOM
//!    (stripped with a warning — a real corruption seen from another optimizer),
//!    atomic temp-file+rename with preserved permissions, and symlinked
//!    `settings.json` (written *through* the link, keeping dotfiles setups intact).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::config;
use crate::state::{BackupRecord, PiggyState};

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const MAX_TIMESTAMPED_BACKUPS: usize = 50;
const PRE_PIGGY: &str = "pre-piggy.json";

/// A parsed `settings.json` together with the formatting facts needed to write
/// it back with minimal diff.
#[derive(Debug, Clone)]
pub struct LoadedSettings {
    /// The parsed top-level object (always a JSON object; `{}` if missing/empty).
    pub value: Value,
    /// Exact original bytes as read from disk (including a BOM if present).
    pub raw: Vec<u8>,
    /// Whether the file existed on disk.
    pub existed: bool,
    /// Whether the original bytes began with a UTF-8 BOM (stripped for parsing).
    pub had_bom: bool,
    /// Whether the original content ended with a trailing newline.
    pub trailing_newline: bool,
    /// Whether the original file used CRLF (`\r\n`) line endings, so a rewrite
    /// preserves them instead of churning every line to LF.
    pub crlf: bool,
    /// Non-fatal problems worth surfacing (e.g. a stripped BOM).
    pub warnings: Vec<String>,
}

impl LoadedSettings {
    /// Serialize `value` back to bytes using the captured formatting: 2-space
    /// indent, preserved trailing newline, preserved CRLF/LF line-ending style,
    /// and **never** a re-emitted BOM.
    pub fn serialize(&self, value: &Value) -> Vec<u8> {
        let mut s = serde_json::to_string_pretty(value).expect("settings value serializes");
        if self.trailing_newline {
            s.push('\n');
        }
        if self.crlf {
            // `to_string_pretty` only ever emits LF, and the parsed value carries
            // no bare `\r`, so a plain replace restores the original CRLF endings.
            // (Newlines *inside* JSON strings are escaped as `\n`, not 0x0A, so
            // this never touches string contents.)
            s = s.replace('\n', "\r\n");
        }
        s.into_bytes()
    }
}

/// Read and parse `settings.json`, capturing formatting for a faithful rewrite.
///
/// A missing file yields `{}` with `existed=false` and a trailing newline (so a
/// freshly created file matches Claude Code's own writer). A present but
/// unparseable file is a hard error — Piggy refuses to overwrite data it cannot
/// understand.
pub fn load(path: &Path) -> Result<LoadedSettings> {
    if !path.exists() {
        return Ok(LoadedSettings {
            value: Value::Object(Map::new()),
            raw: Vec::new(),
            existed: false,
            had_bom: false,
            trailing_newline: true,
            crlf: false,
            warnings: Vec::new(),
        });
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut warnings = Vec::new();

    let (had_bom, body) = if raw.starts_with(&BOM) {
        warnings.push(format!(
            "{} began with a UTF-8 BOM (a known cause of Claude Code 'Invalid Settings' errors); Piggy stripped it",
            path.display()
        ));
        (true, &raw[3..])
    } else {
        (false, &raw[..])
    };

    let trailing_newline = body.last() == Some(&b'\n');
    let crlf = body.windows(2).any(|w| w == b"\r\n");
    let text = std::str::from_utf8(body)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;

    let value = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        let v: Value = serde_json::from_str(text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        if !v.is_object() {
            bail!(
                "{} top-level is a {}, expected an object",
                path.display(),
                json_type(&v)
            );
        }
        v
    };

    Ok(LoadedSettings {
        value,
        raw,
        existed: true,
        had_bom,
        trailing_newline,
        crlf,
        warnings,
    })
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// sha256 hex digest of the given bytes (content hash for external-change
/// detection and the backup ledger).
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Hook merge / remove (pure functions over a Value)
// ---------------------------------------------------------------------------

/// Additively merge hook-group objects into `value["hooks"][event]`.
///
/// User (and prior) entries keep their positions; Piggy's groups are appended,
/// so pre-existing user hooks always stay first in their arrays.
///
/// Malformed-but-present input is never silently clobbered:
/// * If `value["hooks"]` exists and is **not an object** (e.g. a user hand-wrote
///   `"hooks": []`), it is the user's data — Piggy leaves it exactly as-is and
///   injects nothing. The post-install `hook_present` health check then fails and
///   the install rolls back cleanly, rather than Piggy destroying the value.
/// * If a specific event slot (`hooks[event]`) is present but not an array, it is
///   malformed for Claude Code; Piggy replaces it with a fresh array so its hook
///   is actually installed (never a silent no-op that reports success).
///
/// Returns the exact objects actually inserted, per event — the caller records
/// **this** (not the requested set) so state can never claim a hook was injected
/// when it was not.
pub fn merge_hooks(value: &mut Value, hooks: &Map<String, Value>) -> Map<String, Value> {
    let root = ensure_object(value);
    // Never clobber a malformed-but-present `hooks` value: leave it untouched.
    if root.get("hooks").map(|h| !h.is_object()).unwrap_or(false) {
        return Map::new();
    }
    let hooks_map = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("hooks is an object at this point");

    let mut injected: Map<String, Value> = Map::new();
    for (event, groups) in hooks {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        let slot = hooks_map
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        // A non-array event slot is malformed — replace it so the injection is
        // real rather than a silent no-op.
        if !slot.is_array() {
            *slot = Value::Array(Vec::new());
        }
        if let Value::Array(existing) = slot {
            let mut added = Vec::new();
            for g in groups {
                existing.push(g.clone());
                added.push(g.clone());
            }
            if !added.is_empty() {
                injected.insert(event.clone(), Value::Array(added));
            }
        }
    }
    injected
}

/// Structurally remove previously-injected hook groups.
///
/// For each event, each injected group is matched by **value equality** against
/// the current array and the first match is removed (object comparison is
/// key-order-independent; user hooks with different content never match). Arrays
/// emptied by removal — and a `hooks` object emptied of all events — are pruned,
/// leaving no Piggy residue.
///
/// Returns the number of groups actually removed (for reporting; a mismatch
/// means the user edited the entry, and it is left in place).
pub fn remove_hooks(value: &mut Value, injected: &Map<String, Value>) -> usize {
    let Some(root) = value.as_object_mut() else {
        return 0;
    };
    let Some(Value::Object(hooks_map)) = root.get_mut("hooks") else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied_events: Vec<String> = Vec::new();
    for (event, groups) in injected {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        let Some(Value::Array(arr)) = hooks_map.get_mut(event) else {
            continue;
        };
        for g in groups {
            if let Some(pos) = arr.iter().position(|e| e == g) {
                arr.remove(pos);
                removed += 1;
            }
        }
        if arr.is_empty() {
            emptied_events.push(event.clone());
        }
    }
    for event in emptied_events {
        hooks_map.remove(&event);
    }
    if hooks_map.is_empty() {
        root.remove("hooks");
    }
    removed
}

/// Does `value` contain a hook whose command string contains `needle`, under
/// the given event? Backs the `hook_present` health check.
pub fn hook_command_contains(value: &Value, event: &str, needle: &str) -> bool {
    value
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(Value::as_array)
        .map(|groups| {
            groups.iter().any(|grp| {
                grp.get("hooks")
                    .and_then(Value::as_array)
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(Value::as_str)
                                .map(|c| c.contains(needle))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().unwrap()
}

// ---------------------------------------------------------------------------
// The committing write path (backup → apply → atomic write → hash)
// ---------------------------------------------------------------------------

/// A byte-identical restore target: if a mutation's result is value-equal to
/// `value`, the engine writes `bytes` verbatim instead of re-serializing.
pub struct ByteRestore {
    pub value: Value,
    pub bytes: Vec<u8>,
}

/// Outcome of a committed settings write.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// The final bytes written to disk.
    pub bytes: Vec<u8>,
    /// sha256 hex of `bytes` (also stored into `state.settings_hash`).
    pub hash: String,
    /// Path of the timestamped backup taken before this write (if any content
    /// existed to back up).
    pub backup_path: Option<PathBuf>,
    /// True if a byte-identical restore target matched and its exact bytes were
    /// written.
    pub byte_identical: bool,
    /// True if the on-disk content had changed since Piggy's last write.
    pub external_change: bool,
    /// Non-fatal warnings (BOM strip, external change, …).
    pub warnings: Vec<String>,
}

/// Apply `mutate` to the current on-disk `settings.json` and commit it.
///
/// This is the single entry point for all Piggy writes. Steps:
/// 1. Load current bytes (fresh — so external edits are preserved).
/// 2. Detect an external change vs `state.settings_hash` (warn only).
/// 3. Back up current bytes (timestamped; seed `pre-piggy.json` once).
/// 4. Run `mutate` on the freshly-loaded value.
/// 5. If a `byte_restore` target matches the result, write its exact bytes;
///    otherwise serialize with preserved formatting.
/// 6. Atomically write, preserving permissions, and update `state.settings_hash`.
///
/// The caller is responsible for persisting `state` afterwards.
pub fn commit<F>(
    path: &Path,
    reason: &str,
    state: &mut PiggyState,
    byte_restore: Option<&ByteRestore>,
    mutate: F,
) -> Result<CommitOutcome>
where
    F: FnOnce(&mut Value),
{
    let loaded = load(path)?;
    let mut warnings = loaded.warnings.clone();

    // External-change detection: compare current on-disk hash to what Piggy
    // recorded after its last write.
    let current_hash = if loaded.existed {
        Some(hash_bytes(&loaded.raw))
    } else {
        None
    };
    let external_change = match (&state.settings_hash, &current_hash) {
        (Some(prev), Some(cur)) => prev != cur,
        _ => false,
    };
    if external_change {
        warnings.push(
            "settings.json changed since Piggy's last write; re-merging onto the current content"
                .to_string(),
        );
    }

    // Back up current content before mutating.
    let backup_path = backup(&loaded, reason, state)?;

    // Apply the mutation to the fresh value.
    let mut new_value = loaded.value.clone();
    mutate(&mut new_value);

    // Concurrent-write guard (TOCTOU): if the on-disk bytes changed between our
    // initial read and now — a racing editor or Claude Code save landing while we
    // computed the new value — snapshot that raced content into a backup *before*
    // our atomic write overwrites it. Without this the racing edit would be in
    // neither settings.json nor any backup. We cannot merge it (we already ran
    // `mutate`), but it is now always recoverable rather than silently lost.
    if loaded.existed {
        if let Ok(current) = std::fs::read(path) {
            if current != loaded.raw {
                let dir = config::backups_dir();
                if std::fs::create_dir_all(&dir).is_ok() {
                    let _ = write_timestamped_backup(&dir, &current, "concurrent-write", state);
                }
                warnings.push(
                    "settings.json was modified during Piggy's write; the racing content was backed up before Piggy's write landed"
                        .to_string(),
                );
            }
        }
    }

    // Choose bytes: byte-identical restore if the result matches the target.
    let (bytes, byte_identical) = match byte_restore {
        Some(br) if br.value == new_value => (br.bytes.clone(), true),
        _ => (loaded.serialize(&new_value), false),
    };

    atomic_write(path, &bytes, &loaded)?;

    let hash = hash_bytes(&bytes);
    state.settings_hash = Some(hash.clone());

    Ok(CommitOutcome {
        bytes,
        hash,
        backup_path,
        byte_identical,
        external_change,
        warnings,
    })
}

/// Back up the current on-disk content of `path` *without modifying it*, seeding
/// `pre-piggy.json` on first use. Unlike a no-op [`commit`], this never rewrites
/// (or reformats / BOM-strips) `settings.json`. Used for the pre/post-`claude`
/// snapshots and by Restore Defaults before it overwrites the file. Returns the
/// timestamped backup path (None if there is nothing on disk).
pub fn backup_only(path: &Path, reason: &str, state: &mut PiggyState) -> Result<Option<PathBuf>> {
    if !path.exists() {
        // Still (potentially) seeds nothing — there was no file before Piggy.
        return Ok(None);
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    backup_raw(&raw, reason, state)
}

/// Back up the current content of `path` to `<backups>/settings-<ts>.json`,
/// seeding `pre-piggy.json` on first use, and pruning old timestamped backups.
/// Returns the timestamped backup path (None if there was nothing to back up).
fn backup(
    loaded: &LoadedSettings,
    reason: &str,
    state: &mut PiggyState,
) -> Result<Option<PathBuf>> {
    if !loaded.existed {
        // Nothing on disk to snapshot (and nothing to seed pre-piggy from).
        return Ok(None);
    }
    backup_raw(&loaded.raw, reason, state)
}

/// Seed `pre-piggy.json` (once) from `raw`, then write a unique timestamped
/// backup of `raw` and prune. Shared by [`backup`], [`backup_only`], and the
/// concurrent-write guard so every backup path is collision-free.
fn backup_raw(raw: &[u8], reason: &str, state: &mut PiggyState) -> Result<Option<PathBuf>> {
    let dir = config::backups_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Seed the one-time pre-Piggy backup (the Restore Defaults target). Its
    // *absence* is the sentinel for "there was no settings.json before Piggy".
    let pre = dir.join(PRE_PIGGY);
    if !pre.exists() {
        std::fs::write(&pre, raw).with_context(|| format!("writing {}", pre.display()))?;
    }

    let backup_path = write_timestamped_backup(&dir, raw, reason, state)?;
    Ok(Some(backup_path))
}

/// Write `raw` to a fresh, non-colliding `settings-<ts>.json` under `dir`, record
/// it in the ledger, and prune. A nanosecond timestamp plus an existence-checked
/// suffix guarantees two backups taken in the same instant never overwrite each
/// other (which previously could repoint a saver's `pre_install_backup` at the
/// wrong content).
fn write_timestamped_backup(
    dir: &Path,
    raw: &[u8],
    reason: &str,
    state: &mut PiggyState,
) -> Result<PathBuf> {
    let backup_path = unique_backup_path(dir);
    std::fs::write(&backup_path, raw)
        .with_context(|| format!("writing {}", backup_path.display()))?;
    state.backups.push(BackupRecord {
        path: backup_path.to_string_lossy().into_owned(),
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: reason.to_string(),
    });
    prune_backups(dir, state)?;
    Ok(backup_path)
}

/// A `settings-<ts>.json` path that does not yet exist (nanosecond timestamp,
/// with a numeric suffix as a same-instant tiebreaker).
fn unique_backup_path(dir: &Path) -> PathBuf {
    let ts = chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        .replace(':', "-");
    let base = dir.join(format!("settings-{ts}.json"));
    if !base.exists() {
        return base;
    }
    for i in 1.. {
        let p = dir.join(format!("settings-{ts}-{i}.json"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// Keep only the most recent [`MAX_TIMESTAMPED_BACKUPS`] `settings-*.json` files.
/// `pre-piggy.json` is never counted or removed, and any timestamped backup that
/// is a currently-installed saver's `pre_install_backup` (its byte-identical
/// uninstall target) is protected — otherwise a busy machine could prune it and
/// silently downgrade that saver's later uninstall to a structural removal.
fn prune_backups(dir: &Path, state: &mut PiggyState) -> Result<()> {
    let protected: std::collections::HashSet<String> = state
        .savers
        .values()
        .filter_map(|s| s.pre_install_backup.clone())
        .collect();
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("settings-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .filter(|p| !protected.contains(&p.to_string_lossy().into_owned()))
        .collect();
    if backups.len() <= MAX_TIMESTAMPED_BACKUPS {
        return Ok(());
    }
    backups.sort(); // lexical == chronological for our timestamp format
    let remove_n = backups.len() - MAX_TIMESTAMPED_BACKUPS;
    for old in backups.into_iter().take(remove_n) {
        let _ = std::fs::remove_file(&old);
        let old_s = old.to_string_lossy();
        state.backups.retain(|b| b.path != old_s);
    }
    Ok(())
}

/// Atomic write: temp file in the same directory, fsync, rename, then re-apply
/// the original file's permissions (or 0600 for a newly created file).
///
/// If `path` is a **symlink** (e.g. a dotfiles-managed `settings.json` from
/// stow/chezmoi), the write is directed *through* it: the temp file is created
/// next to the real target and renamed onto the target, so the symlink stays a
/// symlink and the tracked dotfiles source is what actually changes — rather than
/// the rename replacing the link with a regular file and leaving the source stale.
fn atomic_write(path: &Path, bytes: &[u8], loaded: &LoadedSettings) -> Result<()> {
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
        let mode = if loaded.existed {
            // metadata() follows the symlink, so this is the target's mode.
            std::fs::metadata(write_path)
                .map(|m| m.permissions().mode())
                .ok()
        } else {
            None
        };
        let mode = mode.unwrap_or(0o600);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = loaded;

    tmp.persist(write_path)
        .map_err(|e| anyhow::anyhow!("persisting {}: {e}", write_path.display()))?;
    Ok(())
}

/// If `path` is a symlink, resolve the concrete file it points at (so a write can
/// go *through* the link). `None` for a regular file, a missing file, or a broken
/// symlink (in which case the caller writes `path` directly).
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
