//! Tests for the managed `piggy` CLI link.
//!
//! Like the M2 engine tests, these read Piggy's path env vars (`PIGGY_HOME`,
//! `PIGGY_SHELL_PROFILE`), so every test takes a global lock and points them at
//! a fresh tempdir. Without the `PIGGY_SHELL_PROFILE` override these would
//! append a PATH line to the real `~/.zshrc` during `cargo test`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::state::{PiggyState, SaverState};
use piggy_core::{cli_link, config};

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
        std::env::set_var("PIGGY_SHELL_PROFILE", dir.path().join("zshrc"));
        Sandbox { _guard: guard, dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A stand-in for the sidecar `piggy` binary shipped inside Piggy.app.
    fn fake_sidecar(&self, name: &str) -> std::path::PathBuf {
        let app = self.root().join("Piggy.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        let bin = app.join(name);
        std::fs::write(&bin, b"#!/bin/sh\necho piggy\n").unwrap();
        bin
    }

    fn profile(&self) -> String {
        std::fs::read_to_string(self.root().join("zshrc")).unwrap_or_default()
    }

    /// Record an installed saver that keeps `file` in `<piggy_home>/bin`, the
    /// way rtk's binary does.
    fn install_saver_with_bin_file(&self, id: &str, file: &str) {
        let mut state = PiggyState::load().unwrap();
        let path = config::piggy_bin_dir().join(file);
        std::fs::create_dir_all(config::piggy_bin_dir()).unwrap();
        std::fs::write(&path, b"binary").unwrap();
        state.savers.insert(
            id.to_string(),
            SaverState {
                id: id.to_string(),
                version: "1.0.0".into(),
                installed_at: "2026-07-16T00:00:00Z".into(),
                enabled: true,
                injected_hooks: Default::default(),
                installed_files: vec![path.to_string_lossy().into_owned()],
                pre_install_backup: None,
                last_toggle_source: None,
                config: Default::default(),
            },
        );
        state.save().unwrap();
    }
}

#[test]
fn install_links_the_cli_and_puts_bin_on_path() {
    let sb = Sandbox::new();
    let sidecar = sb.fake_sidecar("piggy");

    let report = cli_link::install(&sidecar).unwrap();

    assert!(report.linked, "link created");
    assert!(report.path_added, "PATH block appended");
    assert_eq!(report.link, config::piggy_bin_dir().join("piggy"));
    assert_eq!(
        std::fs::read_link(&report.link).unwrap(),
        sidecar.canonicalize().unwrap(),
        "link points at the sidecar"
    );
    assert!(cli_link::exists());
    let profile = sb.profile();
    assert!(profile.contains("piggy (managed PATH)"));
    assert!(profile.contains(&config::piggy_bin_dir().to_string_lossy().to_string()));
}

#[test]
fn install_is_idempotent() {
    let sb = Sandbox::new();
    let sidecar = sb.fake_sidecar("piggy");

    cli_link::install(&sidecar).unwrap();
    let again = cli_link::install(&sidecar).unwrap();

    assert!(!again.linked, "already pointing at the same target");
    assert!(!again.path_added, "PATH block not duplicated");
    assert_eq!(
        sb.profile().matches("# >>> piggy (managed PATH) >>>").count(),
        1,
        "exactly one managed PATH block"
    );
}

#[test]
fn install_repoints_a_stale_link() {
    let sb = Sandbox::new();
    let old = sb.fake_sidecar("piggy");
    cli_link::install(&old).unwrap();

    // The user moves Piggy.app: same link, new target.
    let moved = sb.root().join("Applications/Piggy.app/Contents/MacOS");
    std::fs::create_dir_all(&moved).unwrap();
    let new = moved.join("piggy");
    std::fs::write(&new, b"#!/bin/sh\necho piggy\n").unwrap();

    let report = cli_link::install(&new).unwrap();

    assert!(report.linked, "stale link re-pointed");
    assert_eq!(
        std::fs::read_link(cli_link::link_path()).unwrap(),
        new.canonicalize().unwrap()
    );
}

#[test]
fn uninstall_removes_the_link_and_the_path_block() {
    let sb = Sandbox::new();
    let sidecar = sb.fake_sidecar("piggy");
    cli_link::install(&sidecar).unwrap();

    assert!(cli_link::uninstall().unwrap(), "a link was removed");

    assert!(!cli_link::exists());
    assert!(
        !sb.profile().contains("piggy (managed PATH)"),
        "PATH block removed with the last consumer"
    );
    // Removing the link must never touch the binary it pointed at.
    assert!(sidecar.exists(), "sidecar untouched");
}

#[test]
fn uninstall_keeps_the_path_block_when_a_saver_still_needs_it() {
    let sb = Sandbox::new();
    let sidecar = sb.fake_sidecar("piggy");
    cli_link::install(&sidecar).unwrap();
    sb.install_saver_with_bin_file("rtk", "rtk");

    cli_link::uninstall().unwrap();

    assert!(!cli_link::exists(), "CLI link gone");
    assert!(
        sb.profile().contains("piggy (managed PATH)"),
        "PATH block kept: rtk still keeps a binary in ${{PIGGY_BIN}}"
    );
}

#[test]
fn uninstall_is_a_no_op_when_no_link_exists() {
    let sb = Sandbox::new();
    assert!(!cli_link::uninstall().unwrap(), "nothing to remove");
    assert!(!sb.profile().contains("piggy (managed PATH)"));
}

#[test]
fn a_dangling_link_still_counts_as_present() {
    let sb = Sandbox::new();
    let sidecar = sb.fake_sidecar("piggy");
    cli_link::install(&sidecar).unwrap();

    // The user drags Piggy.app to the Trash: the link survives, dangling.
    std::fs::remove_file(&sidecar).unwrap();

    assert!(
        cli_link::exists(),
        "dangling link is still ours to repair or remove"
    );
    assert!(cli_link::uninstall().unwrap(), "and it can be removed");
}

#[test]
fn install_fails_cleanly_when_the_sidecar_is_missing() {
    let sb = Sandbox::new();
    let missing = sb.root().join("Piggy.app/Contents/MacOS/piggy");

    let err = cli_link::install(&missing).unwrap_err();

    assert!(
        err.to_string().contains("resolving the piggy CLI"),
        "actionable error: {err}"
    );
    assert!(!cli_link::exists(), "no link left behind");
    assert!(
        !sb.profile().contains("piggy (managed PATH)"),
        "and no PATH block written for a link that was never made"
    );
}
