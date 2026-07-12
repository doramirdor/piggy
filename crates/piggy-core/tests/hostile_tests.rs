//! HOSTILE FIXTURE attack suite against the `settings.json` merge engine.
//!
//! Every test here feeds the engine a `settings.json` variant a well-behaved
//! optimizer (or a careless user, or another tool) might plausibly leave on
//! disk, and asserts the engine either round-trips it losslessly or refuses to
//! touch it. Assertions are written against the *data-safe* behaviour; a
//! failing assertion is therefore a finding, and its severity tracks the
//! data-loss risk it exposes.
//!
//! Two flavours of test:
//!   * pure `Value`-level (load / serialize / merge_hooks / remove_hooks on an
//!     explicit temp path — no process env), and
//!   * commit-path (the real backup → mutate → atomic-write pipeline), which
//!     needs `PIGGY_HOME` pointed at a tempdir. Those grab a per-binary env
//!     lock so they serialise among themselves.
//!
//! SAFETY: nothing here writes under the real `~/.claude` or `~/.piggy`. The
//! commit tests set `PIGGY_HOME`/`PIGGY_CLAUDE_DIR` to a `tempfile::tempdir()`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::settings::{self, merge_hooks, remove_hooks};
use piggy_core::state::PiggyState;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The exact `event -> [group]` map the engine records as injected, using an
/// absolute command so it can never accidentally equal a user's relative hook.
fn piggy_rtk_hooks() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "PreToolUse".to_string(),
        json!([{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "/tmp/piggy-abs/bin/rtk hook claude" }]
        }]),
    );
    m
}

fn tmp_settings(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.json");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A sandboxed `PIGGY_HOME`/`PIGGY_CLAUDE_DIR` for commit-path tests.
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
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        Sandbox { _guard: guard, dir }
    }
    fn settings_path(&self) -> PathBuf {
        self.dir.path().join("claude").join("settings.json")
    }
    fn backups_dir(&self) -> PathBuf {
        self.dir.path().join("piggy").join("backups")
    }
    fn seed(&self, bytes: &[u8]) {
        std::fs::write(self.settings_path(), bytes).unwrap();
    }
    fn read(&self) -> Vec<u8> {
        std::fs::read(self.settings_path()).unwrap()
    }
}

/// Does any file under `dir` (recursively) have exactly these bytes?
fn any_file_has_bytes(dir: &Path, needle: &[u8]) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if any_file_has_bytes(&p, needle) {
                return true;
            }
        } else if std::fs::read(&p).map(|b| b == needle).unwrap_or(false) {
            return true;
        }
    }
    false
}

// ===========================================================================
// GROUP A — pure Value-level round-trip attacks (no env)
// ===========================================================================

/// A hostile settings.json whose top level is a JSON *array*, not an object.
/// load() must refuse (Piggy never overwrites what it cannot model).
#[test]
fn a01_top_level_array_is_refused() {
    let (_d, p) = tmp_settings(b"[1, 2, 3]\n");
    let err = settings::load(&p).unwrap_err();
    assert!(
        err.to_string().contains("expected an object"),
        "array top-level must be rejected, got: {err}"
    );
}

/// Top level is a bare JSON string. Must be refused, not silently wrapped.
#[test]
fn a02_top_level_string_is_refused() {
    let (_d, p) = tmp_settings(b"\"just a string\"\n");
    assert!(
        settings::load(&p).is_err(),
        "string top-level must be refused"
    );
}

/// A JSON object with a duplicate top-level key. Standard JSON is ambiguous;
/// verify the engine does not panic and deterministically keeps one. (Documents
/// that the *other* value is silently dropped on any rewrite.)
#[test]
fn a03_duplicate_top_level_keys_do_not_panic() {
    let (_d, p) = tmp_settings(br#"{"theme":"light","theme":"dark"}"#);
    let loaded = settings::load(&p).expect("dup keys should parse, not panic");
    // serde keeps the last value at the first position.
    assert_eq!(loaded.value["theme"], "dark");
    let obj = loaded.value.as_object().unwrap();
    assert_eq!(
        obj.len(),
        1,
        "one of the duplicate keys is silently dropped"
    );
}

/// A high-magnitude integer beyond u64/i64 range. Without `arbitrary_precision`
/// serde parses it as f64, so a merge-engine rewrite mutates the on-disk value.
/// A data-safe engine round-trips unknown numeric config verbatim.
#[test]
fn a04_huge_integer_survives_round_trip() {
    let original = br#"{"customBudget":123456789012345678901234567890}"#;
    let (_d, p) = tmp_settings(original);
    let loaded = settings::load(&p).unwrap();
    let out = String::from_utf8(loaded.serialize(&loaded.value)).unwrap();
    assert!(
        out.contains("123456789012345678901234567890"),
        "large integer must not be reformatted to float; got: {out}"
    );
}

/// A high-precision decimal. Same class as a04 — the shortest-round-trip float
/// formatter may or may not preserve the exact literal.
#[test]
fn a05_high_precision_decimal_survives_round_trip() {
    let original = br#"{"ratio":0.12345678901234567890123}"#;
    let (_d, p) = tmp_settings(original);
    let loaded = settings::load(&p).unwrap();
    let out = String::from_utf8(loaded.serialize(&loaded.value)).unwrap();
    assert!(
        out.contains("0.12345678901234567890123"),
        "high-precision decimal must round-trip verbatim; got: {out}"
    );
}

/// `hooks` present but not an object (an array). load() only checks the *root*
/// is an object, so this parses; merge_hooks then *replaces* the array wholesale
/// — silently discarding the user's data. A safe engine preserves it.
#[test]
fn a06_hooks_as_array_is_not_clobbered_by_merge() {
    let mut v: Value = serde_json::from_str(r#"{"hooks":[{"userData":"keepme"}]}"#).unwrap();
    let before = v.clone();
    merge_hooks(&mut v, &piggy_rtk_hooks());
    assert!(
        v["hooks"]
            .as_array()
            .map(|a| a.contains(&before["hooks"][0]))
            .unwrap_or(false)
            || v.get("_preserved_hooks").is_some(),
        "user's `hooks` array was silently discarded by merge_hooks; got: {v}"
    );
}

/// A `hooks` object whose event slot is a *string* (malformed). merge_hooks
/// silently no-ops (the `if let Value::Array` guard fails), so Piggy's hook is
/// never injected — an install would appear to succeed but do nothing.
#[test]
fn a07_hook_event_as_string_still_injects_or_reports() {
    let mut v: Value = serde_json::from_str(r#"{"hooks":{"PreToolUse":"oops-a-string"}}"#).unwrap();
    merge_hooks(&mut v, &piggy_rtk_hooks());
    let injected_present = settings::hook_command_contains(&v, "PreToolUse", "rtk hook claude");
    assert!(
        injected_present,
        "merge_hooks silently dropped Piggy's hook when the event slot was a non-array; value: {v}"
    );
}

/// `null` values inside hook objects must round-trip through merge∘remove
/// without corruption (defensive: some tools emit `"timeout": null`).
#[test]
fn a08_null_values_in_hook_objects_round_trip() {
    let mut v: Value = serde_json::from_str(
        r#"{"hooks":{"PreToolUse":[{"matcher":null,"hooks":[{"type":"command","command":"x","timeout":null}]}]}}"#,
    )
    .unwrap();
    let original = v.clone();
    let injected = piggy_rtk_hooks();
    merge_hooks(&mut v, &injected);
    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1, "exactly Piggy's group removed");
    assert_eq!(
        v, original,
        "null-bearing user hook must be identity-preserved"
    );
}

/// Various matcher forms (regex-alternation, wildcard, anchored) must survive a
/// merge∘remove cycle untouched.
#[test]
fn a09_matcher_regex_forms_round_trip() {
    let mut v: Value = serde_json::from_str(
        r#"{"hooks":{"PreToolUse":[
             {"matcher":"Bash|Edit|Write","hooks":[{"type":"command","command":"a"}]},
             {"matcher":".*","hooks":[{"type":"command","command":"b"}]},
             {"matcher":"Notebook.*","hooks":[{"type":"command","command":"c"}]}
           ]}}"#,
    )
    .unwrap();
    let original = v.clone();
    let injected = piggy_rtk_hooks();
    merge_hooks(&mut v, &injected);
    assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 4);
    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1);
    assert_eq!(
        v, original,
        "regex matcher hooks must be identity-preserved"
    );
}

/// The user already hand-installed rtk with a hook BYTE-IDENTICAL to what Piggy
/// injects (same absolute path). merge appends a duplicate; remove must take
/// exactly one and leave the user with a working (identical) hook — never zero,
/// never both removed.
#[test]
fn a10_preexisting_identical_rtk_hook_no_over_removal() {
    let injected = piggy_rtk_hooks();
    // Seed the user's settings with exactly Piggy's group already present.
    let mut v = json!({ "hooks": { "PreToolUse": injected["PreToolUse"].clone() } });
    let original = v.clone();
    merge_hooks(&mut v, &injected);
    assert_eq!(
        v["hooks"]["PreToolUse"].as_array().unwrap().len(),
        2,
        "duplicate of the identical hook appended"
    );
    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1, "exactly one identical group removed");
    assert_eq!(v, original, "the user's identical rtk hook survives");
    assert!(
        settings::hook_command_contains(&v, "PreToolUse", "rtk hook claude"),
        "user still has a working rtk hook after uninstall"
    );
}

/// Two byte-identical *user* groups that are NOT Piggy's. Neither may be removed
/// on uninstall (they don't match the injected set).
#[test]
fn a11_duplicate_identical_user_hooks_are_untouched() {
    let mut v: Value = serde_json::from_str(
        r#"{"hooks":{"PreToolUse":[
             {"matcher":"Bash","hooks":[{"type":"command","command":"user-dup"}]},
             {"matcher":"Bash","hooks":[{"type":"command","command":"user-dup"}]}
           ]}}"#,
    )
    .unwrap();
    let original = v.clone();
    let injected = piggy_rtk_hooks();
    merge_hooks(&mut v, &injected);
    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1, "only Piggy's group removed");
    assert_eq!(v, original, "both identical user hooks preserved");
    assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
}

/// A ~10 MB settings.json (giant unknown-key blob) must load, merge, remove, and
/// serialize without panic or corruption.
#[test]
fn a12_ten_megabyte_settings_round_trips() {
    let blob = "a".repeat(10 * 1024 * 1024);
    let doc = json!({ "outputStyle": "Explanatory", "hugeUnknown": blob });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let (_d, p) = tmp_settings(&bytes);
    let loaded = settings::load(&p).expect("10MB file must load");
    let original = loaded.value.clone();
    let injected = piggy_rtk_hooks();
    let mut v = original.clone();
    merge_hooks(&mut v, &injected);
    remove_hooks(&mut v, &injected);
    assert_eq!(v, original, "10MB payload identity-preserved");
}

// ===========================================================================
// GROUP B — commit-path attacks (real backup → atomic-write pipeline)
// ===========================================================================

/// CRLF line endings. A "low diff noise" merge must not rewrite every line's
/// ending. The engine only tracks a trailing-newline flag, so a commit
/// normalises CRLF → LF across the whole file.
#[test]
fn b01_crlf_line_endings_preserved_on_commit() {
    let sb = Sandbox::new();
    let crlf = b"{\r\n  \"theme\": \"dark\"\r\n}\r\n";
    sb.seed(crlf);
    let mut state = PiggyState::default();
    settings::commit(&sb.settings_path(), "test", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    })
    .unwrap();
    let out = sb.read();
    assert!(
        out.windows(2).any(|w| w == b"\r\n"),
        "CRLF line endings were normalised to LF (diff-noise / EOL churn); output: {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// BOM + CRLF combined. The BOM must be stripped (good — it corrupts Claude
/// Code), the JSON must parse, and content must survive. (Pairs with b01 on the
/// CRLF question.)
#[test]
fn b02_bom_plus_crlf_content_survives() {
    let sb = Sandbox::new();
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"{\r\n  \"theme\": \"dark\"\r\n}\r\n");
    sb.seed(&bytes);
    let mut state = PiggyState::default();
    let outcome = settings::commit(&sb.settings_path(), "test", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    })
    .unwrap();
    let out = sb.read();
    assert!(
        !out.starts_with(&[0xEF, 0xBB, 0xBF]),
        "BOM must not be re-emitted"
    );
    assert!(
        outcome.warnings.iter().any(|w| w.contains("BOM")),
        "a BOM strip must be surfaced as a warning"
    );
    let disk: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        disk["theme"], "dark",
        "content preserved after BOM+CRLF strip"
    );
    assert_eq!(disk["piggy"], 1);
}

/// settings.json is a SYMLINK into the user's dotfiles repo. A safe writer keeps
/// the link intact and writes THROUGH it so the dotfiles source stays the source
/// of truth. Piggy's temp-file + rename replaces the symlink with a regular file
/// and leaves the dotfiles target stale.
#[test]
fn b03_symlinked_settings_is_written_through_not_replaced() {
    let sb = Sandbox::new();
    // Real target lives outside ~/.claude (simulating a dotfiles repo).
    let target = sb.dir.path().join("dotfiles-settings.json");
    std::fs::write(&target, b"{\n  \"theme\": \"dark\"\n}\n").unwrap();
    // Replace the seeded settings.json with a symlink to the target.
    let link = sb.settings_path();
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let mut state = PiggyState::default();
    settings::commit(&link, "test", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    })
    .unwrap();

    let still_symlink = std::fs::symlink_metadata(&link)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    let target_updated: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
    assert!(
        still_symlink,
        "settings.json is no longer a symlink after commit (rename broke the dotfiles link)"
    );
    assert_eq!(
        target_updated["piggy"], 1,
        "the write did not reach the symlink target; dotfiles source is now stale"
    );
}

/// A read-only (0444) settings.json. The commit should either fail cleanly
/// (leaving content intact) or succeed — but must never leave a truncated /
/// corrupt file. Verify the file still parses afterwards.
#[test]
#[cfg(unix)]
fn b04_readonly_settings_not_corrupted() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    sb.seed(b"{\n  \"theme\": \"dark\"\n}\n");
    std::fs::set_permissions(sb.settings_path(), std::fs::Permissions::from_mode(0o444)).unwrap();

    let mut state = PiggyState::default();
    let res = settings::commit(&sb.settings_path(), "test", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    });

    // Regardless of success/failure, the on-disk file must remain valid JSON.
    let disk = std::fs::read(sb.settings_path()).unwrap();
    let parsed: Result<Value, _> = serde_json::from_slice(&disk);
    assert!(
        parsed.is_ok(),
        "read-only commit left a non-JSON/corrupt file (commit result: {:?}); bytes: {:?}",
        res.as_ref().map(|_| "ok"),
        String::from_utf8_lossy(&disk)
    );
}

/// TOCTOU: a concurrent writer lands an edit AFTER Piggy has read the file but
/// BEFORE it writes. The mutate closure runs in exactly that window, so we use
/// it to simulate the racing write. Piggy's backup captured the *pre-race*
/// bytes, and its atomic write overwrites the race — so the concurrent content
/// exists in neither settings.json nor any backup. The builder's report claims
/// such an edit is "carried forward, never lost".
#[test]
fn b05_concurrent_write_during_commit_is_not_silently_lost() {
    let sb = Sandbox::new();
    sb.seed(b"{\n  \"theme\": \"dark\"\n}\n");
    // Establish Piggy's recorded hash with a first write.
    let mut state = PiggyState::default();
    settings::commit(&sb.settings_path(), "w1", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy1".into(), json!(1));
    })
    .unwrap();

    let racing_bytes = b"{\n  \"theme\": \"dark\",\n  \"userTypedThisManually\": \"precious\"\n}\n";
    let path = sb.settings_path();
    settings::commit(&path, "w2", &mut state, None, |v| {
        // Simulate a concurrent editor saving the file mid-commit.
        std::fs::write(&path, &racing_bytes[..]).unwrap();
        v.as_object_mut().unwrap().insert("piggy2".into(), json!(2));
    })
    .unwrap();

    let on_disk = sb.read();
    let disk_has_it = on_disk.windows(b"precious".len()).any(|w| w == b"precious");
    let backed_up = any_file_has_bytes(&sb.backups_dir(), &racing_bytes[..]);
    assert!(
        disk_has_it || backed_up,
        "the concurrent write ('precious') was lost: not on disk and not in any backup — unrecoverable"
    );
}

/// A commit onto a top-level array must refuse and leave the original bytes
/// untouched (no partial/atomic write of a wrong shape).
#[test]
fn b06_commit_on_array_refuses_and_preserves_file() {
    let sb = Sandbox::new();
    let original = b"[1, 2, 3]\n";
    sb.seed(original);
    let mut state = PiggyState::default();
    let res = settings::commit(&sb.settings_path(), "test", &mut state, None, |v| {
        v.as_object_mut().unwrap().insert("piggy".into(), json!(1));
    });
    assert!(res.is_err(), "commit onto a JSON array must error");
    assert_eq!(
        sb.read(),
        original,
        "a refused commit must leave the file byte-identical"
    );
}
