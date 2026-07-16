//! End-to-end M2 engine tests.
//!
//! These exercise the parts that read Piggy's path env vars (`PIGGY_HOME`,
//! `PIGGY_CLAUDE_DIR`, `PIGGY_CLAUDE_JSON`, `PIGGY_CLAUDE_BIN`,
//! `PIGGY_ASSET_CACHE_DIR`). Because env is process-global, every test here takes
//! a global lock and points those vars at a fresh tempdir — so nothing ever
//! touches the real `~/.claude` or `~/.piggy`, and the tests are serialized among
//! themselves while the pure/parser/store test binaries still run in parallel.
//!
//! No network: `download_release_asset` is satisfied from a local asset cache
//! holding a *fake* rtk tarball + checksums. No real `claude`: plugin steps run a
//! recording shell shim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::state::PiggyState;
use piggy_core::{engine, settings, sweep, Catalog, ModelTokens, SessionParse, Store};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Sandbox: global env lock + tempdir wiring
// ---------------------------------------------------------------------------

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Sandbox {
    _guard: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("PIGGY_HOME", dir.path().join("piggy"));
        std::env::set_var("PIGGY_CLAUDE_DIR", dir.path().join("claude"));
        std::env::set_var("PIGGY_CLAUDE_JSON", dir.path().join("claude.json"));
        std::env::set_var("PIGGY_CLAUDE_PROJECTS", dir.path().join("projects"));
        // Sandbox the shell profile too: the `ensure_dir_on_path` install step
        // appends a PATH line, and without this override it would edit the real
        // `~/.zshrc` during `cargo test`.
        std::env::set_var("PIGGY_SHELL_PROFILE", dir.path().join("zshrc"));
        // Optional vars off by default; tests opt in.
        std::env::remove_var("PIGGY_CLAUDE_BIN");
        std::env::remove_var("PIGGY_ASSET_CACHE_DIR");
        std::env::remove_var("PIGGY_PYTHON_BIN");
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        Sandbox { _guard: guard, dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn claude_dir(&self) -> PathBuf {
        self.dir.path().join("claude")
    }
    fn settings_path(&self) -> PathBuf {
        self.claude_dir().join("settings.json")
    }
    fn claude_json(&self) -> PathBuf {
        self.dir.path().join("claude.json")
    }
    fn piggy_bin(&self) -> PathBuf {
        self.dir.path().join("piggy").join("bin")
    }

    /// Seed settings.json from a repo fixture, returning its exact bytes.
    fn seed_settings_from_fixture(&self, name: &str) -> Vec<u8> {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/settings")
            .join(name);
        let bytes = std::fs::read(&src).unwrap();
        std::fs::write(self.settings_path(), &bytes).unwrap();
        bytes
    }

    fn seed_settings_bytes(&self, bytes: &[u8]) {
        std::fs::write(self.settings_path(), bytes).unwrap();
    }

    fn read_settings(&self) -> Vec<u8> {
        std::fs::read(self.settings_path()).unwrap()
    }

    /// Point PIGGY_CLAUDE_BIN at the recording claude shim.
    fn use_shim(&self) {
        let shim =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shims/claude-shim.sh");
        std::env::set_var("PIGGY_CLAUDE_BIN", shim);
    }

    /// Point PIGGY_PYTHON_BIN at the fake `python3` shim (fakes `-m venv` + pip,
    /// no network) so venv-based savers like Headroom install offline.
    fn use_python_shim(&self) {
        let shim =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shims/python-shim.sh");
        std::env::set_var("PIGGY_PYTHON_BIN", shim);
    }

    fn piggy_home(&self) -> PathBuf {
        self.dir.path().join("piggy")
    }

    fn shim_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.claude_dir().join("claude-shim.log"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    /// Build a fake rtk tarball (whose `rtk --version` exits `version_exit`) and
    /// place it, under every catalog asset filename, into an asset cache dir with
    /// a matching checksums.txt. Sets PIGGY_ASSET_CACHE_DIR.
    fn use_fake_rtk_asset(&self, version_exit: i32) {
        let cache = self.dir.path().join("assets");
        std::fs::create_dir_all(&cache).unwrap();
        let tarball = build_rtk_tarball(version_exit);
        let sha = settings::hash_bytes(&tarball);

        let catalog = Catalog::embedded();
        let assets = &catalog.get("rtk").unwrap().source.assets;
        let mut checksums = String::new();
        for filename in assets.values() {
            std::fs::write(cache.join(filename), &tarball).unwrap();
            checksums.push_str(&format!("{sha}  {filename}\n"));
        }
        std::fs::write(cache.join("checksums.txt"), checksums).unwrap();
        std::env::set_var("PIGGY_ASSET_CACHE_DIR", &cache);
    }
}

/// A `.tar.gz` containing a single executable `rtk` shell script.
fn build_rtk_tarball(version_exit: i32) -> Vec<u8> {
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'rtk 0.43.0-fake'; exit {version_exit}; fi\nexit 0\n"
    );
    let data = script.into_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path("rtk").unwrap();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();

    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.append(&header, &data[..]).unwrap();
    let enc = builder.into_inner().unwrap();
    enc.finish().unwrap()
}

// ---------------------------------------------------------------------------
// Merge-engine commit path (backup / atomic / external change)
// ---------------------------------------------------------------------------

#[test]
fn commit_backs_up_seeds_pre_piggy_and_records_hash() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    let mut state = PiggyState::default();

    let outcome = settings::commit(&sb.settings_path(), "test-write", &mut state, None, |v| {
        v.as_object_mut()
            .unwrap()
            .insert("addedByPiggy".into(), json!(true));
    })
    .unwrap();

    assert!(
        outcome.backup_path.is_some(),
        "a timestamped backup was taken"
    );
    assert!(state.settings_hash.is_some(), "content hash recorded");
    assert!(!outcome.external_change);
    // pre-piggy.json seeded (Restore Defaults target).
    let pre = piggy_core::config::backups_dir().join("pre-piggy.json");
    assert!(pre.exists());
    // The write landed.
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["addedByPiggy"], true);
}

#[test]
fn commit_detects_external_change_and_preserves_the_edit() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    let mut state = PiggyState::default();

    // First Piggy write establishes the recorded hash.
    settings::commit(&sb.settings_path(), "w1", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    })
    .unwrap();

    // Someone edits settings.json out from under Piggy.
    let mut edited: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    edited
        .as_object_mut()
        .unwrap()
        .insert("userAdded".into(), json!("hi"));
    sb.seed_settings_bytes(
        format!("{}\n", serde_json::to_string_pretty(&edited).unwrap()).as_bytes(),
    );

    // Next Piggy write must notice, warn, and keep the user's edit.
    let outcome = settings::commit(&sb.settings_path(), "w2", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy2".into(), json!(2));
    })
    .unwrap();
    assert!(outcome.external_change, "external change detected");
    assert!(outcome.warnings.iter().any(|w| w.contains("changed since")));

    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["userAdded"], "hi", "user edit preserved");
    assert_eq!(disk["piggy2"], 2, "new Piggy write applied");
}

/// The pre/post-`claude` snapshot (used for plugin installs, which add no hooks)
/// must be a *pure* backup: it may not rewrite, reformat, or BOM-strip the user's
/// settings.json before `claude` even runs.
#[test]
fn backup_only_does_not_rewrite_or_bom_strip_the_file() {
    let sb = Sandbox::new();
    // BOM'd, compact (non-2-space) settings.json — the pure-plugin case.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"{\"enabledPlugins\":{\"x@y\":true}}");
    sb.seed_settings_bytes(&bytes);

    let mut state = PiggyState::default();
    let backup = settings::backup_only(&sb.settings_path(), "pre-cli", &mut state).unwrap();

    assert_eq!(
        sb.read_settings(),
        bytes,
        "backup_only must leave the file byte-for-byte unchanged (BOM + formatting)"
    );
    assert!(backup.is_some(), "a timestamped backup was still taken");
}

// ---------------------------------------------------------------------------
// Acceptance test: install → health → uninstall == byte-identical
// ---------------------------------------------------------------------------

#[test]
fn rtk_install_healthcheck_uninstall_is_byte_identical_on_openbar() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(0); // healthy rtk

    let catalog = Catalog::embedded();

    // Install.
    let report = engine::install(&catalog, "rtk").unwrap();
    assert!(!report.rolled_back, "install should succeed: {report:?}");
    assert!(report.health.as_ref().unwrap().ok(), "health checks pass");

    // Binary extracted into the sandbox (never the real ~/.piggy).
    assert!(sb.piggy_bin().join("rtk").exists());

    // settings.json now has BOTH the openbar hooks and Piggy's rtk hook.
    let after_install: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    let pre = after_install["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "openbar wildcard + rtk Bash hook");
    assert!(pre.iter().any(|g| g["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains("/rtk hook claude")));
    // The rtk command was expanded to an absolute sandbox path, not left as a placeholder.
    let rtk_cmd = pre
        .iter()
        .find_map(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .filter(|c| c.contains("rtk"))
        })
        .unwrap();
    assert!(rtk_cmd.starts_with(sb.piggy_bin().to_str().unwrap()));
    assert!(!rtk_cmd.contains("${PIGGY_BIN}"));

    // Uninstall → byte-for-byte back to the original openbar file.
    let un = engine::uninstall(&catalog, "rtk").unwrap();
    assert!(un.messages.iter().any(|m| m.contains("byte-identical")));
    assert_eq!(sb.read_settings(), original, "settings.json byte-identical");
    assert!(!sb.piggy_bin().join("rtk").exists(), "binary removed");
    assert!(!PiggyState::load().unwrap().is_installed("rtk"));
}

/// Real end-to-end install against the live GitHub release (no asset cache):
/// downloads rtk v0.43.0, verifies the published sha256, extracts and runs the
/// *real* binary's `--version` for the health check, then restores byte-for-byte.
/// `#[ignore]` so `cargo test` stays offline; run with `--ignored`.
#[test]
#[ignore = "network: downloads the real rtk v0.43.0 release from GitHub"]
fn rtk_real_download_install_uninstall_byte_identical() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("openbar.json");
    // No use_fake_rtk_asset(): PIGGY_ASSET_CACHE_DIR is unset → real download.
    // Safety: run the real rtk binary only with a temp HOME and telemetry off.
    std::env::set_var("HOME", sb.root());
    std::env::set_var("RTK_TELEMETRY_DISABLED", "1");
    let catalog = Catalog::embedded();

    let report = engine::install(&catalog, "rtk").unwrap();
    assert!(
        !report.rolled_back,
        "real install should succeed: {report:?}"
    );
    assert!(
        report.health.as_ref().unwrap().ok(),
        "real rtk passes health"
    );
    assert!(sb.piggy_bin().join("rtk").exists());

    engine::uninstall(&catalog, "rtk").unwrap();
    assert_eq!(
        sb.read_settings(),
        original,
        "byte-identical after real round-trip"
    );
}

#[test]
fn rtk_install_over_manual_rtk_keeps_user_hook_and_restores_identically() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("already-has-rtk.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();
    let after: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(
        after["hooks"]["PreToolUse"].as_array().unwrap().len(),
        2,
        "user's manual rtk hook + Piggy's own"
    );

    engine::uninstall(&catalog, "rtk").unwrap();
    assert_eq!(
        sb.read_settings(),
        original,
        "user's rtk hook preserved byte-identically"
    );
}

#[test]
fn failed_health_check_rolls_back_completely() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(1); // rtk --version exits 1 → health fails
    let catalog = Catalog::embedded();

    let report = engine::install(&catalog, "rtk").unwrap();
    assert!(report.rolled_back, "a failing health check rolls back");

    // Everything reverted: settings byte-identical, binary gone, not in state.
    assert_eq!(sb.read_settings(), original);
    assert!(!sb.piggy_bin().join("rtk").exists());
    assert!(!PiggyState::load().unwrap().is_installed("rtk"));
}

#[test]
fn download_rejects_a_checksum_mismatch() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("openbar.json");
    // Build an asset cache whose checksums.txt lists a wrong hash.
    let cache = sb.dir.path().join("assets");
    std::fs::create_dir_all(&cache).unwrap();
    let tarball = build_rtk_tarball(0);
    let catalog = Catalog::embedded();
    let mut checksums = String::new();
    for filename in catalog.get("rtk").unwrap().source.assets.values() {
        std::fs::write(cache.join(filename), &tarball).unwrap();
        checksums.push_str(&format!("{}  {filename}\n", "0".repeat(64)));
    }
    std::fs::write(cache.join("checksums.txt"), checksums).unwrap();
    std::env::set_var("PIGGY_ASSET_CACHE_DIR", &cache);

    let err = engine::install(&catalog, "rtk").unwrap_err();
    assert!(err.to_string().contains("rolled back"));
    let chain = format!("{err:#}");
    assert!(
        chain.contains("checksum mismatch"),
        "cause surfaced: {chain}"
    );
    assert!(!PiggyState::load().unwrap().is_installed("rtk"));
}

// ---------------------------------------------------------------------------
// Plugin savers via the recording claude shim
// ---------------------------------------------------------------------------

#[test]
fn caveman_install_and_uninstall_via_shim() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    let report = engine::install(&catalog, "caveman").unwrap();
    assert!(!report.rolled_back);
    assert!(
        report.health.as_ref().unwrap().ok(),
        "plugin_enabled passes"
    );

    // The engine issued exactly the catalog's two install commands, in order.
    let log = sb.shim_log();
    assert_eq!(log[0], "plugin marketplace add JuliusBrussee/caveman");
    assert_eq!(log[1], "plugin install caveman@caveman");

    // settings.json shows the plugin enabled (shim simulated it).
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["enabledPlugins"]["caveman@caveman"], true);
    assert!(PiggyState::load().unwrap().is_installed("caveman"));

    // Uninstall removes it.
    engine::uninstall(&catalog, "caveman").unwrap();
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert!(disk["enabledPlugins"].get("caveman@caveman").is_none());
    assert!(!PiggyState::load().unwrap().is_installed("caveman"));
}

#[test]
fn ponytail_install_uninstall_runs_full_step_set() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    // Install: require_binary(node, soft) + two claude_cli steps.
    let report = engine::install(&catalog, "ponytail").unwrap();
    assert!(!report.rolled_back, "{report:?}");
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["enabledPlugins"]["ponytail@ponytail"], true);

    // Uninstall: run_plugin_script (ignoreFailure) + uninstall + marketplace
    // remove (ignoreFailure) + verify_no_setting — none should hard-fail.
    let un = engine::uninstall(&catalog, "ponytail").unwrap();
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert!(disk["enabledPlugins"].get("ponytail@ponytail").is_none());
    assert!(!PiggyState::load().unwrap().is_installed("ponytail"));
    // The plugin-script failure was tolerated, surfaced as a note.
    assert!(
        un.warnings
            .iter()
            .any(|w| w.contains("run_plugin_script") || w.contains("scripts/uninstall.js"))
            || un
                .messages
                .iter()
                .any(|m| m.contains("scripts/uninstall.js"))
    );
}

#[test]
fn conflicting_saver_is_auto_disabled_on_install() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    engine::install(&catalog, "caveman").unwrap();
    assert!(PiggyState::load().unwrap().savers["caveman"].enabled);

    // ponytail conflictsWith caveman: installing it auto-disables caveman
    // rather than refusing, so the newly-turned-on saver cleanly wins.
    let report = engine::install(&catalog, "ponytail").unwrap();
    assert!(!report.rolled_back);
    assert!(
        report
            .messages
            .iter()
            .any(|m| m.contains("turned off") && m.contains("caveman")),
        "expected an auto-disable message, got {:?}",
        report.messages
    );

    let state = PiggyState::load().unwrap();
    assert!(state.savers["ponytail"].enabled, "ponytail is on");
    assert!(
        !state.savers["caveman"].enabled,
        "caveman auto-disabled by the conflicting install"
    );
    // The plugin stays installed (only disabled) — auto-disable is a toggle-off,
    // not an uninstall.
    assert!(state.is_installed("caveman"));

    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["enabledPlugins"]["ponytail@ponytail"], true);
    assert_eq!(disk["enabledPlugins"]["caveman@caveman"], false);
}

#[test]
fn conflicting_saver_is_auto_disabled_on_toggle_on() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    // Install both, but leave caveman off and ponytail on: turning caveman back
    // on must auto-disable ponytail (the reverse direction).
    engine::install(&catalog, "caveman").unwrap();
    engine::install(&catalog, "ponytail").unwrap(); // auto-disables caveman
    engine::set_enabled(&catalog, "caveman", true).unwrap(); // auto-disables ponytail

    let state = PiggyState::load().unwrap();
    assert!(state.savers["caveman"].enabled);
    assert!(!state.savers["ponytail"].enabled);
}

// ---------------------------------------------------------------------------
// Headroom: venv + wrapper-launcher install (the proxy saver, wrapper model)
// ---------------------------------------------------------------------------

#[test]
fn headroom_installs_venv_and_launcher_then_uninstalls_clean() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_python_shim();
    let catalog = Catalog::embedded();

    let report = engine::install(&catalog, "headroom").unwrap();
    assert!(!report.rolled_back, "install succeeded: {report:?}");

    let venv = sb.piggy_home().join("venvs/headroom");
    let launcher = sb.piggy_bin().join("piggy-claude");
    assert!(venv.join("bin/headroom").exists(), "venv headroom present");
    assert!(launcher.exists(), "launcher written");

    // The launcher execs the venv headroom binary with `wrap claude` — no global
    // ANTHROPIC_BASE_URL, no daemon: compression is scoped to this command.
    let script = std::fs::read_to_string(&launcher).unwrap();
    assert!(script.contains("venvs/headroom/bin/headroom"));
    assert!(script.contains("wrap") && script.contains("claude"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "launcher is executable");
    }

    // Health check passes (fake `headroom --version` + PATH block present).
    let health = engine::health_check(&catalog, "headroom").unwrap();
    assert!(health.ok(), "health: {:?}", health.checks);

    let state = PiggyState::load().unwrap();
    assert!(state.savers["headroom"].enabled);
    assert!(
        state.savers["headroom"]
            .installed_files
            .iter()
            .any(|f| f.ends_with("venvs/headroom")),
        "venv tracked as an installed artifact for cleanup"
    );

    // Uninstall removes the venv tree, the launcher, and (last consumer) the
    // PATH line.
    engine::uninstall(&catalog, "headroom").unwrap();
    assert!(!venv.exists(), "venv tree removed");
    assert!(!launcher.exists(), "launcher removed");
    assert!(!PiggyState::load().unwrap().is_installed("headroom"));
    let profile = std::fs::read_to_string(sb.root().join("zshrc")).unwrap_or_default();
    assert!(
        !profile.contains("piggy (managed PATH)"),
        "PATH line removed when the last ${{PIGGY_BIN}} consumer is gone"
    );
}

#[test]
fn headroom_auto_disables_rtk_and_shares_the_path_line() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_fake_rtk_asset(0);
    sb.use_python_shim();
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();
    assert!(PiggyState::load().unwrap().savers["rtk"].enabled);

    // Headroom conflictsWith rtk (it wraps rtk internally): installing it
    // auto-disables rtk rather than double-compressing.
    let report = engine::install(&catalog, "headroom").unwrap();
    assert!(
        report
            .messages
            .iter()
            .any(|m| m.contains("turned off") && m.contains("rtk")),
        "expected rtk auto-disable: {:?}",
        report.messages
    );
    let state = PiggyState::load().unwrap();
    assert!(state.savers["headroom"].enabled);
    assert!(!state.savers["rtk"].enabled, "rtk auto-disabled");
    assert!(state.is_installed("rtk"), "rtk kept installed (just off)");

    // Uninstalling headroom KEEPS the shared PATH line — rtk still needs it.
    engine::uninstall(&catalog, "headroom").unwrap();
    let profile = std::fs::read_to_string(sb.root().join("zshrc")).unwrap();
    assert!(
        profile.contains("piggy (managed PATH)"),
        "PATH line kept while rtk remains"
    );

    // Uninstalling rtk (now the last consumer) finally removes it.
    engine::uninstall(&catalog, "rtk").unwrap();
    let profile = std::fs::read_to_string(sb.root().join("zshrc")).unwrap_or_default();
    assert!(
        !profile.contains("piggy (managed PATH)"),
        "PATH line removed after the last consumer"
    );
}

#[test]
fn the_cli_link_keeps_the_path_line_when_the_last_saver_goes() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();

    // The user also opted into the `piggy` command line tool, which lives in the
    // same ${PIGGY_BIN} directory but belongs to no saver.
    let sidecar = sb.root().join("Piggy.app/Contents/MacOS/piggy");
    std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    std::fs::write(&sidecar, b"#!/bin/sh\necho piggy\n").unwrap();
    piggy_core::cli_link::install(&sidecar).unwrap();

    // Uninstalling the last saver must NOT take `piggy` off the user's PATH.
    engine::uninstall(&catalog, "rtk").unwrap();

    let profile = std::fs::read_to_string(sb.root().join("zshrc")).unwrap();
    assert!(
        profile.contains("piggy (managed PATH)"),
        "PATH line kept while the piggy CLI link is installed"
    );
    assert!(
        piggy_core::cli_link::exists(),
        "CLI link survives a saver uninstall"
    );

    // Turning the CLI tool off is what finally removes the line.
    piggy_core::cli_link::uninstall().unwrap();
    let profile = std::fs::read_to_string(sb.root().join("zshrc")).unwrap_or_default();
    assert!(
        !profile.contains("piggy (managed PATH)"),
        "PATH line removed once nothing needs ${{PIGGY_BIN}}"
    );
}

#[test]
fn failed_plugin_install_rolls_back() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    // Make the shim fail on the plugin-install command.
    std::env::set_var("PIGGY_SHIM_FAIL", "plugin install caveman@caveman");
    let catalog = Catalog::embedded();

    let err = engine::install(&catalog, "caveman").unwrap_err();
    std::env::remove_var("PIGGY_SHIM_FAIL");
    assert!(err.to_string().contains("rolled back"));
    assert!(!PiggyState::load().unwrap().is_installed("caveman"));
}

/// State drift on the "turn everything off" path: Claude already has the plugin
/// disabled in settings.json (someone disabled it outside Piggy) while Piggy's
/// ledger still says it is on. Turning it off must heal the ledger without
/// re-issuing a `claude plugin disable` the CLI would reject as "already
/// disabled".
#[test]
fn toggle_off_heals_ledger_when_plugin_already_disabled_in_settings() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    engine::install(&catalog, "caveman").unwrap();
    assert!(PiggyState::load().unwrap().savers["caveman"].enabled);

    // Someone disabled the plugin in Claude's settings, bypassing Piggy: the
    // ledger still thinks it is on.
    let mut disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    disk["enabledPlugins"]["caveman@caveman"] = json!(false);
    sb.seed_settings_bytes(
        format!("{}\n", serde_json::to_string_pretty(&disk).unwrap()).as_bytes(),
    );

    let disable_line = "plugin disable caveman@caveman";
    let before = sb.shim_log().iter().filter(|l| *l == disable_line).count();

    // The master-off path calls set_enabled(.., false); it should succeed by
    // healing the ledger rather than aborting on the CLI's "already disabled".
    engine::set_enabled(&catalog, "caveman", false).unwrap();

    assert!(
        !PiggyState::load().unwrap().savers["caveman"].enabled,
        "ledger healed to disabled"
    );
    let after = sb.shim_log().iter().filter(|l| *l == disable_line).count();
    assert_eq!(
        after, before,
        "no redundant `claude plugin disable` was issued"
    );
}

/// The symmetric drift heal on the way back on: the plugin is already enabled in
/// Claude's settings while Piggy's ledger has it off. Turning it on heals the
/// ledger without a redundant `claude plugin enable`.
#[test]
fn toggle_on_heals_ledger_when_plugin_already_enabled_in_settings() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    engine::install(&catalog, "caveman").unwrap();
    engine::set_enabled(&catalog, "caveman", false).unwrap();
    assert!(!PiggyState::load().unwrap().savers["caveman"].enabled);

    // Someone re-enabled the plugin in Claude's settings, bypassing Piggy: the
    // ledger still thinks it is off.
    let mut disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    disk["enabledPlugins"]["caveman@caveman"] = json!(true);
    sb.seed_settings_bytes(
        format!("{}\n", serde_json::to_string_pretty(&disk).unwrap()).as_bytes(),
    );

    let enable_line = "plugin enable caveman@caveman";
    let before = sb.shim_log().iter().filter(|l| *l == enable_line).count();

    engine::set_enabled(&catalog, "caveman", true).unwrap();

    assert!(
        PiggyState::load().unwrap().savers["caveman"].enabled,
        "ledger healed to enabled"
    );
    let after = sb.shim_log().iter().filter(|l| *l == enable_line).count();
    assert_eq!(
        after, before,
        "no redundant `claude plugin enable` was issued"
    );
}

/// A genuine `claude plugin disable` failure — one that leaves reality NOT
/// matching the request — must still propagate as an error, and must not falsely
/// heal Piggy's ledger to disabled.
#[test]
fn genuine_plugin_disable_failure_propagates_and_keeps_ledger() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    engine::install(&catalog, "caveman").unwrap();
    assert!(PiggyState::load().unwrap().savers["caveman"].enabled);

    // Settings still show the plugin enabled (normal state), so the drift-tolerant
    // helper actually runs the CLI — which we force to fail with no side effects.
    std::env::set_var("PIGGY_SHIM_FAIL", "plugin disable caveman@caveman");
    let res = engine::set_enabled(&catalog, "caveman", false);
    std::env::remove_var("PIGGY_SHIM_FAIL");

    assert!(res.is_err(), "the CLI failure propagates");
    assert!(
        PiggyState::load().unwrap().savers["caveman"].enabled,
        "ledger not falsely healed to disabled on a real failure"
    );
}

// ---------------------------------------------------------------------------
// Toggle on/off (the fast A/B path)
// ---------------------------------------------------------------------------

#[test]
fn toggle_off_and_on_a_hook_saver() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();

    // Off: hooks removed (binary stays).
    engine::set_enabled(&catalog, "rtk", false).unwrap();
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(
        disk["hooks"]["PreToolUse"].as_array().unwrap().len(),
        1,
        "only openbar hook left"
    );
    assert!(sb.piggy_bin().join("rtk").exists(), "binary kept while off");
    assert!(!PiggyState::load().unwrap().savers["rtk"].enabled);

    // On: hook re-added.
    engine::set_enabled(&catalog, "rtk", true).unwrap();
    let disk: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    assert!(PiggyState::load().unwrap().savers["rtk"].enabled);

    // Full uninstall still restores byte-identically.
    engine::uninstall(&catalog, "rtk").unwrap();
    assert_eq!(sb.read_settings(), original);
}

// ---------------------------------------------------------------------------
// Restore Defaults
// ---------------------------------------------------------------------------

#[test]
fn restore_defaults_returns_settings_to_pre_piggy() {
    let sb = Sandbox::new();
    let original = sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();
    assert_ne!(sb.read_settings(), original);

    let report = engine::restore_defaults().unwrap();
    assert!(report.byte_restored);
    assert_eq!(
        sb.read_settings(),
        original,
        "back to exact pre-Piggy bytes"
    );
    assert!(!sb.piggy_bin().join("rtk").exists(), "binary removed");
    assert!(PiggyState::load().unwrap().savers.is_empty());
}

/// Restore Defaults overwrites settings.json with the pre-Piggy snapshot — but a
/// user edit made *after* Piggy's last write must be backed up first, never
/// silently destroyed (the panic button is not allowed to be the one thing that
/// loses data).
#[test]
fn restore_defaults_backs_up_a_post_install_user_edit() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();

    engine::install(&catalog, "rtk").unwrap();

    // The user hand-edits settings.json after Piggy's last write.
    let mut edited: Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    edited
        .as_object_mut()
        .unwrap()
        .insert("userAddedAfterInstall".into(), json!("precious"));
    sb.seed_settings_bytes(
        format!("{}\n", serde_json::to_string_pretty(&edited).unwrap()).as_bytes(),
    );

    let report = engine::restore_defaults().unwrap();
    assert!(report.byte_restored, "settings returned to pre-Piggy bytes");
    assert!(
        !sb.read_settings()
            .windows(b"precious".len())
            .any(|w| w == b"precious"),
        "restore-defaults did overwrite the live file (expected)"
    );
    // The destroyed edit is recoverable from a backup.
    assert!(
        any_backup_contains(&piggy_core::config::backups_dir(), b"precious"),
        "the post-install user edit was overwritten with no backup — unrecoverable"
    );
}

/// Pruning old timestamped backups must never delete a still-installed saver's
/// `pre_install_backup` (its byte-identical uninstall target), even after far
/// more than the 50-backup cap of unrelated writes.
#[test]
fn prune_keeps_an_installed_savers_pre_install_backup() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset(0);
    let catalog = Catalog::embedded();
    engine::install(&catalog, "rtk").unwrap();

    let pre_backup = PiggyState::load().unwrap().savers["rtk"]
        .pre_install_backup
        .clone()
        .expect("rtk records a pre-install backup");
    assert!(
        Path::new(&pre_backup).exists(),
        "backup captured on install"
    );

    // Churn well past MAX_TIMESTAMPED_BACKUPS (50) unrelated settings writes.
    let mut state = PiggyState::load().unwrap();
    for i in 0..60 {
        settings::commit(&sb.settings_path(), "churn", &mut state, None, |v| {
            v.as_object_mut().unwrap().insert("churn".into(), json!(i));
        })
        .unwrap();
    }
    state.save().unwrap();

    assert!(
        Path::new(&pre_backup).exists(),
        "prune deleted a live saver's pre_install_backup (byte-identical uninstall lost)"
    );
}

/// Turning a saver ON via the fast toggle must honour `conflictsWith` the same
/// way a fresh install does — auto-disabling the conflicting saver — otherwise
/// `off A → install B → on A` leaves two mutually-exclusive savers enabled.
#[test]
fn toggle_on_auto_disables_a_conflicting_saver() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("minimal.json");
    sb.use_shim();
    let catalog = Catalog::embedded();

    // caveman conflictsWith ponytail (and vice versa).
    engine::install(&catalog, "caveman").unwrap();
    engine::set_enabled(&catalog, "caveman", false).unwrap(); // off, still installed
    engine::install(&catalog, "ponytail").unwrap(); // caveman already off — no conflict

    // Now `on caveman` auto-disables ponytail (it is on and conflicts).
    let report = engine::set_enabled(&catalog, "caveman", true).unwrap();
    assert!(
        report
            .messages
            .iter()
            .any(|m| m.contains("turned off") && m.contains("ponytail")),
        "toggle-on ignored conflictsWith: {:?}",
        report.messages
    );
    let state = PiggyState::load().unwrap();
    assert!(state.savers["caveman"].enabled, "caveman is on");
    assert!(
        !state.savers["ponytail"].enabled,
        "ponytail auto-disabled by the conflicting toggle-on"
    );
}

/// Recursively scan `dir` for any file containing `needle`.
fn any_backup_contains(dir: &Path, needle: &[u8]) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if any_backup_contains(&p, needle) {
                return true;
            }
        } else if std::fs::read(&p)
            .map(|b| b.windows(needle.len()).any(|w| w == needle))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Sweep: scan → apply → restore
// ---------------------------------------------------------------------------

fn seed_session_with_tools(store: &mut Store, id: &str, last_ts: &str, tools: &[(&str, u64)]) {
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some(last_ts.to_string()),
        last_ts: Some(last_ts.to_string()),
        models: BTreeMap::new(),
        n_assistant_msgs: 1,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: tools.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        parse_errors: 0,
    };
    store
        .upsert_session(
            &parse,
            &piggy_core::Pricing::embedded(),
            &format!("/f/{id}"),
            1,
            1,
        )
        .unwrap();
}

#[test]
fn sweep_flags_unused_mcp_server_and_apply_restore_round_trips() {
    let sb = Sandbox::new();
    // claude.json with two MCP servers under one project.
    let claude_json = json!({
        "projects": {
            "/proj": {
                "mcpServers": {
                    "usedserver": { "command": "npx", "args": ["used"] },
                    "unusedserver": { "command": "npx", "args": ["unused", "--flag"] }
                }
            }
        },
        "pluginUsage": {},
        "skillUsage": {}
    });
    std::fs::write(
        sb.claude_json(),
        format!("{}\n", serde_json::to_string_pretty(&claude_json).unwrap()),
    )
    .unwrap();
    // settings.json with one enabled, unused plugin.
    std::fs::write(
        sb.settings_path(),
        serde_json::to_string_pretty(&json!({"enabledPlugins": {"idle@mkt": true}})).unwrap(),
    )
    .unwrap();

    // Store with usage only for `usedserver`.
    let home = piggy_core::config::piggy_home();
    let mut store = Store::open(&home).unwrap();
    seed_session_with_tools(
        &mut store,
        "s1",
        "2026-07-12T10:00:00.000Z",
        &[("mcp__usedserver__do_thing", 4)],
    );

    let report = sweep::scan(&store, 50).unwrap();
    assert_eq!(report.sessions_considered, 1);

    let used = report.items.iter().find(|i| i.id == "usedserver").unwrap();
    assert_eq!(used.used, 4);
    assert!(!used.recommend_disable, "used server kept");

    let unused = report
        .items
        .iter()
        .find(|i| i.id == "unusedserver")
        .unwrap();
    assert_eq!(unused.used, 0);
    assert!(unused.recommend_disable, "unused server flagged");

    let idle = report.items.iter().find(|i| i.id == "idle@mkt").unwrap();
    assert!(idle.recommend_disable, "unused plugin flagged");

    // Apply: disable the unused MCP server.
    let mut state = PiggyState::load().unwrap();
    let disabled_id = sweep::apply(&store, &mut state, unused.idx, 50).unwrap();
    assert_eq!(disabled_id, "unusedserver");

    // claude.json no longer lists it; the exact config is snapshotted.
    let cj: Value = serde_json::from_slice(&std::fs::read(sb.claude_json()).unwrap()).unwrap();
    assert!(cj["projects"]["/proj"]["mcpServers"]
        .get("unusedserver")
        .is_none());
    assert!(cj["projects"]["/proj"]["mcpServers"]
        .get("usedserver")
        .is_some());
    let state = PiggyState::load().unwrap();
    assert_eq!(state.sweep_disabled.len(), 1);
    assert_eq!(
        state.sweep_disabled[0].snapshot["args"],
        json!(["unused", "--flag"])
    );

    // Restore everything.
    let mut state = PiggyState::load().unwrap();
    let restored = sweep::restore_all(&mut state).unwrap();
    state.save().unwrap();
    assert_eq!(restored, 1);
    let cj: Value = serde_json::from_slice(&std::fs::read(sb.claude_json()).unwrap()).unwrap();
    assert_eq!(
        cj["projects"]["/proj"]["mcpServers"]["unusedserver"]["args"],
        json!(["unused", "--flag"]),
        "server restored with its exact config"
    );
    assert!(PiggyState::load().unwrap().sweep_disabled.is_empty());
}

#[test]
fn sweep_surfaces_user_hooks_as_informational() {
    let sb = Sandbox::new();
    // A user hook, no plugins / MCP servers / skills.
    std::fs::write(
        sb.settings_path(),
        serde_json::to_string_pretty(&json!({
            "hooks": { "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo hi" }] }
            ]}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(sb.claude_json(), "{}\n").unwrap();

    let home = piggy_core::config::piggy_home();
    let store = Store::open(&home).unwrap();
    let report = sweep::scan(&store, 50).unwrap();

    let hook = report
        .items
        .iter()
        .find(|i| i.kind == "hook")
        .expect("the user's hook is surfaced by sweep");
    assert!(
        !hook.recommend_disable,
        "hooks are informational — never auto-recommended for removal"
    );
    assert_eq!(
        hook.est_tokens, 0,
        "hooks cost no per-request context tokens"
    );
    assert!(!hook.used_windowed, "a hook has no windowed usage count");
}

// ---------------------------------------------------------------------------
// Guard: unknown steps are refused (never guessed)
// ---------------------------------------------------------------------------

#[test]
fn unknown_step_kind_refuses_install() {
    let _sb = Sandbox::new();
    let catalog = Catalog::from_json(
        r#"{
          "registryVersion": 99,
          "entries": [{
            "id": "fromfuture",
            "name": "Future Saver",
            "installType": "binary+hook",
            "source": {"type": "github_release"},
            "install": {"steps": [{"step": "quantum_entangle"}]},
            "uninstall": {"steps": []},
            "healthCheck": {"checks": []}
          }]
        }"#,
    )
    .unwrap();
    let err = engine::install(&catalog, "fromfuture").unwrap_err();
    assert!(err.to_string().contains("unknown step kind"));
    assert!(err.to_string().contains("quantum_entangle"));
}
