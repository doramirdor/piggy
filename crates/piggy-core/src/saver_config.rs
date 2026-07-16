//! Per-saver configuration: read and apply the catalog's `configOptions`.
//!
//! An option's `apply` object says how a chosen value lands on disk. v1 knows
//! one kind:
//!
//! * `json_field` — set one string field in a JSON file the saver itself
//!   reads (e.g. Caveman's documented user config
//!   `~/.config/caveman/config.json`, field `defaultMode`). The file is
//!   created if missing; every other field in it is preserved; writes are
//!   atomic (temp + rename). Piggy also remembers the choice in its own
//!   state ledger, but the saver's file is the source of truth reported back
//!   to the UI — if the user edits it by hand, Piggy shows the real value.
//!
//! Unknown apply kinds refuse the action ("catalog newer than app"), the same
//! contract as install steps. Values are validated against the option's
//! declared choices before anything is written.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config;
use crate::registry::{Catalog, ConfigOption, Entry};
use crate::state::PiggyState;

/// Apply kinds this build understands.
pub const KNOWN_APPLY_KINDS: &[&str] = &["json_field"];

/// One option resolved to its current effective value, ready for the UI.
#[derive(Debug, Clone)]
pub struct ResolvedOption {
    pub option: ConfigOption,
    /// The value in effect right now: what the saver's own config says, else
    /// what Piggy last set, else the catalog default.
    pub current: String,
}

/// Expand the placeholders an `apply.path` may use. Kept deliberately tiny:
/// `${XDG_CONFIG}` (user config root) and `${CLAUDE_DIR}`.
fn expand_path(template: &str) -> PathBuf {
    let xdg = config::xdg_config_dir();
    let claude = config::claude_dir();
    PathBuf::from(
        template
            .replace("${XDG_CONFIG}", &xdg.to_string_lossy())
            .replace("${CLAUDE_DIR}", &claude.to_string_lossy()),
    )
}

fn apply_kind(opt: &ConfigOption) -> &str {
    opt.apply.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// The `(path, field)` of a `json_field` apply, expanded and validated.
fn json_field_target(opt: &ConfigOption) -> Result<(PathBuf, String)> {
    let path = opt
        .apply
        .get("path")
        .and_then(Value::as_str)
        .context("configOption apply.path is missing")?;
    let field = opt
        .apply
        .get("field")
        .and_then(Value::as_str)
        .context("configOption apply.field is missing")?;
    Ok((expand_path(path), field.to_string()))
}

/// Read the value a `json_field` target currently holds, if the file exists
/// and parses. Never errors — an unreadable saver config just means "no
/// current value on disk".
fn read_json_field(opt: &ConfigOption) -> Option<String> {
    let (path, field) = json_field_target(opt).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    doc.get(&field).and_then(Value::as_str).map(str::to_string)
}

/// Set one string field in a JSON document file, preserving everything else.
/// Creates the file (and parents) if missing; refuses to clobber a file that
/// exists but is not a JSON object (never destroys a config it can't parse).
fn write_json_field(opt: &ConfigOption, value: &str) -> Result<()> {
    let (path, field) = json_field_target(opt)?;
    let mut doc: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} exists but isn't valid JSON — fix or remove it first",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Value::Object(map) = &mut doc else {
        bail!(
            "{} isn't a JSON object — fix or remove it first",
            path.display()
        );
    };
    map.insert(field, Value::String(value.to_string()));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&doc)?;
    let tmp = path.with_extension("json.piggy-tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn entry<'c>(catalog: &'c Catalog, id: &str) -> Result<&'c Entry> {
    catalog
        .get(id)
        .with_context(|| format!("'{id}' is not in the saver catalog"))
}

/// Resolve every option of a saver to its current effective value.
pub fn get_config(catalog: &Catalog, state: &PiggyState, id: &str) -> Result<Vec<ResolvedOption>> {
    let e = entry(catalog, id)?;
    let chosen = state.savers.get(id).map(|s| &s.config);
    Ok(e.config_options
        .iter()
        .map(|opt| {
            let on_disk = match apply_kind(opt) {
                "json_field" => read_json_field(opt),
                _ => None,
            };
            let remembered = chosen.and_then(|c| c.get(&opt.key).cloned());
            ResolvedOption {
                current: on_disk
                    .or(remembered)
                    .unwrap_or_else(|| opt.default.clone()),
                option: opt.clone(),
            }
        })
        .collect())
}

/// Validate and apply one option value, then remember the choice in Piggy's
/// state ledger (when the saver is installed). Returns the resolved options
/// after the write so callers can repaint from truth.
pub fn set_config(
    catalog: &Catalog,
    id: &str,
    key: &str,
    value: &str,
) -> Result<Vec<ResolvedOption>> {
    let e = entry(catalog, id)?;
    let opt = e
        .config_options
        .iter()
        .find(|o| o.key == key)
        .with_context(|| format!("'{id}' has no option '{key}'"))?;
    if !opt.choices.iter().any(|c| c.value == value) {
        bail!("'{value}' isn't one of the allowed values for '{key}'");
    }
    match apply_kind(opt) {
        "json_field" => write_json_field(opt, value)?,
        other => bail!("unknown config apply kind '{other}' - this catalog is newer than Piggy"),
    }

    // Remember the choice (best-effort bookkeeping; the saver's file is truth).
    if let Ok(mut state) = PiggyState::load() {
        if let Some(s) = state.savers.get_mut(id) {
            s.config.insert(key.to_string(), value.to_string());
            let _ = state.save();
        }
    }

    let state = PiggyState::load().unwrap_or_default();
    get_config(catalog, &state, id)
}

// Tests live in `tests/saver_config_tests.rs` — they mutate process env
// (PIGGY_XDG_CONFIG / PIGGY_HOME) and follow the repo's sandbox+lock pattern.
