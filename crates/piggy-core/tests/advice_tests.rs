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

/// `~/.claude.json` keys `projects` by the exact working directory a session
/// started in. Folding `…/repo/app` into `…/repo` is right for the *decision*
/// (one checkout, not two projects) and wrong for the write: an entry under the
/// repo root does nothing for a session started in the subdirectory, which is
/// where half the calls came from.
#[test]
fn a_server_called_from_a_repo_and_its_subdirectory_is_pinned_to_both() {
    let sb = Sandbox::new();
    let repo = sb.project("repo").to_string_lossy().into_owned();
    let app = sb.project("repo/app").to_string_lossy().into_owned();
    let other = sb.project("other").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": { "github": { "command": "npx", "args": ["gh-mcp"] } },
        "projects": {}
    }));

    let mut store = sb.store();
    for (i, project) in [&repo, &app].iter().enumerate() {
        for j in 0..3 {
            seed_session(
                &mut store,
                &format!("s{i}{j}"),
                project,
                &[("mcp__github__search", 5)],
                5,
                0,
                0,
                0,
            );
        }
    }
    for i in 0..2 {
        seed_session(&mut store, &format!("o{i}"), &other, &[], 5, 0, 0, 0);
    }

    let mut state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ServerScope).clone();
    let Params::ServerScope { projects, .. } = &candidate.params else {
        panic!("wrong params: {:?}", candidate.params);
    };
    assert_eq!(
        projects,
        &vec![repo.clone(), app.clone()],
        "one checkout for the decision, both working directories for the write"
    );
    let freed = candidate
        .evidence
        .iter()
        .find(|e| e.label == "Sessions a month that would stop loading it")
        .expect("a freed-sessions row");
    assert_eq!(
        freed.value, "2",
        "only the sessions that are not pinned stop loading it, and six of the eight are"
    );

    let catalog = Catalog::embedded();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    let after = sb.read_claude_json();
    for project in [&repo, &app] {
        assert_eq!(
            after["projects"][project]["mcpServers"]["github"]["args"],
            json!(["gh-mcp"]),
            "the subdirectory needs its own entry or it loses the server: {after}"
        );
    }
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
         - The device manifest is at /etc/piggy-no-such-file.json.\n\
         - The build script is at scripts/build.sh.\n",
    );
    seed_session(&mut store, "s1", &proj.to_string_lossy(), &[], 5, 0, 0, 0);
    // The last route is the sharp one: its parent directory is real, so any
    // "does the neighbourhood exist" test would delete its line.
    assert!(
        Path::new("/etc").is_dir(),
        "the case only bites where the parent is real"
    );

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
            "/etc/piggy-no-such-file.json".to_string(),
            "scripts/build.sh".to_string(),
        ],
        "route findings"
    );

    let state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    let new = candidate.new_content.as_deref().unwrap();
    for route in [
        "/api/healthz",
        "/login",
        "/openapi.json",
        "/static/app.js",
        "/etc/piggy-no-such-file.json",
    ] {
        assert!(new.contains(route), "the {route} line went:\n{new}");
    }
    assert!(!new.contains("scripts/build.sh"), "the file reference goes");
    assert_eq!(
        candidate.title,
        "Drop 1 dead reference from proj's CLAUDE.md"
    );
}

/// A global rule file has no project root, so a relative reference in it
/// resolves against the home directory - which is nowhere its author meant.
/// Every unanchored reference in one is therefore dead by construction, and this
/// transform deletes whole lines, so it must leave them alone. `~/` is the one
/// anchor that means the same thing wherever the file is read from.
#[test]
fn a_global_rule_files_repo_relative_reference_keeps_its_line() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let rules = sb.claude_dir().join("rules").join("carveout.md");
    sb.write(
        &rules,
        "# Carve-out\n\n\
         - Reproduce with bench/src/report.js and read what it prints.\n\
         - The old note lived at ~/notes/gone.md and can go.\n\
         - Keep the tests fast.\n",
    );
    seed_session(
        &mut store,
        "s1",
        &sb.project("proj").to_string_lossy(),
        &[],
        5,
        0,
        0,
        0,
    );

    // Detection is unchanged: neither resolves, so both are still reported.
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
            "bench/src/report.js".to_string(),
            "~/notes/gone.md".to_string()
        ],
        "both are reported"
    );

    let state = PiggyState::default();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    assert_eq!(candidate.target, rules.to_string_lossy());
    let new = candidate.new_content.as_deref().unwrap();
    assert!(
        new.contains("bench/src/report.js"),
        "a repo-relative reference in a global file resolves against $HOME, which is not \
         evidence that the line is stale:\n{new}"
    );
    assert!(
        !new.contains("~/notes/gone.md"),
        "a `~/` reference means one thing everywhere, so it is still deletable:\n{new}"
    );
    assert!(new.contains("Keep the tests fast."));
    assert_eq!(
        candidate.title,
        "Drop 1 dead reference from your global carveout.md"
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

/// A routing saver's whole effect is on price: it sends much the same tokens to
/// a cheaper model, so four flat per-stream deltas are the outcome
/// docs/measurement.md predicts for it rather than a null result. Piggy has no
/// cost-side A/B yet, so "it did nothing, turn it off" is the one thing it must
/// not say about one.
#[test]
fn a_routing_saver_that_measured_flat_is_never_proposed_for_turning_off() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    // The same 40-a-side flat comparison that proposes dropping `caveman`.
    for i in 0..40u64 {
        for (side, enabled) in [("on", true), ("off", false)] {
            let id = format!("nadir-route-{side}-{i}");
            let output = 1000 + (i % 5) * 5;
            seed_session(&mut store, &id, "/proj", &[], 10, 2000, output * 10, 5000);
            store
                .set_session_savers(
                    &id,
                    &[SaverTag::new("nadir-route", enabled, source::ROTATION)],
                )
                .unwrap();
        }
    }

    let catalog = Catalog::embedded();
    let entry = catalog.get("nadir-route").expect("a catalog entry");
    assert_eq!(entry.layer, "routing");
    assert!(
        entry.behavior_changing,
        "the exemption is only interesting for a saver the turn-off branch would otherwise reach"
    );

    let mut state = PiggyState::default();
    install(&mut state, "nadir-route", true);
    let candidates = generate(&mut store, &state);
    assert!(
        of_kind(&candidates, ActionKind::SaverMix).is_empty(),
        "flat streams are what this saver was predicted to do: {:?}",
        candidates
            .iter()
            .map(|c| c.title.clone())
            .collect::<Vec<_>>()
    );
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
// What the list is worth
// ---------------------------------------------------------------------------

/// `ClaudemdTrim`'s figure is what a file *costs*; every other kind's is what
/// applying it *saves*. It is also the largest number Piggy computes, so one
/// total over both would be a cost presented as money back, several times over,
/// on the strength of the one kind v1 cannot even apply.
#[test]
fn a_burden_is_reported_apart_from_the_savings_it_would_otherwise_swamp() {
    let sb = Sandbox::new();
    let proj = sb.project("proj").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": { "idlesrv": { "command": "npx", "args": ["idle"] } },
        "projects": {}
    }));
    let mut store = sb.store();
    let body = "Prefer plain words over clever ones and say the thing you mean. ".repeat(140);
    sb.write(
        &sb.claude_dir().join("rules").join("style.md"),
        &format!("# Style\n\n{body}\n"),
    );
    for i in 0..3 {
        seed_session(&mut store, &format!("s{i}"), &proj, &[], 5, 0, 0, 0);
    }

    let state = PiggyState::default();
    let candidates = generate(&mut store, &state);
    let trim = one_of_kind(&candidates, ActionKind::ClaudemdTrim).est_tokens_month;
    let disable = one_of_kind(&candidates, ActionKind::ServerDisable).est_tokens_month;
    assert!(
        trim > disable,
        "the burden is the bigger number, as it is in life"
    );

    assert_eq!(
        advice::total_savings(&candidates),
        disable,
        "the headline is what applying this list gives back"
    );
    assert_eq!(
        advice::total_burden(&candidates),
        trim,
        "and the burden is its own clause"
    );
    assert!(ActionKind::ClaudemdTrim.est_is_burden());
    for kind in [
        ActionKind::ServerDisable,
        ActionKind::ServerScope,
        ActionKind::ClaudemdFix,
        ActionKind::SaverMix,
    ] {
        assert!(!kind.est_is_burden(), "{} is a saving", kind.as_str());
    }
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

/// A candidate's id hashes its evidence, and evidence oscillates: the look-back
/// window is a flag on `piggy advise`, and a rolling 30-day count comes back
/// down as well as up. So the id retired an hour ago is regularly the plan the
/// world supports now, and `stale` being a one-way door left it permanently
/// unapplyable - which contradicts the spec's "re-scan regenerates".
#[test]
fn a_stale_row_comes_back_open_when_its_candidate_regenerates() {
    let sb = Sandbox::new();
    let proj = sb.project("proj").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": { "idlesrv": { "command": "npx", "args": ["idle"] } },
        "projects": {}
    }));
    let mut store = sb.store();
    for i in 0..4 {
        seed_session(&mut store, &format!("s{i}"), &proj, &[], 5, 0, 0, 0);
    }

    let catalog = Catalog::embedded();
    let pricing = Pricing::embedded();
    let state = PiggyState::default();
    let window = |n: usize| {
        let mut opts = GenerateOptions::new(&catalog, &pricing, &state);
        opts.n_sessions = n;
        opts
    };
    let disable_id = |c: &[Candidate]| one_of_kind(c, ActionKind::ServerDisable).id.clone();

    let wide = disable_id(&advice::generate(&mut store, &window(4)).unwrap());
    // A narrower window says "uses in the last 2 sessions" instead, which is a
    // different evidence row and so a different suggestion.
    let narrow = disable_id(&advice::generate(&mut store, &window(2)).unwrap());
    assert_ne!(wide, narrow);
    assert_eq!(
        store.advice(&wide).unwrap().unwrap().status,
        advice_status::STALE
    );

    // Back to the wider window: the retired plan is the live one again.
    let back = advice::generate(&mut store, &window(4)).unwrap();
    let again = one_of_kind(&back, ActionKind::ServerDisable).clone();
    assert_eq!(again.id, wide);
    assert_eq!(
        again.status,
        advice_status::OPEN,
        "regenerating is proof the plan is live, and nothing else would ever move a row out \
         of stale"
    );
    assert_eq!(
        store.advice(&wide).unwrap().unwrap().status,
        advice_status::OPEN
    );

    // And the point of all that: it can be applied.
    let mut state = PiggyState::default();
    advice::apply(&mut store, &mut state, &catalog, &again).unwrap();
    assert!(sb.read_claude_json()["mcpServers"]
        .as_object()
        .unwrap()
        .is_empty());
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

/// Undo must not be a weaker gate than apply. Apply refuses to write a file
/// whose hash moved; a restore cannot refuse, because somebody who fixed a typo
/// the week after applying still has to be able to undo. So it keeps their bytes
/// instead of guarding against them.
#[test]
fn undoing_a_file_edited_since_the_apply_backs_that_edit_up_first() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");
    let before = std::fs::read(&target).unwrap();

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();

    // Three weeks of the user's own writing on top of Piggy's edit.
    let theirs = format!(
        "{}- Always run the linter before you push.\n",
        std::fs::read_to_string(&target).unwrap()
    );
    std::fs::write(&target, &theirs).unwrap();

    let undone = advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    assert!(undone.complete(), "failures: {:?}", undone.failures);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "the undo still puts the original back"
    );

    let kept: Vec<&piggy_core::snapshots::FileBackup> = state
        .file_backups
        .iter()
        .filter(|b| b.path == target.to_string_lossy())
        .collect();
    assert_eq!(
        kept.len(),
        1,
        "what the user wrote after the apply is copied aside, not overwritten unrecorded: {:?}",
        state.file_backups
    );
    assert_eq!(std::fs::read_to_string(&kept[0].backup).unwrap(), theirs);
    assert!(
        state.file_snapshots.is_empty(),
        "and it lands in the other ledger, where nothing can offer it as an undo: {:?}",
        state.file_snapshots
    );

    // A file that is exactly as the apply left it costs no second copy.
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    let copies = state.file_backups.len();
    advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    assert!(
        state.file_snapshots.is_empty(),
        "the edit's own record goes with the undo: {:?}",
        state.file_snapshots
    );
    assert_eq!(
        state.file_backups.len(),
        copies,
        "an untouched file is restored with nothing else copied aside: {:?}",
        state.file_backups
    );
}

/// Two Piggy edits to one file come off the stack newest first. Undoing the
/// older one writes the content from before *both* over the newer one's result
/// and leaves its record behind pointing at bytes that are now nobody's, which a
/// later Restore Defaults would then write back over the user.
#[test]
fn undoing_the_older_of_two_edits_to_one_file_is_refused_by_name() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let first = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &first).unwrap();

    // The user adds a line pointing at something that is not there, so Piggy
    // proposes a second edit to the same file.
    let after_first = std::fs::read_to_string(&target).unwrap();
    std::fs::write(
        &target,
        format!("{after_first}- See docs/vanished.md for the rest.\n"),
    )
    .unwrap();
    let second = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    assert_ne!(second.id, first.id);
    advice::apply(&mut store, &mut state, &catalog, &second).unwrap();
    let after_second = std::fs::read(&target).unwrap();

    let err = advice::undo(&mut store, &mut state, &catalog, &first.id).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&second.title),
        "the later edit is named so the user can act on it: {msg}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        after_second,
        "a refused undo writes nothing"
    );
    assert_eq!(
        store.advice(&first.id).unwrap().unwrap().status,
        advice_status::APPLIED,
        "and leaves the row applied, with its restore reference"
    );
    assert_eq!(state.file_snapshots.len(), 2, "both records survive");

    // Newest first is what it asked for, and then the older one goes back too.
    assert!(advice::undo(&mut store, &mut state, &catalog, &second.id)
        .unwrap()
        .complete());
    assert!(advice::undo(&mut store, &mut state, &catalog, &first.id)
        .unwrap()
        .complete());
}

/// "Not for me" is a thing to say about a suggestion, not about a change that is
/// already on disk: `dismissed` carries no `applied_at` or `restore_ref`, so the
/// transition would drop the only handle Undo has.
#[test]
fn dismissing_an_applied_row_is_refused_so_its_undo_survives() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    seed_claudemd(&sb, &mut store);

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();

    let err = advice::dismiss(&mut store, &candidate.id, Some("on reflection, fine")).unwrap_err();
    assert!(
        format!("{err:#}").contains("undo it first"),
        "error: {err:#}"
    );
    let row = store.advice(&candidate.id).unwrap().unwrap();
    assert_eq!(row.status, advice_status::APPLIED);
    assert!(
        row.applied_at.is_some() && row.restore_ref.is_some(),
        "the stamps Undo reads are still there: {row:?}"
    );
    let undone = advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    assert!(undone.complete(), "failures: {:?}", undone.failures);
    // Once it is only a suggestion again, waving it away is fine.
    assert!(advice::dismiss(&mut store, &candidate.id, None).unwrap());
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

/// The panic button has to be idempotent. A record with no recorded
/// `after_hash` - every record written before that field existed, so an upgrade
/// alone gets here with no user edit anywhere - reads as "edited since the
/// apply", so the first press copies Piggy's own edit aside before putting the
/// original back. Restoring *that* copy on the second press writes the edit back
/// over the original, and the press after that takes it off again, for ever.
#[test]
fn restore_defaults_pressed_twice_leaves_the_original_in_place() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");
    let before = std::fs::read(&target).unwrap();

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    assert_ne!(std::fs::read(&target).unwrap(), before);

    // A state.json written by a Piggy that did not record what it wrote.
    let mut upgraded = PiggyState::load().unwrap();
    for snap in &mut upgraded.file_snapshots {
        snap.after_hash = None;
    }
    upgraded.save().unwrap();

    let first = engine::restore_defaults().unwrap();
    assert_eq!(first.files_restored, 1);
    assert_eq!(std::fs::read(&target).unwrap(), before);

    let second = engine::restore_defaults().unwrap();
    assert_eq!(
        second.files_restored, 0,
        "there was nothing left to put back: {:?}",
        second.messages
    );
    assert!(
        !second.messages.iter().any(|m| m.contains("put back")),
        "so the second press does not claim it put a file back: {:?}",
        second.messages
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "and it does not write Piggy's edit back over the original"
    );

    // The copy the first press took stays as a recovery copy, in the ledger that
    // nothing restores from.
    let after = PiggyState::load().unwrap();
    assert!(after.file_snapshots.is_empty(), "nothing left to put back");
    assert_eq!(after.file_backups.len(), 1);
}

/// An Undo backs up whatever the user wrote over Piggy's edit before it puts the
/// original back, and records that copy in `file_backups` because it is their
/// content, not an edit of ours waiting to be reversed. Restore Defaults reads
/// only the other ledger: putting that copy back would hand the user Piggy's edit
/// again, on top of the original their Undo just restored.
#[test]
fn restore_defaults_does_not_write_back_what_an_undo_saved() {
    let sb = Sandbox::new();
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");
    let before = std::fs::read(&target).unwrap();

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();

    let theirs = format!(
        "{}- Always run the linter before you push.\n",
        std::fs::read_to_string(&target).unwrap()
    );
    std::fs::write(&target, &theirs).unwrap();
    assert!(
        advice::undo(&mut store, &mut state, &catalog, &candidate.id)
            .unwrap()
            .complete()
    );
    assert_eq!(std::fs::read(&target).unwrap(), before);

    let report = engine::restore_defaults().unwrap();
    assert_eq!(
        report.files_restored, 0,
        "the undo already put the only edited file back: {:?}",
        report.messages
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "Restore Defaults must not re-apply Piggy's edit out of the user's own backup"
    );
    let after = PiggyState::load().unwrap();
    assert!(after.file_snapshots.is_empty());
    assert_eq!(after.file_backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&after.file_backups[0].backup).unwrap(),
        theirs,
        "their work is still recoverable by hand"
    );
}

/// `chmod 000` denies nobody who is root, and some filesystems ignore the mode
/// outright. Probe it rather than assume it, so a test about unreadable files
/// skips instead of failing for the wrong reason.
#[cfg(unix)]
fn mode_000_denies_a_read(sb: &Sandbox) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = sb.dir.path().join("readable-probe");
    std::fs::write(&probe, b"x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied = std::fs::read(&probe).is_err();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&probe).unwrap();
    denied
}

/// "Gone" and "there but unreadable" are not the same answer. A restore that
/// cannot copy a file's current bytes must not write over them: `write_atomic`
/// needs only a writable parent directory, so it would succeed on a file nobody
/// can read and destroy the content with no backup anywhere.
#[test]
#[cfg(unix)]
fn undoing_a_file_that_cannot_be_read_fails_by_name_instead_of_destroying_it() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    if !mode_000_denies_a_read(&sb) {
        return;
    }
    sb.write_claude_json(&json!({ "mcpServers": {}, "projects": {} }));
    let mut store = sb.store();
    let (_, proj) = seed_claudemd(&sb, &mut store);
    let target = proj.join("CLAUDE.md");

    let mut state = PiggyState::default();
    let catalog = Catalog::embedded();
    let candidate = one_of_kind(&generate(&mut store, &state), ActionKind::ClaudemdFix).clone();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    let applied = std::fs::read(&target).unwrap();

    // The file is still there and still has content; it is reading it that is
    // refused. Note the mode is exactly what the apply left, so the record's
    // `after_hash` matches what is on disk - it is the read failure alone that
    // decides this.
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
    let undone = advice::undo(&mut store, &mut state, &catalog, &candidate.id).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        !undone.complete(),
        "a file that could not be read is not a file that was restored"
    );
    let failure = &undone.failures[0];
    assert_eq!(failure.item, target.to_string_lossy());
    assert!(
        failure.reason.contains("CLAUDE.md"),
        "the reason names the file: {}",
        failure.reason
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        applied,
        "the content nobody could copy is still on disk"
    );
    assert_eq!(
        store.advice(&candidate.id).unwrap().unwrap().status,
        advice_status::APPLIED,
        "and the row keeps its restore reference, so the undo can be retried"
    );
    assert_eq!(
        state.file_snapshots.len(),
        1,
        "the backup is still the only copy of the original"
    );
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

/// The mirror of the CLAUDE.md rule, for the other secret Piggy can see. An MCP
/// server's `env` is where API tokens live, `~/.claude.json` is 0600 and
/// `piggy.db` is not, and `payload_json` is written for every candidate the
/// generator produces - no apply, no consent, and nothing ever deletes the row.
#[test]
fn the_stored_payload_never_carries_a_servers_env() {
    let sb = Sandbox::new();
    let alpha = sb.project("alpha").to_string_lossy().into_owned();
    let beta = sb.project("beta").to_string_lossy().into_owned();
    sb.write_claude_json(&json!({
        "mcpServers": {
            "github": {
                "command": "npx",
                "args": ["gh-mcp"],
                "env": { "GITHUB_TOKEN": "ghp-not-a-real-secret" }
            }
        },
        "projects": {}
    }));

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
    let payload = store
        .advice(&candidate.id)
        .unwrap()
        .unwrap()
        .payload_json
        .expect("a payload");
    assert!(
        !payload.contains("ghp-not-a-real-secret") && !payload.contains("GITHUB_TOKEN"),
        "listing advice must not copy an MCP server's env into the database: {payload}"
    );

    // A payload written by the build that did carry it still reads back, so an
    // Undo recorded before this change is not stranded.
    let legacy: Params = serde_json::from_value(json!({
        "server-scope": {
            "server": "github",
            "projects": [alpha.clone()],
            "config": { "command": "npx", "env": { "GITHUB_TOKEN": "ghp-not-a-real-secret" } }
        }
    }))
    .expect("an older payload still deserializes");
    assert!(matches!(legacy, Params::ServerScope { .. }));

    // A key the config hash does not cover, added between generate and apply.
    // The fingerprint check still passes, so the move goes ahead - and it has to
    // carry the key, which a copy taken at generation time could not have known
    // about.
    let mut edited = sb.read_claude_json();
    edited["mcpServers"]["github"]["timeout"] = json!(60);
    sb.write_claude_json(&edited);

    let catalog = Catalog::embedded();
    advice::apply(&mut store, &mut state, &catalog, &candidate).unwrap();
    let moved = &sb.read_claude_json()["projects"][&alpha]["mcpServers"]["github"];
    assert_eq!(
        moved["env"]["GITHUB_TOKEN"],
        json!("ghp-not-a-real-secret"),
        "the entry still moves verbatim: apply re-reads it from the file it moves it inside"
    );
    assert_eq!(
        moved["timeout"],
        json!(60),
        "including the keys the fingerprint does not cover: {moved}"
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
