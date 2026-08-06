//! Advice-engine tests: the five generators, the id, and the apply/undo pairs.
//!
//! Everything here reads Piggy's path env vars (`PIGGY_HOME`,
//! `PIGGY_CLAUDE_DIR`, `PIGGY_CLAUDE_JSON`), and env is process-global, so every
//! test takes the same global lock the M2 engine tests use and points every path
//! at a fresh tempdir. Nothing here ever touches a real `~/.claude` or
//! `~/.piggy`.
//!
//! The generators are pure, but the interesting properties are about the *round
//! trip* - what is written, what comes back, and what survives a user editing
//! the same file in between - so most of these drive the real
//! `generate -> apply -> undo` path against fixture configs on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::advice::{self, ActionKind, Candidate, GenerateOptions, Params};
use piggy_core::snapshots::Conflict;
use piggy_core::state::{PiggyState, SaverState};
use piggy_core::store::{advice_status, source, SaverTag};
use piggy_core::{engine, Catalog, ModelTokens, Pricing, SessionParse, Store};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
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
        std::env::set_var("PIGGY_SHELL_PROFILE", dir.path().join("zshrc"));
        std::env::remove_var("PIGGY_CLAUDE_BIN");
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        Sandbox { _guard: guard, dir }
    }

    fn claude_dir(&self) -> PathBuf {
        self.dir.path().join("claude")
    }
    fn claude_json(&self) -> PathBuf {
        self.dir.path().join("claude.json")
    }
    fn store(&self) -> Store {
        Store::open(&self.dir.path().join("piggy")).unwrap()
    }

    /// A project directory under the sandbox, created.
    fn project(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_claude_json(&self, value: &Value) {
        std::fs::write(
            self.claude_json(),
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }

    fn read_claude_json(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.claude_json()).unwrap()).unwrap()
    }

    fn write(&self, path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
}

/// Now, in the format the store compares cutoffs against.
fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// One session with a project, tool calls, and optional model tokens.
#[allow(clippy::too_many_arguments)]
fn seed_session(
    store: &mut Store,
    id: &str,
    project: &str,
    tools: &[(&str, u64)],
    turns: u64,
    input: u64,
    output: u64,
    cache_read: u64,
) {
    let mut models = BTreeMap::new();
    if input + output + cache_read > 0 {
        models.insert(
            "claude-sonnet-4-5".to_string(),
            ModelTokens {
                input_tokens: input,
                output_tokens: output,
                cache_creation_tokens: 0,
                cache_creation_1h_tokens: 0,
                cache_read_tokens: cache_read,
            },
        );
    }
    let ts = now();
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some(project.to_string()),
        git_branch: None,
        first_ts: Some(ts.clone()),
        last_ts: Some(ts),
        models,
        n_assistant_msgs: turns,
        n_user_msgs: turns,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: tools.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        context: BTreeMap::new(),
        tasks: Default::default(),
        parse_errors: 0,
    };
    store
        .upsert_session(
            &parse,
            &Pricing::embedded(),
            &format!("/logs/{id}.jsonl"),
            1,
            1,
        )
        .unwrap();
}

/// A saver in the ledger, installed and at the given on/off state.
fn install(state: &mut PiggyState, id: &str, enabled: bool) {
    state.savers.insert(
        id.to_string(),
        SaverState {
            id: id.to_string(),
            version: "test".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            enabled,
            injected_hooks: BTreeMap::new(),
            installed_files: Vec::new(),
            pre_install_backup: None,
            last_toggle_source: None,
            manual_enabled: None,
            config: BTreeMap::new(),
        },
    );
}

fn generate(store: &mut Store, state: &PiggyState) -> Vec<Candidate> {
    let catalog = Catalog::embedded();
    let pricing = Pricing::embedded();
    let opts = GenerateOptions::new(&catalog, &pricing, state);
    advice::generate(store, &opts).unwrap()
}

fn of_kind(candidates: &[Candidate], kind: ActionKind) -> Vec<&Candidate> {
    candidates.iter().filter(|c| c.kind == kind).collect()
}

fn one_of_kind(candidates: &[Candidate], kind: ActionKind) -> &Candidate {
    let found = of_kind(candidates, kind);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one {} candidate, got {:?}",
        kind.as_str(),
        candidates
            .iter()
            .map(|c| (c.kind.as_str(), c.title.as_str()))
            .collect::<Vec<_>>()
    );
    found[0]
}

/// A paragraph long enough for the duplicate detector to care about (its floor
/// is 80 normalized bytes), carrying no path-shaped token.
const SHARED_BLOCK: &str = "Always answer in plain prose and never reach for an emoji, \
because the reader is a person who is skimming and every extra glyph costs them time.";

// ---------------------------------------------------------------------------
// Generator: ServerDisable
// ---------------------------------------------------------------------------

#[test]
fn an_unused_server_becomes_a_candidate_and_a_project_configured_one_does_not() {
    let sb = Sandbox::new();
    let proj = sb.project("proj");
    let proj_s = proj.to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": {
            "usedsrv": { "command": "npx", "args": ["used"] },
            "idlesrv": { "command": "npx", "args": ["idle"] },
            "vendored": { "command": "npx", "args": ["vendored"] }
        },
        "projects": {}
    }));
    // `vendored` is checked into the project's own repo, so however quiet the
    // window is, Piggy must not call it unused.
    sb.write(
        &proj.join(".mcp.json"),
        &json!({ "mcpServers": { "vendored": { "command": "npx" } } }).to_string(),
    );

    let mut store = sb.store();
    seed_session(
        &mut store,
        "s1",
        &proj_s,
        &[("mcp__usedsrv__go", 4)],
        5,
        0,
        0,
        0,
    );

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let disables = of_kind(&candidates, ActionKind::ServerDisable);
    let targets: Vec<&str> = disables.iter().map(|c| c.target.as_str()).collect();
    assert_eq!(
        targets,
        vec!["idlesrv (user scope)"],
        "only the idle, non-vendored server is proposed for removal"
    );

    let candidate = disables[0];
    assert_eq!(candidate.risk_tier, advice::RISK_TOGGLE);
    assert_eq!(candidate.title, "Turn off the idlesrv server");
    // One session in the window, so the monthly cost is one load of its schemas.
    let per_session: i64 = candidate
        .evidence
        .iter()
        .find(|e| e.label == "Context cost per session")
        .map(|e| {
            e.value
                .trim_start_matches('~')
                .split(' ')
                .next()
                .unwrap()
                .replace(',', "")
                .parse()
                .unwrap()
        })
        .expect("a per-session cost row");
    assert_eq!(candidate.est_tokens_month, per_session);
    assert!(
        candidate
            .evidence
            .iter()
            .any(|e| e.basis == advice::basis::ESTIMATED),
        "an unprobed server's cost is labelled an estimate: {:?}",
        candidate.evidence
    );
}

// ---------------------------------------------------------------------------
// Generator: ServerScope
// ---------------------------------------------------------------------------

#[test]
fn a_server_used_from_one_project_is_proposed_for_pinning_to_it() {
    let sb = Sandbox::new();
    let alpha = sb.project("alpha").to_string_lossy().into_owned();
    let beta = sb.project("beta").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": { "github": { "command": "npx", "args": ["gh-mcp"] } },
        "projects": {}
    }));

    let mut store = sb.store();
    // Three sessions in alpha, all of the github calls; two in beta with none.
    for i in 0..3 {
        seed_session(
            &mut store,
            &format!("a{i}"),
            &alpha,
            &[("mcp__github__search", 5)],
            5,
            0,
            0,
            0,
        );
    }
    for i in 0..2 {
        seed_session(&mut store, &format!("b{i}"), &beta, &[], 5, 0, 0, 0);
    }

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::ServerScope);

    assert_eq!(candidate.title, "Pin the github server to alpha");
    assert_eq!(candidate.risk_tier, advice::RISK_CONFIG_MOVE);
    let Params::ServerScope { projects, .. } = &candidate.params else {
        panic!("wrong params: {:?}", candidate.params);
    };
    assert_eq!(projects, &vec![alpha.clone()]);
    // The saving is the sessions that stop loading it: the two in beta.
    let freed = candidate
        .evidence
        .iter()
        .find(|e| e.label == "Sessions a month that would stop loading it")
        .expect("a freed-sessions row");
    assert_eq!(freed.value, "2");
    assert_eq!(freed.basis, advice::basis::OBSERVED);
    assert!(candidate.est_tokens_month > 0);
}

#[test]
fn a_server_used_everywhere_is_left_at_user_scope() {
    let sb = Sandbox::new();
    let projects: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|n| sb.project(n).to_string_lossy().into_owned())
        .collect();
    sb.write_claude_json(&json!({
        "mcpServers": { "github": { "command": "npx", "args": ["gh-mcp"] } },
        "projects": {}
    }));

    let mut store = sb.store();
    for (i, project) in projects.iter().enumerate() {
        seed_session(
            &mut store,
            &format!("s{i}"),
            project,
            &[("mcp__github__search", 5)],
            5,
            0,
            0,
            0,
        );
    }

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    assert!(
        of_kind(&candidates, ActionKind::ServerScope).is_empty(),
        "three projects is not a server that belongs to a project"
    );
}

// ---------------------------------------------------------------------------
// Generator: ClaudemdFix / ClaudemdTrim
// ---------------------------------------------------------------------------

/// Seed a global CLAUDE.md and a project one that share a block, plus dead
/// references in the project file. Returns the project path.
fn seed_claudemd(sb: &Sandbox, store: &mut Store) -> (String, PathBuf) {
    let proj = sb.project("proj");
    let proj_s = proj.to_string_lossy().into_owned();
    sb.write(
        &sb.claude_dir().join("CLAUDE.md"),
        &format!("# Global\n\n{SHARED_BLOCK}\n"),
    );
    sb.write(
        &proj.join("CLAUDE.md"),
        &format!(
            "# Project\n\n\
             - The build script lives at scripts/build.sh and is worth reading.\n\
             - See ./docs/design.md before changing anything.\n\
             - Keep the tests fast.\n\n\
             {SHARED_BLOCK}\n\n\
             Run the suite before every commit.\n"
        ),
    );
    seed_session(store, "s1", &proj_s, &[], 5, 0, 0, 0);
    (proj_s, proj)
}

#[test]
fn the_deterministic_fix_drops_dead_lines_and_the_redundant_copy_of_a_block() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::ClaudemdFix);
    assert_eq!(candidate.risk_tier, advice::RISK_CONTENT_EDIT);
    assert_eq!(
        candidate.target,
        proj.join("CLAUDE.md").to_string_lossy(),
        "the project copy is the redundant one; the global file is loaded anyway"
    );

    let new = candidate
        .new_content
        .as_deref()
        .expect("a transform result");
    assert!(
        !new.contains("scripts/build.sh") && !new.contains("./docs/design.md"),
        "the dead-reference lines are gone:\n{new}"
    );
    assert!(
        !new.contains(SHARED_BLOCK),
        "the duplicated block is gone from the project copy:\n{new}"
    );
    assert!(
        new.contains("Keep the tests fast.") && new.contains("Run the suite before every commit."),
        "everything else survives:\n{new}"
    );
    assert!(new.ends_with('\n'), "the trailing newline survives");
    // The global copy keeps the block, so nothing proposes editing it.
    assert_eq!(
        of_kind(&candidates, ActionKind::ClaudemdFix).len(),
        1,
        "only the redundant copy is edited"
    );
    assert_eq!(
        candidate.source_hash(),
        Some(candidate.fingerprint.as_str()),
        "a content candidate carries the hash it was computed against"
    );
}

/// A project whose guidance documents its own HTTP routes must not have those
/// lines reported as broken references, and must never have them deleted. The
/// extension-bearing routes are the sharp case: `/openapi.json` looks exactly
/// like a file, so the scanner still reports it and the deletion gate is the one
/// thing standing between a documented route and a silent removal.
#[test]
fn a_route_that_looks_like_a_path_is_never_deleted() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let proj = sb.project("proj");
    sb.write(
        &proj.join("CLAUDE.md"),
        "# Project\n\n\
         - The health probe is served at /api/healthz and must stay public.\n\
         - Sign-in lives at /login.\n\
         - The schema is published at /openapi.json.\n\
         - The bundle is served from /static/app.js.\n\
         - The build script is at scripts/build.sh.\n",
    );
    seed_session(&mut store, "s1", &proj.to_string_lossy(), &[], 5, 0, 0, 0);

    // The two extension-less routes are not references at all, so they are not
    // findings. The two that carry an extension are indistinguishable from a
    // file reference into a directory that is also gone, so they are reported.
    let report = piggy_core::claudemd::scan(&mut store).unwrap();
    let flagged: Vec<String> = report
        .findings()
        .filter_map(|f| match &f.kind {
            piggy_core::FindingKind::DeadRef { reference, .. } => Some(reference.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        flagged,
        vec![
            "/openapi.json".to_string(),
            "/static/app.js".to_string(),
            "scripts/build.sh".to_string(),
        ],
        "route findings"
    );

    let state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    let new = candidate.new_content.as_deref().unwrap();
    for route in ["/api/healthz", "/login", "/openapi.json", "/static/app.js"] {
        assert!(new.contains(route), "the {route} line went:\n{new}");
    }
    assert!(!new.contains("scripts/build.sh"), "the file reference goes");
    assert_eq!(
        candidate.title,
        "Drop 1 dead reference from proj's CLAUDE.md"
    );
}

#[test]
fn a_block_shared_by_two_projects_is_never_removed_from_either() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let alpha = sb.project("alpha");
    let beta = sb.project("beta");
    for dir in [&alpha, &beta] {
        sb.write(
            &dir.join("CLAUDE.md"),
            &format!("# Project\n\n{SHARED_BLOCK}\n"),
        );
    }
    seed_session(&mut store, "a", &alpha.to_string_lossy(), &[], 5, 0, 0, 0);
    seed_session(&mut store, "b", &beta.to_string_lossy(), &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    assert!(
        of_kind(&candidates, ActionKind::ClaudemdFix).is_empty(),
        "two projects never load each other's CLAUDE.md, so neither copy is redundant"
    );
}

#[test]
fn an_oversized_file_asks_for_the_advisor_and_carries_no_draft() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    // > 2,000 estimated tokens, which is > 7,000 bytes, in prose with no
    // path-shaped token in it.
    let body = "Prefer plain words over clever ones and say the thing you mean. ".repeat(140);
    sb.write(
        &sb.claude_dir().join("rules").join("style.md"),
        &format!("# Style\n\n{body}\n"),
    );
    seed_session(&mut store, "s1", "/proj", &[], 5, 0, 0, 0);
    seed_session(&mut store, "s2", "/proj", &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::ClaudemdTrim);

    assert_eq!(candidate.title, "Trim your global style.md");
    assert_eq!(
        candidate.prerequisites,
        vec![advice::Prerequisite::NeedsAdvisor]
    );
    assert!(candidate.new_content.is_none(), "M5.3 drafts nothing");
    assert!(candidate.blocked(), "not applyable without a draft");
    // Two sessions loaded it, so the monthly burden is twice one load.
    assert_eq!(
        candidate.est_tokens_month,
        piggy_core::claudemd::est_tokens(
            std::fs::metadata(sb.claude_dir().join("rules").join("style.md"))
                .unwrap()
                .len() as i64
        ) * 2
    );
}

// ---------------------------------------------------------------------------
// Generator: SaverMix
// ---------------------------------------------------------------------------

/// Seed a randomized A/B for `saver` where the ON arm really is cheaper, with a
/// second saver pinned on by hand in every session so the isolation rule holds.
fn seed_measured_win(store: &mut Store, saver: &str, pinned: &str) {
    for i in 0..40 {
        // Symmetric jitter around each side's centre so the bootstrap has a
        // spread to work with and the medians land where they were planted.
        let jitter = |base: u64, i: u64| base + (i % 7) * base / 100;
        for (side, enabled, output) in [
            ("on", true, jitter(600, i)),
            ("off", false, jitter(1000, i)),
        ] {
            let id = format!("{saver}-{side}-{i}");
            seed_session(store, &id, "/proj", &[], 10, 2000, output * 10, 500);
            store
                .set_session_savers(
                    &id,
                    &[
                        SaverTag::new(saver, enabled, source::ROTATION),
                        // `manual` so it never breaks the other saver's
                        // isolation, and `enabled` so it is not a scheduler-off.
                        SaverTag::new(pinned, true, source::MANUAL),
                    ],
                )
                .unwrap();
        }
    }
}

#[test]
fn a_measured_win_proposes_turning_the_saver_on_unless_a_rival_is_running() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    // `ponytail` conflicts with `caveman` in the catalog.
    seed_measured_win(&mut store, "ponytail", "caveman");

    // Rival running: the win is real but taking it is a trade, so nothing is
    // proposed.
    let mut state = PiggyState::default();
    install(&mut state, "ponytail", false);
    install(&mut state, "caveman", true);
    let candidates = generate(&mut store, &state);
    assert!(
        of_kind(&candidates, ActionKind::SaverMix).is_empty(),
        "a conflicting saver is on, so turning this one on is not a free win: {:?}",
        candidates
            .iter()
            .map(|c| c.title.clone())
            .collect::<Vec<_>>()
    );

    // Rival off: now it is just a saver being left on the table.
    let mut state = PiggyState::default();
    install(&mut state, "ponytail", false);
    install(&mut state, "caveman", false);
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::SaverMix);
    assert_eq!(candidate.target, "ponytail");
    let Params::SaverMix { turn_on, .. } = &candidate.params else {
        panic!("wrong params: {:?}", candidate.params);
    };
    assert!(*turn_on);
    assert!(
        candidate.title.contains("turn it on"),
        "title: {}",
        candidate.title
    );
    assert!(
        candidate
            .evidence
            .iter()
            .any(|e| e.basis == advice::basis::MEASURED),
        "the evidence quotes a measured arm: {:?}",
        candidate.evidence
    );
    assert!(
        candidate.est_tokens_month > 0,
        "a measured delta over observed tokens is a number worth showing"
    );
}

#[test]
fn a_behaviour_changing_saver_that_moved_nothing_proposes_off() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    // 40 randomized sessions a side, both drawn from the same distribution: the
    // comparison ran and found nothing.
    for i in 0..40u64 {
        for (side, enabled) in [("on", true), ("off", false)] {
            let id = format!("caveman-{side}-{i}");
            let output = 1000 + (i % 5) * 5;
            seed_session(&mut store, &id, "/proj", &[], 10, 2000, output * 10, 5000);
            store
                .set_session_savers(&id, &[SaverTag::new("caveman", enabled, source::ROTATION)])
                .unwrap();
        }
    }

    let mut state = PiggyState::default();
    install(&mut state, "caveman", true);
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::SaverMix);

    assert!(
        candidate
            .title
            .ends_with("has not moved the needle; turn it off"),
        "title: {}",
        candidate.title
    );
    let Params::SaverMix { turn_on, .. } = &candidate.params else {
        panic!("wrong params: {:?}", candidate.params);
    };
    assert!(!*turn_on);
    assert_eq!(
        candidate.est_tokens_month, 0,
        "no saving was measured, so none is claimed"
    );
    assert_eq!(
        candidate.evidence[0].label, "Randomized sessions compared",
        "the sample the claim rests on leads the evidence"
    );
    assert_eq!(candidate.evidence[0].value, "40 with it on, 40 with it off");
}

#[test]
fn a_thin_comparison_never_proposes_dropping_a_saver() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    // Twelve a side: enough for a badge, well short of the bar for telling
    // somebody to drop a saver they chose.
    for i in 0..12u64 {
        for (side, enabled) in [("on", true), ("off", false)] {
            let id = format!("caveman-{side}-{i}");
            let output = 1000 + (i % 5) * 5;
            seed_session(&mut store, &id, "/proj", &[], 10, 2000, output * 10, 5000);
            store
                .set_session_savers(&id, &[SaverTag::new("caveman", enabled, source::ROTATION)])
                .unwrap();
        }
    }

    let mut state = PiggyState::default();
    install(&mut state, "caveman", true);
    let candidates = generate(&mut store, &state);
    assert!(of_kind(&candidates, ActionKind::SaverMix).is_empty());
}

// ---------------------------------------------------------------------------
// The id
// ---------------------------------------------------------------------------

#[test]
fn the_same_inputs_give_the_same_id_and_moved_evidence_gives_a_new_one() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let body = "Prefer plain words over clever ones and say the thing you mean. ".repeat(140);
    sb.write(
        &sb.claude_dir().join("rules").join("style.md"),
        &format!("# Style\n\n{body}\n"),
    );
    seed_session(&mut store, "s1", "/proj", &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let first = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim)
        .id
        .clone();
    let again = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim)
        .id
        .clone();
    assert_eq!(first, again, "same facts, same id");
    assert!(
        first.starts_with("claudemd-trim-"),
        "the id names its own kind: {first}"
    );

    // A second session doubles the monthly burden, which is a different claim.
    seed_session(&mut store, "s2", "/proj", &[], 5, 0, 0, 0);
    let moved = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim)
        .id
        .clone();
    assert_ne!(first, moved, "moved evidence is a different suggestion");
}

// ---------------------------------------------------------------------------
// Lifecycle: stale, dismissed, reopened
// ---------------------------------------------------------------------------

#[test]
fn an_open_row_whose_evidence_moved_goes_stale() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let body = "Prefer plain words over clever ones and say the thing you mean. ".repeat(140);
    sb.write(
        &sb.claude_dir().join("rules").join("style.md"),
        &format!("# Style\n\n{body}\n"),
    );
    seed_session(&mut store, "s1", "/proj", &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let first = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim)
        .id
        .clone();
    assert_eq!(
        store.advice(&first).unwrap().unwrap().status,
        advice_status::OPEN
    );

    seed_session(&mut store, "s2", "/proj", &[], 5, 0, 0, 0);
    let second = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim)
        .id
        .clone();
    assert_eq!(
        store.advice(&first).unwrap().unwrap().status,
        advice_status::STALE,
        "the plan that described the old world is retired, not left applyable"
    );
    assert_eq!(
        store.advice(&second).unwrap().unwrap().status,
        advice_status::OPEN
    );
    assert_eq!(
        store.advice_by_status(advice_status::OPEN).unwrap().len(),
        1,
        "exactly one live row per target"
    );
}

#[test]
fn a_dismissal_suppresses_the_target_until_its_cost_doubles() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let body = "Prefer plain words over clever ones and say the thing you mean. ".repeat(140);
    sb.write(
        &sb.claude_dir().join("rules").join("style.md"),
        &format!("# Style\n\n{body}\n"),
    );
    seed_session(&mut store, "s1", "/proj", &[], 5, 0, 0, 0);
    seed_session(&mut store, "s2", "/proj", &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim).clone();
    assert!(advice::dismiss(&mut store, &candidate.id, Some("I need all of it")).unwrap());

    // Same file, same cost: still not for them.
    let candidates = generate(&mut store, &state);
    assert!(
        of_kind(&candidates, ActionKind::ClaudemdTrim).is_empty(),
        "a dismissed target stays dismissed while its evidence stands still"
    );

    // A third session moves the cost by half, which is not a doubling.
    seed_session(&mut store, "s3", "/proj", &[], 5, 0, 0, 0);
    assert!(
        of_kind(&generate(&mut store, &state), ActionKind::ClaudemdTrim).is_empty(),
        "half again is not roughly double"
    );

    // Four sessions is exactly twice the two it was dismissed at.
    seed_session(&mut store, "s4", "/proj", &[], 5, 0, 0, 0);
    let back = generate(&mut store, &state);
    let reopened = one_of_kind(&back, ActionKind::ClaudemdTrim);
    assert_eq!(reopened.est_tokens_month, candidate.est_tokens_month * 2);
    assert_eq!(reopened.status, advice_status::OPEN);
    assert_eq!(
        store.advice(&candidate.id).unwrap().unwrap().status,
        advice_status::STALE,
        "the spent dismissal is retired so it cannot suppress from a baseline twice"
    );
}

// ---------------------------------------------------------------------------
// Apply + undo: ServerScope
// ---------------------------------------------------------------------------

#[test]
fn a_scope_move_and_its_undo_leave_claude_json_as_it_was_plus_the_users_own_edit() {
    let sb = Sandbox::new();
    let alpha = sb.project("alpha").to_string_lossy().into_owned();
    let beta = sb.project("beta").to_string_lossy().into_owned();
    let original = json!({
        "mcpServers": { "github": { "command": "npx", "args": ["gh-mcp"], "env": { "TOKEN": "abc123" } } },
        "projects": { "/somewhere": { "allowedTools": ["Bash"] } },
        "numberFormat": 1.2500000000000002
    });
    sb.write_claude_json(&original);

    let mut store = sb.store();
    for i in 0..3 {
        seed_session(
            &mut store,
            &format!("a{i}"),
            &alpha,
            &[("mcp__github__search", 5)],
            5,
            0,
            0,
            0,
        );
    }
    seed_session(&mut store, "b0", &beta, &[], 5, 0, 0, 0);

    let mut state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ServerScope).clone();
    let catalog = Catalog::embedded();
    let applied = advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    assert!(applied.restore_ref.starts_with("scope-move:"));

    let after = sb.read_claude_json();
    assert!(
        after["mcpServers"].get("github").is_none(),
        "the user-scope copy is gone: {after}"
    );
    assert_eq!(
        after["projects"][&alpha]["mcpServers"]["github"]["env"]["TOKEN"],
        json!("abc123"),
        "the entry moved verbatim, secrets included, inside the same file"
    );
    assert_eq!(
        store.advice(&candidate.id).unwrap().unwrap().status,
        advice_status::APPLIED
    );

    // The user edits the same file while the move is applied.
    let mut edited = sb.read_claude_json();
    edited
        .as_object_mut()
        .unwrap()
        .insert("theirOwnKey".into(), json!({ "keep": true }));
    sb.write_claude_json(&edited);

    let undone = advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    assert!(undone.complete(), "failures: {:?}", undone.failures);
    let restored = sb.read_claude_json();
    assert_eq!(
        restored["theirOwnKey"],
        json!({ "keep": true }),
        "undo re-reads the file, so an edit made in between survives"
    );
    let mut without_theirs = restored.clone();
    without_theirs
        .as_object_mut()
        .unwrap()
        .remove("theirOwnKey");
    assert_eq!(
        without_theirs, original,
        "everything else is structurally identical, the project entry included"
    );
    assert_eq!(
        store.advice(&candidate.id).unwrap().unwrap().status,
        advice_status::OPEN,
        "an undone suggestion is open again, with its stamps cleared"
    );
    assert!(PiggyState::load().unwrap().scope_moves.is_empty());
}

// ---------------------------------------------------------------------------
// Apply + undo: ClaudemdFix
// ---------------------------------------------------------------------------

#[test]
fn a_content_edit_refuses_a_changed_file_and_restores_an_unchanged_one_byte_for_byte() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");
    let before = std::fs::read(&target).unwrap();

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();

    // Somebody edits the file between the draft and the click.
    std::fs::write(
        &target,
        [before.clone(), b"\nOne more rule.\n".to_vec()].concat(),
    )
    .unwrap();
    let err = advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap_err();
    match err.downcast_ref::<Conflict>() {
        Some(Conflict::Changed { path, .. }) => {
            assert_eq!(path, &target.to_string_lossy().into_owned())
        }
        other => panic!("expected a Changed conflict, got {other:?} ({err:#})"),
    }
    assert_eq!(
        std::fs::read(&target).unwrap(),
        [before.clone(), b"\nOne more rule.\n".to_vec()].concat(),
        "a refused apply writes nothing"
    );

    // Put it back and apply for real.
    std::fs::write(&target, &before).unwrap();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    let applied = advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    assert!(applied.restore_ref.starts_with("file-snapshot:"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        candidate.new_content.clone().unwrap(),
        "what was written is exactly what the diff showed"
    );
    assert_eq!(state.file_snapshots.len(), 1);

    let undone = advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    assert!(undone.complete(), "failures: {:?}", undone.failures);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "undo restores the original bytes, not a re-serialization of them"
    );
    assert!(
        state.file_snapshots.is_empty(),
        "a restored record is not left claiming there is something to undo"
    );
    assert_eq!(
        store.advice(&candidate.id).unwrap().unwrap().status,
        advice_status::OPEN
    );
}

#[test]
fn an_applied_row_refuses_a_second_apply() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    seed_claudemd(&sb, &mut store);

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    let err = advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap_err();
    assert!(
        format!("{err:#}").contains("already applied"),
        "error: {err:#}"
    );
}

// ---------------------------------------------------------------------------
// Apply + undo: ServerDisable
// ---------------------------------------------------------------------------

#[test]
fn disabling_a_server_through_advice_puts_exactly_that_one_back() {
    let sb = Sandbox::new();
    let proj = sb.project("proj").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": {
            "idlea": { "command": "npx", "args": ["a"] },
            "idleb": { "command": "npx", "args": ["b"] }
        },
        "projects": {}
    }));
    let mut store = sb.store();
    seed_session(&mut store, "s1", &proj, &[], 5, 0, 0, 0);

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidates = generate(&mut store, &state);
    let find = |prefix: &str| {
        of_kind(&candidates, ActionKind::ServerDisable)
            .into_iter()
            .find(|c| c.target.starts_with(prefix))
            .unwrap_or_else(|| panic!("a candidate for {prefix}"))
            .clone()
    };
    let first = find("idlea");
    let second = find("idleb");

    advice::apply(&mut store, &mut state, &catalog, &first).unwrap();
    advice::apply(&mut store, &mut state, &catalog, &second).unwrap();
    assert!(sb.read_claude_json()["mcpServers"]
        .as_object()
        .unwrap()
        .is_empty());

    // Undo one: the other stays off, and its snapshot stays recorded.
    let undone = advice::undo(&mut store, &mut state, &catalog, &first.id).unwrap();
    assert!(undone.complete(), "failures: {:?}", undone.failures);
    let after = sb.read_claude_json();
    assert_eq!(after["mcpServers"]["idlea"]["args"], json!(["a"]));
    assert!(after["mcpServers"].get("idleb").is_none());
    assert_eq!(PiggyState::load().unwrap().sweep_disabled.len(), 1);
}

// ---------------------------------------------------------------------------
// Restore Defaults
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn restore_defaults_puts_edited_files_back_and_names_the_one_it_could_not() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();

    // Two projects with the same fixable file, so one can be made unwritable
    // while the other still has to come back.
    let mut originals = Vec::new();
    for name in ["alpha", "beta"] {
        let dir = sb.project(name);
        sb.write(
            &dir.join("CLAUDE.md"),
            "# Project\n\n\
             - The build script lives at scripts/build.sh and is worth reading.\n\
             - Keep the tests fast.\n",
        );
        seed_session(&mut store, name, &dir.to_string_lossy(), &[], 5, 0, 0, 0);
        originals.push((dir.clone(), std::fs::read(dir.join("CLAUDE.md")).unwrap()));
    }

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    for candidate in of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>()
    {
        advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    }
    assert_eq!(state.file_snapshots.len(), 2);
    for (dir, original) in &originals {
        assert_ne!(&std::fs::read(dir.join("CLAUDE.md")).unwrap(), original);
    }

    // Revoke write on beta's directory: the atomic write needs to create a temp
    // file next to the target, so this is what a real permissions failure looks
    // like.
    let beta_dir = &originals[1].0;
    std::fs::set_permissions(beta_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let report = engine::restore_defaults().unwrap();
    std::fs::set_permissions(beta_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(report.files_restored, 1, "alpha came back");
    assert_eq!(
        std::fs::read(originals[0].0.join("CLAUDE.md")).unwrap(),
        originals[0].1,
        "alpha is byte-identical to how it started"
    );
    let beta_path = beta_dir.join("CLAUDE.md").to_string_lossy().into_owned();
    assert!(
        report.messages.iter().any(|m| m.contains(&beta_path)),
        "the file it could not put back is named: {:?}",
        report.messages
    );
    let after = PiggyState::load().unwrap();
    assert_eq!(
        after.file_snapshots.len(),
        1,
        "only the failure keeps its record; the backup is its only copy"
    );
    assert_eq!(after.file_snapshots[0].path, beta_path);
}

#[test]
fn restore_defaults_moves_a_re_scoped_server_back() {
    let sb = Sandbox::new();
    let alpha = sb.project("alpha").to_string_lossy().into_owned();
    let beta = sb.project("beta").to_string_lossy().into_owned();
    let original = json!({
        "mcpServers": { "github": { "command": "npx", "args": ["gh-mcp"] } },
        "projects": {}
    });
    sb.write_claude_json(&original);

    let mut store = sb.store();
    for i in 0..3 {
        seed_session(
            &mut store,
            &format!("a{i}"),
            &alpha,
            &[("mcp__github__search", 5)],
            5,
            0,
            0,
            0,
        );
    }
    seed_session(&mut store, "b0", &beta, &[], 5, 0, 0, 0);

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ServerScope).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    assert!(sb.read_claude_json()["mcpServers"].get("github").is_none());

    let report = engine::restore_defaults().unwrap();
    assert_eq!(report.scopes_restored, 1);
    assert_eq!(
        sb.read_claude_json()["mcpServers"]["github"],
        original["mcpServers"]["github"]
    );
    assert!(PiggyState::load().unwrap().scope_moves.is_empty());
}

// ---------------------------------------------------------------------------
// What the rewrite keeps, and what the database never sees
// ---------------------------------------------------------------------------

#[test]
fn a_rewrite_keeps_the_files_byte_order_mark_and_its_line_endings() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let proj = sb.project("proj");
    let target = proj.join("CLAUDE.md");
    // A BOM (a real cause of Claude Code parse failures, and never content) and
    // CRLF endings (a dotfiles repo checked out on Windows).
    let original =
        "\u{FEFF}# Project\r\n\r\n- Read scripts/gone.sh first.\r\n- Keep the tests fast.\r\n";
    sb.write(&target, original);
    seed_session(&mut store, "s1", &proj.to_string_lossy(), &[], 5, 0, 0, 0);

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let candidate = one_of_kind(&candidates, ActionKind::ClaudemdFix);
    let new = candidate.new_content.as_deref().unwrap();

    assert!(new.starts_with('\u{FEFF}'), "the BOM survives: {new:?}");
    assert!(!new.contains("scripts/gone.sh"), "the dead line is gone");
    assert_eq!(
        new, "\u{FEFF}# Project\r\n\r\n- Keep the tests fast.\r\n",
        "every surviving line keeps its own CRLF ending"
    );
}

#[test]
fn the_stored_payload_never_carries_the_file_it_rewrites() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    seed_claudemd(&sb, &mut store);

    let state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    let row = store.advice(&candidate.id).unwrap().unwrap();
    let payload = row.payload_json.clone().expect("a payload");

    assert!(
        !payload.contains("Keep the tests fast"),
        "CLAUDE.md contents are read at call time and never stored: {payload}"
    );
    assert!(
        !payload.contains(SHARED_BLOCK),
        "not even the block being removed: {payload}"
    );
    // What is stored is enough to reverse the edit and to name the row.
    assert_eq!(ActionKind::parse(&row.kind), Some(ActionKind::ClaudemdFix));
    let rebuilt = Candidate::from_row(&row).unwrap();
    assert_eq!(rebuilt.id, candidate.id);
    assert_eq!(rebuilt.fingerprint, candidate.fingerprint);
    assert_eq!(rebuilt.evidence, candidate.evidence);
    assert!(
        rebuilt.new_content.is_none(),
        "a row read back has no draft in it"
    );
}

// ---------------------------------------------------------------------------
// Sweep's own surface keeps working
// ---------------------------------------------------------------------------

#[test]
fn the_sweep_command_still_sees_what_advice_sees() {
    let sb = Sandbox::new();
    let proj = sb.project("proj").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": { "idlesrv": { "command": "npx", "args": ["idle"] } },
        "projects": {}
    }));
    let mut store = sb.store();
    seed_session(&mut store, "s1", &proj, &[], 5, 0, 0, 0);

    let report = piggy_core::sweep::scan(&store, piggy_core::sweep::DEFAULT_N_SESSIONS).unwrap();
    assert_eq!(report.recommended().count(), 1);
    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    assert_eq!(
        of_kind(&candidates, ActionKind::ServerDisable).len(),
        report.recommended().count(),
        "one source of truth, two entry points"
    );
}
