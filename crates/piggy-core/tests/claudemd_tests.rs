//! CLAUDE.md scanner tests: the three detectors against their fixtures, and the
//! inventory against a sandboxed home.
//!
//! The detector tests are pure - they build a [`FileText`] from a fixture and
//! call the function - so they need no database and no environment. The scan
//! tests read `PIGGY_CLAUDE_DIR` (the global half of the inventory, and the base
//! a `~/…` reference resolves against), so they take the same global env lock
//! the M2 engine tests use and point every path at a tempdir. Nothing here ever
//! touches the real `~/.claude`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use piggy_core::claudemd::{self, DeadRef, FileText, FindingKind};
use piggy_core::{ModelTokens, Pricing, SessionParse, Store};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claudemd")
        .join(name)
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claudemd")
}

/// A fixture file read as a project file of the fixture directory, so relative
/// references resolve inside the fixture tree.
fn fixture_text(name: &str) -> FileText {
    claudemd::read_file_text(
        &fixture(name),
        Some(fixture_dir().to_string_lossy().into_owned()),
    )
    .unwrap()
}

fn refs(findings: &[piggy_core::Finding]) -> BTreeSet<String> {
    findings
        .iter()
        .filter_map(|f| match &f.kind {
            FindingKind::DeadRef { reference, .. } => Some(reference.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Detector: dead references
// ---------------------------------------------------------------------------

#[test]
fn dead_refs_flags_the_missing_and_spares_the_resolving() {
    let f = fixture_text("dead-refs.md");
    let found = claudemd::dead_refs(&f);
    let got = refs(&found);

    let expected: BTreeSet<String> = [
        "src/gone.rs",
        "./docs/removed.md",
        "scripts/old-build.sh",
        "docs/nope.md",
        "src/missing-tail.rs",
        "/nonexistent-piggy-fixture-root/absolute.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(got, expected, "dead references");

    // The four that resolve inside the fixture tree are not findings, and the
    // prose that merely looks path-shaped never becomes one.
    for spared in [
        "empty.md",
        "./dup-pair/global.md",
        "oversized.md",
        "dup-pair/project.md",
        "and/or",
        "read/write",
        "Rust/TypeScript",
        "example.com/docs/host.md",
        "https://example.com/docs/thing.md",
        ".claude/rules/*.md",
        "<project>/CLAUDE.md",
        "project>/CLAUDE.md",
        "/fenced/example/path.rs",
    ] {
        assert!(!got.contains(spared), "{spared} should not be flagged");
    }

    // A relative reference resolves against the project root, and the finding
    // carries where it looked.
    let gone = found
        .iter()
        .find(|f| matches!(&f.kind, FindingKind::DeadRef { reference, .. } if reference == "src/gone.rs"))
        .unwrap();
    let FindingKind::DeadRef { resolved, more, .. } = &gone.kind else {
        panic!("expected a dead reference");
    };
    assert_eq!(
        Path::new(resolved),
        fixture_dir().join("src/gone.rs"),
        "resolved against the project root"
    );
    assert_eq!(*more, 0, "six dead refs is under the cap");
    // A dead reference misdirects the model; it does not cost context tokens,
    // and claiming a saving here would be inventing one.
    assert_eq!(gone.est_tokens, 0);
    assert!(gone.action.starts_with("Update or delete"), "{}", gone.action);
}

#[test]
fn documented_routes_are_not_dead_references() {
    let f = fixture_text("routes.md");
    let got = refs(&claudemd::dead_refs(&f));

    // A route with no file extension is not a reference at all, so it is not a
    // finding either.
    for route in [
        "/v1/sessions",
        "/users/:id/refresh",
        "/healthz/live",
        "/login",
        "/docs/getting-started",
    ] {
        assert!(!got.contains(route), "{route} is a route, not a reference");
    }
    assert!(got.contains("src/gone.rs"), "the real one still flags");

    // These two clear the extension test, so nothing about the token itself
    // tells them apart from a file. They are still reported (a file reference
    // into a directory that is itself gone looks the same), and the deletion
    // gate is what has to refuse them.
    let located = claudemd::dead_refs_located(&f);
    let by_ref = |r: &str| located.iter().find(|d| d.reference == r);
    for route in ["/openapi.json", "/static/app.js"] {
        let dead = by_ref(route).unwrap_or_else(|| panic!("{route} was dropped from the findings"));
        assert!(
            !claudemd::deletable_ref(dead, &f),
            "{route} would have had its line deleted"
        );
    }
    // A relative reference to a file is exactly what the deletion is for.
    assert!(claudemd::deletable_ref(by_ref("src/gone.rs").unwrap(), &f));
}

/// The two shapes a line is never deleted over, whatever is on disk around
/// them. Both are still *reported*: the question here is only whether Piggy is
/// sure enough to remove the line.
#[test]
fn deletable_ref_spares_a_root_anchored_token_and_a_global_files_relative_one() {
    let project = fixture_text("routes.md");
    // The same bytes read as a *global* rule file: no project root, so a
    // relative reference in it resolves against the home directory instead of
    // anywhere its author meant.
    let global = claudemd::read_file_text(&fixture("routes.md"), None).unwrap();

    let dead = |reference: &str, resolved: &str| DeadRef {
        line: 0,
        reference: reference.to_string(),
        resolved: PathBuf::from(resolved),
    };

    // Root-anchored with a real parent directory: a "does the neighbourhood
    // exist" test clears this, and it is still a URL route as often as a path.
    assert!(
        Path::new("/etc").is_dir(),
        "the case only bites where the parent is real"
    );
    let absolute = dead(
        "/etc/piggy-no-such-file.json",
        "/etc/piggy-no-such-file.json",
    );
    assert!(!claudemd::deletable_ref(&absolute, &project));
    assert!(!claudemd::deletable_ref(&absolute, &global));

    // Relative: the project file's base is that project, the global file's is a
    // fallback, so in a global file every relative reference is dead by
    // construction and none of them may cost a line.
    let relative = dead("bench/src/report.js", "/nowhere/bench/src/report.js");
    assert!(claudemd::deletable_ref(&relative, &project));
    assert!(!claudemd::deletable_ref(&relative, &global));

    // `~/` means the same thing wherever the file is read from, so it is the one
    // anchor a global file can be held to.
    let tilde = dead("~/notes/gone.md", "/home/nobody/notes/gone.md");
    assert!(claudemd::deletable_ref(&tilde, &project));
    assert!(claudemd::deletable_ref(&tilde, &global));
}

#[test]
fn dead_ref_list_is_capped_with_the_rest_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = String::from("# Many\n\n");
    for i in 0..12 {
        body.push_str(&format!("- see src/missing-{i:02}.rs for that\n"));
    }
    let path = dir.path().join("CLAUDE.md");
    std::fs::write(&path, body).unwrap();
    let f = claudemd::read_file_text(&path, Some(dir.path().to_string_lossy().into_owned()))
        .unwrap();

    let found = claudemd::dead_refs(&f);
    assert_eq!(found.len(), 10, "capped at ten reported");
    let FindingKind::DeadRef { more, .. } = &found.last().unwrap().kind else {
        panic!("expected a dead reference");
    };
    assert_eq!(*more, 2, "the other two are counted, not dropped silently");
    assert!(
        found.last().unwrap().detail.contains("2 further dead reference"),
        "the overflow is stated: {}",
        found.last().unwrap().detail
    );
}

// ---------------------------------------------------------------------------
// Detector: duplicate blocks
// ---------------------------------------------------------------------------

#[test]
fn duplicate_blocks_finds_the_shared_paragraph_across_the_pair() {
    let global = fixture_text("dup-pair/global.md");
    let project = fixture_text("dup-pair/project.md");
    let global_path = global.path.to_string_lossy().into_owned();
    let project_path = project.path.to_string_lossy().into_owned();

    let found = claudemd::duplicate_blocks(&[global, project]);
    assert_eq!(found.len(), 2, "one finding per file carrying the block");

    let on_global = found.iter().find(|f| f.path == global_path).unwrap();
    let FindingKind::DuplicateBlock {
        others,
        label,
        bytes,
    } = &on_global.kind
    else {
        panic!("expected a duplicate block");
    };
    assert_eq!(others, &vec![project_path.clone()], "names the other file");
    assert_eq!(label.chars().count(), 60, "label is the first 60 characters");
    assert!(
        label.starts_with("Never reproduce a number the model produced"),
        "{label}"
    );
    // The two copies are wrapped differently on disk; only normalization makes
    // them the same block.
    assert_eq!(*bytes, 152);
    assert_eq!(on_global.est_tokens, claudemd::est_tokens(152));
    assert!(on_global.detail.contains(&project_path));

    let on_project = found.iter().find(|f| f.path == project_path).unwrap();
    let FindingKind::DuplicateBlock { others, .. } = &on_project.kind else {
        panic!("expected a duplicate block");
    };
    assert_eq!(others, &vec![global_path]);
}

#[test]
fn short_shared_boilerplate_is_not_a_duplicate() {
    let global = fixture_text("dup-pair/global.md");
    let project = fixture_text("dup-pair/project.md");
    // Both files carry "Run the tests before you commit." verbatim.
    assert!(global.text.contains("Run the tests before you commit."));
    assert!(project.text.contains("Run the tests before you commit."));

    let found = claudemd::duplicate_blocks(&[global, project]);
    for f in &found {
        let FindingKind::DuplicateBlock { label, .. } = &f.kind else {
            panic!("expected a duplicate block");
        };
        assert!(!label.starts_with("Run the tests"), "{label}");
    }
}

#[test]
fn a_paragraph_repeated_inside_one_file_is_not_a_cross_file_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let para = "The same long paragraph twice over, well past the eighty byte floor so that \
                length is never what keeps it out of the report.";
    let path = dir.path().join("CLAUDE.md");
    std::fs::write(&path, format!("{para}\n\n{para}\n")).unwrap();
    let f =
        claudemd::read_file_text(&path, Some(dir.path().to_string_lossy().into_owned())).unwrap();

    // One file, one copy paid for: repeating yourself inside a file is a style
    // problem, not a second load.
    assert!(claudemd::duplicate_blocks(&[f]).is_empty());
}

// ---------------------------------------------------------------------------
// Detector: oversize
// ---------------------------------------------------------------------------

#[test]
fn oversize_fires_past_the_threshold_and_not_before() {
    let big = fixture_text("oversized.md");
    let finding = claudemd::oversize(&big).expect("the generated fixture is over the line");
    assert_eq!(
        finding.kind,
        FindingKind::Oversize {
            threshold: claudemd::OVERSIZE_EST_TOKENS
        }
    );
    assert!(big.est_tokens() > claudemd::OVERSIZE_EST_TOKENS);
    assert_eq!(finding.est_tokens, big.est_tokens());
    assert!(finding.claim.starts_with("oversized.md is about"), "{}", finding.claim);

    for small in ["empty.md", "dup-pair/global.md", "bom.md"] {
        assert!(
            claudemd::oversize(&fixture_text(small)).is_none(),
            "{small} is under the line"
        );
    }
}

// ---------------------------------------------------------------------------
// BOM and empty files
// ---------------------------------------------------------------------------

#[test]
fn bom_is_stripped_for_the_detectors_and_kept_in_the_hash() {
    let f = fixture_text("bom.md");
    assert!(!f.text.starts_with('\u{feff}'), "the BOM never reaches a detector");
    assert!(f.text.starts_with("docs/missing-from-bom.md"));

    // `bytes` and `hash` describe the file as it sits on disk, so the hash an
    // inventory row carries is the one the snapshot store checks before an edit.
    assert_eq!(f.bytes, std::fs::metadata(fixture("bom.md")).unwrap().len() as i64);
    assert!(piggy_core::snapshots::check_unchanged(&fixture("bom.md"), &f.hash).is_ok());

    // Without the strip this reference comes back with three invisible bytes on
    // the front and resolves to a path nobody wrote.
    let got = refs(&claudemd::dead_refs(&f));
    assert_eq!(
        got,
        ["docs/missing-from-bom.md".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn empty_file_is_inventoried_with_no_findings() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    std::fs::write(proj.join("CLAUDE.md"), b"").unwrap();
    let mut store = sandbox.store();
    seed_session(&mut store, "s-empty", &proj, &now());

    let report = claudemd::scan(&mut store).unwrap();
    assert_eq!(report.files.len(), 1);
    let f = &report.files[0];
    assert_eq!(f.file.bytes, 0);
    assert_eq!(f.file.est_tokens, 0);
    assert_eq!(f.est_tokens_month, 0);
    assert!(f.findings.is_empty(), "{:?}", f.findings);
    assert!(report.warnings.is_empty());
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

#[test]
fn scan_inventories_both_scopes_then_updates_and_deletes_on_rescan() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    let claude = sandbox.claude_dir();
    std::fs::create_dir_all(claude.join("rules")).unwrap();
    std::fs::create_dir_all(proj.join(".claude/rules")).unwrap();

    // A global file that points at one rule that exists and one that does not:
    // a global reference resolves against home, not against any project.
    std::fs::write(
        claude.join("CLAUDE.md"),
        "# Global\n\nStyle lives in ~/.claude/rules/style.md and the old one was\n\
         ~/.claude/rules/retired.md.\n",
    )
    .unwrap();
    std::fs::write(claude.join("rules/style.md"), "# Style\n\nShort sentences.\n").unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "# Proj\n\nOne line.\n").unwrap();
    std::fs::write(proj.join("CLAUDE.local.md"), "# Local\n\nAnother line.\n").unwrap();
    std::fs::write(proj.join(".claude/rules/local.md"), "# Local rule\n").unwrap();

    let mut store = sandbox.store();
    seed_session(&mut store, "s1", &proj, &now());

    let report = claudemd::scan(&mut store).unwrap();
    let paths: Vec<&str> = report.files.iter().map(|f| f.file.path.as_str()).collect();
    assert_eq!(paths.len(), 5, "{paths:?}");
    assert!(paths.contains(&claude.join("CLAUDE.md").to_string_lossy().as_ref()));
    assert!(paths.contains(&claude.join("rules/style.md").to_string_lossy().as_ref()));
    assert!(paths.contains(&proj.join("CLAUDE.md").to_string_lossy().as_ref()));
    assert!(paths.contains(&proj.join("CLAUDE.local.md").to_string_lossy().as_ref()));
    assert!(paths.contains(&proj.join(".claude/rules/local.md").to_string_lossy().as_ref()));

    // Scope: the two under ~/.claude carry no project, the three under the
    // project carry its path.
    let global: Vec<_> = report.files.iter().filter(|f| f.scope() == "global").collect();
    assert_eq!(global.len(), 2);
    assert!(report
        .files
        .iter()
        .filter(|f| f.scope() == "project")
        .all(|f| f.file.project.as_deref() == Some(proj.to_string_lossy().as_ref())));

    // The global file's home-anchored references: the present one is silent,
    // the retired one is a finding.
    let global_md = report
        .files
        .iter()
        .find(|f| f.file.path == claude.join("CLAUDE.md").to_string_lossy())
        .unwrap();
    let got = refs(&global_md.findings);
    assert_eq!(
        got,
        ["~/.claude/rules/retired.md".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "the rule that exists resolves and is not flagged"
    );

    // The rows landed in the database exactly as reported.
    let rows = store.claudemd_files().unwrap();
    assert_eq!(rows.len(), 5);

    // Rescan after an edit: the changed file's size and hash move, the row count
    // does not.
    let proj_claude = proj.join("CLAUDE.md");
    let before = rows
        .iter()
        .find(|r| r.path == proj_claude.to_string_lossy())
        .unwrap()
        .clone();
    std::fs::write(&proj_claude, "# Proj\n\nOne line, and then a second one.\n").unwrap();
    claudemd::scan(&mut store).unwrap();
    let after = store
        .claudemd_files()
        .unwrap()
        .into_iter()
        .find(|r| r.path == proj_claude.to_string_lossy())
        .unwrap();
    assert_eq!(store.claudemd_files().unwrap().len(), 5);
    assert!(after.bytes > before.bytes);
    assert_ne!(after.hash, before.hash);
    assert_eq!(after.est_tokens, claudemd::est_tokens(after.bytes));

    // Rescan after a delete: the row goes with the file, and the scan says so.
    std::fs::remove_file(proj.join("CLAUDE.local.md")).unwrap();
    let report = claudemd::scan(&mut store).unwrap();
    assert_eq!(
        report.removed,
        vec![proj.join("CLAUDE.local.md").to_string_lossy().into_owned()]
    );
    let rows = store.claudemd_files().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(!rows.iter().any(|r| r.path.ends_with("CLAUDE.local.md")));

    // The delete helper reports a miss rather than claiming a write landed.
    assert!(!store
        .delete_claudemd_file(&proj.join("CLAUDE.local.md").to_string_lossy())
        .unwrap());
    assert!(!store.delete_claudemd_file("/no/such/file.md").unwrap());
    assert!(store
        .delete_claudemd_file(&proj_claude.to_string_lossy())
        .unwrap());
}

#[test]
fn unreadable_files_warn_and_the_scan_carries_on() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    // Invalid UTF-8 is not text, and a detector running on replacement
    // characters would report references nobody wrote.
    std::fs::write(proj.join("CLAUDE.md"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
    std::fs::write(proj.join("CLAUDE.local.md"), "# Fine\n\nStill counted.\n").unwrap();

    let mut store = sandbox.store();
    seed_session(&mut store, "s1", &proj, &now());

    let report = claudemd::scan(&mut store).unwrap();
    assert_eq!(report.files.len(), 1, "the readable one is still inventoried");
    assert!(report.files[0].file.path.ends_with("CLAUDE.local.md"));
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("CLAUDE.md"), "{:?}", report.warnings);
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

#[test]
fn monthly_burden_is_est_tokens_times_sessions_in_the_window() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    let other = sandbox.project("other");
    let claude = sandbox.claude_dir();
    std::fs::write(claude.join("CLAUDE.md"), "# Global\n\nOne rule for everyone.\n").unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "# Proj\n\nA project rule.\n").unwrap();

    let mut store = sandbox.store();
    for i in 0..3 {
        seed_session(&mut store, &format!("in-{i}"), &proj, &now());
    }
    // Outside the 30-day window: it happened, it just does not describe what
    // this file costs now.
    seed_session(&mut store, "old", &proj, &days_ago(60));
    for i in 0..2 {
        seed_session(&mut store, &format!("other-{i}"), &other, &now());
    }

    let report = claudemd::scan(&mut store).unwrap();
    let project_file = report
        .files
        .iter()
        .find(|f| f.scope() == "project")
        .unwrap();
    assert_eq!(project_file.sessions_30d, 3, "its own project's sessions only");
    assert_eq!(
        project_file.est_tokens_month,
        project_file.file.est_tokens * 3
    );

    // A global file is loaded by every Claude Code session, whatever project it
    // ran in.
    let global_file = report.files.iter().find(|f| f.scope() == "global").unwrap();
    assert_eq!(global_file.sessions_30d, 5);
    assert_eq!(global_file.est_tokens_month, global_file.file.est_tokens * 5);
    assert_eq!(
        report.est_tokens_month(),
        project_file.est_tokens_month + global_file.est_tokens_month
    );
}

#[test]
fn codex_sessions_do_not_count_toward_a_claude_md_burden() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    let codex_only = sandbox.project("codex-only");
    let claude = sandbox.claude_dir();
    std::fs::write(claude.join("CLAUDE.md"), "# Global\n\nOne rule for everyone.\n").unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "# Proj\n\nA project rule.\n").unwrap();
    std::fs::write(codex_only.join("CLAUDE.md"), "# Codex only\n\nA rule.\n").unwrap();

    let mut store = sandbox.store();
    for i in 0..2 {
        seed_session(&mut store, &format!("cc-{i}"), &proj, &now());
    }
    // Codex never loads a CLAUDE.md, in this project or any other.
    for i in 0..5 {
        seed_session_from(&mut store, &format!("cx-{i}"), &proj, &now(), "codex");
    }
    for i in 0..3 {
        seed_session_from(&mut store, &format!("cxo-{i}"), &codex_only, &now(), "codex");
    }

    let report = claudemd::scan(&mut store).unwrap();
    let file = |name: &str| {
        report
            .files
            .iter()
            .find(|f| f.file.path.starts_with(name))
            .unwrap_or_else(|| panic!("{name} not inventoried"))
    };

    let project_file = file(proj.to_string_lossy().as_ref());
    assert_eq!(project_file.sessions_30d, 2, "the two Claude Code ones only");
    assert_eq!(
        project_file.est_tokens_month,
        project_file.file.est_tokens * 2
    );

    // A global file is loaded by every Claude Code session and by no Codex one.
    let global_file = report.files.iter().find(|f| f.scope() == "global").unwrap();
    assert_eq!(global_file.sessions_30d, 2);
    assert_eq!(global_file.est_tokens_month, global_file.file.est_tokens * 2);

    // Inventoried because the project exists, at zero because nothing there
    // ever loaded it.
    let codex_file = file(codex_only.to_string_lossy().as_ref());
    assert_eq!(codex_file.sessions_30d, 0);
    assert_eq!(codex_file.est_tokens_month, 0);
    assert!(codex_file.file.est_tokens > 0, "the file itself is still sized");
}

#[test]
fn oversize_finding_carries_the_monthly_burden_of_the_whole_file() {
    let sandbox = Sandbox::new();
    let proj = sandbox.project("proj");
    std::fs::copy(fixture("oversized.md"), proj.join("CLAUDE.md")).unwrap();

    let mut store = sandbox.store();
    for i in 0..4 {
        seed_session(&mut store, &format!("s{i}"), &proj, &now());
    }

    let report = claudemd::scan(&mut store).unwrap();
    let f = &report.files[0];
    let finding = f
        .findings
        .iter()
        .find(|x| x.kind.as_str() == "oversize")
        .unwrap();
    assert_eq!(finding.est_tokens, f.file.est_tokens);
    assert_eq!(finding.est_tokens_month, f.file.est_tokens * 4);
    assert_eq!(finding.est_tokens_month, f.est_tokens_month);
}

// ---------------------------------------------------------------------------
// .mcp.json
// ---------------------------------------------------------------------------

#[test]
fn mcp_json_helper_reads_the_valid_and_warns_on_the_malformed() {
    let sandbox = Sandbox::new();
    let good = sandbox.project("good");
    let bad = sandbox.project("bad");
    let bare = sandbox.project("bare");
    std::fs::write(
        good.join(".mcp.json"),
        r#"{"mcpServers": {"linear": {"command": "npx"}, "github": {"command": "npx"}}}"#,
    )
    .unwrap();
    std::fs::write(bad.join(".mcp.json"), "{ this is not json").unwrap();

    let mut store = sandbox.store();
    seed_session(&mut store, "g", &good, &now());
    seed_session(&mut store, "b", &bad, &now());
    seed_session(&mut store, "n", &bare, &now());

    let found = claudemd::project_mcp_servers(&store).unwrap();
    assert_eq!(
        found.by_project.get(good.to_string_lossy().as_ref()),
        Some(&vec!["github".to_string(), "linear".to_string()]),
        "server names, sorted"
    );
    assert!(!found.by_project.contains_key(bad.to_string_lossy().as_ref()));
    assert!(!found.by_project.contains_key(bare.to_string_lossy().as_ref()));
    assert_eq!(found.warnings.len(), 1);
    assert!(
        found.warnings[0].contains(bad.to_string_lossy().as_ref()),
        "{:?}",
        found.warnings
    );

    // Read-only: the scanner stores nothing about `.mcp.json`, and never edits
    // one.
    let before = std::fs::read(good.join(".mcp.json")).unwrap();
    claudemd::scan(&mut store).unwrap();
    assert_eq!(std::fs::read(good.join(".mcp.json")).unwrap(), before);
    assert!(store
        .claudemd_files()
        .unwrap()
        .iter()
        .all(|r| !r.path.ends_with(".mcp.json")));
}

// ---------------------------------------------------------------------------
// Sandbox: global env lock + tempdir wiring
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
        // `claude_dir()`'s parent is the home a `~/…` reference resolves
        // against, so this one override sandboxes both halves of the scan.
        std::env::set_var("PIGGY_CLAUDE_DIR", dir.path().join(".claude"));
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        Sandbox { _guard: guard, dir }
    }

    fn claude_dir(&self) -> PathBuf {
        self.dir.path().join(".claude")
    }

    /// A project directory under the sandbox home, created.
    fn project(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn store(&self) -> Store {
        Store::open(&self.dir.path().join("piggy")).unwrap()
    }
}

/// One Claude Code session in `project` whose last activity is `ended_at`.
fn seed_session(store: &mut Store, id: &str, project: &Path, ended_at: &str) {
    seed_session_from(store, id, project, ended_at, "claude-code");
}

/// One session in `project` from a named tool (`sessions.source`).
fn seed_session_from(store: &mut Store, id: &str, project: &Path, ended_at: &str, source: &str) {
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "claude-sonnet-5".to_string(),
        ModelTokens {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: 0,
        },
    );
    let parse = SessionParse {
        session_id: id.to_string(),
        source: source.to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some(project.to_string_lossy().into_owned()),
        git_branch: None,
        first_ts: Some(ended_at.to_string()),
        last_ts: Some(ended_at.to_string()),
        models,
        n_assistant_msgs: 1,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: std::collections::BTreeMap::new(),
        context: std::collections::BTreeMap::new(),
        tasks: Default::default(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, &Pricing::embedded(), &format!("/f/{id}.jsonl"), 1, 1)
        .unwrap();
}

/// Timestamps in the shape real logs carry (`…Z`, milliseconds), because the
/// 30-day window is a string comparison against `Period::cutoff()`.
fn now() -> String {
    stamp(0)
}

fn days_ago(n: i64) -> String {
    stamp(n)
}

fn stamp(days_ago: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
