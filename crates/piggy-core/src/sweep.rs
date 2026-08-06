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
//! Every token cost is an **estimate** (config-size / file-size heuristic)
//! except an MCP server the manifest probe has measured, and every number
//! carries the label that says which it is ([`SweepItem::cost_basis`]) - Piggy
//! never presents a guessed number as measured, or the reverse.
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
use crate::probe;
use crate::settings;
use crate::state::{PiggyState, SweepDisabled};
use crate::store::{McpManifest, Store};

/// Default look-back window for usage cross-reference.
pub const DEFAULT_N_SESSIONS: usize = 50;

/// [`SweepItem::cost_basis`] when `est_tokens` is the config-size heuristic:
/// a guess, and labelled as one everywhere it is shown.
pub const COST_BASIS_ESTIMATE: &str = "rough estimate";

/// [`SweepItem::cost_basis`] when `est_tokens` came from [`crate::probe`]
/// measuring this server's *current* config: a real byte count of the tool
/// schemas, not a guess.
pub const COST_BASIS_MEASURED: &str = "measured manifest";

/// Tool-call counts over the window, keyed by tool name then by the project the
/// calls came from (see [`Store::recent_tool_usage`]).
type ProjectUsage = BTreeMap<String, BTreeMap<String, u64>>;

/// One candidate the sweep found.
#[derive(Debug, Clone)]
pub struct SweepItem {
    /// 1-based index, the handle for `piggy sweep --apply <n>`.
    pub idx: usize,
    /// `"mcp"`, `"plugin"`, or `"skill"`.
    pub kind: String,
    /// Server name / `plugin@marketplace` / skill dir name.
    pub id: String,
    /// For MCP: the `~/.claude.json` project path it is configured under, or
    /// `None` when it sits at the top level (user scope, loaded in every
    /// session).
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
    /// Per-session context cost, in tokens. An estimate unless
    /// [`Self::cost_basis`] says otherwise.
    pub est_tokens: u64,
    /// Where [`Self::est_tokens`] came from: [`COST_BASIS_ESTIMATE`] (the
    /// config-size heuristic) or [`COST_BASIS_MEASURED`] (a probe measurement of
    /// this server's current config). The label travels with the number so no
    /// surface can show a measured figure as a guess, or the reverse.
    pub cost_basis: String,
    /// Whether Piggy recommends turning it off (unused in the window).
    pub recommend_disable: bool,
    /// For a user-scope MCP server whose calls all come from one project: that
    /// project. Keeping it at user scope makes every *other* session load its
    /// schemas for nothing, so the fix is to move it, not to remove it.
    ///
    /// `None` whenever the item is not an MCP server, is already project-scoped,
    /// is unused (that is [`Self::recommend_disable`]'s job), or is genuinely
    /// spread across projects.
    pub scope_to: Option<String>,
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
    /// Items that are used, but only from one project while configured to load
    /// everywhere. Sweep recommends re-scoping these rather than removing them.
    pub fn rescope(&self) -> impl Iterator<Item = &SweepItem> {
        self.items.iter().filter(|i| i.scope_to.is_some())
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
    // Anything the probe has measured, so a real schema cost can replace the
    // heuristic for the servers that have one.
    let manifests = store.mcp_manifests()?;

    let mut items: Vec<SweepItem> = Vec::new();

    scan_mcp_servers(&usage, &manifests, &mut items)?;
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

fn scan_mcp_servers(
    usage: &ProjectUsage,
    manifests: &[McpManifest],
    out: &mut Vec<SweepItem>,
) -> Result<()> {
    let path = config::claude_json_path();
    if !path.exists() {
        return Ok(());
    }
    let root: Value = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;

    // Count MCP usage per (normalized) server name per project across the window.
    let mut server_used: ProjectUsage = BTreeMap::new();
    for (name, by_project) in usage {
        let Some(server) = mcp_server_of(name) else {
            continue;
        };
        let entry = server_used.entry(normalize(server)).or_default();
        for (project, n) in by_project {
            *entry.entry(project.clone()).or_insert(0) += n;
        }
    }

    // Dedup servers by name (a server can be configured in several places; we
    // report the first). The enumeration itself lives in `probe`, which lists
    // user scope first - the copy every session pays for, so the one whose
    // placement is worth judging - and then each project's.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for server in probe::servers_from_root(&root) {
        if seen.insert(server.key.clone()) {
            out.push(mcp_item(&server, &server_used, manifests));
        }
    }
    Ok(())
}

/// Share of a user-scope server's calls that must come from a single project
/// before Sweep calls it that project's tool rather than a global one. Not 1.0:
/// one stray call from another cwd should not keep a server loaded everywhere.
const SCOPE_CONCENTRATION: f64 = 0.9;

/// One configured MCP server as a sweep item.
///
/// The scope call only applies to user scope: a project-scoped server already
/// costs nothing outside its own project, so telling its owner where it is used
/// would be noise. A user-scope server is loaded by every session, which is only
/// worth it if more than one project actually calls it.
fn mcp_item(
    server: &probe::ConfiguredServer,
    server_used: &ProjectUsage,
    manifests: &[McpManifest],
) -> SweepItem {
    let source = server.project.clone();
    let by_project = server_used.get(&normalize(&server.key)).map(fold_subpaths);
    let used: u64 = by_project.as_ref().map(|m| m.values().sum()).unwrap_or(0);
    // Sessions with no recorded project cannot vote on where a server belongs,
    // so they count toward `used` but never toward the concentration.
    let scope_to = match (&source, &by_project) {
        (None, Some(by_project)) if used > 0 => by_project
            .iter()
            .filter(|(project, _)| !project.is_empty())
            .max_by_key(|(_, n)| **n)
            .filter(|(_, n)| **n as f64 >= used as f64 * SCOPE_CONCENTRATION)
            .map(|(project, _)| project.clone()),
        _ => None,
    };
    let n_projects = by_project
        .as_ref()
        .map(|m| m.keys().filter(|p| !p.is_empty()).count())
        .unwrap_or(0);

    let reason = if used == 0 {
        "no tool calls in the look-back window".to_string()
    } else if let Some(project) = &scope_to {
        format!(
            "{used} tool call(s) in the window, effectively all from {project}. \
             It loads at user scope, so every other session pays for it too: \
             re-add it in that project instead."
        )
    } else if source.is_none() {
        format!("{used} tool call(s) across {n_projects} project(s) - global, keep at user scope")
    } else {
        format!("{used} tool call(s) in the window")
    };

    // A probe measurement of *this* config beats the heuristic. Anything else -
    // never probed, config changed since, or a failed probe - keeps the estimate
    // and its label.
    let (est_tokens, cost_basis) = match probe::measured_tokens(manifests, server) {
        Some(tokens) => (tokens.max(0) as u64, COST_BASIS_MEASURED),
        None => (est_mcp_tokens(&server.config), COST_BASIS_ESTIMATE),
    };

    SweepItem {
        idx: 0,
        kind: "mcp".into(),
        id: server.key.clone(),
        source,
        used,
        used_windowed: true,
        est_tokens,
        cost_basis: cost_basis.into(),
        recommend_disable: used == 0,
        scope_to,
        reason,
    }
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
            cost_basis: COST_BASIS_ESTIMATE.into(),
            recommend_disable: recommend,
            scope_to: None, // plugins have no per-project scope to move to
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
    let piggy_owned = piggy_owned_skill_dirs();
    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // A skill Piggy installed as a saver is managed by the saver's own
        // on/off (and its rotation), so sweeping it would park a file the
        // engine still believes it owns and leave the two disagreeing. Skipped
        // entirely, like Piggy-owned hooks.
        if piggy_owned.contains(&name) {
            continue;
        }
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
            cost_basis: COST_BASIS_ESTIMATE.into(),
            recommend_disable: recommend,
            scope_to: None, // skills are user-wide; Claude Code has no project scope for them
            reason: if recommend {
                "installed but never invoked (lifetime)".into()
            } else {
                format!("invoked {used} time(s) (lifetime)")
            },
        });
    }
    Ok(())
}

/// Names of skill directories under `~/.claude/skills` that a Piggy saver
/// installed, taken from the files each installed saver recorded in
/// `state.json`. Empty when state is unreadable — a scan never fails on it.
fn piggy_owned_skill_dirs() -> std::collections::BTreeSet<String> {
    let skills_dir = config::claude_skills_dir();
    let Ok(state) = PiggyState::load() else {
        return Default::default();
    };
    state
        .savers
        .values()
        .flat_map(|s| s.installed_files.iter())
        .filter_map(|f| {
            Path::new(f)
                .strip_prefix(&skills_dir)
                .ok()?
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
        })
        .collect()
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
                cost_basis: COST_BASIS_ESTIMATE.into(),
                recommend_disable: false,
                scope_to: None,
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
///
/// Items carrying a [`SweepItem::scope_to`] are refused: that row's advice is
/// about *where* an in-use server is configured, not about removing it.
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

    // A "scope to <project>" row belongs to a server the user actually calls, so
    // disabling it would take away the tool its own suggestion told them to keep,
    // and re-scoping it here would be Piggy moving config it did not write. Refuse
    // and point at the manual re-add, the way the hook arm below bails.
    if let Some(project) = item.scope_to.as_deref().filter(|_| !item.recommend_disable) {
        bail!(
            "'{}' is in use ({} tool call(s) in the window) and only needs re-scoping, not removing. \
             Re-add it in {project} yourself and then drop the user-scope copy: Piggy will not move config it did not write.",
            item.id,
            item.used
        );
    }

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
    let source = item.source.clone();
    let path = config::claude_json_path();
    let mut snapshot = Value::Null;
    edit_json_atomic(&path, |root| {
        if let Some(servers) = mcp_servers_mut(root, source.as_deref(), false) {
            if let Some(removed) = servers.remove(&item.id) {
                snapshot = removed;
            }
        }
        Ok(())
    })?;
    if snapshot.is_null() {
        bail!(
            "MCP server '{}' not found under {}",
            item.id,
            source.as_deref().unwrap_or("user scope")
        );
    }
    state.sweep_disabled.push(SweepDisabled {
        kind: "mcp".into(),
        id: item.id.clone(),
        source,
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

/// An item `restore_all` could not put back, and why. The reason names the
/// file and missing key, which is what the user has to act on.
pub struct RestoreFailure {
    pub id: String,
    pub reason: String,
}

pub struct RestoreOutcome {
    pub restored: usize,
    pub failures: Vec<RestoreFailure>,
}

/// Restore every Sweep-disabled item and clear the list. Items that fail stay
/// in `state.sweep_disabled` (the snapshot is their only copy) and are reported
/// in `failures`. Used by the Sweep saver's uninstall (`builtin_disable`), the
/// app's Undo all, and `piggy restore-defaults`.
pub fn restore_all(state: &mut PiggyState) -> Result<RestoreOutcome> {
    let items = std::mem::take(&mut state.sweep_disabled);
    let mut outcome = RestoreOutcome {
        restored: 0,
        failures: Vec::new(),
    };
    let mut failed: Vec<SweepDisabled> = Vec::new();
    for item in items {
        match restore_one(state, &item) {
            Ok(()) => outcome.restored += 1,
            Err(e) => {
                eprintln!("warning: {e:#}");
                outcome.failures.push(RestoreFailure {
                    id: item.id.clone(),
                    reason: format!("{e:#}"),
                });
                failed.push(item);
            }
        }
    }
    // Keep any that could not be restored so the record is not lost.
    state.sweep_disabled = failed;
    Ok(outcome)
}

fn restore_one(state: &mut PiggyState, item: &SweepDisabled) -> Result<()> {
    match item.kind.as_str() {
        "mcp" => {
            // `None` is user scope, the top level of `~/.claude.json`. Entries
            // written before Sweep looked there always carry a project path.
            let source = item.source.clone();
            let path = config::claude_json_path();
            let id = item.id.clone();
            let snap = item.snapshot.clone();
            let key = match &source {
                None => "mcpServers".to_string(),
                Some(project) => format!("projects.{project}.mcpServers"),
            };
            edit_json_atomic(&path, |root| {
                // No map to write into means the config was replaced under us. A
                // silent success here would drop the snapshot with it, so fail and
                // let the caller keep the record.
                let m = mcp_servers_mut(root, source.as_deref(), true).ok_or_else(|| {
                    anyhow!(
                        "MCP server '{id}' cannot be restored: {} has no '{key}' map to put it back in",
                        path.display()
                    )
                })?;
                m.insert(id.clone(), snap.clone());
                Ok(())
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

/// Fold each project path into the shortest ancestor path also present.
///
/// A project is a session's cwd, so one checkout arrives under several keys the
/// moment a session starts in a subdirectory (`…/Stacked` and
/// `…/Stacked/app/src`). Left split, a server used in exactly one repo counts as
/// two projects and passes as global, which is the one wrong answer this whole
/// scope call exists to avoid.
fn fold_subpaths(by_project: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for (project, n) in by_project {
        let root = by_project
            .keys()
            .filter(|p| {
                !p.is_empty()
                    && project
                        .strip_prefix(p.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .min_by_key(|p| p.len())
            .unwrap_or(project);
        *out.entry(root.clone()).or_insert(0) += n;
    }
    out
}

/// The `mcpServers` map a server lives in inside `~/.claude.json`: the top level
/// when `source` is `None` (user scope), otherwise that project's. `create`
/// inserts the map when it is missing, which restore needs and disable does not.
fn mcp_servers_mut<'a>(
    root: &'a mut Value,
    source: Option<&str>,
    create: bool,
) -> Option<&'a mut Map<String, Value>> {
    let parent = match source {
        None => root.as_object_mut()?,
        Some(project) => root
            .get_mut("projects")?
            .get_mut(project)?
            .as_object_mut()?,
    };
    if create {
        parent
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    parent.get_mut("mcpServers")?.as_object_mut()
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
/// is a deliberately rough, clearly-labelled heuristic - the true cost is the
/// server's tool-schema manifest, which [`crate::probe`] can only see by
/// connecting, and which the user has to ask for one server at a time.
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

/// Read a JSON file, apply `mutate`, back it up to Piggy's backups dir, and
/// atomically write it back preserving a trailing newline. Used for
/// `~/.claude.json`. A `mutate` that returns an error aborts before the file or
/// the backups dir is touched.
///
/// The re-serialization touches the whole document, but `preserve_order` keeps
/// every key in place and `arbitrary_precision` keeps every number's exact source
/// text (so Claude Code's telemetry floats no longer shift by a ULP). The net
/// diff is therefore just the one entry `mutate` changed.
fn edit_json_atomic<F>(path: &Path, mutate: F) -> Result<()>
where
    F: FnOnce(&mut Value) -> Result<()>,
{
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let trailing_newline = bytes.last() == Some(&b'\n');

    let mut root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    mutate(&mut root)?;

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
