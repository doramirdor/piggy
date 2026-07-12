//! Catalog parsing, installability gating, and a regression lock on the
//! live-verified rtk hook shape.

use piggy_core::registry::{step_kind, Catalog, KNOWN_STEP_KINDS};

#[test]
fn embedded_catalog_parses_and_has_v1_savers() {
    let c = Catalog::embedded();
    assert!(c.registry_version >= 1);
    for id in ["rtk", "caveman", "ponytail", "sweep"] {
        assert!(c.get(id).is_some(), "{id} present in catalog");
    }
}

#[test]
fn v1_savers_are_installable_and_deferred_ones_are_not() {
    let c = Catalog::embedded();
    // Curated v1 savers use only known step kinds.
    for id in ["rtk", "caveman", "ponytail", "sweep"] {
        assert!(
            c.get(id).unwrap().installable().is_ok(),
            "{id} should be installable"
        );
    }
    // Deferred entries carry placeholder steps (todo_v1_1 / todo_v2) → refused.
    for id in ["cto", "context-mode", "headroom"] {
        assert!(
            c.get(id).unwrap().installable().is_err(),
            "{id} should be refused (catalog newer than app / deferred)"
        );
    }
    // A listed-only entry with no steps is not installable-with-steps.
    let mcp = c.get("token-optimizer-mcp").unwrap();
    assert!(!mcp.has_install_steps());
}

#[test]
fn rtk_hook_matches_the_verified_v0_43_0_shape() {
    // Locked to what `rtk init -g --auto-patch` wrote on 2026-07-12:
    // matcher "Bash", command "rtk hook claude", and NO timeout field. Piggy
    // stores it with the ${PIGGY_BIN} placeholder for the pinned binary path.
    let c = Catalog::embedded();
    let rtk = c.get("rtk").unwrap();
    let merge = rtk
        .install
        .steps
        .iter()
        .find(|s| step_kind(s) == "merge_hooks")
        .expect("rtk has a merge_hooks step");
    let group = &merge["hooks"]["PreToolUse"][0];
    assert_eq!(group["matcher"], "Bash");
    let handler = &group["hooks"][0];
    assert_eq!(handler["type"], "command");
    assert_eq!(handler["command"], "${PIGGY_BIN}/rtk hook claude");
    assert!(
        handler.get("timeout").is_none(),
        "verified rtk shape has no timeout field"
    );
}

#[test]
fn rtk_asset_names_are_the_real_release_filenames() {
    // Regression lock: the real v0.43.0 assets carry no version in the filename.
    let c = Catalog::embedded();
    let assets = &c.get("rtk").unwrap().source.assets;
    assert_eq!(
        assets.get("darwin-aarch64").map(String::as_str),
        Some("rtk-aarch64-apple-darwin.tar.gz")
    );
    assert_eq!(
        assets.get("darwin-x86_64").map(String::as_str),
        Some("rtk-x86_64-apple-darwin.tar.gz")
    );
}

#[test]
fn every_v1_step_kind_is_known() {
    let c = Catalog::embedded();
    for id in ["rtk", "caveman", "ponytail", "sweep"] {
        let e = c.get(id).unwrap();
        for kind in e.install.kinds().iter().chain(e.uninstall.kinds().iter()) {
            assert!(
                KNOWN_STEP_KINDS.contains(&kind.as_str()),
                "{id}: step '{kind}' must be a known kind"
            );
        }
    }
}

#[test]
fn ordered_sorts_by_ordering_field() {
    let c = Catalog::embedded();
    let ids: Vec<&str> = c.ordered().iter().map(|e| e.id.as_str()).collect();
    // sweep (ordering 5) precedes rtk (10) precedes caveman (50) precedes ponytail (60).
    let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap();
    assert!(pos("sweep") < pos("rtk"));
    assert!(pos("rtk") < pos("caveman"));
    assert!(pos("caveman") < pos("ponytail"));
}
