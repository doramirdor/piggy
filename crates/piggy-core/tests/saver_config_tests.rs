//! Per-saver configuration tests (catalog `configOptions` + `json_field`
//! apply). Env-mutating, so every test takes the global lock and sandboxes
//! `PIGGY_XDG_CONFIG` / `PIGGY_HOME` at a tempdir — the same discipline as the
//! M2 engine tests: nothing ever touches the real `~/.config` or `~/.piggy`.

use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::registry::Catalog;
use piggy_core::saver_config::{get_config, set_config};
use piggy_core::{config, PiggyState};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

struct Sandbox {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

fn sandbox() -> Sandbox {
    let guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("PIGGY_XDG_CONFIG", dir.path().join("xdg"));
    std::env::set_var("PIGGY_HOME", dir.path().join("piggy"));
    Sandbox {
        _guard: guard,
        _dir: dir,
    }
}

#[test]
fn caveman_exposes_intensity_option_with_default_full() {
    let _sb = sandbox();
    let catalog = Catalog::embedded();
    let state = PiggyState::default();
    let opts = get_config(&catalog, &state, "caveman").unwrap();
    assert_eq!(opts.len(), 1);
    assert_eq!(opts[0].option.key, "defaultMode");
    assert_eq!(opts[0].current, "full");
    let values: Vec<&str> = opts[0]
        .option
        .choices
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(values, vec!["lite", "full", "ultra"]);
}

#[test]
fn other_curated_savers_have_no_options() {
    let _sb = sandbox();
    let catalog = Catalog::embedded();
    let state = PiggyState::default();
    for id in ["rtk", "ponytail", "headroom", "sweep"] {
        assert!(
            get_config(&catalog, &state, id).unwrap().is_empty(),
            "{id} should expose no options in v1"
        );
    }
}

#[test]
fn set_config_writes_json_field_and_preserves_other_fields() {
    let _sb = sandbox();
    let dir = config::xdg_config_dir().join("caveman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        r#"{"defaultMode":"full","other":{"keep":true}}"#,
    )
    .unwrap();

    let catalog = Catalog::embedded();
    let opts = set_config(&catalog, "caveman", "defaultMode", "ultra").unwrap();
    assert_eq!(opts[0].current, "ultra");

    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
    assert_eq!(doc["defaultMode"], "ultra");
    assert_eq!(doc["other"]["keep"], true, "unrelated fields preserved");
}

#[test]
fn set_config_creates_missing_file_and_rejects_bad_values() {
    let _sb = sandbox();
    let catalog = Catalog::embedded();
    assert!(set_config(&catalog, "caveman", "defaultMode", "shouty").is_err());
    let opts = set_config(&catalog, "caveman", "defaultMode", "lite").unwrap();
    assert_eq!(opts[0].current, "lite");
    assert!(config::xdg_config_dir()
        .join("caveman/config.json")
        .exists());
}

#[test]
fn refuses_to_clobber_non_object_json() {
    let _sb = sandbox();
    let dir = config::xdg_config_dir().join("caveman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), "[1,2,3]").unwrap();
    let catalog = Catalog::embedded();
    assert!(set_config(&catalog, "caveman", "defaultMode", "lite").is_err());
    assert_eq!(
        std::fs::read_to_string(dir.join("config.json")).unwrap(),
        "[1,2,3]",
        "original content untouched"
    );
}

#[test]
fn on_disk_value_wins_over_remembered_choice() {
    let _sb = sandbox();
    let catalog = Catalog::embedded();
    set_config(&catalog, "caveman", "defaultMode", "ultra").unwrap();
    // User hand-edits the saver's own config afterwards.
    let path = config::xdg_config_dir().join("caveman/config.json");
    std::fs::write(&path, r#"{"defaultMode":"lite"}"#).unwrap();
    let state = PiggyState::load().unwrap_or_default();
    let opts = get_config(&catalog, &state, "caveman").unwrap();
    assert_eq!(opts[0].current, "lite", "the file on disk is the truth");
}
