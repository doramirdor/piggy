//! M3 tests that need the environment sandbox (`PIGGY_HOME` / `PIGGY_CLAUDE_*`):
//! the rotation scheduler applying real toggles, the manual-override pause, the
//! no-apply-while-active guard, and the filesystem watcher's index + snapshot.
//!
//! Like the M2 engine tests, these take a global env lock and point every path
//! var at a fresh tempdir, so nothing ever touches the real `~/.claude` or
//! `~/.piggy`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use piggy_core::rotation::{self, RotationOutcome, RotationPlan};
use piggy_core::state::{PiggyState, SaverState};
use piggy_core::store::source;
use piggy_core::{Catalog, Pricing, SessionWatcher, Store};

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
        // Sandbox the shell profile: rtk's install runs `ensure_dir_on_path`,
        // which would otherwise append a PATH line to the real `~/.zshrc`.
        std::env::set_var("PIGGY_SHELL_PROFILE", dir.path().join("zshrc"));
        std::env::remove_var("PIGGY_CLAUDE_BIN");
        std::env::remove_var("PIGGY_ASSET_CACHE_DIR");
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        std::fs::create_dir_all(dir.path().join("projects")).unwrap();
        std::fs::create_dir_all(dir.path().join("piggy")).unwrap();
        Sandbox { _guard: guard, dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("piggy")
    }
    fn projects(&self) -> PathBuf {
        self.dir.path().join("projects")
    }
    fn settings_path(&self) -> PathBuf {
        self.dir.path().join("claude").join("settings.json")
    }
    fn piggy_bin(&self) -> PathBuf {
        self.home().join("bin")
    }

    fn seed_settings_from_fixture(&self, name: &str) -> Vec<u8> {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/settings")
            .join(name);
        let bytes = std::fs::read(&src).unwrap();
        std::fs::write(self.settings_path(), &bytes).unwrap();
        bytes
    }

    fn read_settings(&self) -> Vec<u8> {
        std::fs::read(self.settings_path()).unwrap()
    }

    /// Fake rtk tarball + checksums into an asset cache, so install is offline.
    fn use_fake_rtk_asset(&self) {
        let cache = self.dir.path().join("assets");
        std::fs::create_dir_all(&cache).unwrap();
        let tarball = build_rtk_tarball();
        let sha = piggy_core::settings::hash_bytes(&tarball);
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

fn build_rtk_tarball() -> Vec<u8> {
    let script = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'rtk 0.43.0-fake'; exit 0; fi\nexit 0\n";
    let data = script.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path("rtk").unwrap();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.append(&header, data).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

// ---------------------------------------------------------------------------
// Rotation plan determinism (pure, but kept here with the rest of rotation)
// ---------------------------------------------------------------------------

#[test]
fn rotation_plan_is_deterministic_and_covers_every_slot() {
    let savers = vec![
        "rtk".to_string(),
        "caveman".to_string(),
        "ponytail".to_string(),
    ];
    let plan = RotationPlan::new(savers.clone(), 0.1, true);
    assert_eq!(plan.block_len(), 10, "1/0.1 = 10 sessions per block");

    // Identical inputs → identical output.
    for pos in 0..25 {
        assert_eq!(plan.assignment_at(pos).set, plan.assignment_at(pos).set);
    }
    // Position 0 is the all-off holdout.
    let h = plan.assignment_at(0);
    assert_eq!(h.kind, rotation::SlotKind::Holdout);
    assert_eq!(h.source, source::HOLDOUT);
    assert!(h.set.values().all(|&on| !on));

    // Positions 1..=3 are single-off, one per saver, everything else on.
    for (i, s) in savers.iter().enumerate() {
        let a = plan.assignment_at(1 + i);
        assert_eq!(a.kind, rotation::SlotKind::SingleOff(s.clone()));
        assert_eq!(a.source, source::ROTATION);
        assert!(!a.set[s], "the single-off saver is off");
        for other in &savers {
            if other != s {
                assert!(a.set[other], "everything else is on");
            }
        }
    }
    // The remainder is full-on.
    let f = plan.assignment_at(4);
    assert_eq!(f.kind, rotation::SlotKind::FullOn);
    assert!(f.set.values().all(|&on| on));

    // Exactly one holdout and one single-off per saver across a block.
    let mut holdouts = 0;
    let mut single_offs = 0;
    for pos in 0..plan.block_len() {
        match plan.assignment_at(pos).kind {
            rotation::SlotKind::Holdout => holdouts += 1,
            rotation::SlotKind::SingleOff(_) => single_offs += 1,
            rotation::SlotKind::FullOn => {}
        }
    }
    assert_eq!(holdouts, 1);
    assert_eq!(single_offs, 3);

    // The pattern wraps.
    assert_eq!(plan.assignment_at(0).set, plan.assignment_at(10).set);
}

#[test]
fn manual_savers_are_excluded_from_the_rotation_set() {
    let catalog = Catalog::embedded();
    let mut state = PiggyState::default();
    state.savers.insert("rtk".into(), saver("rtk", true, None));
    state.savers.insert(
        "caveman".into(),
        saver("caveman", true, Some(source::MANUAL)),
    );
    state.savers.insert(
        "ponytail".into(),
        saver("ponytail", false, Some(source::ROTATION)),
    );

    let controlled = rotation::controlled_savers(&catalog, &state);
    assert!(
        !controlled.contains(&"caveman".to_string()),
        "a manually-toggled saver is paused (excluded from rotation)"
    );
    assert_eq!(controlled, vec!["rtk".to_string(), "ponytail".to_string()]);
}

fn saver(id: &str, enabled: bool, src: Option<&str>) -> SaverState {
    SaverState {
        id: id.to_string(),
        version: "test".into(),
        installed_at: "2026-01-01T00:00:00Z".into(),
        enabled,
        injected_hooks: BTreeMap::new(),
        installed_files: Vec::new(),
        pre_install_backup: None,
        last_toggle_source: src.map(String::from),
        config: BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Rotation tick: skips while active, then applies, then respects manual pause.
// ---------------------------------------------------------------------------

#[test]
fn rotation_skips_while_a_session_is_active() {
    let sb = Sandbox::new();
    // A freshly-written .jsonl in the projects dir → a session is "active".
    std::fs::write(sb.projects().join("live.jsonl"), b"{}\n").unwrap();

    let catalog = Catalog::embedded();
    let mut store = Store::open(&sb.home()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let outcome = rotation::tick(&catalog, &mut store, &sb.projects(), now, 600).unwrap();
    assert!(
        matches!(outcome, RotationOutcome::SkippedActive),
        "rotation must not perturb a live session"
    );
    // Cursor untouched.
    assert_eq!(store.rotation_state().unwrap().0, 0);
}

#[test]
fn rotation_applies_when_idle_then_manual_pauses_it() {
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("openbar.json");
    sb.use_fake_rtk_asset();
    let catalog = Catalog::embedded();

    // Install rtk (enabled, never manually toggled → rotation-controlled).
    piggy_core::engine::install(&catalog, "rtk").unwrap();
    assert!(sb.piggy_bin().join("rtk").exists());
    assert!(PiggyState::load().unwrap().savers["rtk"].enabled);

    // Idle: no jsonl in projects. Use a far-future "now" so any stray mtime is old.
    let mut store = Store::open(&sb.home()).unwrap();
    let far_future = 4_000_000_000u64; // year 2096
    let outcome = rotation::tick(&catalog, &mut store, &sb.projects(), far_future, 600).unwrap();

    // Block position 0 is the holdout → rtk turned off with source=holdout.
    match outcome {
        RotationOutcome::Applied { changed, .. } => {
            assert_eq!(changed, vec!["rtk".to_string()]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    let st = PiggyState::load().unwrap();
    assert!(!st.savers["rtk"].enabled, "holdout turned rtk off");
    assert_eq!(
        st.savers["rtk"].last_toggle_source.as_deref(),
        Some(source::HOLDOUT)
    );
    assert_eq!(store.rotation_state().unwrap().0, 1, "cursor advanced");
    // The rtk hook is gone from settings while off.
    let disk: serde_json::Value = serde_json::from_slice(&sb.read_settings()).unwrap();
    assert_eq!(disk["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);

    // The user now manually turns rtk back on → rotation must pause it.
    piggy_core::engine::set_enabled(&catalog, "rtk", true).unwrap();
    assert_eq!(
        PiggyState::load().unwrap().savers["rtk"]
            .last_toggle_source
            .as_deref(),
        Some(source::MANUAL)
    );

    let outcome = rotation::tick(&catalog, &mut store, &sb.projects(), far_future, 600).unwrap();
    assert!(
        matches!(outcome, RotationOutcome::NothingToRotate),
        "the only saver is manually pinned → nothing left to rotate"
    );
    // rtk untouched (still on), cursor NOT advanced.
    assert!(PiggyState::load().unwrap().savers["rtk"].enabled);
    assert_eq!(store.rotation_state().unwrap().0, 1);
}

#[test]
fn a_saver_left_off_after_the_holdout_is_restamped_not_left_tagged_holdout() {
    // Regression: `tick` used to call `set_enabled_src` only when a saver's
    // enabled state actually flipped. The holdout slot (pos 0) turns every saver
    // off; the first single-off slot (pos 1) wants savers[0] off too, so it did
    // not flip and kept `last_toggle_source = "holdout"` from the slot before.
    // `tagging::source_for` then labels that saver's row "holdout", and
    // `attribution` files ANY session carrying a holdout row into the holdout
    // baseline - so pos-1 sessions, which ran with the other savers ON, polluted
    // the all-off baseline and biased the headline toward understating savings.
    let sb = Sandbox::new();
    sb.seed_settings_from_fixture("openbar.json");
    let catalog = Catalog::embedded();

    // Three rotation-controlled savers, none a claude_plugin (so toggling stays
    // local to settings.json) and none conflicting. Ordering: sweep, rtk, cto.
    let mut state = PiggyState::default();
    for id in ["sweep", "rtk", "cto"] {
        state.savers.insert(id.into(), saver(id, true, None));
    }
    state.save().unwrap();
    assert_eq!(
        rotation::controlled_savers(&catalog, &PiggyState::load().unwrap()),
        vec!["sweep".to_string(), "rtk".to_string(), "cto".to_string()]
    );

    let mut store = Store::open(&sb.home()).unwrap();
    let far_future = 4_000_000_000u64; // idle: no jsonl in projects

    // pos 0 = holdout: every saver off, every source "holdout".
    rotation::tick(&catalog, &mut store, &sb.projects(), far_future, 600).unwrap();
    let st = PiggyState::load().unwrap();
    for id in ["sweep", "rtk", "cto"] {
        assert!(!st.savers[id].enabled, "{id} is off during the holdout");
        assert_eq!(
            st.savers[id].last_toggle_source.as_deref(),
            Some(source::HOLDOUT),
            "{id} is sourced to the holdout"
        );
    }
    assert_eq!(store.rotation_state().unwrap().0, 1);

    // pos 1 = single-off(sweep): sweep stays off, rtk and cto come back on.
    rotation::tick(&catalog, &mut store, &sb.projects(), far_future, 600).unwrap();
    let st = PiggyState::load().unwrap();
    assert!(!st.savers["sweep"].enabled, "sweep is the single-off saver");
    assert!(st.savers["rtk"].enabled && st.savers["cto"].enabled);

    // The bug: sweep did not flip, so its source was never re-stamped.
    assert_eq!(
        st.savers["sweep"].last_toggle_source.as_deref(),
        Some(source::ROTATION),
        "sweep stayed off across the slot boundary, but this is a rotation slot, \
         not a holdout - leaving it tagged 'holdout' files sessions that ran with \
         rtk and cto ON into the all-off baseline"
    );
    for id in ["rtk", "cto"] {
        assert_eq!(
            st.savers[id].last_toggle_source.as_deref(),
            Some(source::ROTATION)
        );
    }
}

// ---------------------------------------------------------------------------
// Watcher smoke test: a new jsonl is indexed and snapshot-tagged.
// ---------------------------------------------------------------------------

#[test]
fn watcher_indexes_and_tags_a_new_session() {
    let sb = Sandbox::new();
    let pricing = Pricing::embedded();

    // A saver is installed & enabled, so a new session snapshots it.
    let mut state = PiggyState::default();
    state.savers.insert("rtk".into(), saver("rtk", true, None));
    state.ensure_created_at();
    state.save().unwrap();

    // Start the watcher with a short poll interval, then let the baseline settle.
    let mut watcher =
        SessionWatcher::with_poll_interval(sb.projects(), &sb.home(), Duration::from_millis(80))
            .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    // Create a new session file (copied from a real fixture).
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic.jsonl");
    let dst = sb.projects().join("watched-sess.jsonl");
    std::fs::copy(&src, &dst).unwrap();

    // Poll for up to a few seconds; the poll watcher should report the create.
    let events = watcher.tick(Duration::from_secs(8), &pricing).unwrap();
    assert!(
        !events.is_empty(),
        "the watcher should observe the new .jsonl"
    );
    let ev = events
        .iter()
        .find(|e| e.session_id == "watched-sess")
        .expect("event for the new session");
    assert!(ev.newly_tagged, "a brand-new session gets a saver snapshot");

    // Indexed: the session is in the DB.
    assert!(watcher.store().session_count().unwrap() >= 1);
    // Tagged: the enabled rtk snapshot was recorded.
    let tags = watcher.store().session_savers("watched-sess").unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].saver_id, "rtk");
    assert!(tags[0].enabled);
    assert_eq!(tags[0].source, source::ROTATION);
}

// ---------------------------------------------------------------------------
// Agent workflow journals are not sessions and must not be indexed as one.
// ---------------------------------------------------------------------------
//
// Claude Code writes one `subagents/workflows/<run>/journal.jsonl` per workflow
// run. Session ids come from the file stem, so every one of them claimed
// `session_id = "journal"` and `INSERT OR REPLACE` silently dropped all but the
// last, leaving a phantom ("journal", project=NULL) row. A real tree had 56.
// They carry no usage, so nothing was miscounted, but a non-session should not
// be indexed as a session at all, and journals are the only basename collision
// a real tree produces (session logs are UUIDs, subagent logs are agent-<hash>).

#[test]
fn agent_workflow_journals_are_not_indexed_as_sessions() {
    let sb = Sandbox::new();
    let projects = sb.projects();

    // Two workflow runs, each with its own journal.jsonl: the colliding shape.
    for run in ["wf_44923d8c-cde", "wf_9f57aba1-4cb"] {
        let dir = projects
            .join("-Users-x-proj")
            .join("3db00708-aba4-4672-bd62-a5bd5c9d40b8")
            .join("subagents")
            .join("workflows")
            .join(run);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("journal.jsonl"),
            b"{\"type\":\"result\",\"key\":\"a\",\"result\":\"x\"}\n",
        )
        .unwrap();
    }
    // A real session log alongside them, to prove the walk still works.
    std::fs::write(
        projects.join("11111111-2222-3333-4444-555555555555.jsonl"),
        b"{\"type\":\"assistant\",\"requestId\":\"r1\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}}\n",
    )
    .unwrap();

    let mut store = Store::open(&sb.home()).unwrap();
    let pricing = Pricing::embedded();
    let rep = piggy_core::index::run_index(&mut store, &pricing, &projects, false).unwrap();

    // One session indexed: the real log. The two journals are not sessions.
    assert_eq!(
        store.session_count().unwrap(),
        1,
        "a workflow journal is not a session: indexing it creates a phantom 'journal' \
         row that every other workflow run then collides with via INSERT OR REPLACE"
    );
    assert_eq!(rep.scanned, 1, "journals are not even scanned");
    // The real session's own row survived, and it is the UUID one.
    assert!(
        store
            .session_savers("11111111-2222-3333-4444-555555555555")
            .is_ok(),
        "the real session log must still be indexed"
    );
}
