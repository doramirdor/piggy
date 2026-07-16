//! Session sources: which tool wrote a session log, and through which surface.
//!
//! Piggy measures two tools — Claude Code and Codex — each usable from a GUI
//! (desktop app / IDE extension) or a TUI (terminal CLI / headless exec). The
//! log format itself carries the discriminator:
//!
//! * Claude Code lines carry an `entrypoint` field. Observed values on real
//!   installs: `claude-desktop`, `claude-vscode`, `cli`, `sdk-cli`.
//! * Codex rollout files carry `session_meta.payload.originator`. The known
//!   value set is enumerated in `openai/codex` (`codex-rs/otel/src/metrics/
//!   tags.rs`): `codex_desktop`, `codex_vscode`, `codex_cli_rs`, `codex-tui`,
//!   `codex-cli`, `codex_exec`, `codex_mcp_server`, …
//!
//! Unknown values classify as [`Interface::Unknown`] — shown honestly as
//! "other", never guessed into a bucket.

/// Which tool produced a session log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    ClaudeCode,
    Codex,
}

impl SourceKind {
    /// Stable string stored in the `sessions.source` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::ClaudeCode => "claude-code",
            SourceKind::Codex => "codex",
        }
    }
}

/// The surface a session ran in: a windowed GUI (desktop app, IDE extension)
/// or a terminal TUI (interactive CLI, headless exec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interface {
    Gui,
    Tui,
    Unknown,
}

impl Interface {
    /// Stable string stored in the `sessions.interface` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Interface::Gui => "gui",
            Interface::Tui => "tui",
            Interface::Unknown => "unknown",
        }
    }
}

/// Classify a Claude Code `entrypoint` value.
pub fn classify_claude_entrypoint(entrypoint: &str) -> Interface {
    match entrypoint {
        "claude-desktop" | "claude-vscode" => Interface::Gui,
        "cli" | "sdk-cli" => Interface::Tui,
        _ => Interface::Unknown,
    }
}

/// Classify a Codex `originator` value (from `session_meta`).
pub fn classify_codex_originator(originator: &str) -> Interface {
    match originator {
        "codex_desktop" | "codex_vscode" | "codex-app-server" | "codex-app-server-sdk" => {
            Interface::Gui
        }
        "codex_cli_rs" | "codex-tui" | "codex-cli" | "codex_exec" => Interface::Tui,
        _ => Interface::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_entrypoints_classify() {
        assert_eq!(classify_claude_entrypoint("claude-desktop"), Interface::Gui);
        assert_eq!(classify_claude_entrypoint("claude-vscode"), Interface::Gui);
        assert_eq!(classify_claude_entrypoint("cli"), Interface::Tui);
        assert_eq!(classify_claude_entrypoint("sdk-cli"), Interface::Tui);
        assert_eq!(classify_claude_entrypoint("mystery"), Interface::Unknown);
        assert_eq!(classify_claude_entrypoint(""), Interface::Unknown);
    }

    #[test]
    fn codex_originators_classify() {
        assert_eq!(classify_codex_originator("codex_desktop"), Interface::Gui);
        assert_eq!(classify_codex_originator("codex_vscode"), Interface::Gui);
        assert_eq!(classify_codex_originator("codex_cli_rs"), Interface::Tui);
        assert_eq!(classify_codex_originator("codex-tui"), Interface::Tui);
        assert_eq!(classify_codex_originator("codex_exec"), Interface::Tui);
        assert_eq!(
            classify_codex_originator("codex_mcp_server"),
            Interface::Unknown
        );
    }

    #[test]
    fn stable_column_strings() {
        assert_eq!(SourceKind::ClaudeCode.as_str(), "claude-code");
        assert_eq!(SourceKind::Codex.as_str(), "codex");
        assert_eq!(Interface::Gui.as_str(), "gui");
        assert_eq!(Interface::Tui.as_str(), "tui");
        assert_eq!(Interface::Unknown.as_str(), "unknown");
    }
}
