//! The install engine: interpret a catalog entry's declarative steps to install,
//! uninstall, toggle, and health-check a saver — with automatic rollback.
//!
//! Safety invariants:
//! * All writes to `settings.json` go through [`crate::settings`] (backup +
//!   atomic + structural ownership). The engine never edits it directly.
//! * Unknown step kinds refuse the action ("catalog newer than app") — the
//!   engine never guesses.
//! * A failed post-install health check triggers an automatic rollback to the
//!   pre-install state (settings restored, downloaded binary removed, plugin
//!   best-effort uninstalled) with a plain-language error.
//! * The real `claude` CLI is located via [`crate::config::claude_bin`], which
//!   tests point at a recording shim; the real binary is never run in tests.
//! * Network downloads follow redirects only to GitHub hosts, verify a sha256
//!   from the release's own checksum file, and can be satisfied offline from a
//!   local cache dir (`PIGGY_ASSET_CACHE_DIR`) so `cargo test` never hits the
//!   network.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use crate::config;
use crate::registry::{check_kind, step_kind, Catalog, Entry};
use crate::settings::{self, ByteRestore};
use crate::state::{PiggyState, SaverState};

/// Env var pointing at a directory that already holds `<asset>` and the
/// checksum file, used to satisfy `download_release_asset` offline (tests).
const ASSET_CACHE_ENV: &str = "PIGGY_ASSET_CACHE_DIR";

/// Result of an install / uninstall / toggle action, for the CLI to render.
#[derive(Debug, Clone, Default)]
pub struct ActionReport {
    pub saver: String,
    pub action: String,
    pub messages: Vec<String>,
    pub warnings: Vec<String>,
    pub health: Option<HealthReport>,
    /// True if an install was rolled back after a failed health check.
    pub rolled_back: bool,
}

/// Outcome of running a saver's health checks.
#[derive(Debug, Clone, Default)]
pub struct HealthReport {
    /// `(description, passed, detail)` per check.
    pub checks: Vec<(String, bool, String)>,
}

impl HealthReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|(_, p, _)| *p)
    }
    fn push(&mut self, desc: impl Into<String>, passed: bool, detail: impl Into<String>) {
        self.checks.push((desc.into(), passed, detail.into()));
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Install `id` from `catalog`: run its install steps, health-check, and roll
/// back automatically on failure.
pub fn install(catalog: &Catalog, id: &str) -> Result<ActionReport> {
    let entry = catalog
        .get(id)
        .ok_or_else(|| anyhow!("no saver named '{id}' in the registry"))?;
    entry
        .installable()
        .map_err(|e| anyhow!("cannot install '{id}': {e}"))?;
    if !entry.has_install_steps() {
        bail!("'{id}' has no install steps (it is display-only in this version)");
    }

    let mut state = PiggyState::load()?;
    if state.is_installed(id) {
        let mut r = ActionReport {
            saver: id.to_string(),
            action: "install".into(),
            ..Default::default()
        };
        r.messages
            .push(format!("'{id}' is already installed; nothing to do"));
        return Ok(r);
    }
    let settings_path = config::claude_settings_path();

    // Capture pre-install settings BEFORE auto-disabling any conflict, so a
    // rollback restores them — re-adding the conflicting saver's hooks / plugin
    // enable — as well as undoing this install.
    let pre = settings::load(&settings_path)?;
    let pre_bytes = pre.raw.clone();
    let pre_existed = pre.existed;

    // Mutual exclusion: rather than refuse, turn OFF any enabled saver that
    // conflicts with this one (symmetric — either side may declare it), so
    // turning one on cleanly wins. e.g. rtk ↔ headroom, headroom ↔ nadirclaw.
    let mut auto_disabled: Vec<String> = Vec::new();
    let mut auto_disable_warnings: Vec<String> = Vec::new();
    for other in conflicting_enabled_savers(catalog, entry, &state) {
        disable_saver_in_place(
            catalog,
            &other,
            &mut state,
            &settings_path,
            crate::store::source::MANUAL,
            &mut auto_disable_warnings,
        )
        .with_context(|| format!("auto-disabling '{other}' (conflicts with '{id}')"))?;
        auto_disabled.push(other);
    }
    if !auto_disabled.is_empty() {
        state.save()?;
    }

    let mut ctx = InstallCtx {
        entry,
        saver: SaverStateBuilder {
            id: id.to_string(),
            version: entry
                .source
                .pinned_version
                .clone()
                .unwrap_or_else(|| "n/a".into()),
            installed_at: chrono::Utc::now().to_rfc3339(),
            enabled: true,
            injected_hooks: BTreeMap::new(),
            installed_files: Vec::new(),
            pre_install_backup: None,
            asset_bytes: None,
        },
        warnings: Vec::new(),
        settings_path: settings_path.clone(),
    };

    // Run install steps; on any error, roll back and return it.
    let mut run: Result<()> = Ok(());
    for step in &entry.install.steps {
        if let Err(e) = ctx.run_install_step(step, &mut state) {
            run = Err(e);
            break;
        }
    }

    if let Err(e) = run {
        let installed_files = ctx.saver.installed_files.clone();
        rollback(
            &mut state,
            id,
            &settings_path,
            &pre_bytes,
            pre_existed,
            &installed_files,
            &auto_disabled,
        );
        state.save()?;
        return Err(e.context(format!("install of '{id}' failed and was rolled back")));
    }

    let mut warnings = ctx.warnings.clone();
    warnings.extend(auto_disable_warnings.iter().cloned());
    let saver: SaverState = ctx.saver.clone().into();
    let installed_files = saver.installed_files.clone();
    drop(ctx);
    state.savers.insert(id.to_string(), saver);
    state.save()?;

    // Health check → rollback on failure.
    let health = run_health_checks(entry, &settings_path)?;
    if !health.ok() {
        rollback(
            &mut state,
            id,
            &settings_path,
            &pre_bytes,
            pre_existed,
            &installed_files,
            &auto_disabled,
        );
        state.save()?;
        let failed: Vec<String> = health
            .checks
            .iter()
            .filter(|(_, p, _)| !p)
            .map(|(d, _, det)| format!("{d} ({det})"))
            .collect();
        return Ok(ActionReport {
            saver: id.to_string(),
            action: "install".into(),
            messages: vec![format!(
                "'{id}' failed its health check and was rolled back — your setup is unchanged"
            )],
            warnings,
            health: Some(health),
            rolled_back: true,
        })
        .map(|mut r| {
            r.warnings
                .push(format!("failed checks: {}", failed.join("; ")));
            r
        });
    }

    let mut messages = vec![format!("turned on '{}' ({})", entry.name, id)];
    for other in &auto_disabled {
        messages.push(format!("turned off '{other}' (conflicts with '{id}')"));
    }
    Ok(ActionReport {
        saver: id.to_string(),
        action: "install".into(),
        messages,
        warnings,
        health: Some(health),
        rolled_back: false,
    })
}

/// Uninstall `id`: run its uninstall steps and remove it from state. Hook savers
/// get a byte-identical settings restore when the structural removal returns the
/// file to its pre-install content.
pub fn uninstall(catalog: &Catalog, id: &str) -> Result<ActionReport> {
    let entry = catalog
        .get(id)
        .ok_or_else(|| anyhow!("no saver named '{id}' in the registry"))?;
    let mut state = PiggyState::load()?;
    if !state.is_installed(id) {
        let mut r = ActionReport {
            saver: id.to_string(),
            action: "uninstall".into(),
            ..Default::default()
        };
        r.messages.push(format!("'{id}' is not installed"));
        return Ok(r);
    }
    entry
        .installable()
        .map_err(|e| anyhow!("cannot uninstall '{id}': {e}"))?;

    let settings_path = config::claude_settings_path();
    let mut warnings = Vec::new();
    let mut messages = Vec::new();

    for step in &entry.uninstall.steps {
        match run_uninstall_step(entry, id, step, &settings_path, &mut state, &mut warnings) {
            Ok(Some(msg)) => messages.push(msg),
            Ok(None) => {}
            Err(e) => {
                let ignore = step
                    .get("ignoreFailure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ignore {
                    warnings.push(format!(
                        "uninstall step {} failed (ignored): {e}",
                        step_kind(step)
                    ));
                } else {
                    return Err(e.context(format!("uninstall of '{id}' failed")));
                }
            }
        }
    }

    state.savers.remove(id);
    state.save()?;
    messages.push(format!("turned off and removed '{}' ({})", entry.name, id));

    Ok(ActionReport {
        saver: id.to_string(),
        action: "uninstall".into(),
        messages,
        warnings,
        health: None,
        rolled_back: false,
    })
}

/// Toggle a saver on or off without uninstalling it (the fast A/B path),
/// recording the toggle as `manual` — i.e. an explicit user choice, which pauses
/// rotation for this saver. Rotation uses [`set_enabled_src`] with a non-manual
/// source instead.
///
/// Hook savers remove/re-add their owned hooks; plugin savers disable/enable via
/// the `claude` CLI; the binary/plugin stays installed either way.
pub fn set_enabled(catalog: &Catalog, id: &str, on: bool) -> Result<ActionReport> {
    set_enabled_src(catalog, id, on, crate::store::source::MANUAL)
}

/// As [`set_enabled`], but records `source` (`manual` / `rotation` / `holdout`)
/// so the attribution layer knows who last flipped this saver. A `manual` source
/// pauses rotation for the saver; `rotation`/`holdout` are the scheduler.
pub fn set_enabled_src(
    catalog: &Catalog,
    id: &str,
    on: bool,
    source: &str,
) -> Result<ActionReport> {
    let entry = catalog
        .get(id)
        .ok_or_else(|| anyhow!("no saver named '{id}' in the registry"))?;
    let mut state = PiggyState::load()?;
    let Some(saver) = state.savers.get(id).cloned() else {
        bail!("'{id}' is not installed - turn it on with `piggy install {id}` first");
    };
    let settings_path = config::claude_settings_path();
    let mut warnings = Vec::new();
    let mut messages = Vec::new();

    if saver.enabled == on {
        // Already in the requested state — but still record who asked, so a
        // manual "confirm on/off" pins the saver against rotation.
        if let Some(s) = state.savers.get_mut(id) {
            if s.last_toggle_source.as_deref() != Some(source) {
                s.last_toggle_source = Some(source.to_string());
                state.save()?;
            }
        }
        messages.push(format!(
            "'{id}' is already {}",
            if on { "on" } else { "off" }
        ));
        return Ok(ActionReport {
            saver: id.to_string(),
            action: if on { "on".into() } else { "off".into() },
            messages,
            warnings,
            health: None,
            rolled_back: false,
        });
    }

    // Turning a saver ON auto-disables any enabled saver it conflicts with, the
    // same mutual-exclusion rule a fresh install applies — otherwise
    // `off A → install B → on A` could leave two mutually-exclusive savers
    // enabled at once. The check is symmetric: either side may declare it.
    let mut auto_disabled: Vec<String> = Vec::new();
    if on {
        for other in conflicting_enabled_savers(catalog, entry, &state) {
            disable_saver_in_place(
                catalog,
                &other,
                &mut state,
                &settings_path,
                source,
                &mut warnings,
            )
            .with_context(|| format!("auto-disabling '{other}' (conflicts with '{id}')"))?;
            auto_disabled.push(other);
        }
    }

    let is_plugin = entry.install_type == "claude_plugin";
    if is_plugin {
        // Enable/disable via claude CLI (keeps the plugin installed). Backup
        // settings before AND after — the CLI writes enabledPlugins itself.
        let plugin = plugin_ref(entry);
        let verb = if on { "enable" } else { "disable" };
        snapshot(&settings_path, &format!("pre-{verb}:{id}"), &mut state)?;
        set_plugin_enabled_via_cli(&settings_path, &plugin, on)?;
        snapshot(&settings_path, &format!("post-{verb}:{id}"), &mut state)?;
        resync_settings_hash(&settings_path, &mut state)?;
    } else {
        // Hook saver: remove or re-add the exact owned hooks.
        if on {
            // Re-merge the recorded hooks.
            let merged: Map<String, Value> = saver
                .injected_hooks
                .iter()
                .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
                .collect();
            let outcome = settings::commit(
                &settings_path,
                &format!("on:{id}"),
                &mut state,
                None,
                |val| {
                    settings::merge_hooks(val, &merged);
                },
            )?;
            warnings.extend(outcome.warnings);
        } else {
            let injected: Map<String, Value> = saver
                .injected_hooks
                .iter()
                .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
                .collect();
            let outcome = settings::commit(
                &settings_path,
                &format!("off:{id}"),
                &mut state,
                None,
                |val| {
                    settings::remove_hooks(val, &injected);
                },
            )?;
            warnings.extend(outcome.warnings);
        }
    }

    if let Some(s) = state.savers.get_mut(id) {
        s.enabled = on;
        s.last_toggle_source = Some(source.to_string());
    }
    state.save()?;
    messages.push(format!(
        "turned {} '{}'",
        if on { "on" } else { "off" },
        entry.name
    ));
    for other in &auto_disabled {
        messages.push(format!("turned off '{other}' (conflicts with '{id}')"));
    }
    Ok(ActionReport {
        saver: id.to_string(),
        action: if on { "on".into() } else { "off".into() },
        messages,
        warnings,
        health: None,
        rolled_back: false,
    })
}

/// The ids of every installed, **enabled** saver that conflicts with `entry` —
/// in either direction (`entry.conflictsWith` names it, or its own
/// `conflictsWith` names `entry`). Shared by `install` and the `on` toggle so
/// mutual exclusion is enforced identically on both paths. Sorted for a stable
/// order in messages and tests.
fn conflicting_enabled_savers(catalog: &Catalog, entry: &Entry, state: &PiggyState) -> Vec<String> {
    let mut out: Vec<String> = state
        .savers
        .iter()
        .filter(|(other_id, other)| {
            if *other_id == &entry.id || !other.enabled {
                return false;
            }
            let declared_here = entry.conflicts_with.iter().any(|c| c == *other_id);
            let declared_there = catalog
                .get(other_id)
                .map(|oe| oe.conflicts_with.iter().any(|c| c == &entry.id))
                .unwrap_or(false);
            declared_here || declared_there
        })
        .map(|(id, _)| id.clone())
        .collect();
    out.sort();
    out
}

/// Turn OFF an installed, enabled saver in place, mutating `state` directly (no
/// fresh load) so a caller mid-operation keeps a single consistent state.
/// Plugin savers disable via the `claude` CLI; hook savers have their owned
/// hooks removed. The binary/plugin stays installed either way. This is the
/// mutual-exclusion auto-disable used by `install` and the `on` toggle; it does
/// not itself call `state.save()` — the caller persists.
fn disable_saver_in_place(
    catalog: &Catalog,
    id: &str,
    state: &mut PiggyState,
    settings_path: &Path,
    source: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(entry) = catalog.get(id) else {
        return Ok(());
    };
    let Some(saver) = state.savers.get(id).cloned() else {
        return Ok(());
    };
    if !saver.enabled {
        return Ok(());
    }
    if entry.install_type == "claude_plugin" {
        let plugin = plugin_ref(entry);
        snapshot(settings_path, &format!("pre-disable:{id}"), state)?;
        set_plugin_enabled_via_cli(settings_path, &plugin, false)?;
        snapshot(settings_path, &format!("post-disable:{id}"), state)?;
        resync_settings_hash(settings_path, state)?;
    } else {
        let injected: Map<String, Value> = saver
            .injected_hooks
            .iter()
            .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
            .collect();
        let outcome = settings::commit(settings_path, &format!("off:{id}"), state, None, |val| {
            settings::remove_hooks(val, &injected);
        })?;
        warnings.extend(outcome.warnings);
    }
    if let Some(s) = state.savers.get_mut(id) {
        s.enabled = false;
        s.last_toggle_source = Some(source.to_string());
    }
    Ok(())
}

/// Run a saver's declared health checks (also used by `piggy doctor`).
pub fn health_check(catalog: &Catalog, id: &str) -> Result<HealthReport> {
    let entry = catalog
        .get(id)
        .ok_or_else(|| anyhow!("no saver named '{id}' in the registry"))?;
    run_health_checks(entry, &config::claude_settings_path())
}

/// What `restore_defaults` did, for the CLI to report.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub swept_restored: usize,
    pub savers_removed: usize,
    pub files_removed: usize,
    /// True if `settings.json` was returned to its exact pre-Piggy bytes.
    pub byte_restored: bool,
    pub messages: Vec<String>,
}

/// The Restore Defaults panic button: undo everything Piggy changed.
///
/// Restores every Sweep-disabled item, returns `settings.json` to its exact
/// pre-Piggy bytes (the one-time `pre-piggy.json` backup) when available — which
/// also clears any Piggy-added `enabledPlugins`/hook entries — otherwise strips
/// Piggy's owned hooks structurally, deletes Piggy-installed binaries, and
/// clears the saver ledger. Always safe to run.
pub fn restore_defaults() -> Result<RestoreReport> {
    let mut state = PiggyState::load()?;
    let mut report = RestoreReport {
        swept_restored: crate::sweep::restore_all(&mut state)?,
        ..Default::default()
    };

    let settings_path = config::claude_settings_path();
    let pre_piggy = config::backups_dir().join("pre-piggy.json");
    if pre_piggy.exists() {
        // Back up the CURRENT settings.json first: Restore Defaults overwrites it
        // with the pre-Piggy snapshot, so any edits the user made *after* Piggy's
        // last write must be captured here or they would be unrecoverable. This
        // preserves the "every write is preceded by a timestamped backup"
        // invariant even for the panic button.
        settings::backup_only(&settings_path, "pre-restore-defaults", &mut state)?;
        let bytes = std::fs::read(&pre_piggy)?;
        force_write(&settings_path, &bytes)?;
        state.settings_hash = Some(settings::hash_bytes(&bytes));
        report.byte_restored = true;
        report
            .messages
            .push("settings.json restored to its exact pre-Piggy contents".into());
    } else {
        // No pre-Piggy snapshot (Piggy never wrote, or the file was absent):
        // strip any owned hooks structurally.
        let injected: Map<String, Value> = state
            .savers
            .values()
            .flat_map(|s| s.injected_hooks.iter())
            .fold(Map::new(), |mut acc, (event, groups)| {
                let arr = acc
                    .entry(event.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(a) = arr {
                    a.extend(groups.iter().cloned());
                }
                acc
            });
        if !injected.is_empty() {
            let _ = settings::commit(
                &settings_path,
                "restore-defaults",
                &mut state,
                None,
                |val| {
                    settings::remove_hooks(val, &injected);
                },
            )?;
        }
    }

    // Remove Piggy-installed binaries and clear the ledger.
    for saver in state.savers.values() {
        for f in &saver.installed_files {
            if remove_path_best_effort(Path::new(f)) {
                report.files_removed += 1;
            }
        }
    }
    report.savers_removed = state.savers.len();
    state.savers.clear();
    state.save()?;

    report.messages.push(format!(
        "cleared {} saver(s), restored {} swept item(s)",
        report.savers_removed, report.swept_restored
    ));
    Ok(report)
}

// ---------------------------------------------------------------------------
// Install step interpreter
// ---------------------------------------------------------------------------

struct InstallCtx<'a> {
    entry: &'a Entry,
    saver: SaverStateBuilder,
    warnings: Vec<String>,
    settings_path: PathBuf,
}

/// `SaverState` plus the transient `asset_bytes` carried between the download
/// and extract steps.
struct SaverStateBuilder {
    id: String,
    version: String,
    installed_at: String,
    enabled: bool,
    injected_hooks: BTreeMap<String, Vec<Value>>,
    installed_files: Vec<String>,
    pre_install_backup: Option<String>,
    asset_bytes: Option<Vec<u8>>,
}

impl Clone for SaverStateBuilder {
    fn clone(&self) -> Self {
        SaverStateBuilder {
            id: self.id.clone(),
            version: self.version.clone(),
            installed_at: self.installed_at.clone(),
            enabled: self.enabled,
            injected_hooks: self.injected_hooks.clone(),
            installed_files: self.installed_files.clone(),
            pre_install_backup: self.pre_install_backup.clone(),
            asset_bytes: self.asset_bytes.clone(),
        }
    }
}

impl From<SaverStateBuilder> for SaverState {
    fn from(b: SaverStateBuilder) -> Self {
        SaverState {
            id: b.id,
            version: b.version,
            installed_at: b.installed_at,
            enabled: b.enabled,
            injected_hooks: b.injected_hooks,
            installed_files: b.installed_files,
            pre_install_backup: b.pre_install_backup,
            // Freshly installed: enabled as-installed, never explicitly toggled,
            // no per-saver options chosen yet.
            last_toggle_source: None,
            config: BTreeMap::new(),
        }
    }
}

impl InstallCtx<'_> {
    fn run_install_step(&mut self, step: &Value, state: &mut PiggyState) -> Result<()> {
        match step_kind(step) {
            "download_release_asset" => self.step_download(step),
            "extract_binary" => self.step_extract(step),
            "merge_hooks" => self.step_merge_hooks(step, state),
            "claude_cli" => self.step_claude_cli(step, state),
            "require_binary" => self.step_require_binary(step),
            "ensure_dir_on_path" => self.step_ensure_dir_on_path(step),
            "require_python" => self.step_require_python(step),
            "create_venv" => self.step_create_venv(step),
            "pip_install" => self.step_pip_install(step),
            "write_launcher" => self.step_write_launcher(step),
            "builtin_enable" => Ok(()), // sweep: state bookkeeping only (recorded on insert)
            other => bail!("unknown install step '{other}' - catalog is newer than Piggy"),
        }
    }

    fn step_download(&mut self, _step: &Value) -> Result<()> {
        let src = &self.entry.source;
        let repo = src
            .repo
            .as_deref()
            .ok_or_else(|| anyhow!("source.repo missing for '{}'", self.entry.id))?;
        let tag = src
            .pinned_version
            .as_deref()
            .ok_or_else(|| anyhow!("source.pinnedVersion missing for '{}'", self.entry.id))?;
        let key = arch_key();
        let asset = src
            .assets
            .get(&key)
            .ok_or_else(|| anyhow!("no release asset for this platform ({key})"))?;
        let checksum_file = src.checksum_file.as_deref().unwrap_or("checksums.txt");

        let (asset_bytes, checksums) = fetch_asset(repo, tag, asset, checksum_file)?;
        let expected = checksum_for(&checksums, asset)
            .ok_or_else(|| anyhow!("{asset} not listed in {checksum_file}"))?;
        let actual = settings::hash_bytes(&asset_bytes);
        if !actual.eq_ignore_ascii_case(&expected) {
            bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
        }
        self.warnings
            .push(format!("verified {asset} (sha256 {})", &actual[..16]));
        self.saver.asset_bytes = Some(asset_bytes);
        Ok(())
    }

    fn step_extract(&mut self, step: &Value) -> Result<()> {
        let binary = step
            .get("binary")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("extract_binary: missing 'binary'"))?;
        let dest = step
            .get("dest")
            .and_then(Value::as_str)
            .map(expand_path)
            .unwrap_or_else(|| config::piggy_bin_dir().join(binary));
        let bytes =
            self.saver.asset_bytes.take().ok_or_else(|| {
                anyhow!("extract_binary: no downloaded asset (download step first)")
            })?;
        extract_gz_binary(&bytes, binary, &dest)
            .with_context(|| format!("extracting {binary} to {}", dest.display()))?;
        self.saver
            .installed_files
            .push(dest.to_string_lossy().into_owned());
        self.warnings.push(format!(
            "installed {} ({} bytes)",
            dest.display(),
            bytes.len()
        ));
        Ok(())
    }

    /// Ensure `dir` (default `${PIGGY_BIN}`) is on `PATH` by appending a
    /// delimited, idempotent block to the user's shell profile. Needed by savers
    /// whose runtime invokes the binary by bare name (e.g. rtk's command
    /// rewrite). Reversed by the `remove_dir_from_path` uninstall step.
    fn step_ensure_dir_on_path(&mut self, step: &Value) -> Result<()> {
        let dir = step
            .get("dir")
            .and_then(Value::as_str)
            .map(expand_str)
            .unwrap_or_else(|| config::piggy_bin_dir().to_string_lossy().into_owned());
        let profile = config::shell_profile_path();
        let changed = ensure_path_block(&profile, &dir)
            .with_context(|| format!("adding {dir} to PATH via {}", profile.display()))?;
        self.warnings.push(if changed {
            format!("added {dir} to PATH in {}", profile.display())
        } else {
            format!("{dir} already on PATH in {}", profile.display())
        });
        Ok(())
    }

    fn step_merge_hooks(&mut self, step: &Value, state: &mut PiggyState) -> Result<()> {
        let hooks_val = step
            .get("hooks")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("merge_hooks: missing 'hooks' object"))?;
        // Expand ${PIGGY_BIN} in every command string before injecting.
        let expanded = expand_hook_placeholders(hooks_val);

        // Record pre-install backup on the first settings write. Capture exactly
        // what `merge_hooks` actually injected (which can differ from `expanded`
        // if the user's `hooks` value was malformed) so state never claims a hook
        // was added that was not.
        let mut actually_injected: Map<String, Value> = Map::new();
        let outcome = settings::commit(
            &self.settings_path,
            &format!("pre-install:{}", self.entry.id),
            state,
            None,
            |val| {
                actually_injected = settings::merge_hooks(val, &expanded);
            },
        )?;
        if self.saver.pre_install_backup.is_none() {
            self.saver.pre_install_backup = outcome
                .backup_path
                .map(|p| p.to_string_lossy().into_owned());
        }
        self.warnings.extend(outcome.warnings);
        // Record exactly what we injected, per event, for structural removal.
        for (event, groups) in &actually_injected {
            if let Some(arr) = groups.as_array() {
                self.saver
                    .injected_hooks
                    .entry(event.clone())
                    .or_default()
                    .extend(arr.iter().cloned());
            }
        }
        Ok(())
    }

    fn step_claude_cli(&mut self, step: &Value, state: &mut PiggyState) -> Result<()> {
        let args = string_args(step)?;
        // Backup settings before AND after (plugin installs write to it).
        let backup = snapshot(
            &self.settings_path,
            &format!("pre-cli:{}", self.entry.id),
            state,
        )?;
        if self.saver.pre_install_backup.is_none() {
            self.saver.pre_install_backup = backup.map(|p| p.to_string_lossy().into_owned());
        }
        let ignore = step
            .get("ignoreFailure")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        run_claude(&args, ignore).with_context(|| format!("claude {}", args.join(" ")))?;
        snapshot(
            &self.settings_path,
            &format!("post-cli:{}", self.entry.id),
            state,
        )?;
        resync_settings_hash(&self.settings_path, state)?;
        Ok(())
    }

    fn step_require_binary(&mut self, step: &Value) -> Result<()> {
        let bin = step
            .get("binary")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("require_binary: missing 'binary'"))?;
        let soft = step.get("soft").and_then(Value::as_bool).unwrap_or(false);
        if !binary_on_path(bin) {
            let reason = step
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("required by this saver");
            if soft {
                self.warnings.push(format!(
                    "'{bin}' not found on PATH ({reason}); continuing degraded"
                ));
            } else {
                bail!("'{bin}' is required but not found on PATH ({reason})");
            }
        }
        Ok(())
    }

    /// Verify a Python interpreter of at least `minVersion` is available (a
    /// Python-package saver like Headroom needs it to build its venv). The
    /// interpreter is resolved via [`config::python_bin`] so tests can point it
    /// at a shim.
    fn step_require_python(&mut self, step: &Value) -> Result<()> {
        let py = config::python_bin();
        let min = step
            .get("minVersion")
            .and_then(Value::as_str)
            .unwrap_or("3.10");
        let out = Command::new(&py).arg("--version").output();
        let ver = match out {
            Ok(o) if o.status.success() => {
                // `python --version` prints to stdout on 3.4+, stderr on older.
                let s = String::from_utf8_lossy(&o.stdout);
                let s = if s.trim().is_empty() {
                    String::from_utf8_lossy(&o.stderr).into_owned()
                } else {
                    s.into_owned()
                };
                s.trim().trim_start_matches("Python").trim().to_string()
            }
            _ => {
                let reason = step
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("required by this saver");
                bail!(
                    "Python interpreter `{py}` not found or not runnable ({reason}); \
                     install Python {min}+ and try again"
                );
            }
        };
        if !version_at_least(&ver, min) {
            bail!("Python {min}+ is required but `{py}` reports {ver}");
        }
        self.warnings
            .push(format!("found Python {ver} (need {min}+)"));
        Ok(())
    }

    /// Create an isolated virtualenv at `dir` (`python -m venv`). The directory
    /// is recorded as an installed artifact so rollback / uninstall removes the
    /// whole tree.
    fn step_create_venv(&mut self, step: &Value) -> Result<()> {
        let dir = step
            .get("dir")
            .and_then(Value::as_str)
            .map(expand_path)
            .ok_or_else(|| anyhow!("create_venv: missing 'dir'"))?;
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let py = config::python_bin();
        run_cmd(
            &py,
            &[
                "-m".to_string(),
                "venv".to_string(),
                dir.to_string_lossy().into_owned(),
            ],
        )
        .with_context(|| format!("creating virtualenv at {}", dir.display()))?;
        self.saver
            .installed_files
            .push(dir.to_string_lossy().into_owned());
        self.warnings
            .push(format!("created Python venv at {}", dir.display()));
        Ok(())
    }

    /// `pip install` a package (with optional pinned `version` and pip extras
    /// already in `package`, e.g. `headroom-ai[all]`) into an existing venv.
    fn step_pip_install(&mut self, step: &Value) -> Result<()> {
        let venv = step
            .get("venv")
            .and_then(Value::as_str)
            .map(expand_path)
            .ok_or_else(|| anyhow!("pip_install: missing 'venv'"))?;
        let package = step
            .get("package")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("pip_install: missing 'package'"))?;
        let spec = match step.get("version").and_then(Value::as_str) {
            Some(v) => format!("{package}=={v}"),
            None => package.to_string(),
        };
        let pip = venv.join("bin").join("pip");
        run_cmd(
            &pip.to_string_lossy(),
            &[
                "install".to_string(),
                "--disable-pip-version-check".to_string(),
                spec.clone(),
            ],
        )
        .with_context(|| format!("pip install {spec}"))?;
        self.warnings
            .push(format!("installed {spec} into {}", venv.display()));
        Ok(())
    }

    /// Write an executable launcher shim into `${PIGGY_BIN}` that execs
    /// `<exec> <args...> "$@"`. This is the wrapper-launcher integration (e.g.
    /// `piggy-claude` → `headroom wrap claude`): it changes nothing global — no
    /// `ANTHROPIC_BASE_URL`, no daemon — so a session only routes through the
    /// wrapper when the user launches Claude via this command.
    fn step_write_launcher(&mut self, step: &Value) -> Result<()> {
        let name = step
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("write_launcher: missing 'name'"))?;
        let exec = step
            .get("exec")
            .and_then(Value::as_str)
            .map(expand_str)
            .ok_or_else(|| anyhow!("write_launcher: missing 'exec'"))?;
        let args: Vec<String> = step
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let dest = config::piggy_bin_dir().join(name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let quoted_args = args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "#!/bin/sh\n# Generated by Piggy - launches Claude Code through the {name} wrapper.\nexec {} {} \"$@\"\n",
            shell_quote(&exec),
            quoted_args
        );
        std::fs::write(&dest, script.as_bytes())
            .with_context(|| format!("writing launcher {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
        }
        self.saver
            .installed_files
            .push(dest.to_string_lossy().into_owned());
        self.warnings.push(format!(
            "launch Claude with `{name}` (in {}) to route the session through this saver",
            config::piggy_bin_dir().display()
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Uninstall step interpreter
// ---------------------------------------------------------------------------

fn run_uninstall_step(
    entry: &Entry,
    id: &str,
    step: &Value,
    settings_path: &Path,
    state: &mut PiggyState,
    warnings: &mut Vec<String>,
) -> Result<Option<String>> {
    match step_kind(step) {
        "remove_hooks" => {
            let saver = state
                .savers
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("no state for '{id}'"))?;
            let injected: Map<String, Value> = saver
                .injected_hooks
                .iter()
                .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
                .collect();
            // Byte-identical restore target: the pre-install backup.
            let byte_restore = load_byte_restore(saver.pre_install_backup.as_deref());
            let mut removed = 0usize;
            let outcome = settings::commit(
                settings_path,
                &format!("uninstall:{id}"),
                state,
                byte_restore.as_ref(),
                |val| {
                    removed = settings::remove_hooks(val, &injected);
                },
            )?;
            warnings.extend(outcome.warnings);
            let how = if outcome.byte_identical {
                "settings.json restored byte-identical to pre-install"
            } else {
                "hooks removed structurally (your later edits kept)"
            };
            Ok(Some(format!(
                "removed {removed} owned hook group(s); {how}"
            )))
        }
        "delete_file" => {
            let path = step
                .get("path")
                .and_then(Value::as_str)
                .map(expand_path)
                .ok_or_else(|| anyhow!("delete_file: missing 'path'"))?;
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("deleting {}", path.display()))?;
                Ok(Some(format!("deleted {}", path.display())))
            } else {
                Ok(None)
            }
        }
        "delete_dir" => {
            let dir = step
                .get("dir")
                .and_then(Value::as_str)
                .map(expand_path)
                .ok_or_else(|| anyhow!("delete_dir: missing 'dir'"))?;
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("deleting {}", dir.display()))?;
                Ok(Some(format!("deleted {}", dir.display())))
            } else {
                Ok(None)
            }
        }
        "remove_dir_from_path" => {
            // Keep the ${PIGGY_BIN} PATH line if another installed saver still
            // ships something there (rtk's binary, another launcher shim) — only
            // the last such saver's uninstall removes it, or we'd break the
            // survivor. (The current saver is still in `state` at this point.)
            if any_saver_uses_bin_dir(state, Some(id)) {
                warnings.push(
                    "kept the ${PIGGY_BIN} PATH line — another saver still uses it".to_string(),
                );
                return Ok(None);
            }
            // The opt-in `piggy` CLI link lives in the same directory but is
            // owned by the app, not by any saver, so it needs its own check:
            // without it, turning off the last saver would silently take `piggy`
            // off the user's PATH.
            if crate::cli_link::exists() {
                warnings.push(
                    "kept the ${PIGGY_BIN} PATH line: the piggy command line tool uses it"
                        .to_string(),
                );
                return Ok(None);
            }
            let profile = config::shell_profile_path();
            let removed = remove_path_block(&profile)
                .with_context(|| format!("removing Piggy PATH block from {}", profile.display()))?;
            Ok(if removed {
                Some(format!(
                    "removed Piggy PATH line from {}",
                    profile.display()
                ))
            } else {
                None
            })
        }
        "claude_cli" => {
            let args = string_args(step)?;
            snapshot(settings_path, &format!("pre-cli:{id}"), state)?;
            let ignore = step
                .get("ignoreFailure")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            run_claude(&args, ignore).with_context(|| format!("claude {}", args.join(" ")))?;
            snapshot(settings_path, &format!("post-cli:{id}"), state)?;
            resync_settings_hash(settings_path, state)?;
            Ok(Some(format!("ran claude {}", args.join(" "))))
        }
        "run_plugin_script" => {
            let script = step
                .get("script")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("run_plugin_script: missing 'script'"))?;
            let runner = step.get("runner").and_then(Value::as_str).unwrap_or("node");
            run_plugin_script(entry, runner, script)?;
            Ok(Some(format!("ran {runner} {script}")))
        }
        "verify_no_setting" => {
            let key = step.get("path").and_then(Value::as_str).unwrap_or("");
            let needle = step.get("contains").and_then(Value::as_str).unwrap_or("");
            let loaded = settings::load(settings_path)?;
            let present = loaded
                .value
                .get(key)
                .map(|v| v.to_string().contains(needle))
                .unwrap_or(false);
            if present {
                warnings.push(format!(
                    "leftover '{needle}' still present under settings key '{key}' after uninstall"
                ));
            }
            Ok(None)
        }
        "builtin_disable" => {
            // Sweep off: restore every item Sweep disabled, then drop them.
            let restored = crate::sweep::restore_all(state)?;
            Ok(Some(format!("restored {restored} swept item(s)")))
        }
        other => bail!("unknown uninstall step '{other}' - catalog is newer than Piggy"),
    }
}

// ---------------------------------------------------------------------------
// Health checks
// ---------------------------------------------------------------------------

fn run_health_checks(entry: &Entry, settings_path: &Path) -> Result<HealthReport> {
    let mut report = HealthReport::default();
    for check in &entry.health_check.checks {
        match check_kind(check) {
            "binary_runs" => {
                let cmd = check
                    .get("cmd")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(expand_str))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let (ok, detail) = run_binary_check(&cmd);
                report.push(format!("binary runs: {}", cmd.join(" ")), ok, detail);
            }
            "hook_present" => {
                let event = check.get("event").and_then(Value::as_str).unwrap_or("");
                let needle = check
                    .get("commandContains")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let loaded = settings::load(settings_path)?;
                let present = settings::hook_command_contains(&loaded.value, event, needle);
                report.push(
                    format!("hook present: {event} contains '{needle}'"),
                    present,
                    if present { "found" } else { "not found" },
                );
            }
            "plugin_enabled" => {
                let plugin = check.get("plugin").and_then(Value::as_str).unwrap_or("");
                let (ok, detail) = plugin_enabled(settings_path, plugin);
                report.push(format!("plugin enabled: {plugin}"), ok, detail);
            }
            "builtin" => {
                report.push("builtin module", true, "ok");
            }
            "path_configured" => {
                let profile = config::shell_profile_path();
                let content = std::fs::read_to_string(&profile).unwrap_or_default();
                let ok = content.contains(PIGGY_PATH_BEGIN);
                report.push(
                    format!("PATH configured in {}", profile.display()),
                    ok,
                    if ok {
                        "Piggy PATH block present"
                    } else {
                        "Piggy PATH block not found"
                    },
                );
            }
            other => {
                report.push(format!("unknown check '{other}'"), false, "unsupported");
            }
        }
    }
    Ok(report)
}

fn run_binary_check(cmd: &[String]) -> (bool, String) {
    let Some((prog, args)) = cmd.split_first() else {
        return (false, "empty command".into());
    };
    match Command::new(prog).args(args).output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            (true, v.lines().next().unwrap_or("").trim().to_string())
        }
        Ok(out) => (false, format!("exit {:?}", out.status.code())),
        Err(e) => (false, format!("could not run: {e}")),
    }
}

/// Is a plugin marked enabled in settings.json's `enabledPlugins`?
fn plugin_enabled(settings_path: &Path, plugin: &str) -> (bool, String) {
    let Ok(loaded) = settings::load(settings_path) else {
        return (false, "settings.json unreadable".into());
    };
    let enabled = loaded
        .value
        .get("enabledPlugins")
        .and_then(|p| p.get(plugin))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if enabled {
        (true, "enabledPlugins=true".into())
    } else {
        (false, "not in enabledPlugins".into())
    }
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// Restore the pre-install state after a failed install: settings.json back to
/// its exact pre-install bytes (or removed if it did not exist), downloaded
/// files removed, and a best-effort plugin uninstall.
fn rollback(
    state: &mut PiggyState,
    id: &str,
    settings_path: &Path,
    pre_bytes: &[u8],
    pre_existed: bool,
    installed_files: &[String],
    re_enable: &[String],
) {
    // Best-effort plugin uninstall (undoes plugin-cache / marketplace writes the
    // settings restore below cannot reach).
    if let Some(entry) = Catalog::embedded().get(id).cloned() {
        if entry.install_type == "claude_plugin" {
            let plugin = plugin_ref(&entry);
            let _ = run_claude(&["plugin".into(), "uninstall".into(), plugin], true);
        }
    }
    // Force settings back to pre-install bytes. Snapshot the current content
    // first (best effort) so a user edit that landed during the failed install
    // is recoverable rather than destroyed by the rollback.
    if pre_existed {
        let _ = settings::backup_only(settings_path, "pre-rollback", state);
        let _ = force_write(settings_path, pre_bytes);
        state.settings_hash = Some(settings::hash_bytes(pre_bytes));
    } else if settings_path.exists() {
        let _ = std::fs::remove_file(settings_path);
        state.settings_hash = None;
    }
    // Remove any files (or venv trees) we created.
    for f in installed_files {
        remove_path_best_effort(Path::new(f));
    }
    state.savers.remove(id);
    // Re-mark any saver we auto-disabled for this install: the pre-install
    // settings bytes were already restored above (re-adding its hooks / plugin
    // enable), so this only returns the state ledger to a consistent `enabled`.
    for other in re_enable {
        if let Some(s) = state.savers.get_mut(other) {
            s.enabled = true;
        }
    }
}

/// Non-atomic-safe force write used only by rollback (best effort).
fn force_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings snapshot helpers (backups without mutation)
// ---------------------------------------------------------------------------

/// Take a timestamped backup of the current settings.json without changing it.
/// Returns the backup path (None if there was nothing to snapshot).
///
/// This is a *pure* backup: it never rewrites the file. (A no-op `commit` would
/// re-serialize — reformatting and stripping a BOM — even when Piggy is only
/// installing a plugin and adds no hooks, which the doc explicitly says it must
/// not do.)
fn snapshot(settings_path: &Path, reason: &str, state: &mut PiggyState) -> Result<Option<PathBuf>> {
    settings::backup_only(settings_path, reason, state)
}

/// Re-read settings.json and update Piggy's recorded content hash (called after
/// the `claude` CLI writes to it, so the next commit does not see a false
/// external change).
fn resync_settings_hash(settings_path: &Path, state: &mut PiggyState) -> Result<()> {
    let loaded = settings::load(settings_path)?;
    state.settings_hash = if loaded.existed {
        Some(settings::hash_bytes(&loaded.raw))
    } else {
        None
    };
    Ok(())
}

/// Load a byte-identical restore target from a backup file path.
fn load_byte_restore(path: Option<&str>) -> Option<ByteRestore> {
    let path = path?;
    let bytes = std::fs::read(path).ok()?;
    // Parse for value-equality comparison (strip a BOM if the backup had one).
    let body = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        &bytes[..]
    };
    let text = std::str::from_utf8(body).ok()?;
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    Some(ByteRestore { value, bytes })
}

// ---------------------------------------------------------------------------
// Download / extract
// ---------------------------------------------------------------------------

/// Fetch the asset bytes and the checksum file text, from a local cache
/// (`PIGGY_ASSET_CACHE_DIR`) if set, else from GitHub Releases.
fn fetch_asset(
    repo: &str,
    tag: &str,
    asset: &str,
    checksum_file: &str,
) -> Result<(Vec<u8>, String)> {
    if let Ok(dir) = std::env::var(ASSET_CACHE_ENV) {
        let dir = PathBuf::from(dir);
        let a = std::fs::read(dir.join(asset))
            .with_context(|| format!("reading cached asset {asset}"))?;
        let c = std::fs::read_to_string(dir.join(checksum_file))
            .with_context(|| format!("reading cached {checksum_file}"))?;
        return Ok((a, c));
    }
    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    let asset_bytes = http_get_bytes(&format!("{base}/{asset}"))?;
    let checksums = String::from_utf8(http_get_bytes(&format!("{base}/{checksum_file}"))?)
        .context("checksum file is not UTF-8")?;
    Ok((asset_bytes, checksums))
}

/// HTTP GET following redirects only to GitHub-owned hosts (github.com,
/// githubusercontent.com), returning the body bytes.
fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let ok = attempt
            .url()
            .host_str()
            .map(is_github_host)
            .unwrap_or(false);
        if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else if ok {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let client = reqwest::blocking::Client::builder()
        .redirect(policy)
        .user_agent("piggy/0.1")
        .build()?;
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let resp = resp
        .error_for_status()
        .with_context(|| format!("GET {url} returned an error status"))?;
    Ok(resp.bytes()?.to_vec())
}

fn is_github_host(host: &str) -> bool {
    host == "github.com"
        || host.ends_with(".github.com")
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// Find the sha256 for `asset` in a `sha  filename` checksum file.
fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    for line in checksums.lines() {
        let mut it = line.split_whitespace();
        let sha = it.next()?;
        let name = it.next().unwrap_or("");
        // Checksum files sometimes prefix the filename with '*' (binary mode).
        let name = name.trim_start_matches('*');
        if name == asset {
            return Some(sha.to_string());
        }
    }
    None
}

/// Extract a single binary named `binary` from `.tar.gz` bytes to `dest`,
/// chmod 755.
fn extract_gz_binary(gz: &[u8], binary: &str, dest: &Path) -> Result<()> {
    let dec = flate2::read::GzDecoder::new(gz);
    let mut ar = tar::Archive::new(dec);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let matches = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == binary)
            .unwrap_or(false);
        if matches {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            std::fs::write(dest, &buf)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
            }
            return Ok(());
        }
    }
    bail!("binary '{binary}' not found in archive")
}

// ---------------------------------------------------------------------------
// claude CLI + misc helpers
// ---------------------------------------------------------------------------

/// Run the `claude` CLI (via [`config::claude_bin`]) with `args`. A missing
/// binary is reported as a clean "needs Claude Code CLI" error unless `ignore`.
fn run_claude(args: &[String], ignore_failure: bool) -> Result<()> {
    let bin = config::claude_bin();
    let mut cmd = Command::new(&bin);
    cmd.args(args);
    match output_bounded(cmd) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            if ignore_failure {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stderr = stderr.trim();
                let code = out.status.code();
                let cmd = args.join(" ");
                if stderr.is_empty() {
                    bail!("`{bin} {cmd}` failed (exit {code:?})")
                } else {
                    // Lead with stderr so the GUI banner, which truncates on ':',
                    // shows the human-readable reason instead of the raw command.
                    bail!("{stderr} (from `{bin} {cmd}` exit {code:?})")
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("this saver needs the Claude Code CLI, but `{bin}` was not found on your PATH")
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            if ignore_failure {
                Ok(())
            } else {
                bail!(
                    "`{bin} {}` did not finish within {}s and was stopped — it may be downloading; check your network and try again",
                    args.join(" "),
                    SUBPROCESS_TIMEOUT.as_secs()
                )
            }
        }
        Err(e) => bail!("could not run `{bin}`: {e}"),
    }
}

/// Enable or disable a plugin through the `claude` CLI while tolerating drift.
/// If Claude's settings already have the plugin in the requested state the CLI
/// call is skipped. And if the CLI refuses because reality already matches the
/// request (it reports the plugin "already disabled"/"already enabled"), that
/// counts as success so the caller can heal Piggy's ledger rather than abort.
fn set_plugin_enabled_via_cli(settings_path: &Path, plugin: &str, on: bool) -> Result<()> {
    if plugin_enabled(settings_path, plugin).0 == on {
        return Ok(());
    }
    let verb = if on { "enable" } else { "disable" };
    let args = vec!["plugin".to_string(), verb.to_string(), plugin.to_string()];
    match run_claude(&args, false) {
        Err(e) if plugin_enabled(settings_path, plugin).0 != on => Err(e),
        _ => Ok(()),
    }
}

fn run_plugin_script(entry: &Entry, runner: &str, script: &str) -> Result<()> {
    // Locate the plugin's install dir from installed_plugins.json.
    let plugin = plugin_ref(entry);
    let ledger = config::installed_plugins_path();
    let dir = std::fs::read_to_string(&ledger)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("plugins")?
                .get(&plugin)?
                .as_array()?
                .first()?
                .get("installPath")?
                .as_str()
                .map(PathBuf::from)
        });
    let Some(dir) = dir else {
        bail!("could not find install path for plugin '{plugin}' to run {script}");
    };
    let script_path = dir.join(script);
    let status = Command::new(runner)
        .arg(&script_path)
        .status()
        .with_context(|| format!("running {runner} {}", script_path.display()))?;
    if !status.success() {
        bail!(
            "{runner} {} exited with {:?}",
            script_path.display(),
            status.code()
        );
    }
    Ok(())
}

/// The `plugin@marketplace` reference for a plugin saver.
fn plugin_ref(entry: &Entry) -> String {
    let plugin = entry.source.plugin.as_deref().unwrap_or(&entry.id);
    match entry.source.marketplace.as_deref() {
        Some(m) => format!("{plugin}@{m}"),
        None => plugin.to_string(),
    }
}

/// Extract a `claude_cli` step's `args` array as owned Strings.
fn string_args(step: &Value) -> Result<Vec<String>> {
    step.get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| anyhow!("claude_cli: missing 'args' array"))
}

pub fn binary_on_path(bin: &str) -> bool {
    // Try `<bin> --version`; a spawn failure means it is not on PATH.
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|_| true)
        .unwrap_or(false)
}

/// Cap on install subprocesses that hit the network (`claude plugin` runs a git
/// clone; pip downloads). Without it a stalled clone or an offline network leaves
/// the whole GUI toggle spinning forever with no way out.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(120);

/// `Command::output()` but killed after [`SUBPROCESS_TIMEOUT`]. stdin is closed so
/// anything expecting a TTY fails fast rather than blocking. A timeout surfaces as
/// an `ErrorKind::TimedOut` io error for the caller to phrase.
// ponytail: 50ms poll + pipe-drain threads; outputs here are a few lines so the
// pipe buffer never fills. Swap for the `wait-timeout` crate if that stops holding.
fn output_bounded(cmd: Command) -> std::io::Result<Output> {
    output_bounded_with(cmd, SUBPROCESS_TIMEOUT)
}

fn output_bounded_with(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = out.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = err.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = out_h.join().unwrap_or_default();
            let stderr = err_h.join().unwrap_or_default();
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run an arbitrary command to completion, mapping a non-zero exit or a missing
/// binary to a clean error. Used by the venv/pip install steps.
fn run_cmd(prog: &str, args: &[String]) -> Result<()> {
    let mut cmd = Command::new(prog);
    cmd.args(args);
    match output_bounded(cmd) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "`{prog} {}` failed (exit {:?}): {}",
                args.join(" "),
                out.status.code(),
                stderr.trim()
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("`{prog}` was not found"),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => bail!(
            "`{prog} {}` did not finish within {}s and was stopped — check your network and try again",
            args.join(" "),
            SUBPROCESS_TIMEOUT.as_secs()
        ),
        Err(e) => bail!("could not run `{prog}`: {e}"),
    }
}

/// Single-quote a string for safe embedding in the generated `/bin/sh` launcher.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Compare a `python --version` string (`"3.12.0"`) against a `major.minor`
/// minimum (`"3.10"`), on major-then-minor only.
fn version_at_least(have: &str, min: &str) -> bool {
    fn parse(s: &str) -> (u32, u32) {
        let mut it = s.split(['.', ' ']).filter(|p| !p.is_empty());
        let maj = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let minor = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        (maj, minor)
    }
    parse(have) >= parse(min)
}

/// Remove a path whether it is a file or a directory tree; returns whether
/// anything was removed. Used for artifact cleanup (a saver's `installed_files`
/// can include a venv directory as well as plain files).
fn remove_path_best_effort(p: &Path) -> bool {
    if p.is_dir() {
        std::fs::remove_dir_all(p).is_ok()
    } else {
        std::fs::remove_file(p).is_ok()
    }
}

// ---------------------------------------------------------------------------
// Shell-profile PATH management
// ---------------------------------------------------------------------------

/// Opening marker of Piggy's managed `PATH` block in the shell profile.
const PIGGY_PATH_BEGIN: &str = "# >>> piggy (managed PATH) >>>";
/// Closing marker of Piggy's managed `PATH` block.
const PIGGY_PATH_END: &str = "# <<< piggy (managed PATH) <<<";

/// Whether any installed saver other than `except` keeps a file directly in
/// `<piggy_home>/bin`, i.e. whether something there still needs the managed
/// `PATH` line.
///
/// Only direct children count: a saver's venv lives under `<piggy_home>/venvs`
/// and is reached by absolute path, so it never justifies the `PATH` line.
pub(crate) fn any_saver_uses_bin_dir(state: &PiggyState, except: Option<&str>) -> bool {
    let bin = config::piggy_bin_dir();
    state.savers.iter().any(|(id, s)| {
        Some(id.as_str()) != except
            && s.installed_files
                .iter()
                .any(|f| Path::new(f).parent() == Some(bin.as_path()))
    })
}

/// Ensure `dir` is on `PATH` by appending a delimited block to `profile`.
///
/// Idempotent: if the block is already present it makes no change and returns
/// `false`. Returns `true` when it appended the block. The block is bounded by
/// [`PIGGY_PATH_BEGIN`]/[`PIGGY_PATH_END`] so [`remove_path_block`] can strip it
/// back out on uninstall without disturbing the user's own lines.
pub(crate) fn ensure_path_block(profile: &Path, dir: &str) -> Result<bool> {
    let existing = std::fs::read_to_string(profile).unwrap_or_default();
    if existing.contains(PIGGY_PATH_BEGIN) {
        return Ok(false);
    }
    let block = format!("{PIGGY_PATH_BEGIN}\nexport PATH=\"{dir}:$PATH\"\n{PIGGY_PATH_END}\n");
    let mut updated = existing;
    // Separate our block from prior content with exactly one blank line.
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
    }
    updated.push_str(&block);
    write_file_atomic(profile, updated.as_bytes())?;
    Ok(true)
}

/// Remove Piggy's managed `PATH` block (and the blank-line separator it added)
/// from `profile`. Returns `true` if a block was found and removed.
pub(crate) fn remove_path_block(profile: &Path) -> Result<bool> {
    let content = match std::fs::read_to_string(profile) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    let Some(mut start) = content.find(PIGGY_PATH_BEGIN) else {
        return Ok(false);
    };
    let Some(end_rel) = content[start..].find(PIGGY_PATH_END) else {
        return Ok(false);
    };
    let mut end = start + end_rel + PIGGY_PATH_END.len();
    // Consume the trailing newline after the closing marker, if any.
    if content[end..].starts_with('\n') {
        end += 1;
    }
    // Drop the single blank-line separator we inserted before the block,
    // leaving the newline that terminates the user's own last line.
    if content[..start].ends_with("\n\n") {
        start -= 1;
    }
    let mut updated = String::with_capacity(content.len());
    updated.push_str(&content[..start]);
    updated.push_str(&content[end..]);
    write_file_atomic(profile, updated.as_bytes())?;
    Ok(true)
}

/// Atomic write via a sibling temp file + rename, creating parent dirs as
/// needed. Used for the shell profile (not `settings.json`, which has its own
/// backup-and-commit path in [`crate::settings`]).
fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "profile".to_string());
    let tmp = parent.join(format!(".{name}.piggy-tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Expand `${PIGGY_BIN}`, `${PIGGY_HOME}`, and a leading `~` in a path string.
fn expand_path(s: &str) -> PathBuf {
    PathBuf::from(expand_str(s))
}

fn expand_str(s: &str) -> String {
    let piggy_bin = config::piggy_bin_dir().to_string_lossy().into_owned();
    let piggy_home = config::piggy_home().to_string_lossy().into_owned();
    let mut out = s
        .replace("${PIGGY_BIN}", &piggy_bin)
        .replace("${PIGGY_HOME}", &piggy_home);
    // Defensive: map any literal `~/.piggy` to the (env-overridable) piggy home,
    // so a catalog that hard-codes `~/.piggy/...` can never escape the sandbox in
    // tests or write outside PIGGY_HOME in production.
    if let Some(rest) = out.strip_prefix("~/.piggy/") {
        out = config::piggy_home()
            .join(rest)
            .to_string_lossy()
            .into_owned();
    } else if out == "~/.piggy" {
        out = piggy_home.clone();
    } else if let Some(rest) = out.strip_prefix("~/") {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        out = home.join(rest).to_string_lossy().into_owned();
    } else if out == "~" {
        out = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
    }
    out
}

/// Expand `${PIGGY_BIN}` (etc.) inside every hook command string of a
/// `merge_hooks` `hooks` object, returning the concrete objects to inject.
fn expand_hook_placeholders(hooks: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (event, groups) in hooks {
        let mut new_groups = Vec::new();
        if let Some(arr) = groups.as_array() {
            for grp in arr {
                new_groups.push(expand_value_commands(grp));
            }
        }
        out.insert(event.clone(), Value::Array(new_groups));
    }
    out
}

fn expand_value_commands(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut nm = Map::new();
            for (k, val) in m {
                if k == "command" {
                    if let Some(s) = val.as_str() {
                        nm.insert(k.clone(), Value::String(expand_str(s)));
                        continue;
                    }
                }
                nm.insert(k.clone(), expand_value_commands(val));
            }
            Value::Object(nm)
        }
        Value::Array(a) => Value::Array(a.iter().map(expand_value_commands).collect()),
        other => other.clone(),
    }
}

/// Map the running platform to a catalog `assets` key.
fn arch_key() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64".into(),
        ("macos", "x86_64") => "darwin-x86_64".into(),
        ("linux", "x86_64") => "linux-x86_64".into(),
        ("linux", "aarch64") => "linux-aarch64".into(),
        (os, arch) => format!("{os}-{arch}"),
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn output_bounded_kills_a_hung_child_and_returns_fast() {
        // `sleep 600` would block the install forever without the cap. Bound it low
        // so the test is quick, then assert it timed out well under the sleep.
        let mut cmd = Command::new("sleep");
        cmd.arg("600");
        let start = Instant::now();
        let err = output_bounded_with(cmd, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should stop promptly"
        );
    }

    #[test]
    fn output_bounded_returns_output_for_a_fast_command() {
        let mut cmd = Command::new("echo");
        cmd.arg("hi");
        let out = output_bounded_with(cmd, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}

#[cfg(test)]
mod path_block_tests {
    use super::*;
    use std::fs;

    fn tmp_profile(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("piggy-pathtest-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn appends_block_to_existing_profile_then_removes_it_cleanly() {
        let profile = tmp_profile("existing");
        fs::write(&profile, "export FOO=bar\n").unwrap();

        assert!(ensure_path_block(&profile, "/home/x/.piggy/bin").unwrap());
        let after = fs::read_to_string(&profile).unwrap();
        assert!(after.contains(PIGGY_PATH_BEGIN));
        assert!(after.contains("export PATH=\"/home/x/.piggy/bin:$PATH\""));
        // User's own line is preserved, separated by exactly one blank line.
        assert_eq!(after, format!("export FOO=bar\n\n{PIGGY_PATH_BEGIN}\nexport PATH=\"/home/x/.piggy/bin:$PATH\"\n{PIGGY_PATH_END}\n"));

        assert!(remove_path_block(&profile).unwrap());
        // Byte-identical restore of the user's original content.
        assert_eq!(fs::read_to_string(&profile).unwrap(), "export FOO=bar\n");
        let _ = fs::remove_file(&profile);
    }

    #[test]
    fn ensure_is_idempotent() {
        let profile = tmp_profile("idem");
        assert!(ensure_path_block(&profile, "/p/bin").unwrap());
        // Second call must not append a duplicate block.
        assert!(!ensure_path_block(&profile, "/p/bin").unwrap());
        let content = fs::read_to_string(&profile).unwrap();
        assert_eq!(content.matches(PIGGY_PATH_BEGIN).count(), 1);
        let _ = fs::remove_file(&profile);
    }

    #[test]
    fn remove_on_missing_or_unmanaged_profile_is_noop() {
        let missing = tmp_profile("missing");
        assert!(!remove_path_block(&missing).unwrap());

        let unmanaged = tmp_profile("unmanaged");
        fs::write(&unmanaged, "export FOO=bar\n").unwrap();
        assert!(!remove_path_block(&unmanaged).unwrap());
        assert_eq!(fs::read_to_string(&unmanaged).unwrap(), "export FOO=bar\n");
        let _ = fs::remove_file(&unmanaged);
    }

    #[test]
    fn create_from_absent_profile() {
        let profile = tmp_profile("absent");
        assert!(ensure_path_block(&profile, "/p/bin").unwrap());
        let content = fs::read_to_string(&profile).unwrap();
        assert!(content.starts_with(PIGGY_PATH_BEGIN));
        assert!(remove_path_block(&profile).unwrap());
        assert_eq!(fs::read_to_string(&profile).unwrap(), "");
        let _ = fs::remove_file(&profile);
    }
}
