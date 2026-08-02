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
    // Curated v1 savers use only known step kinds. Headroom is installable now
    // that it ships venv+wrapper steps (require_python / create_venv /
    // pip_install / write_launcher). token-optimizer is a claude_plugin saver
    // wired with require_binary + marketplace add/install.
    for id in [
        "rtk",
        "caveman",
        "ponytail",
        "sweep",
        "headroom",
        "token-optimizer",
        "barber",
        "nadir-route",
    ] {
        assert!(
            c.get(id).unwrap().installable().is_ok(),
            "{id} should be installable"
        );
    }
    // Deferred entries carry placeholder steps (todo_v1_1 / todo_v2) → refused.
    for id in ["cto", "context-mode", "nadirclaw"] {
        assert!(
            c.get(id).unwrap().installable().is_err(),
            "{id} should be refused (catalog newer than app / deferred)"
        );
    }
    // Listed-only entries have no steps → shown in Discover, never installable.
    // boost: mechanically an rtk-style hook, but excluded upstream (auto-allows
    // Bash + telemetry with no opt-out) — see its exclusionReason.
    for id in ["token-optimizer-mcp", "boost"] {
        assert!(
            !c.get(id).unwrap().has_install_steps(),
            "{id} is listed-only (no install steps)"
        );
    }
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
    for id in [
        "rtk",
        "caveman",
        "ponytail",
        "sweep",
        "headroom",
        "token-optimizer",
        "barber",
        "nadir-route",
    ] {
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
fn nadir_route_is_pinned_to_the_file_the_site_tells_users_to_curl() {
    // getnadir.com/skill documents exactly one install:
    //   mkdir -p ~/.claude/skills/nadir-route &&
    //   curl -fsSL https://getnadir.com/skills/nadir-route/SKILL.md -o …/SKILL.md
    // Piggy must land the same path from the same URL, and — because that URL
    // is unversioned — must carry a sha256, or an upstream edit would install
    // whatever Nadir last published straight into ~/.claude.
    let c = Catalog::embedded();
    let e = c.get("nadir-route").unwrap();
    assert_eq!(e.install_type, "claude_skill");
    let dl = e
        .install
        .steps
        .iter()
        .find(|s| step_kind(s) == "download_file")
        .expect("nadir-route installs one downloaded file");
    assert_eq!(
        dl["url"], "https://getnadir.com/skills/nadir-route/SKILL.md",
        "the URL the site publishes"
    );
    assert_eq!(dl["dest"], "${CLAUDE_SKILLS}/nadir-route/SKILL.md");
    assert_eq!(
        dl["sha256"].as_str().map(str::len),
        Some(64),
        "an unversioned URL must be hash-pinned"
    );
    assert_eq!(
        e.skill_file(),
        Some("${CLAUDE_SKILLS}/nadir-route/SKILL.md"),
        "the engine's on/off rename keys on this"
    );
    // The router and the skill both decide the same thing; running both makes
    // either one's measurement unreadable.
    assert!(e.conflicts_with.iter().any(|x| x == "nadirclaw"));
}

#[test]
fn skill_file_is_exposed_for_skill_savers_only() {
    let c = Catalog::embedded();
    for id in ["rtk", "caveman", "barber", "headroom", "sweep"] {
        assert_eq!(c.get(id).unwrap().skill_file(), None, "{id}");
    }
}

#[test]
fn launch_command_is_exposed_for_wrapper_savers_only() {
    let c = Catalog::embedded();
    assert_eq!(
        c.get("headroom").unwrap().launch_command().as_deref(),
        Some("piggy-claude")
    );
    for id in ["rtk", "caveman", "ponytail", "sweep", "token-optimizer"] {
        assert_eq!(c.get(id).unwrap().launch_command(), None, "{id}");
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
