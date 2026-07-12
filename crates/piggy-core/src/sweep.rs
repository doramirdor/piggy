//! The built-in **Sweep** saver: find add-ons that cost context tokens on every
//! request but are never actually used, and switch them off reversibly.
//!
//! Data sources (all read-only during a scan):
//! * `~/.claude/settings.json` → `enabledPlugins`,
//! * `~/.claude.json` → `projects.<path>.mcpServers`,
//! * `~/.claude/plugins/installed_plugins.json`,
//! * `~/.claude/skills/`.
//!
//! Usage cross-reference comes from the session DB: MCP tools appear in assistant
//! `tool_use` blocks as `mcp__<server>__<tool>` and skills as the `Skill` tool
//! (see [`crate::parser`]); we count those over the last N sessions. Per-plugin
//! and per-skill usage, which is *not* recoverable from tool names, is read from
//! `~/.claude.json`'s own `pluginUsage` / `skillUsage` counters.
//!
//! Every token cost is an **estimate** (config-size / file-size heuristic) and is
//! always labelled as such — Piggy never presents a guessed number as measured.
//!
//! Disable is reversible: MCP servers are removed but their exact JSON is
//! snapshotted into `state.json`; plugins are set `enabledPlugins=false`; skills
//! are moved to `~/.piggy/disabled/skills/`. Restore reverses each.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use crate::config;
use crate::settings;
use crate::state::{PiggyState, SweepDisabled};
use crate::store::Store;

/// Default look-back window for usage cross-reference.
pub const DEFAULT_N_SESSIONS: usize = 50;

/// One candidate the sweep found.
#[derive(Debug, Clone)]
pub struct SweepItem {
    /// 1-based index, the handle for `piggy sweep --apply <n>`.
    pub idx: usize,
    /// `"mcp"`, `"plugin"`, or `"skill"`.
    pub kind: String,
    /// Server name / `plugin@marketplace` / skill dir name.
    pub id: String,
    /// For MCP: the `~/.claude.json` project path it is configured under.
    pub source: Option<String>,
    /// Usage count for this item. Its meaning depends on [`Self::used_windowed`]:
    /// for MCP servers it is invocations over the last N sessions; for plugins and
    /// skills it is Claude Code's own *lifetime* `usageCount` (Piggy cannot derive
    /// a per-session count for those from tool names).
    pub used: u64,
    /// True when [`Self::used`] is the windowed session count (MCP servers), false
    /// when it is a lifetime counter (plugins, skills) or not measurable (hooks).
    /// Lets callers avoid presenting a lifetime number under a "last N sessions"
    /// window label.
    pub used_windowed: bool,
    /// Estimated per-session context cost, in tokens. **Always an estimate.**
    pub est_tokens: u64,
    /// Whether Piggy recommends turning it off (unused in the window).
    pub recommend_disable: bool,
    /// Plain-language rationale.
    pub reason: String,
}

/// A full scan result.
#[derive(Debug, Clone)]
pub struct SweepReport {
    /// How many sessions the usage cross-reference actually covered.
    pub sessions_considered: u64,
    /// All discovered items (used and unused), stable order.
    pub items: Vec<SweepItem>,
}

impl SweepReport {
    /// Only the items Piggy recommends disabling.
    pub fn recommended(&self) -> impl Iterator<Item = &SweepItem> {
        self.items.iter().filter(|i| i.recommend_disable)
    }
    /// Sum of estimated tokens across recommended-disable items.
    pub fn est_recoverable_tokens(&self) -> u64 {
        self.recommended().map(|i| i.est_tokens).sum()
    }
}

/// Scan all sources and cross-reference usage over the last `n_sessions`.
pub fn scan(store: &Store, n_sessions: usize) -> Result<SweepReport> {
    let usage = store.recent_tool_usage(n_sessions)?;
    let sessions_considered = store.recent_session_count(n_sessions)?;
    let usage_maps = UsageMaps::load();

    let mut items: Vec<SweepItem> = Vec::new();

    scan_mcp_servers(&usage, &mut items)?;
    scan_plugins(&usage_maps, &mut items)?;
    scan_skills(&usage_maps, &mut items)?;
    scan_hooks(&mut items)?;

    // Assign stable 1-based indices (recommended-disable first, then by cost).
    items.sort_by(|a, b| {
        b.recommend_disable
            .cmp(&a.recommend_disable)
            .then_with(|| b.est_tokens.cmp(&a.est_tokens))
            .then_with(|| a.id.cmp(&b.id))
    });
    for (i, item) in items.iter_mut().enumerate() {
        item.idx = i + 1;
    }

    Ok(SweepReport {
        sessions_considered,
        items,
    })
}

// ---------------------------------------------------------------------------
// Scanning each source
// ---------------------------------------------------------------------------

fn scan_mcp_servers(usage: &BTreeMap<String, u64>, out: &mut Vec<SweepItem>) -> Result<()> {
    let path = config::claude_json_path();
    if !path.exists() {
        return Ok(());
    }
    let root: Value = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    let Some(projects) = root.get("projects").and_then(Value::as_object) else {
        return Ok(());
    };

    // Count MCP usage per (normalized) server name across the window.
    let mut server_used: BTreeMap<String, u64> = BTreeMap::new();
    for (name, n) in usage {
        if let Some(server) = mcp_server_of(name) {
            *server_used.entry(normalize(server)).or_insert(0) += n;
        }
    }

    // Dedup servers by name (a server can be configured in multiple projects; we
    // report the first project it appears under).
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (proj_path, proj) in projects {
        let Some(servers) = proj.get("mcpServers").and_then(Value::as_object) else {
            continue;
        };
        for (server, cfg) in servers {
            if !seen.insert(server.clone()) {
                continue;
            }
            let used = server_used.get(&normalize(server)).copied().unwrap_or(0);
            let est = est_mcp_tokens(cfg);
            let recommend = used == 0;
            out.push(SweepItem {
                idx: 0,
                kind: "mcp".into(),
                id: server.clone(),
                source: Some(proj_path.clone()),
                used,
                used_windowed: true,
                est_tokens: est,
                recommend_disable: recommend,
                reason: if recommend {
                    "no tool calls in the look-back window".into()
                } else {
                    format!("{used} tool call(s) in the window")
                },
            });
        }
    }
    Ok(())
}

fn scan_plugins(usage: &UsageMaps, out: &mut Vec<SweepItem>) -> Result<()> {
    let settings_path = config::claude_settings_path();
    let loaded = match settings::load(&settings_path) {
        Ok(l) => l,
        Err(_) => return Ok(()), // unreadable settings — nothing to scan
    };
    let Some(enabled) = loaded
        .value
        .get("enabledPlugins")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    for (plugin, on) in enabled {
        if !on.as_bool().unwrap_or(false) {
            continue; // already off
        }
        let used = usage.plugin_usage.get(plugin).copied().unwrap_or(0);
        let recommend = used == 0;
        out.push(SweepItem {
            idx: 0,
            kind: "plugin".into(),
            id: plugin.clone(),
            source: None,
            used,
            // pluginUsage is a lifetime counter in ~/.claude.json, not windowed.
            used_windowed: false,
            est_tokens: 800, // estimate: a plugin's skills/commands manifest
            recommend_disable: recommend,
            reason: if recommend {
                "enabled but never used (lifetime)".into()
            } else {
                format!("used {used} time(s) (lifetime)")
            },
        });
    }
    Ok(())
}

fn scan_skills(usage: &UsageMaps, out: &mut Vec<SweepItem>) -> Result<()> {
    let dir = config::claude_skills_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let skill_md = entry.path().join("SKILL.md");
        let est = std::fs::metadata(&skill_md)
            .map(|m| (m.len() / 4).max(50))
            .unwrap_or(200);
        let used = usage.skill_usage.get(&name).copied().unwrap_or(0);
        let recommend = used == 0;
        out.push(SweepItem {
            idx: 0,
            kind: "skill".into(),
            id: name,
            source: Some(entry.path().to_string_lossy().into_owned()),
            used,
            // skillUsage is a lifetime counter in ~/.claude.json, not windowed.
            used_windowed: false,
            est_tokens: est,
            recommend_disable: recommend,
            reason: if recommend {
                "installed but never invoked (lifetime)".into()
            } else {
                format!("invoked {used} time(s) (lifetime)")
            },
        });
    }
    Ok(())
}

/// Surface the user's own hooks from `settings.json` (a spec'd data source).
///
/// Hooks are the one source Piggy cannot usage-measure — they fire on events
/// rather than appearing as tool calls, and unlike MCP servers / plugins / skills
/// they cost **no** per-request context tokens. So they are listed as
/// informational only and never auto-recommended for removal. Piggy-owned hooks
/// (recorded in `state.json`) are excluded, so this shows only the user's.
fn scan_hooks(out: &mut Vec<SweepItem>) -> Result<()> {
    let settings_path = config::claude_settings_path();
    let loaded = match settings::load(&settings_path) {
        Ok(l) => l,
        Err(_) => return Ok(()), // unreadable settings — nothing to scan
    };
    let Some(hooks) = loaded.value.get("hooks").and_then(Value::as_object) else {
        return Ok(());
    };
    // The exact hook-group objects Piggy injected, so we never list our own.
    let piggy_owned: Vec<Value> = PiggyState::load()
        .map(|s| {
            s.savers
                .values()
                .flat_map(|sv| sv.injected_hooks.values())
                .flatten()
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    for (event, groups) in hooks {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for (i, group) in groups.iter().enumerate() {
            if piggy_owned.contains(group) {
                continue;
            }
            out.push(SweepItem {
                idx: 0,
                kind: "hook".into(),
                id: format!("{event}#{}", i + 1),
                source: Some(event.clone()),
                used: 0,
                used_windowed: false,
                est_tokens: 0, // hooks fire on events; they cost no context tokens
                recommend_disable: false,
                reason: "hook — fires on events, not usage-measurable and costs no context tokens (informational)".into(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Apply / restore
// ---------------------------------------------------------------------------

/// Disable the item at 1-based `idx` from a fresh scan, recording a restore
/// snapshot in `state`. Returns the disabled item's human id.
pub fn apply(
    store: &Store,
    state: &mut PiggyState,
    idx: usize,
    n_sessions: usize,
) -> Result<String> {
    let report = scan(store, n_sessions)?;
    let item = report
        .items
        .iter()
        .find(|i| i.idx == idx)
        .ok_or_else(|| anyhow!("no sweep item #{idx} (run `piggy sweep` to see the list)"))?
        .clone();

    match item.kind.as_str() {
        "mcp" => disable_mcp(state, &item)?,
        "plugin" => disable_plugin(state, &item)?,
        "skill" => disable_skill(state, &item)?,
        "hook" => bail!(
            "hooks are listed for information only and are not removable via sweep — edit them in Claude's settings yourself"
        ),
        other => bail!("cannot disable unknown sweep kind '{other}'"),
    }
    state.save()?;
    Ok(item.id)
}

fn disable_mcp(state: &mut PiggyState, item: &SweepItem) -> Result<()> {
    let source = item
        .source
        .clone()
        .ok_or_else(|| anyhow!("mcp item has no source project"))?;
    let path = config::claude_json_path();
    let mut snapshot = Value::Null;
    edit_json_atomic(&path, |root| {
        if let Some(servers) = root
            .get_mut("projects")
            .and_then(|p| p.get_mut(&source))
            .and_then(|proj| proj.get_mut("mcpServers"))
            .and_then(Value::as_object_mut)
        {
            if let Some(removed) = servers.remove(&item.id) {
                snapshot = removed;
            }
        }
    })?;
    if snapshot.is_null() {
        bail!("MCP server '{}' not found under {}", item.id, source);
    }
    state.sweep_disabled.push(SweepDisabled {
        kind: "mcp".into(),
        id: item.id.clone(),
        source: Some(source),
        snapshot,
        restore_path: None,
        disabled_at: chrono::Utc::now().to_rfc3339(),
    });
    Ok(())
}

fn disable_plugin(state: &mut PiggyState, item: &SweepItem) -> Result<()> {
    let settings_path = config::claude_settings_path();
    // Snapshot the prior value (true) and set enabledPlugins[id] = false.
    let outcome = settings::commit(
        &settings_path,
        &format!("sweep-disable-plugin:{}", item.id),
        state,
        None,
        |val| {
            if let Some(obj) = val.as_object_mut() {
                let ep = obj
                    .entry("enabledPlugins")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(m) = ep.as_object_mut() {
                    m.insert(item.id.clone(), Value::Bool(false));
                }
            }
        },
    )?;
    let _ = outcome;
    state.sweep_disabled.push(SweepDisabled {
        kind: "plugin".into(),
        id: item.id.clone(),
        source: None,
        snapshot: Value::Bool(true),
        restore_path: None,
        disabled_at: chrono::Utc::now().to_rfc3339(),
    });
    Ok(())
}

fn disable_skill(state: &mut PiggyState, item: &SweepItem) -> Result<()> {
    let src = config::claude_skills_dir().join(&item.id);
    let dest_dir = config::disabled_dir().join("skills");
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(&item.id);
    std::fs::rename(&src, &dest)
        .with_context(|| format!("moving {} to {}", src.display(), dest.display()))?;
    state.sweep_disabled.push(SweepDisabled {
        kind: "skill".into(),
        id: item.id.clone(),
        source: Some(src.to_string_lossy().into_owned()),
        snapshot: Value::Null,
        restore_path: Some(dest.to_string_lossy().into_owned()),
        disabled_at: chrono::Utc::now().to_rfc3339(),
    });
    Ok(())
}

/// Restore every Sweep-disabled item and clear the list. Returns the count
/// restored. Used by the Sweep saver's uninstall (`builtin_disable`) and by
/// `piggy restore-defaults`.
pub fn restore_all(state: &mut PiggyState) -> Result<usize> {
    let items = std::mem::take(&mut state.sweep_disabled);
    let mut restored = 0;
    let mut failed: Vec<SweepDisabled> = Vec::new();
    for item in items {
        match restore_one(state, &item) {
            Ok(()) => restored += 1,
            Err(_) => failed.push(item),
        }
    }
    // Keep any that could not be restored so the record is not lost.
    state.sweep_disabled = failed;
    Ok(restored)
}

fn restore_one(state: &mut PiggyState, item: &SweepDisabled) -> Result<()> {
    match item.kind.as_str() {
        "mcp" => {
            let source = item
                .source
                .clone()
                .ok_or_else(|| anyhow!("mcp restore missing source"))?;
            let path = config::claude_json_path();
            let id = item.id.clone();
            let snap = item.snapshot.clone();
            edit_json_atomic(&path, |root| {
                let servers = root
                    .as_object_mut()
                    .and_then(|o| o.get_mut("projects"))
                    .and_then(|p| p.get_mut(&source))
                    .and_then(|proj| proj.as_object_mut())
                    .map(|proj| {
                        proj.entry("mcpServers")
                            .or_insert_with(|| Value::Object(Map::new()))
                    });
                if let Some(Value::Object(m)) = servers {
                    m.insert(id.clone(), snap.clone());
                }
            })?;
            Ok(())
        }
        "plugin" => {
            let settings_path = config::claude_settings_path();
            let id = item.id.clone();
            settings::commit(
                &settings_path,
                &format!("sweep-restore-plugin:{id}"),
                state,
                None,
                |val| {
                    if let Some(m) = val
                        .as_object_mut()
                        .and_then(|o| o.get_mut("enabledPlugins"))
                        .and_then(Value::as_object_mut)
                    {
                        m.insert(id.clone(), Value::Bool(true));
                    }
                },
            )?;
            Ok(())
        }
        "skill" => {
            let from = item
                .restore_path
                .clone()
                .ok_or_else(|| anyhow!("skill restore missing path"))?;
            let to = item
                .source
                .clone()
                .ok_or_else(|| anyhow!("skill restore missing original path"))?;
            std::fs::rename(&from, &to).with_context(|| format!("moving {from} back to {to}"))?;
            Ok(())
        }
        other => bail!("unknown sweep kind '{other}'"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `pluginUsage` / `skillUsage` counters read from `~/.claude.json`.
struct UsageMaps {
    plugin_usage: BTreeMap<String, u64>,
    skill_usage: BTreeMap<String, u64>,
}

impl UsageMaps {
    fn load() -> Self {
        let mut plugin_usage = BTreeMap::new();
        let mut skill_usage = BTreeMap::new();
        if let Ok(bytes) = std::fs::read(config::claude_json_path()) {
            if let Ok(root) = serde_json::from_slice::<Value>(&bytes) {
                read_usage(root.get("pluginUsage"), &mut plugin_usage);
                read_usage(root.get("skillUsage"), &mut skill_usage);
            }
        }
        UsageMaps {
            plugin_usage,
            skill_usage,
        }
    }
}

fn read_usage(v: Option<&Value>, out: &mut BTreeMap<String, u64>) {
    if let Some(Value::Object(m)) = v {
        for (k, val) in m {
            let n = val.get("usageCount").and_then(Value::as_u64).unwrap_or(0);
            out.insert(k.clone(), n);
        }
    }
}

/// The server segment of an `mcp__<server>__<tool>` name (`None` otherwise).
fn mcp_server_of(name: &str) -> Option<&str> {
    name.strip_prefix("mcp__")?.split("__").next()
}

/// Normalize a server name for matching config keys against tool-name segments
/// (lowercase; non-alphanumerics folded to `_`).
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Estimate an MCP server's per-session context cost from its config size. This
/// is a deliberately rough, clearly-labelled heuristic — the true cost is the
/// server's tool-schema manifest, which Piggy cannot see without connecting.
fn est_mcp_tokens(cfg: &Value) -> u64 {
    let len = cfg.to_string().len() as u64;
    (300 + len / 3).min(4000)
}

/// A `<stem>-<ts>.bak` path under `dir` that does not yet exist (nanosecond
/// timestamp with a numeric suffix as a same-instant tiebreaker).
fn unique_bak_path(dir: &Path, stem: &str) -> std::path::PathBuf {
    let ts = chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        .replace(':', "-");
    let base = dir.join(format!("{stem}-{ts}.bak"));
    if !base.exists() {
        return base;
    }
    for i in 1.. {
        let p = dir.join(format!("{stem}-{ts}-{i}.bak"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// Read a JSON file, back it up to Piggy's backups dir, apply `mutate`, and
/// atomically write it back preserving a trailing newline. Used for
/// `~/.claude.json`.
///
/// The re-serialization touches the whole document, but `preserve_order` keeps
/// every key in place and `arbitrary_precision` keeps every number's exact source
/// text (so Claude Code's telemetry floats no longer shift by a ULP). The net
/// diff is therefore just the one entry `mutate` changed.
fn edit_json_atomic<F>(path: &Path, mutate: F) -> Result<()>
where
    F: FnOnce(&mut Value),
{
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let trailing_newline = bytes.last() == Some(&b'\n');

    // Back up before touching it, to a collision-free path (a nanosecond stamp
    // plus an existence-checked suffix, so two edits in the same instant never
    // overwrite each other's backup).
    let backups = config::backups_dir();
    std::fs::create_dir_all(&backups)?;
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("claude.json");
    std::fs::write(unique_bak_path(&backups, stem), &bytes)?;

    let mut root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    mutate(&mut root);

    let mut text = serde_json::to_string_pretty(&root)?;
    if trailing_newline {
        text.push('\n');
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| anyhow!("persisting {}: {e}", path.display()))?;
    Ok(())
}
