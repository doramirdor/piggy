//! File-snapshot tests: back up, edit, put back byte for byte.
//!
//! These read `PIGGY_HOME` (the snapshot directory hangs off it), and env is
//! process-global, so every test takes a lock and points it at a fresh tempdir.
//! Nothing here ever touches a real `~/.piggy`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::snapshots::{self, Conflict};
use piggy_core::state::PiggyState;

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
        Sandbox { _guard: guard, dir }
    }

    /// A file under the sandbox with the given content.
    fn file(&self, name: &str, content: &[u8]) -> PathBuf {
        let p = self.dir.path().join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }
}

/// A CLAUDE.md with a trailing newline and non-ASCII content, so a round trip
/// that is only *nearly* faithful shows up.
const ORIGINAL: &[u8] =
    b"# Rules\n\n- Never use an emoji\n- Prefer plain prose\n- Keep it under 200 lines \xc2\xa9\n";
const TRIMMED: &[u8] = b"# Rules\n\n- Never use an emoji\n";

#[test]
fn a_snapshot_restores_the_file_byte_for_byte() {
    let sb = Sandbox::new();
    let path = sb.file("proj/CLAUDE.md", ORIGINAL);
    let mut state = PiggyState::default();

    let record = snapshots::snapshot(&path, Some("advice-1"), &mut state).unwrap();
    assert_eq!(state.file_snapshots.len(), 1, "recorded in state.json");
    assert_eq!(record.advice_id.as_deref(), Some("advice-1"));
    assert!(
        Path::new(&record.backup).starts_with(snapshots::files_backup_dir()),
        "backups live under <piggy_home>/backups/files"
    );
    assert!(record.backup.ends_with(".bak"));

    // Apply the trim.
    snapshots::write_atomic(&path, TRIMMED).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), TRIMMED);

    // Undo.
    let records = state.file_snapshots.clone();
    let outcome = snapshots::restore(&records, &mut state);
    assert_eq!(outcome.restored, 1);
    assert!(outcome.failures.is_empty());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        ORIGINAL,
        "restore must be byte-identical, not merely equivalent"
    );

    // The state round-trips through state.json, so an Undo survives a restart.
    let state_path = sb.dir.path().join("state.json");
    state.save_to(&state_path).unwrap();
    let reloaded = PiggyState::load_from(&state_path).unwrap();
    assert_eq!(reloaded.file_snapshots, state.file_snapshots);
}

#[test]
fn an_old_state_file_without_the_field_still_loads() {
    let sb = Sandbox::new();
    // Every state.json written before M5. A new Vec field that is not
    // defaulted turns an upgrade into "Piggy forgot everything it installed".
    let path = sb.file(
        "state.json",
        br#"{"version":1,"savers":{},"sweep_disabled":[],"backups":[]}"#,
    );
    let state = PiggyState::load_from(&path).unwrap();
    assert!(state.file_snapshots.is_empty());
}

#[test]
fn restoring_the_whole_list_undoes_every_edit_back_to_the_original() {
    let sb = Sandbox::new();
    let path = sb.file("proj/CLAUDE.md", ORIGINAL);
    let mut state = PiggyState::default();

    // Two applies against the same file: a dead-reference fix, then a trim.
    snapshots::snapshot(&path, Some("fix"), &mut state).unwrap();
    let fixed = b"# Rules\n\n- Never use an emoji\n- Prefer plain prose\n";
    snapshots::write_atomic(&path, fixed).unwrap();
    snapshots::snapshot(&path, Some("trim"), &mut state).unwrap();
    snapshots::write_atomic(&path, TRIMMED).unwrap();

    let records = state.file_snapshots.clone();
    let outcome = snapshots::restore(&records, &mut state);
    assert_eq!(outcome.restored, 2);
    assert!(outcome.failures.is_empty());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        ORIGINAL,
        "restoring everything lands on the content Piggy first found, not an intermediate"
    );
}

#[test]
fn a_failed_restore_names_the_file_and_the_others_still_land() {
    let sb = Sandbox::new();
    let good = sb.file("proj/CLAUDE.md", ORIGINAL);
    let broken = sb.file("proj/.claude/rules/style.md", b"# Style\n");
    let mut state = PiggyState::default();

    snapshots::snapshot(&good, Some("a1"), &mut state).unwrap();
    let lost = snapshots::snapshot(&broken, Some("a1"), &mut state).unwrap();
    snapshots::write_atomic(&good, TRIMMED).unwrap();
    snapshots::write_atomic(&broken, b"").unwrap();

    // The backup is the only copy of the original bytes. Losing it must be
    // reported, not counted as a restore that happened.
    std::fs::remove_file(&lost.backup).unwrap();

    let records = state.file_snapshots.clone();
    let outcome = snapshots::restore(&records, &mut state);
    assert_eq!(outcome.restored, 1, "the healthy item still came back");
    assert_eq!(outcome.failures.len(), 1);
    let f = &outcome.failures[0];
    assert_eq!(f.path, broken.to_string_lossy());
    assert!(
        f.reason.contains("style.md"),
        "the reason must name the file: {}",
        f.reason
    );
    assert_eq!(std::fs::read(&good).unwrap(), ORIGINAL);
    assert_eq!(std::fs::read(&broken).unwrap(), b"", "still not restored");
}

#[test]
fn an_apply_refuses_a_file_that_moved_since_the_draft() {
    let sb = Sandbox::new();
    let path = sb.file("proj/CLAUDE.md", ORIGINAL);
    let drafted_against = piggy_core::settings::hash_bytes(ORIGINAL);

    // Unchanged: the apply may proceed.
    snapshots::check_unchanged(&path, &drafted_against).expect("untouched file is no conflict");

    // Touching a file without editing it is not a conflict: the gate is the
    // content, not the mtime.
    std::fs::write(&path, ORIGINAL).unwrap();
    snapshots::check_unchanged(&path, &drafted_against)
        .expect("a rewrite of identical bytes is fine");

    // The user edited it while the draft sat in the sheet.
    std::fs::write(&path, b"# Rules\n\n- Actually I like emoji\n").unwrap();
    match snapshots::check_unchanged(&path, &drafted_against) {
        Err(Conflict::Changed { path: p, expected, actual }) => {
            assert_eq!(p, path.to_string_lossy());
            assert_eq!(expected, drafted_against);
            assert_ne!(actual, drafted_against);
        }
        other => panic!("expected a Changed conflict, got {other:?}"),
    }

    // And a file that is gone is its own case, not a mystery hash mismatch.
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        snapshots::check_unchanged(&path, &drafted_against),
        Err(Conflict::Missing { .. })
    ));
}

#[cfg(unix)]
#[test]
fn an_atomic_write_keeps_the_files_permissions_and_writes_through_a_symlink() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    let real = sb.file("dotfiles/CLAUDE.md", ORIGINAL);
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();

    // A CLAUDE.md managed by stow/chezmoi: the file in the project is a link
    // into the dotfiles repo, and the edit belongs in the tracked source.
    let link = sb.dir.path().join("proj/CLAUDE.md");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    snapshots::write_atomic(&link, TRIMMED).unwrap();

    assert!(
        std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link must survive its own edit"
    );
    assert_eq!(std::fs::read(&real).unwrap(), TRIMMED);
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640,
        "an atomic write must not widen or narrow the file's mode"
    );
}
