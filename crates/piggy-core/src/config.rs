//! Filesystem locations used by Piggy.
//!
//! Everything is overridable by environment variable so tests never touch a
//! real `~/.piggy` or `~/.claude`.

use std::path::PathBuf;

/// Directory that holds `piggy.db` and the optional `pricing.json` override.
///
/// Resolution order: `PIGGY_HOME` env var, else `~/.piggy`.
pub fn piggy_home() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_HOME") {
        return PathBuf::from(v);
    }
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".piggy");
    p
}

/// Directory containing Claude Code session logs (`<project>/<session>.jsonl`).
///
/// Resolution order: `PIGGY_CLAUDE_PROJECTS` env var, else `~/.claude/projects`.
/// This directory is only ever read, never written.
pub fn claude_projects_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PIGGY_CLAUDE_PROJECTS") {
        return PathBuf::from(v);
    }
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".claude");
    p.push("projects");
    p
}

/// Path to Claude Code's `settings.json` (read-only; used by `piggy doctor`).
pub fn claude_settings_path() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".claude");
    p.push("settings.json");
    p
}
