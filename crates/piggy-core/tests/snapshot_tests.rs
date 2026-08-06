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

    let record = snapshots::snapshot(&path, "advice-1", &mut state).unwrap();
    assert_eq!(state.file_snapshots.len(), 1, "recorded in state.json");
    assert_eq!(record.advice_id, "advice-1");
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
    snapshots::snapshot(&path, "fix", &mut state).unwrap();
    let fixed = b"# Rules\n\n- Never use an emoji\n- Prefer plain prose\n";
    snapshots::write_atomic(&path, fixed).unwrap();
    snapshots::snapshot(&path, "trim", &mut state).unwrap();
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

    snapshots::snapshot(&good, "a1", &mut state).unwrap();
    let lost = snapshots::snapshot(&broken, "a1", &mut state).unwrap();
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

/// One copy per apply, and now one more per restore that finds the file edited,
/// with nothing ever deleting either: the directory grew for ever. It is bounded
/// the way `settings.json`'s history is, except that a copy some record still
/// names is the only thing standing between an Undo and the original bytes, so
/// it is never a candidate however old it gets.
#[test]
fn spent_copies_are_pruned_and_a_copy_a_record_still_names_is_not() {
    let sb = Sandbox::new();
    let path = sb.file("proj/CLAUDE.md", ORIGINAL);
    let mut state = PiggyState::default();

    // The oldest copy in the directory, and the one that must survive all of it.
    let live = snapshots::snapshot(&path, "advice-1", &mut state).unwrap();

    // Sixty applies whose records have since been restored and dropped from the
    // ledger, each leaving a copy nobody points at any more.
    let spent: Vec<String> = (0..60)
        .map(|_| {
            let rec = snapshots::snapshot(&path, "restored", &mut state).unwrap();
            state.file_snapshots.pop();
            rec.backup
        })
        .collect();

    assert!(Path::new(&live.backup).exists(), "the live copy stays");
    assert!(
        !Path::new(&spent[0]).exists(),
        "the oldest spent copy goes first"
    );
    assert!(
        Path::new(&spent[59]).exists(),
        "the recent ones are still a recovery trail"
    );
    let kept = std::fs::read_dir(snapshots::files_backup_dir())
        .unwrap()
        .count();
    assert!(
        (50..=52).contains(&kept),
        "sixty-one applies must leave about fifty copies, not sixty-one: {kept}"
    );
}

/// The copy a restore takes of the user's own bytes is the only copy of that
/// work. Pruning bounds the directory by deleting copies no record points at, and
/// a `file_backups` record is a record: it is swept for live names alongside
/// `file_snapshots`, so splitting the ledgers could not quietly make somebody's
/// writing prunable.
#[test]
fn pruning_never_deletes_the_copy_of_the_users_own_work() {
    let sb = Sandbox::new();
    let path = sb.file("proj/CLAUDE.md", ORIGINAL);
    let mut state = PiggyState::default();

    // Apply, the user writes over it, then undo: the undo copies their bytes
    // aside before it puts the original back.
    snapshots::snapshot_and_write(&path, TRIMMED, "advice-1", &mut state).unwrap();
    let theirs = b"# Rules\n\n- Never use an emoji\n- And one of mine\n";
    snapshots::write_atomic(&path, theirs).unwrap();
    let records = state.file_snapshots.clone();
    assert_eq!(snapshots::restore(&records, &mut state).restored, 1);
    assert_eq!(
        state.file_backups.len(),
        1,
        "their work went to the other ledger"
    );
    let theirs_copy = state.file_backups[0].backup.clone();
    assert_eq!(std::fs::read(&theirs_copy).unwrap(), theirs);

    // Sixty applies whose records have since been restored and dropped, which is
    // what drives pruning past its ceiling.
    for _ in 0..60 {
        snapshots::snapshot(&path, "restored", &mut state).unwrap();
        state.file_snapshots.pop();
    }

    assert!(
        Path::new(&theirs_copy).exists(),
        "their work is not a spent copy, however old it gets"
    );
}

/// `file_snapshots` used to hold both kinds of record, told apart only by whether
/// `advice_id` was set. A state file written then must come back with the id-less
/// ones moved into `file_backups`, where no consumer has to remember the
/// convention, rather than sitting in the list every restore walks.
#[test]
fn an_id_less_record_moves_to_the_backup_ledger_on_load() {
    let sb = Sandbox::new();
    let path = sb.file(
        "state.json",
        br#"{
          "version": 1,
          "file_snapshots": [
            {"path":"/p/CLAUDE.md","backup":"/b/a.bak","advice_id":"advice-1",
             "after_hash":"deadbeef","applied_at":"2026-01-02T03:04:05Z"},
            {"path":"/p/CLAUDE.md","backup":"/b/theirs.bak","advice_id":null,
             "applied_at":"2026-01-03T03:04:05Z"},
            {"path":"/p/rules.md","backup":"/b/older.bak",
             "applied_at":"2026-01-04T03:04:05Z"}
          ]
        }"#,
    );

    let state = PiggyState::load_from(&path).unwrap();
    assert_eq!(state.file_snapshots.len(), 1, "only the real edit restores");
    assert_eq!(state.file_snapshots[0].advice_id, "advice-1");

    // Both an explicit null and an absent key are the old spelling of "not an
    // edit of Piggy's", and both keep their path and their copy.
    assert_eq!(state.file_backups.len(), 2);
    assert_eq!(state.file_backups[0].backup, "/b/theirs.bak");
    assert_eq!(state.file_backups[0].taken_at, "2026-01-03T03:04:05Z");
    assert_eq!(state.file_backups[1].path, "/p/rules.md");

    // And the move is durable: what was written back does not migrate again.
    let round = sb.dir.path().join("round.json");
    state.save_to(&round).unwrap();
    let again = PiggyState::load_from(&round).unwrap();
    assert_eq!(again.file_snapshots, state.file_snapshots);
    assert_eq!(again.file_backups, state.file_backups);
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
