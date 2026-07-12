//! Pure merge-engine tests: hook merge/remove semantics, formatting fidelity,
//! BOM handling, unknown-key preservation, and the `merge ∘ remove == identity`
//! property over every settings fixture. None of these touch process env — they
//! operate on `Value`s and explicit temp files — so they parallelize freely.

use std::path::PathBuf;

use piggy_core::settings::{self, hook_command_contains, merge_hooks, remove_hooks};
use serde_json::{json, Map, Value};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/settings")
        .join(name)
}

/// A distinct Piggy-owned rtk hook (absolute command, so it never accidentally
/// equals a user's hook) as the `event -> [group]` map the engine records.
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

/// Every fixture that represents a parseable settings file.
const PARSEABLE_FIXTURES: &[&str] = &[
    "openbar.json",
    "minimal.json",
    "empty.json",
    "already-has-rtk.json",
    "unknown-keys.json",
    "hostile.json",
    "bom.json",
];

#[test]
fn merge_then_remove_is_identity_over_all_fixtures() {
    let injected = piggy_rtk_hooks();
    for name in PARSEABLE_FIXTURES {
        let loaded = settings::load(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let original = loaded.value.clone();

        let mut v = original.clone();
        merge_hooks(&mut v, &injected);
        assert_ne!(v, original, "{name}: merge should change the value");

        let removed = remove_hooks(&mut v, &injected);
        assert_eq!(removed, 1, "{name}: exactly one injected group removed");
        assert_eq!(v, original, "{name}: merge∘remove must equal identity");
    }
}

#[test]
fn merge_then_remove_is_identity_for_missing_file() {
    // A missing file loads as `{}`; the property must still hold.
    let missing = fixture("does-not-exist.json");
    assert!(!missing.exists());
    let loaded = settings::load(&missing).unwrap();
    assert!(!loaded.existed);
    let original = loaded.value.clone();

    let injected = piggy_rtk_hooks();
    let mut v = original.clone();
    merge_hooks(&mut v, &injected);
    remove_hooks(&mut v, &injected);
    assert_eq!(v, original);
    assert_eq!(v, json!({}));
}

#[test]
fn user_hooks_stay_first_and_untouched() {
    // openbar's six wildcard hooks must be invisible to Piggy's removal.
    let loaded = settings::load(&fixture("openbar.json")).unwrap();
    let original = loaded.value.clone();
    let injected = piggy_rtk_hooks();

    let mut v = original.clone();
    merge_hooks(&mut v, &injected);

    // The openbar PreToolUse hook is still element 0; Piggy's is appended last.
    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0], original["hooks"]["PreToolUse"][0]);
    assert!(pre[1]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains("rtk"));

    // Removing Piggy's hook leaves openbar's exactly as it was.
    remove_hooks(&mut v, &injected);
    assert_eq!(v, original);
}

#[test]
fn already_has_rtk_keeps_user_rtk_hook() {
    // A user who hand-installed rtk has command "rtk hook claude"; Piggy's owned
    // hook is a different (absolute) command, so removal never takes the user's.
    let loaded = settings::load(&fixture("already-has-rtk.json")).unwrap();
    let original = loaded.value.clone();
    let injected = piggy_rtk_hooks();

    let mut v = original.clone();
    merge_hooks(&mut v, &injected);
    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "both the user's and Piggy's rtk hook present");

    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1);
    // The user's manual rtk hook survives.
    assert_eq!(v, original);
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "rtk hook claude"
    );
}

#[test]
fn removing_piggy_only_hook_prunes_empty_arrays_and_object() {
    // Start from minimal `{}`, merge, remove: the `hooks` scaffolding Piggy
    // created is fully pruned back to `{}`.
    let mut v = json!({});
    let injected = piggy_rtk_hooks();
    merge_hooks(&mut v, &injected);
    assert!(v["hooks"]["PreToolUse"].is_array());
    remove_hooks(&mut v, &injected);
    assert_eq!(v, json!({}), "no empty `hooks` residue left behind");
}

#[test]
fn bom_is_stripped_with_warning_and_never_reemitted() {
    let loaded = settings::load(&fixture("bom.json")).unwrap();
    assert!(loaded.had_bom, "BOM detected");
    assert!(
        loaded.warnings.iter().any(|w| w.contains("BOM")),
        "a BOM warning is surfaced"
    );
    // Value parsed fine despite the BOM.
    assert_eq!(loaded.value["theme"], "light");
    // Re-serialization must not put the BOM back.
    let bytes = loaded.serialize(&loaded.value);
    assert!(
        !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "serialized output must not re-emit a BOM"
    );
}

#[test]
fn unknown_top_level_keys_round_trip_verbatim() {
    let loaded = settings::load(&fixture("unknown-keys.json")).unwrap();
    // Unknown keys are present after load.
    assert!(loaded.value.get("aFutureFieldPiggyHasNeverSeen").is_some());
    assert_eq!(loaded.value["outputStyle"], "Explanatory");

    // A merge+remove cycle leaves the value identical, unknown keys and all.
    let injected = piggy_rtk_hooks();
    let mut v = loaded.value.clone();
    merge_hooks(&mut v, &injected);
    remove_hooks(&mut v, &injected);
    assert_eq!(v, loaded.value);

    // Key order is preserved (preserve_order): $schema first, trailing key last.
    let text = String::from_utf8(loaded.serialize(&v)).unwrap();
    let schema_pos = text.find("$schema").unwrap();
    let trailing_pos = text.find("zzz_trailing_unknown").unwrap();
    assert!(schema_pos < trailing_pos, "original key order preserved");
}

#[test]
fn hostile_unicode_and_nesting_survive_round_trip() {
    let loaded = settings::load(&fixture("hostile.json")).unwrap();
    let original = loaded.value.clone();
    // Deeply nested unicode is intact.
    assert_eq!(
        original["hooks"]["PostToolUse"][0]["hooks"][0]["extra"]["level1"]["level2"]["level3"][1],
        "🐷"
    );
    let injected = piggy_rtk_hooks();
    let mut v = original.clone();
    merge_hooks(&mut v, &injected);
    // Two identical-looking user Bash groups exist; Piggy appends a third,
    // distinct one and removes exactly it.
    assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 3);
    let removed = remove_hooks(&mut v, &injected);
    assert_eq!(removed, 1);
    assert_eq!(v, original);
}

#[test]
fn serialize_uses_two_space_indent_and_preserves_trailing_newline() {
    let loaded = settings::load(&fixture("minimal.json")).unwrap();
    assert!(loaded.trailing_newline);
    let mut v = json!({});
    merge_hooks(&mut v, &piggy_rtk_hooks());
    let text = String::from_utf8(loaded.serialize(&v)).unwrap();
    assert!(text.contains("\n  \"hooks\""), "2-space top-level indent");
    assert!(
        text.contains("\n    \"PreToolUse\""),
        "4-space nested indent"
    );
    assert!(text.ends_with("\n"), "trailing newline preserved");
    assert!(!text.ends_with("\n\n"), "exactly one trailing newline");
}

#[test]
fn empty_file_loads_as_empty_object() {
    let loaded = settings::load(&fixture("empty.json")).unwrap();
    assert!(loaded.existed);
    assert_eq!(loaded.value, json!({}));
}

#[test]
fn hook_command_contains_matches_only_the_right_event() {
    let loaded = settings::load(&fixture("already-has-rtk.json")).unwrap();
    assert!(hook_command_contains(&loaded.value, "PreToolUse", "rtk"));
    assert!(!hook_command_contains(&loaded.value, "PreToolUse", "nope"));
    assert!(!hook_command_contains(&loaded.value, "PostToolUse", "rtk"));
}

#[test]
fn non_object_top_level_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.json");
    std::fs::write(&p, "[1, 2, 3]\n").unwrap();
    let err = settings::load(&p).unwrap_err();
    assert!(err.to_string().contains("expected an object"));
}
