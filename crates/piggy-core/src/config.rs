//! Filesystem locations used by Piggy.
//!
//! Everything is overridable by environment variable so tests never touch a
//! real `~/.piggy` or `~/.claude`. The M2 install engine mutates real files, so
//! the override discipline here is a hard safety boundary — see the crate tests,
//! which point every one of these at a `tempfile::tempdir()`.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Directory that holds `piggy.db`, `state.json`, `backups/`, `bin/`,
/// `disabled/`, and the optional `pricing.json` override.
///
/// Resolution order: `PIGGY_HOME` env var, else `~/.piggy`.
pub fn piggy_home() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_HOME") {
        return PathBuf::from(v);
    }
    let mut p = home();
    p.push(".piggy");
    p
}

/// Directory where Piggy extracts downloaded binaries (e.g. `rtk`).
///
/// This is the expansion target for the `${PIGGY_BIN}` placeholder used in
/// catalog hook commands. `<piggy_home>/bin`.
pub fn piggy_bin_dir() -> PathBuf {
    piggy_home().join("bin")
}

/// The shell profile Piggy appends a `PATH` line to so binaries in
/// [`piggy_bin_dir`] resolve by *bare name*.
///
/// This is required by savers whose runtime self-invokes by bare name rather
/// than an absolute path — notably `rtk`, whose `PreToolUse` hook rewrites a
/// matched command to `rtk <cmd>` (no path). The hook itself is injected with an
/// absolute path, but that rewrite is not, so `<piggy_bin_dir>` must be on
/// `PATH` for the rewritten command to run.
///
/// Resolution order: `PIGGY_SHELL_PROFILE` env var (tests point this at a temp
/// file), else `~/.zshrc`. The app targets macOS/zsh, and `~/.zshrc` is where
/// this user's interactive `PATH` is established and what Claude Code's shell
/// snapshot sources.
pub fn shell_profile_path() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_SHELL_PROFILE") {
        return PathBuf::from(v);
    }
    home().join(".zshrc")
}

/// Directory holding timestamped `settings.json` backups plus the one-time
/// `pre-piggy.json` (the Restore Defaults target). `<piggy_home>/backups`.
pub fn backups_dir() -> PathBuf {
    piggy_home().join("backups")
}

/// Path to Piggy's own state file (`state.json`). `<piggy_home>/state.json`.
pub fn state_path() -> PathBuf {
    piggy_home().join("state.json")
}

/// Path to the cached saver-discovery results (`discovered.json`), refreshed at
/// most once a day from GitHub search. `<piggy_home>/discovered.json`.
pub fn discovered_path() -> PathBuf {
    piggy_home().join("discovered.json")
}

/// Directory where Sweep parks disabled skills for reversible restore.
/// `<piggy_home>/disabled`.
pub fn disabled_dir() -> PathBuf {
    piggy_home().join("disabled")
}

/// Claude Code's configuration directory (default `~/.claude`).
///
/// Resolution order: `PIGGY_CLAUDE_DIR` env var, else `~/.claude`. **Every**
/// path the install engine writes under Claude's config resolves from here, so
/// setting `PIGGY_CLAUDE_DIR` to a temp dir fully sandboxes a mutation test.
pub fn claude_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_CLAUDE_DIR") {
        return PathBuf::from(v);
    }
    home().join(".claude")
}

/// The `claude` CLI executable used for plugin/marketplace steps.
///
/// Resolution order (per docs/m2-spec.md — "locate binary: `which claude`,
/// fallback known paths"):
/// 1. `PIGGY_CLAUDE_BIN` env var (an absolute path to a real or fake shim — tests
///    point this at a recording shim so no real `claude` is ever invoked);
/// 2. the first `claude` found on `$PATH` (a `which claude` equivalent);
/// 3. a set of well-known install locations, for a GUI-launched process whose
///    inherited `$PATH` is minimal (macOS LaunchServices does not source a login
///    shell, so a Homebrew/npm-global `claude` would otherwise be invisible);
/// 4. the bare name `claude` as a last resort.
pub fn claude_bin() -> String {
    if let Ok(v) = std::env::var("PIGGY_CLAUDE_BIN") {
        return v;
    }
    if let Some(p) = which_on_path("claude") {
        return p;
    }
    for cand in known_claude_paths() {
        if is_executable_file(&cand) {
            return cand.to_string_lossy().into_owned();
        }
    }
    "claude".to_string()
}

/// Search `$PATH` for an executable named `bin`, returning its full path (the
/// `which <bin>` behaviour the spec calls for).
fn which_on_path(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if is_executable_file(&cand) {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// Well-known locations Claude Code's CLI installs to, checked when it is not on
/// `$PATH` (e.g. a menu-bar app launched with a minimal environment).
fn known_claude_paths() -> Vec<PathBuf> {
    let h = home();
    vec![
        h.join(".claude/local/claude"),
        h.join(".local/bin/claude"),
        h.join(".npm-global/bin/claude"),
        h.join("bin/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
        PathBuf::from("/usr/bin/claude"),
    ]
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// The `python3` interpreter used to build a saver's isolated venv (e.g.
/// Headroom, a Python package installed via `pip` into `<piggy_home>/venvs/…`).
///
/// Resolution order: `PIGGY_PYTHON_BIN` env var (tests point this at a shim that
/// fakes `-m venv` + `pip` with no network), else the bare name `python3`.
pub fn python_bin() -> String {
    std::env::var("PIGGY_PYTHON_BIN").unwrap_or_else(|_| "python3".to_string())
}

/// Directory holding per-saver isolated Python virtualenvs.
/// `<piggy_home>/venvs`.
pub fn piggy_venvs_dir() -> PathBuf {
    piggy_home().join("venvs")
}

/// Directory containing Claude Code session logs (`<project>/<session>.jsonl`).
///
/// Resolution order: `PIGGY_CLAUDE_PROJECTS`, else `<claude_dir>/projects`.
/// This directory is only ever read, never written.
pub fn claude_projects_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_CLAUDE_PROJECTS") {
        return PathBuf::from(v);
    }
    claude_dir().join("projects")
}

/// Codex's home directory (default `~/.codex`).
///
/// Resolution order: `PIGGY_CODEX_DIR` env var, else `~/.codex`. Only ever
/// read — Piggy never writes under Codex's home.
pub fn codex_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_CODEX_DIR") {
        return PathBuf::from(v);
    }
    home().join(".codex")
}

/// Directories containing Codex rollout session logs
/// (`sessions/YYYY/MM/DD/rollout-*.jsonl`, plus `archived_sessions`). Only the
/// directories that exist are returned; both are read-only for Piggy.
pub fn codex_sessions_dirs() -> Vec<PathBuf> {
    let root = codex_dir();
    ["sessions", "archived_sessions"]
        .iter()
        .map(|d| root.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// The XDG-style user config root (default `~/.config`) — where some savers
/// keep their own user configuration (e.g. Caveman's
/// `~/.config/caveman/config.json`).
///
/// Resolution order: `PIGGY_XDG_CONFIG` env var (tests point this at a temp
/// dir), else `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn xdg_config_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_XDG_CONFIG") {
        return PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".config")
}

/// Path to Claude Code's `settings.json` (`<claude_dir>/settings.json`).
///
/// The install/merge engine's single write target under Claude's config.
pub fn claude_settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

/// Path to Claude Code's top-level `~/.claude.json` (holds `projects.*` MCP
/// server configs — a Sweep data source).
///
/// Resolution order: `PIGGY_CLAUDE_JSON` env var, else `~/.claude.json`. Note
/// this file is a *sibling* of `claude_dir`, not inside it, so it has its own
/// override.
pub fn claude_json_path() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_CLAUDE_JSON") {
        return PathBuf::from(v);
    }
    home().join(".claude.json")
}

/// Path to the installed-plugins ledger (`<claude_dir>/plugins/installed_plugins.json`).
pub fn installed_plugins_path() -> PathBuf {
    claude_dir().join("plugins").join("installed_plugins.json")
}

/// Directory of standalone user skills (`<claude_dir>/skills`) — a Sweep source.
pub fn claude_skills_dir() -> PathBuf {
    claude_dir().join("skills")
}
