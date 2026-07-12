//! Piggy's own state ledger at `~/.piggy/state.json`.
//!
//! This is the source of truth for *what Piggy did*, so that every action is
//! reversible with no guessing:
//!
//! * which savers are installed / enabled and at what version,
//! * the **exact hook objects Piggy injected** per saver (removal matches these
//!   structurally — user hooks are never touched),
//! * files Piggy created (downloaded binaries, managed docs),
//! * the pre-install `settings.json` backup for each saver (the byte-identical
//!   uninstall target),
//! * Sweep's disabled items with the exact JSON removed (for one-click restore),
//! * a backup ledger and the content hash of `settings.json` after Piggy's last
//!   write (used to detect external edits before the next write).
//!
//! Writes are atomic (temp-file + rename) just like the settings merge engine.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;

/// Current on-disk schema version for `state.json`.
pub const STATE_VERSION: u32 = 1;

/// The whole Piggy state document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiggyState {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Installed savers, keyed by saver id.
    #[serde(default)]
    pub savers: BTreeMap<String, SaverState>,
    /// Items Sweep has switched off, each with its restore snapshot.
    #[serde(default)]
    pub sweep_disabled: Vec<SweepDisabled>,
    /// Backup ledger (newest last).
    #[serde(default)]
    pub backups: Vec<BackupRecord>,
    /// Hash (sha256 hex) of `settings.json` bytes after Piggy's last write.
    /// `None` until Piggy has written once.
    #[serde(default)]
    pub settings_hash: Option<String>,
    /// User-tunable measurement settings (M3), e.g. the holdout fraction.
    #[serde(default)]
    pub settings: Settings,
    /// When Piggy first took ownership of this machine (RFC3339). Sessions that
    /// started before this are the `pre_install` observational baseline. Set once,
    /// the first time an M3 command runs against a state file that lacks it.
    #[serde(default)]
    pub created_at: Option<String>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for PiggyState {
    fn default() -> Self {
        PiggyState {
            version: STATE_VERSION,
            savers: BTreeMap::new(),
            sweep_disabled: Vec::new(),
            backups: Vec::new(),
            settings_hash: None,
            settings: Settings::default(),
            created_at: None,
        }
    }
}

/// User-tunable measurement settings, persisted in `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Fraction of rotation sessions that run with **all** savers off, so savings
    /// are measured against a live holdout rather than only pre-install history.
    #[serde(default = "default_holdout_fraction")]
    pub holdout_fraction: f64,
    /// When false, rotation never assigns a holdout session (badges fall back to
    /// the observational pre-install baseline and are labelled `estimated`).
    #[serde(default = "default_true")]
    pub holdout_enabled: bool,
}

fn default_holdout_fraction() -> f64 {
    0.1
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            holdout_fraction: default_holdout_fraction(),
            holdout_enabled: true,
        }
    }
}

/// Per-saver installed state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaverState {
    pub id: String,
    pub version: String,
    pub installed_at: String,
    /// `false` means installed-but-toggled-off (hooks removed / plugin disabled,
    /// artifacts kept) — the fast A/B path.
    pub enabled: bool,
    /// The exact hook-group objects Piggy injected, per event name. These are
    /// matched structurally on removal so user hooks are never disturbed.
    #[serde(default)]
    pub injected_hooks: BTreeMap<String, Vec<Value>>,
    /// Absolute paths of files Piggy created for this saver (binary, managed
    /// docs). Deleted on uninstall.
    #[serde(default)]
    pub installed_files: Vec<String>,
    /// Path to the `settings.json` backup captured immediately before this
    /// saver's first settings mutation — the byte-identical uninstall target.
    #[serde(default)]
    pub pre_install_backup: Option<String>,
    /// Who last flipped this saver's on/off state: `manual` (the user, via the
    /// GUI/CLI), `rotation`, or `holdout`. `manual` pauses rotation for this saver
    /// (Piggy respects an explicit choice). `None` == as-installed, never toggled.
    #[serde(default)]
    pub last_toggle_source: Option<String>,
}

/// One Sweep-disabled item (MCP server / plugin / skill) plus what to restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepDisabled {
    /// `"mcp"`, `"plugin"`, or `"skill"`.
    pub kind: String,
    /// Server name / `plugin@marketplace` / skill dir name.
    pub id: String,
    /// For MCP: the `~/.claude.json` project path the server was removed from.
    #[serde(default)]
    pub source: Option<String>,
    /// The exact JSON value removed (MCP server config, or the prior
    /// `enabledPlugins` bool), stored so restore is byte-faithful.
    #[serde(default)]
    pub snapshot: Value,
    /// For skills: where the directory was moved to (restore = move back).
    #[serde(default)]
    pub restore_path: Option<String>,
    pub disabled_at: String,
}

/// A `settings.json` backup on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub path: String,
    pub created_at: String,
    /// Why the backup was taken, e.g. `"pre-install:rtk"` or `"pre-write"`.
    pub reason: String,
}

impl PiggyState {
    /// Load state from `<piggy_home>/state.json`, or a fresh default if absent.
    /// A present-but-unparseable file is an error (never silently discarded —
    /// that would orphan installed artifacts).
    pub fn load() -> Result<Self> {
        Self::load_from(&config::state_path())
    }

    /// Load from an explicit path (used by tests).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading state file {}", path.display()))?;
        let state: PiggyState = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing state file {}", path.display()))?;
        Ok(state)
    }

    /// Persist to `<piggy_home>/state.json` atomically.
    pub fn save(&self) -> Result<()> {
        self.save_to(&config::state_path())
    }

    /// Persist to an explicit path atomically (temp-file + rename).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(json.as_bytes())?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(path)
            .map_err(|e| anyhow::anyhow!("persisting state file: {e}"))?;
        Ok(())
    }

    /// Convenience: is a saver present in the ledger (installed, enabled or not)?
    pub fn is_installed(&self, id: &str) -> bool {
        self.savers.contains_key(id)
    }

    /// Stamp `created_at` with the current time if it is unset, returning `true`
    /// if a change was made (so the caller can persist it). This anchors the
    /// `pre_install` baseline: the first time any M3 command runs, every session
    /// already on disk is treated as predating Piggy.
    pub fn ensure_created_at(&mut self) -> bool {
        if self.created_at.is_none() {
            self.created_at = Some(chrono::Utc::now().to_rfc3339());
            true
        } else {
            false
        }
    }

    /// The install-anchor timestamp (RFC3339), if known.
    pub fn install_time(&self) -> Option<&str> {
        self.created_at.as_deref()
    }
}
