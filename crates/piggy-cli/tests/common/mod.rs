//! A fully sandboxed Piggy, for tests that run the real `piggy` binary.
//!
//! # Why this exists
//!
//! Piggy reads and **writes** files that belong to the person running it: the
//! Claude Code settings, the MCP server configuration, the shell profile, the
//! session logs. Every one of those locations is overridable by an environment
//! variable (see `crates/piggy-core/src/config.rs`), and a test that sets some
//! of them is not sandboxed, it is *partly* sandboxed, which looks identical
//! until the day it does not.
//!
//! That is not hypothetical. An earlier run pointed `PIGGY_CLAUDE_PROJECTS` at a
//! temp dir and still indexed the developer's real Codex history, because Codex
//! discovery has its own override (`PIGGY_CODEX_DIR`) that the run never set.
//! Reading was the harmless version. `PIGGY_CLAUDE_JSON` is the dangerous one:
//! the advice engine's `ServerScope` apply **rewrites that file**, so a test
//! that forgets it edits the developer's own MCP configuration.
//!
//! So there is one list, in one place, and [`Sandbox::assert_sandboxed`] fails
//! loudly if any path in it escapes the sandbox. Add an override to
//! `config.rs`, add it here, and every test that uses this harness is safe by
//! construction rather than by remembering.
//!
//! # How to use it
//!
//! ```ignore
//! let sb = Sandbox::new();
//! sb.write_claude_json(&json!({ "mcpServers": {} }));
//! let out = sb.json(&["probe", "--json"]);
//! ```
//!
//! The environment is set **both** in this process and on every child command:
//! in-process so a test can drive `piggy_core` directly (applying advice, say,
//! which the CLI deliberately has no verb for), and per-child so the binary
//! under test never depends on inheriting it. Because the in-process half is
//! global, every sandbox holds a lock for its lifetime and the tests in a file
//! serialise against each other.

#![allow(dead_code)] // Not every test file uses every helper.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;

/// Every environment variable that moves a path Piggy reads or writes.
///
/// Kept as names as well as values so a failure can print the one that was
/// wrong, and so this list reads as documentation of the whole surface.
pub const OVERRIDES: [&str; 10] = [
    // The whole `~/.piggy` data dir: database, state.json, backups, bin, venvs.
    "PIGGY_HOME",
    // `~/.claude`: settings.json, rules, skills, plugins.
    "PIGGY_CLAUDE_DIR",
    // The session log dir, which defaults to `<claude_dir>/projects` but is
    // separately overridable, so setting the parent is not enough.
    "PIGGY_CLAUDE_PROJECTS",
    // `~/.claude.json`, the MCP server config the advice engine EDITS.
    "PIGGY_CLAUDE_JSON",
    // `~/.codex`, the Codex rollout dir. The one an earlier sandbox forgot.
    "PIGGY_CODEX_DIR",
    // Config-dir writes. `PIGGY_XDG_CONFIG` wins, and `XDG_CONFIG_HOME` is the
    // fallback underneath it; both are set so the sandbox holds even if the
    // first one is ever dropped.
    "PIGGY_XDG_CONFIG",
    "XDG_CONFIG_HOME",
    // The shell rc file the rtk saver appends a PATH line to.
    "PIGGY_SHELL_PROFILE",
    // The `claude` binary the plugin savers invoke. Pointed at a path that does
    // not exist: a saver reaching for it should fail, never find the real one.
    "PIGGY_CLAUDE_BIN",
    // The python the venv savers build against. Same reasoning.
    "PIGGY_PYTHON_BIN",
];

/// The env is process-global, so sandboxes cannot overlap.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub struct Sandbox {
    _guard: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let guard = env_lock();
        let dir = tempfile::tempdir().expect("a temp dir for the sandbox");
        let sb = Sandbox { _guard: guard, dir };
        // Checked before anything is set, so a mistake in the table below can
        // never reach a real home directory even for one call.
        sb.assert_sandboxed();
        for (key, value) in sb.env() {
            std::env::set_var(key, value);
        }
        for d in ["piggy", "claude", "claude/projects", "codex", "xdg", "bin"] {
            std::fs::create_dir_all(sb.root().join(d)).unwrap();
        }
        sb
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The whole sandbox, as name/value pairs. One definition, used by the
    /// in-process setup, by every child command, and by the assertion.
    pub fn env(&self) -> Vec<(&'static str, PathBuf)> {
        let r = self.root();
        let paths = [
            r.join("piggy"),
            r.join("claude"),
            r.join("claude/projects"),
            r.join("claude.json"),
            r.join("codex"),
            r.join("xdg"),
            r.join("xdg"),
            r.join("zshrc"),
            r.join("bin/claude"),
            r.join("bin/python3"),
        ];
        OVERRIDES.into_iter().zip(paths).collect()
    }

    /// Fail loudly if any override still resolves outside this sandbox.
    ///
    /// The strong form of "not in the real home": every value must sit under
    /// the sandbox root. It also catches the failure mode a plain
    /// `!starts_with($HOME)` check misses, which is a variable that is simply
    /// absent and silently defaulting on a machine whose `TMPDIR` happens to
    /// live under the home directory.
    ///
    /// The named real paths are spelled out underneath it anyway, because those
    /// are the accidents that actually cost something.
    pub fn assert_sandboxed(&self) {
        let root = self.root();
        let real_home = std::env::var_os("HOME").map(PathBuf::from);
        for (key, value) in self.env() {
            assert!(
                value.starts_with(root),
                "{key} escaped the sandbox: {}",
                value.display()
            );
            if let Some(home) = &real_home {
                for name in [".piggy", ".claude", ".claude.json", ".codex", ".zshrc", ".config"] {
                    assert_ne!(
                        value,
                        home.join(name),
                        "{key} resolves to the real {name}, which this test would then edit"
                    );
                }
            }
        }
    }

    // -- running the binary --------------------------------------------------

    /// The compiled `piggy` under test, with the whole sandbox on its
    /// environment.
    ///
    /// `CARGO_BIN_EXE_piggy` is Cargo's own path to the binary this crate
    /// builds, so the test always runs the code it was compiled beside.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_piggy"));
        c.args(args);
        // Explicit as well as inherited: a test that spawns the binary must not
        // depend on this process having set the same thing.
        for (key, value) in self.env() {
            c.env(key, value);
        }
        c
    }

    pub fn output(&self, args: &[&str]) -> Output {
        self.cmd(args)
            .output()
            .unwrap_or_else(|e| panic!("running `piggy {}`: {e}", args.join(" ")))
    }

    /// Run and return stdout, failing the test with both streams if the command
    /// did not succeed.
    pub fn run(&self, args: &[&str]) -> String {
        let out = self.output(args);
        assert!(
            out.status.success(),
            "`piggy {}` failed ({})\n--- stdout\n{}\n--- stderr\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let stdout = self.run(args);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("`piggy {}` did not emit JSON: {e}\n{stdout}", args.join(" ")))
    }

    // -- fixtures ------------------------------------------------------------

    pub fn claude_json(&self) -> PathBuf {
        self.root().join("claude.json")
    }

    pub fn write_claude_json(&self, value: &Value) {
        std::fs::write(
            self.claude_json(),
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }

    /// A project directory inside the sandbox, created.
    pub fn project(&self, name: &str) -> PathBuf {
        let p = self.root().join("projects").join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn write(&self, path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    pub fn store(&self) -> piggy_core::Store {
        piggy_core::Store::open(&self.root().join("piggy")).unwrap()
    }

    /// Write one session log Claude Code could have written.
    ///
    /// The first assistant message's write is the session floor (see
    /// `parser::attribute_context`), so `floor` and `work` land in the two
    /// buckets the ledger reports separately. `ago_days` places it in a window.
    pub fn seed_session(&self, id: &str, cwd: &Path, ago_days: i64, floor: u64, work: u64) {
        let at = |secs: i64| {
            (chrono::Utc::now() - chrono::Duration::days(ago_days) + chrono::Duration::seconds(secs))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        };
        let cwd = cwd.display().to_string();
        let assistant = |ts: String, req: &str, uuid: &str, input: u64| {
            serde_json::json!({
                "type": "assistant",
                "parentUuid": "p0",
                "isSidechain": false,
                "timestamp": ts,
                "userType": "external",
                "entrypoint": "cli",
                "cwd": cwd,
                "sessionId": id,
                "version": "2.1.0",
                "requestId": req,
                "uuid": uuid,
                "message": {
                    "id": format!("msg_{req}"),
                    "model": "claude-sonnet-4-5",
                    "usage": { "input_tokens": input, "output_tokens": 20 }
                }
            })
            .to_string()
        };
        let user = serde_json::json!({
            "type": "user",
            "parentUuid": "u1",
            "isSidechain": false,
            "timestamp": at(1),
            "userType": "external",
            "cwd": cwd,
            "sessionId": id,
            "version": "2.1.0",
            "uuid": "u2",
            "message": { "role": "user", "content": "carry on" }
        })
        .to_string();

        let lines = [
            assistant(at(0), "req_a", "u1", floor),
            user,
            assistant(at(2), "req_b", "u3", work),
        ];
        let dir = self.root().join("claude/projects/-sandbox-project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    }
}

/// The MCP fixture servers, which live with the crate whose probe they exercise.
pub fn mcp_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../piggy-core/tests/fixtures/mcp")
        .join(name)
}

/// `which node`, or `None`. The fixture servers are node scripts.
pub fn node_bin() -> Option<String> {
    let out = Command::new("which").arg("node").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}
