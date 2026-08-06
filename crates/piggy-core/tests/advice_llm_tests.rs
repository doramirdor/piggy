//! The M5.4 advice pass, everything that does not need weights.
//!
//! Which is almost all of it. What is faked here is the model's raw output (a
//! hand-written string in the shape a 4B actually returns) and the tokenizer (a
//! trait, so three lines). What is real is the fact sheet, the whole guard, the
//! draft checks, the cache, the sectioning, and every prompt string.
//!
//! The four tests that genuinely need a downloaded model live in
//! `advisor_live_tests.rs` behind `#[ignore]`.

use std::collections::BTreeMap;

use piggy_core::advice::{
    self, basis, ActionKind, Candidate, EvidenceRow, Params, Prerequisite, RISK_CONFIG_MOVE,
    RISK_CONTENT_EDIT, RISK_TOGGLE,
};
use piggy_core::advisor::cache::{self, AdviceCache, AdviceOverlay, Draft};
use piggy_core::advisor::draft::{self, DraftReject};
use piggy_core::advisor::facts::{AdviceInput, Facts, FloorTrend};
use piggy_core::advisor::guard::{self, Allowlist, MAX_RATIONALE};
use piggy_core::advisor::prompts::{
    self, DRAFT_CLOSE, DRAFT_OPEN, SUGGEST_EXAMPLE_ID, SUGGEST_EXAMPLE_WHY,
};
use piggy_core::attribution::{Badge, SaverAttribution, Stream, StreamStat};
use piggy_core::claudemd::{
    ClaudemdReport, Finding, FindingKind, ProjectMcpServers, ScannedFile,
};
use piggy_core::insights::{Insight, Severity};
use piggy_core::ledger::{Ledger, LedgerRow, ProjectRow};
use piggy_core::parser::{CTX_CONVERSATION, CTX_FLOOR};
use piggy_core::registry::Entry;
use piggy_core::store::{ClaudemdFile, McpManifest, SCOPE_USER};
use piggy_core::sweep::{self, SweepItem, SweepReport};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PROJECT: &str = "/Users/dor/Documents/code/Stacked";

fn ledger() -> Ledger {
    Ledger {
        rows: vec![
            LedgerRow { kind: CTX_FLOOR.into(), tokens: 700_000, n: 100 },
            LedgerRow { kind: CTX_CONVERSATION.into(), tokens: 250_000, n: 100 },
        ],
        projects: vec![ProjectRow {
            project: PROJECT.into(),
            sessions: 40,
            msgs: 90,
            floor_tokens: 700_000,
            work_tokens: 300_000,
        }],
        cost_units: 1_400_000.0,
        write_weight: 1.4,
    }
}

/// A window with a different per-session floor, so the trend has a direction.
fn window(sessions: u64, floor_tokens: u64) -> Ledger {
    Ledger {
        rows: Vec::new(),
        projects: vec![ProjectRow {
            project: PROJECT.into(),
            sessions,
            msgs: sessions * 2,
            floor_tokens,
            work_tokens: 1_000,
        }],
        cost_units: 0.0,
        write_weight: 1.0,
    }
}

fn findings() -> Vec<Insight> {
    vec![Insight {
        id: "floor-dominates".into(),
        severity: Severity::High,
        title: "70% of your tokens went to starting sessions".into(),
        detail: "700,000 of 1,000,000 cache-write tokens were the session floor.".into(),
        tokens: 700_000,
        action: "Fewer, longer sessions pay this once.".into(),
    }]
}

fn sweep_report() -> SweepReport {
    SweepReport {
        sessions_considered: 200,
        items: vec![SweepItem {
            idx: 1,
            kind: "mcp".into(),
            id: "quiet-server".into(),
            source: None,
            used: 0,
            used_windowed: true,
            est_tokens: 1_200,
            cost_basis: sweep::COST_BASIS_ESTIMATE.into(),
            tokens_estimated: true,
            scope_to: None,
            recommend_disable: true,
            reason: "never invoked".into(),
        }],
    }
}

fn manifest(server: &str, tokens: i64) -> McpManifest {
    McpManifest {
        server_key: server.into(),
        scope: SCOPE_USER.into(),
        config_hash: "abc".into(),
        tool_count: 7,
        schema_bytes: tokens * 4,
        schema_tokens: tokens,
        tokenizer: "qwen3-4b-instruct-2507".into(),
        measured_at: "2026-01-01T00:00:00Z".into(),
        ok: true,
        error: None,
    }
}

fn claudemd_report() -> ClaudemdReport {
    ClaudemdReport {
        files: vec![ScannedFile {
            file: ClaudemdFile {
                path: format!("{PROJECT}/CLAUDE.md"),
                project: Some(PROJECT.into()),
                bytes: 12_000,
                est_tokens: 3_400,
                hash: "hash-of-the-file".into(),
                mtime_ns: 0,
                last_scanned: "2026-01-01T00:00:00Z".into(),
            },
            sessions_30d: 40,
            est_tokens_month: 136_000,
            findings: vec![Finding {
                id: "oversize:CLAUDE.md".into(),
                kind: FindingKind::Oversize { threshold: 2_000 },
                path: format!("{PROJECT}/CLAUDE.md"),
                claim: "This file is oversized".into(),
                detail: "3,400 estimated tokens against a 2,000 line".into(),
                est_tokens: 3_400,
                est_tokens_month: 136_000,
                action: "Trim it".into(),
            }],
        }],
        removed: Vec::new(),
        warnings: Vec::new(),
    }
}

fn project_mcp() -> ProjectMcpServers {
    let mut by_project = BTreeMap::new();
    by_project.insert(PROJECT.to_string(), vec!["quiet-server".to_string()]);
    ProjectMcpServers {
        by_project,
        warnings: Vec::new(),
    }
}

fn server_usage() -> BTreeMap<String, BTreeMap<String, u64>> {
    let mut inner = BTreeMap::new();
    inner.insert(PROJECT.to_string(), 43u64);
    let mut out = BTreeMap::new();
    out.insert("github".to_string(), inner);
    out
}

/// A minimal catalog entry. `Entry` is only ever built by deserializing the
/// catalog, and every field the sheet does not read defaults.
fn entry(id: &str, name: &str) -> Entry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": name,
        "description": "Drops stale context out of the session floor.",
    }))
    .expect("a minimal catalog entry")
}

/// A saver with one settled stream and one that is still waiting, which is the
/// pair the sheet has to describe differently.
fn attribution(reduced_pct: f64) -> SaverAttribution {
    SaverAttribution {
        saver_id: "honey-for-devs".into(),
        n_on: 60,
        n_off: 55,
        on_by_source: BTreeMap::from([("rotation".to_string(), 60usize)]),
        off_by_source: BTreeMap::from([("holdout".to_string(), 55usize)]),
        streams: vec![StreamStat {
            stream: Stream::Output,
            n_on: 60,
            n_off: 55,
            median_on: 1_000.0,
            median_off: 1_000.0 / (1.0 - reduced_pct / 100.0),
            delta: Some(reduced_pct / 100.0),
            ci: Some((reduced_pct / 100.0 - 0.02, reduced_pct / 100.0 + 0.02)),
            badge: Badge::Measured,
        }],
        turns: StreamStat {
            stream: Stream::Turns,
            n_on: 2,
            n_off: 2,
            median_on: 4.0,
            median_off: 4.0,
            delta: None,
            ci: None,
            badge: Badge::Measuring,
        },
    }
}

fn evidence(label: &str, value: &str, b: &str) -> EvidenceRow {
    EvidenceRow {
        label: label.into(),
        value: value.into(),
        basis: b.into(),
    }
}

fn candidate(
    id: &str,
    kind: ActionKind,
    target: &str,
    title: &str,
    est: i64,
    params: Params,
) -> Candidate {
    Candidate {
        id: id.into(),
        kind,
        target: target.into(),
        title: title.into(),
        evidence: vec![evidence(
            "Tokens a month it costs you",
            "~14,200 tokens",
            basis::ESTIMATED,
        )],
        est_tokens_month: est,
        risk_tier: match kind {
            ActionKind::SaverMix => RISK_TOGGLE,
            ActionKind::ServerDisable | ActionKind::ServerScope => RISK_CONFIG_MOVE,
            _ => RISK_CONTENT_EDIT,
        },
        prerequisites: match kind {
            ActionKind::ClaudemdTrim => vec![Prerequisite::NeedsAdvisor],
            _ => Vec::new(),
        },
        fingerprint: "hash-of-the-file".into(),
        params,
        new_content: None,
        status: "open".into(),
    }
}

fn candidates() -> Vec<Candidate> {
    vec![
        candidate(
            "server-disable-1111111111111111",
            ActionKind::ServerDisable,
            "quiet-server (user scope)",
            "Turn off the quiet-server server",
            48_000,
            Params::ServerDisable {
                item_kind: "mcp".into(),
                id: "quiet-server".into(),
                source: None,
                n_sessions: 200,
            },
        ),
        candidate(
            "claudemd-trim-2222222222222222",
            ActionKind::ClaudemdTrim,
            format!("{PROJECT}/CLAUDE.md").as_str(),
            "Trim Stacked's CLAUDE.md",
            136_000,
            Params::Claudemd {
                path: format!("{PROJECT}/CLAUDE.md"),
            },
        ),
        candidate(
            "saver-mix-3333333333333333",
            ActionKind::SaverMix,
            "honey-for-devs",
            "Turn Honey off",
            9_000,
            Params::SaverMix {
                saver: "honey-for-devs".into(),
                turn_on: false,
            },
        ),
    ]
}

/// The whole sheet, every block populated.
fn facts() -> Facts {
    let ledger = ledger();
    let recent = window(10, 190_000);
    let prior = window(30, 510_000);
    let insights = findings();
    let sweep = sweep_report();
    let manifests = vec![manifest("github", 4_100)];
    let usage = server_usage();
    let claudemd = claudemd_report();
    let mcp = project_mcp();
    let e = entry("honey-for-devs", "Honey");
    let attr = attribution(31.0);
    let savers = vec![(&e, true, &attr)];
    let cands = candidates();
    Facts::advice(&AdviceInput {
        ledger: &ledger,
        trend: Some(FloorTrend {
            recent: &recent,
            prior: &prior,
        }),
        insights: &insights,
        sweep: Some(&sweep),
        manifests: &manifests,
        server_usage: &usage,
        claudemd: &claudemd,
        project_mcp: &mcp,
        savers: &savers,
        headline: None,
        candidates: &cands,
    })
}

/// The same sheet with nothing but a ledger behind it.
fn bare_facts() -> Facts {
    let ledger = ledger();
    let claudemd = ClaudemdReport::default();
    let mcp = ProjectMcpServers::default();
    let usage = BTreeMap::new();
    Facts::advice(&AdviceInput {
        ledger: &ledger,
        trend: None,
        insights: &[],
        sweep: None,
        manifests: &[],
        server_usage: &usage,
        claudemd: &claudemd,
        project_mcp: &mcp,
        savers: &[],
        headline: None,
        candidates: &[],
    })
}

/// One pick, in the shape a model returns it.
fn pick(id: &str, why: &str) -> String {
    format!("{{\"picks\":[{{\"id\":\"{id}\",\"why\":\"{why}\"}}]}}")
}

// ---------------------------------------------------------------------------
// Facts v2
// ---------------------------------------------------------------------------

#[test]
fn advice_facts_carry_every_block() {
    let f = facts();
    for key in [
        "totals",
        "context_ledger",
        "projects",
        "findings",
        "configuration",
        "measured_manifests",
        "server_usage",
        "project_mcp",
        "claudemd",
        "savers",
        "candidates",
        "candidate_totals",
        "note",
    ] {
        assert!(f.value.get(key).is_some(), "{key} missing from the sheet");
    }

    // And a sheet with nothing behind it carries no empty arrays: an empty
    // block is an invitation to write about nothing.
    let bare = bare_facts();
    for key in [
        "configuration",
        "measured_manifests",
        "server_usage",
        "project_mcp",
        "claudemd",
        "savers",
        "holdout",
        "candidates",
    ] {
        assert!(bare.value.get(key).is_none(), "{key} should be omitted");
    }
    assert!(bare.value.get("totals").is_some());
    assert!(bare.value.get("note").is_some());
}

#[test]
fn candidate_evidence_numbers_are_quotable() {
    // The property that lets a rationale cite the card: the evidence value is a
    // formatted string, and the allow-list reads numbers out of strings.
    let allow = Allowlist::from_facts(&facts());
    assert!(allow.offenders("that is ~14,200 tokens a month").is_empty());
    assert!(!allow.offenders("that is 14,300 tokens a month").is_empty());
}

#[test]
fn candidate_ids_are_the_only_ids_on_the_advice_sheet() {
    let f = facts();
    assert!(f.insight_ids.is_empty(), "the advice sheet annotates nothing");
    assert_eq!(
        f.candidate_ids,
        candidates().into_iter().map(|c| c.id).collect::<Vec<_>>()
    );
}

#[test]
fn basenames_only_never_home_paths() {
    let s = facts().prompt_json();
    assert!(s.contains("Stacked"), "the project should be named");
    assert!(!s.contains("/Users/"), "the sheet leaked a full path: {s}");
    if let Some(home) = dirs::home_dir() {
        assert!(!s.contains(&home.to_string_lossy().to_string()));
    }
}

#[test]
fn a_burden_is_never_labelled_a_saving() {
    let f = facts();
    let items = f.value["candidates"].as_array().unwrap();
    let trim = items
        .iter()
        .find(|c| c["kind"] == "claudemd-trim")
        .expect("the trim candidate is on the sheet");
    assert_eq!(trim["est_is"], "burden");
    let server = items
        .iter()
        .find(|c| c["kind"] == "server-disable")
        .unwrap();
    assert_eq!(server["est_is"], "saving");

    // And the two totals never merge. 136,000 of burden must not appear inside
    // a savings figure.
    assert_eq!(f.value["candidate_totals"]["savings_tokens_month"], 57_000);
    assert_eq!(f.value["candidate_totals"]["burden_tokens_month"], 136_000);
}

#[test]
fn unsettled_streams_still_withhold_their_medians() {
    let f = facts();
    let streams = f.value["savers"][0]["streams"].as_array().unwrap();
    let waiting = streams
        .iter()
        .find(|s| s["stream"] == Stream::Turns.label())
        .expect("the turns arm is on the sheet");
    assert!(waiting.get("result").is_some());
    assert!(waiting.get("per_session_with_it_on").is_none());
    assert!(waiting.get("reduced_by_pct").is_none());
    assert!(waiting.get("increased_by_pct").is_none());
}

#[test]
fn a_regression_is_quotable_under_the_key_that_says_so() {
    let ledger = ledger();
    let claudemd = ClaudemdReport::default();
    let mcp = ProjectMcpServers::default();
    let usage = BTreeMap::new();
    let e = entry("honey-for-devs", "Honey");
    // A saver that made the stream 12% worse.
    let attr = attribution(-12.0);
    let savers = vec![(&e, true, &attr)];
    let f = Facts::advice(&AdviceInput {
        ledger: &ledger,
        trend: None,
        insights: &[],
        sweep: None,
        manifests: &[],
        server_usage: &usage,
        claudemd: &claudemd,
        project_mcp: &mcp,
        savers: &savers,
        headline: None,
        candidates: &[],
    });
    let stream = &f.value["savers"][0]["streams"][0];
    assert_eq!(stream["increased_by_pct"], 12.0);
    assert!(stream.get("reduced_by_pct").is_none());
    // The magnitude is positive, so the one honest sentence about it is
    // writable rather than dropped as a fabrication.
    assert!(Allowlist::from_facts(&f)
        .offenders("12% more cache write")
        .is_empty());
}

#[test]
fn the_floor_trend_compares_two_adjacent_windows() {
    let f = facts();
    let project = &f.value["projects"][0];
    // 190,000 over 10 sessions against 510,000 over 30: 19,000 against 17,000.
    assert_eq!(project["floor_tokens_per_session_last_7d"], 19_000);
    assert_eq!(project["floor_tokens_per_session_prior_23d"], 17_000);
    assert_eq!(project["floor_direction"], "up");

    // No trend without both windows, rather than a direction invented from one.
    assert!(bare_facts().value["projects"][0]
        .get("floor_direction")
        .is_none());
}

#[test]
fn the_advice_sheet_fits_the_16k_window() {
    // Every cap at once: 20 candidates x 6 evidence rows, 16 CLAUDE.md files,
    // 12 savers, 16 manifests, 10 ledger rows, 10 projects, 8 findings.
    let ledger = Ledger {
        rows: (0..12)
            .map(|i| LedgerRow {
                kind: format!("floor:component_number_{i}"),
                tokens: 100_000 + i,
                n: 100,
            })
            .collect(),
        projects: (0..14)
            .map(|i| ProjectRow {
                project: format!("/Users/someone/code/a-project-with-a-long-name-{i}"),
                sessions: 40 + i,
                msgs: 90,
                floor_tokens: 700_000,
                work_tokens: 300_000,
            })
            .collect(),
        cost_units: 1_400_000.0,
        write_weight: 1.4,
    };
    let insights: Vec<Insight> = (0..10)
        .map(|i| Insight {
            id: format!("finding-number-{i}"),
            severity: Severity::High,
            title: "70% of your tokens went to starting sessions".into(),
            detail: "x".repeat(200),
            tokens: 700_000,
            action: "Fewer, longer sessions pay this once.".into(),
        })
        .collect();
    let sweep = SweepReport {
        sessions_considered: 200,
        items: (0..20)
            .map(|i| SweepItem {
                idx: i,
                kind: "mcp".into(),
                id: format!("a-server-with-a-name-{i}"),
                source: None,
                used: 0,
                used_windowed: true,
                est_tokens: 1_200,
                cost_basis: sweep::COST_BASIS_ESTIMATE.into(),
                tokens_estimated: true,
                scope_to: None,
                recommend_disable: true,
                reason: "never invoked".into(),
            })
            .collect(),
    };
    let manifests: Vec<McpManifest> = (0..20)
        .map(|i| manifest(&format!("a-server-with-a-name-{i}"), 4_000 + i))
        .collect();
    let mut usage = BTreeMap::new();
    for i in 0..20 {
        let mut inner = BTreeMap::new();
        for p in 0..12 {
            inner.insert(format!("/Users/someone/code/a-project-{p}"), 40u64 + p);
        }
        usage.insert(format!("a-server-with-a-name-{i}"), inner);
    }
    let claudemd = ClaudemdReport {
        files: (0..20)
            .map(|i| ScannedFile {
                file: ClaudemdFile {
                    path: format!("/Users/someone/code/project-{i}/CLAUDE.md"),
                    project: Some(format!("/Users/someone/code/project-{i}")),
                    bytes: 12_000,
                    est_tokens: 3_400,
                    hash: "h".into(),
                    mtime_ns: 0,
                    last_scanned: "2026-01-01T00:00:00Z".into(),
                },
                sessions_30d: 40,
                est_tokens_month: 136_000 + i,
                findings: Vec::new(),
            })
            .collect(),
        removed: Vec::new(),
        warnings: Vec::new(),
    };
    let mut by_project = BTreeMap::new();
    for p in 0..14 {
        by_project.insert(
            format!("/Users/someone/code/a-project-{p}"),
            (0..10).map(|s| format!("a-server-with-a-name-{s}")).collect(),
        );
    }
    let mcp = ProjectMcpServers {
        by_project,
        warnings: Vec::new(),
    };
    let entries: Vec<Entry> = (0..14)
        .map(|i| entry(&format!("saver-number-{i}"), &format!("Saver {i}")))
        .collect();
    let attrs: Vec<SaverAttribution> = (0..14).map(|_| attribution(31.0)).collect();
    let savers: Vec<(&Entry, bool, &SaverAttribution)> = entries
        .iter()
        .zip(attrs.iter())
        .map(|(e, a)| (e, true, a))
        .collect();
    let cands: Vec<Candidate> = (0..24)
        .map(|i| {
            let mut c = candidate(
                &format!("server-disable-{i:016}"),
                ActionKind::ServerDisable,
                "a-server-with-a-name (user scope)",
                "Turn off the a-server-with-a-name server",
                48_000 + i,
                Params::ServerDisable {
                    item_kind: "mcp".into(),
                    id: format!("a-server-with-a-name-{i}"),
                    source: None,
                    n_sessions: 200,
                },
            );
            c.evidence = (0..8)
                .map(|e| {
                    evidence(
                        &format!("A reasonably long evidence label number {e}"),
                        "~1,234,567 tokens",
                        basis::ESTIMATED,
                    )
                })
                .collect();
            c
        })
        .collect();

    let f = Facts::advice(&AdviceInput {
        ledger: &ledger,
        trend: None,
        insights: &insights,
        sweep: Some(&sweep),
        manifests: &manifests,
        server_usage: &usage,
        claudemd: &claudemd,
        project_mcp: &mcp,
        savers: &savers,
        headline: None,
        candidates: &cands,
    });

    let json = f.prompt_json();
    // ~3 characters per token is a deliberate over-estimate for dense JSON,
    // which tokenizes worse than prose.
    let est = json.len() / 3;
    println!("saturated advice sheet: {} bytes, ~{est} tokens", json.len());
    assert!(
        est < 11_000,
        "the advice sheet is ~{est} tokens, too close to the 16,384 window"
    );
}

// ---------------------------------------------------------------------------
// Determinism: "same facts, same advice" starts here
// ---------------------------------------------------------------------------

#[test]
fn the_same_inputs_produce_byte_identical_facts() {
    let a = facts();
    let b = facts();
    assert_eq!(a.prompt_json(), b.prompt_json());
    assert_eq!(a.hash(), b.hash());
}

#[test]
fn input_order_does_not_move_the_payload() {
    let build = |order: &[&str]| {
        let ledger = ledger();
        let claudemd = ClaudemdReport::default();
        let mut by_project = BTreeMap::new();
        let mut usage = BTreeMap::new();
        for name in order {
            by_project.insert(format!("/code/{name}"), vec![name.to_string()]);
            let mut inner = BTreeMap::new();
            inner.insert(format!("/code/{name}"), 7u64);
            usage.insert(name.to_string(), inner);
        }
        let mcp = ProjectMcpServers {
            by_project,
            warnings: Vec::new(),
        };
        Facts::advice(&AdviceInput {
            ledger: &ledger,
            trend: None,
            insights: &[],
            sweep: None,
            manifests: &[],
            server_usage: &usage,
            claudemd: &claudemd,
            project_mcp: &mcp,
            savers: &[],
            headline: None,
            candidates: &[],
        })
        .prompt_json()
    };
    assert_eq!(
        build(&["alpha", "beta", "gamma"]),
        build(&["gamma", "alpha", "beta"]),
        "insertion order reached the payload, which would move the cache key"
    );
}

#[test]
fn the_facts_hash_moves_when_a_number_moves() {
    let before = facts().hash();
    let ledger = ledger();
    let claudemd = ClaudemdReport::default();
    let mcp = ProjectMcpServers::default();
    let usage = BTreeMap::new();
    let mut cands = candidates();
    cands[0].evidence[0].value = "~14,201 tokens".into();
    let after = Facts::advice(&AdviceInput {
        ledger: &ledger,
        trend: None,
        insights: &[],
        sweep: None,
        manifests: &[],
        server_usage: &usage,
        claudemd: &claudemd,
        project_mcp: &mcp,
        savers: &[],
        headline: None,
        candidates: &cands,
    })
    .hash();
    assert_ne!(before, after);
    assert_eq!(before.len(), 16);
}

// ---------------------------------------------------------------------------
// Guard v2: picks
// ---------------------------------------------------------------------------

#[test]
fn a_candidate_id_outside_the_list_is_dropped() {
    let f = facts();
    let raw = r#"{"picks":[
      {"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so it costs you every session for nothing."},
      {"id":"server-disable-9999999999999999","why":"This one was invented by the model and quiet-server is not it."}
    ]}"#;
    let got = guard::accept_suggestion(raw, &f).unwrap();
    assert_eq!(got.picks.len(), 1);
    assert_eq!(got.picks[0].id, "server-disable-1111111111111111");
}

#[test]
fn the_same_candidate_twice_keeps_the_first() {
    let f = facts();
    let raw = r#"{"picks":[
      {"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so it costs you every session for nothing."},
      {"id":"server-disable-1111111111111111","why":"quiet-server again, said differently and worth nothing extra."}
    ]}"#;
    let got = guard::accept_suggestion(raw, &f).unwrap();
    assert_eq!(got.picks.len(), 1);
    assert!(got.picks[0].rationale.starts_with("Nothing has called"));
}

#[test]
fn a_rationale_over_280_chars_is_truncated_not_dropped() {
    let f = facts();
    let long = format!("Nothing has called quiet-server {}", "and it just sits there ".repeat(20));
    let got = guard::accept_suggestion(&pick("server-disable-1111111111111111", &long), &f).unwrap();
    assert_eq!(got.picks.len(), 1, "truncation must not cost the pick");
    let r = &got.picks[0].rationale;
    assert!(r.ends_with("..."), "{r}");
    assert!(
        r.chars().count() <= MAX_RATIONALE + 3,
        "{} characters",
        r.chars().count()
    );
}

#[test]
fn truncation_never_splits_a_number() {
    let f = facts();
    // A figure that starts four characters before the cut, so a naive
    // truncation would leave `1,23` behind: a number the model never wrote,
    // which the allow-list would either admit by coincidence or reject as a
    // fabrication its author did not commit.
    let filler: String = format!("quiet-server {}", "costs ".repeat(80))
        .chars()
        .take(MAX_RATIONALE - 4)
        .collect();
    let why = format!("{filler}1,234,567 tokens every single session");
    assert_eq!(
        why.chars().take(MAX_RATIONALE).collect::<String>(),
        format!("{filler}1,23"),
        "the fixture no longer cuts inside the number"
    );

    let got = guard::accept_suggestion(&pick("server-disable-1111111111111111", &why), &f).unwrap();
    assert_eq!(got.picks.len(), 1, "cutting at a space keeps the pick");
    assert!(
        !got.picks[0].rationale.contains("1,23"),
        "a half-written number reached a pick: {}",
        got.picks[0].rationale
    );
}

#[test]
fn a_fabricated_number_in_a_rationale_drops_the_pick() {
    let f = facts();
    let raw = pick(
        "server-disable-1111111111111111",
        "quiet-server is costing you 918,273 tokens a month.",
    );
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn a_hedged_rationale_is_dropped() {
    let f = facts();
    let raw = pick(
        "server-disable-1111111111111111",
        "quiet-server is probably loaded in every session.",
    );
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn reassurance_about_an_unmeasured_stream_is_dropped() {
    let f = facts();
    let raw = pick(
        "saver-mix-3333333333333333",
        "Turning Honey off has no impact on how long your sessions run.",
    );
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn the_worked_example_pasted_back_is_rejected() {
    let f = facts();
    let raw = pick("server-disable-1111111111111111", SUGGEST_EXAMPLE_WHY);
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn a_rationale_that_only_repeats_the_title_is_dropped() {
    let f = facts();
    // The card already says "Turn off the quiet-server server".
    let raw = pick(
        "server-disable-1111111111111111",
        "Turn off the quiet-server server.",
    );
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn a_rationale_naming_nothing_is_dropped() {
    let f = facts();
    let raw = pick(
        "server-disable-1111111111111111",
        "This one is worth doing before the others because it is reversible.",
    );
    assert!(guard::accept_suggestion(&raw, &f).unwrap().picks.is_empty());
}

#[test]
fn a_saver_pick_may_name_the_saver_the_reader_sees() {
    let f = facts();
    // "Honey" is on the row; `honey-for-devs` is the id nobody reads.
    let raw = pick(
        "saver-mix-3333333333333333",
        "Honey has been compared over both arms and moved nothing you can see.",
    );
    let got = guard::accept_suggestion(&raw, &f).unwrap();
    assert_eq!(got.picks.len(), 1, "the display name is a valid anchor");
}

#[test]
fn picks_are_capped() {
    let f = facts();
    let ids: Vec<String> = f.candidate_ids.clone();
    let items: Vec<String> = (0..12)
        .map(|i| {
            format!(
                "{{\"id\":\"{}\",\"why\":\"Nothing has called quiet-server in the window at all.\"}}",
                ids[i % ids.len()]
            )
        })
        .collect();
    let raw = format!("{{\"picks\":[{}]}}", items.join(","));
    let got = guard::accept_suggestion(&raw, &f).unwrap();
    // Deduplication bites before the cap here, which is the point: neither rule
    // can be defeated by repetition.
    assert!(got.picks.len() <= guard::MAX_PICKS);
}

// ---------------------------------------------------------------------------
// Guard v2: bundles
// ---------------------------------------------------------------------------

/// Two accepted picks plus a bundle over them.
fn two_picks_and_bundle(project: &str, ids: &[&str]) -> String {
    let list = ids
        .iter()
        .map(|i| format!("\"{i}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"picks":[
          {{"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so every session pays for it."}},
          {{"id":"claudemd-trim-2222222222222222","why":"CLAUDE.md in Stacked is the biggest single thing loaded before you type."}}
        ],"bundles":[{{"project":"{project}","ids":[{list}]}}]}}"#
    )
}

#[test]
fn a_bundle_naming_an_unknown_project_is_dropped() {
    let f = facts();
    let raw = two_picks_and_bundle(
        "NotAProject",
        &[
            "server-disable-1111111111111111",
            "claudemd-trim-2222222222222222",
        ],
    );
    let got = guard::accept_suggestion(&raw, &f).unwrap();
    assert_eq!(got.picks.len(), 2);
    assert!(got.bundles.is_empty());
}

#[test]
fn a_bundle_loses_ids_whose_picks_were_dropped() {
    let f = facts();
    let raw = two_picks_and_bundle(
        "Stacked",
        &[
            "server-disable-1111111111111111",
            "claudemd-trim-2222222222222222",
            "saver-mix-3333333333333333",
        ],
    );
    let got = guard::accept_suggestion(&raw, &f).unwrap();
    assert_eq!(got.bundles.len(), 1);
    // The third id was never an accepted pick, so it is not in the bundle.
    assert_eq!(
        got.bundles[0].ids,
        vec![
            "server-disable-1111111111111111".to_string(),
            "claudemd-trim-2222222222222222".to_string()
        ]
    );
}

#[test]
fn a_bundle_of_one_is_dropped() {
    let f = facts();
    let raw = two_picks_and_bundle("Stacked", &["server-disable-1111111111111111"]);
    assert!(guard::accept_suggestion(&raw, &f).unwrap().bundles.is_empty());
}

#[test]
fn an_id_is_bundled_at_most_once() {
    let f = facts();
    let raw = r#"{"picks":[
      {"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so every session pays for it."},
      {"id":"claudemd-trim-2222222222222222","why":"CLAUDE.md in Stacked is the biggest single thing loaded before you type."}
    ],"bundles":[
      {"project":"Stacked","ids":["server-disable-1111111111111111","claudemd-trim-2222222222222222"]},
      {"project":"Stacked","ids":["server-disable-1111111111111111","claudemd-trim-2222222222222222"]}
    ]}"#;
    let got = guard::accept_suggestion(raw, &f).unwrap();
    assert_eq!(got.bundles.len(), 1, "one bundle per project");
}

// ---------------------------------------------------------------------------
// Guard v2: parsing
// ---------------------------------------------------------------------------

#[test]
fn a_code_fence_or_preamble_does_not_lose_valid_picks() {
    let f = facts();
    let raw = format!(
        "Sure, here you go:\n```json\n{}\n```\nHope that helps.",
        pick(
            "server-disable-1111111111111111",
            "Nothing has called quiet-server in the window at all."
        )
    );
    assert_eq!(guard::accept_suggestion(&raw, &f).unwrap().picks.len(), 1);
}

#[test]
fn a_truncated_object_keeps_its_complete_picks() {
    let f = facts();
    let raw = r#"{"picks":[
      {"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so every session pays for it."},
      {"id":"claudemd-trim-2222222222222222","why":"CLAUDE.md in Stacked is the biggest single thing loaded before you type."},
      {"id":"saver-mix-3333333333333333","why":"Honey has been compared over both"#;
    let got = guard::accept_suggestion(raw, &f).unwrap();
    assert_eq!(got.picks.len(), 2, "the two complete picks survive the cut");
}

#[test]
fn garbage_fails_closed_rather_than_panicking() {
    let f = facts();
    for raw in [
        "",
        "I am afraid I cannot help with that.",
        "[]",
        "42",
        "null",
        "{\"picks\": \"not an array\"}",
        "{",
        "```json\n```",
    ] {
        let got = guard::accept_suggestion(raw, &f);
        assert!(got.is_err() || got.unwrap().picks.is_empty(), "raw: {raw:?}");
    }
}

// ---------------------------------------------------------------------------
// Drafts
// ---------------------------------------------------------------------------

const SOURCE: &str = "# Project rules\n\nAlways run the tests before pushing. Always run the tests \
before pushing, every time, without exception.\n\nSee docs/testing.md for the details of the \
suite and how it is wired up.\n\n## Style\n\nKeep the lines under 100 characters wide.\n";

fn wrapped(body: &str) -> String {
    format!("{DRAFT_OPEN}{body}{DRAFT_CLOSE}")
}

/// A draft that is comfortably over a tenth smaller and introduces nothing.
fn good_draft() -> String {
    "# Project rules\n\nAlways run the tests before pushing.\n\nSee docs/testing.md.\n\n## Style\n\nKeep lines under 100 characters.\n".to_string()
}

#[test]
fn a_draft_that_grows_the_file_is_rejected() {
    let bigger = format!("{SOURCE}\nAnd one more line about nothing.\n");
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(&bigger)),
        Err(DraftReject::TooLarge { .. })
    ));
}

#[test]
fn the_shrink_rule_is_exactly_a_tenth() {
    let original = "x".repeat(1_000);
    // 900 bytes is exactly 10% smaller and is accepted.
    assert!(draft::accept_draft(&original, &wrapped(&"x".repeat(900))).is_ok());
    // 910 is 9% and is not.
    assert!(matches!(
        draft::accept_draft(&original, &wrapped(&"x".repeat(910))),
        Err(DraftReject::TooLarge { .. })
    ));
}

#[test]
fn a_draft_introducing_a_path_is_rejected() {
    let sneaky = good_draft().replace("docs/testing.md.", "docs/testing.md and src/secret/plan.rs.");
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(&sneaky)),
        Err(DraftReject::NewReference(_))
    ));
}

#[test]
fn a_draft_introducing_a_url_is_rejected() {
    // The case `claudemd::path_token` would have missed entirely: it rejects
    // URLs on purpose.
    let sneaky = good_draft().replace("docs/testing.md.", "https://example.com/setup.");
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(&sneaky)),
        Err(DraftReject::NewReference(_))
    ));
}

#[test]
fn a_path_smuggled_into_a_code_fence_is_rejected() {
    // This test is the reason `refs_in` exists rather than reusing the CLAUDE.md
    // scanner, which skips fenced blocks. Do not delete it as redundant.
    let sneaky =
        "# Project rules\n\nAlways run the tests.\n\n```sh\ncat ../../etc/hosts\n```\n\n## Style\n\nShort lines.\n";
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(sneaky)),
        Err(DraftReject::NewReference(_))
    ));
}

#[test]
fn a_path_hidden_in_a_markdown_link_target_is_caught() {
    let sneaky = good_draft().replace("docs/testing.md.", "[docs](../secret/plan.md).");
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(&sneaky)),
        Err(DraftReject::NewReference(_))
    ));
}

#[test]
fn a_draft_may_drop_headings_and_may_not_add_one() {
    // Dropping "## Style" is a merge, which the spec allows.
    let merged = "# Project rules\n\nAlways run the tests. Keep lines short.\n\nSee docs/testing.md.\n";
    assert!(draft::accept_draft(SOURCE, &wrapped(merged)).is_ok());

    let invented = "# Project rules\n\nRun tests.\n\n## Style\n\nShort.\n\n## Extra\n\nNew.\n";
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(invented)),
        Err(DraftReject::NewHeading(_))
    ));
}

#[test]
fn a_heading_that_only_changes_case_or_spacing_is_not_new() {
    let restyled = "# project    RULES\n\nRun the tests.\n\n##   style ##\n\nShort lines.\n";
    assert!(
        draft::accept_draft(SOURCE, &wrapped(restyled)).is_ok(),
        "case and spacing are not a new heading"
    );
}

#[test]
fn a_draft_that_changes_a_number_is_rejected() {
    // The hole the other three checks leave: a rewrite that silently edits the
    // user's own instruction.
    let changed = good_draft().replace("100 characters", "120 characters");
    assert!(matches!(
        draft::accept_draft(SOURCE, &wrapped(&changed)),
        Err(DraftReject::NewNumber(n)) if n == "120"
    ));
}

#[test]
fn a_draft_with_no_sentinels_is_rejected() {
    assert_eq!(
        draft::accept_draft(SOURCE, &good_draft()),
        Err(DraftReject::NoSentinels)
    );
    let unclosed = format!("{DRAFT_OPEN}{}", good_draft());
    assert_eq!(
        draft::accept_draft(SOURCE, &unclosed),
        Err(DraftReject::NoSentinels)
    );
}

#[test]
fn an_empty_draft_is_rejected() {
    assert_eq!(
        draft::accept_draft(SOURCE, &wrapped("   \n  ")),
        Err(DraftReject::Empty)
    );
}

#[test]
fn crlf_survives_a_draft() {
    let crlf = SOURCE.replace('\n', "\r\n");
    let drafted = draft::accept_draft(&crlf, &wrapped(&good_draft())).expect("accepted");
    assert!(drafted.contains("\r\n"), "the source's line endings are not ours to change");
    assert!(!drafted.replace("\r\n", "").contains('\n'));
    assert!(drafted.ends_with("\r\n"));
}

#[test]
fn a_source_without_a_trailing_newline_keeps_none() {
    let no_newline = SOURCE.trim_end();
    let drafted = draft::accept_draft(no_newline, &wrapped(&good_draft())).expect("accepted");
    assert!(!drafted.ends_with('\n'));
}

#[test]
fn a_rejected_draft_leaves_the_candidate_blocked() {
    let mut c = candidates()
        .into_iter()
        .find(|c| c.kind == ActionKind::ClaudemdTrim)
        .unwrap();
    assert!(c.blocked(), "a trim candidate starts blocked");
    // Nothing calls `attach_draft`, which is what a rejection means.
    assert!(c.new_content.is_none());
    // And a draft that passed does clear it, so the test is about the rejection
    // and not about a candidate that could never unblock.
    advice::attach_draft(&mut c, &good_draft(), false).unwrap();
    assert!(!c.blocked());
}

#[test]
fn a_draft_belongs_only_to_a_trim_candidate() {
    let mut c = candidates()
        .into_iter()
        .find(|c| c.kind == ActionKind::ServerDisable)
        .unwrap();
    assert!(advice::attach_draft(&mut c, &good_draft(), false).is_err());
    assert!(c.new_content.is_none());
}

#[test]
fn attaching_a_draft_puts_the_bom_back() {
    let mut c = candidates()
        .into_iter()
        .find(|c| c.kind == ActionKind::ClaudemdTrim)
        .unwrap();
    advice::attach_draft(&mut c, "# rules\n", true).unwrap();
    assert!(c.new_content.as_deref().unwrap().starts_with('\u{FEFF}'));
}

// ---------------------------------------------------------------------------
// Sectioning
// ---------------------------------------------------------------------------

/// Roughly a token per four characters, which is all the splitter needs.
fn fake_tokens(s: &str) -> usize {
    s.len() / 4
}

#[test]
fn a_file_under_the_cap_is_one_call() {
    let sections = draft::split_sections(SOURCE, 10_000, &fake_tokens);
    // Still split (the function is pure), but nothing is over the cap and the
    // caller only splits when the whole file is.
    assert!(sections.iter().all(|s| !s.too_large));
    assert_eq!(
        sections.iter().map(|s| s.text.clone()).collect::<String>(),
        SOURCE,
        "a concatenation of the sections is the file"
    );
}

#[test]
fn a_file_over_the_cap_splits_on_level_two_headings() {
    let text = "Preamble line.\n\n## One\n\nBody one.\n\n```md\n## Not a heading\n```\n\n### Three\n\nDeeper.\n\n## Two\n\nBody two.\n";
    let sections = draft::split_sections(text, 10, &fake_tokens);
    let headings: Vec<Option<String>> = sections.iter().map(|s| s.heading.clone()).collect();
    assert_eq!(
        headings,
        vec![None, Some("One".to_string()), Some("Two".to_string())],
        "a `##` inside a fence and a `###` are not boundaries"
    );
    assert_eq!(
        sections.iter().map(|s| s.text.clone()).collect::<String>(),
        text
    );
}

#[test]
fn a_section_over_the_cap_on_its_own_is_marked_for_pass_through() {
    let text = format!("## One\n\n{}\n\n## Two\n\nShort.\n", "x".repeat(4_000));
    let sections = draft::split_sections(&text, 100, &fake_tokens);
    assert_eq!(sections.len(), 2);
    assert!(sections[0].too_large, "the enormous section is flagged");
    assert!(!sections[1].too_large);
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

fn overlay(facts_hash: &str, model: &str) -> AdviceOverlay {
    AdviceOverlay {
        facts_hash: facts_hash.into(),
        model_id: model.into(),
        suggestion: guard::Suggestion::default(),
        drafts: BTreeMap::from([(
            cache::draft_key(model, "claudemd-trim-2222222222222222", "hash-of-the-file"),
            Draft {
                candidate_id: "claudemd-trim-2222222222222222".into(),
                text: good_draft(),
                had_bom: false,
            },
        )]),
    }
}

#[test]
fn the_same_facts_hash_hits_the_cache() {
    let mut c = AdviceCache::default();
    c.put(overlay("aaaa", "qwen3-4b-instruct-2507"));
    assert!(c.get(&cache::advice_key("qwen3-4b-instruct-2507", "aaaa")).is_some());
    assert!(c.get(&cache::advice_key("qwen3-4b-instruct-2507", "bbbb")).is_none());
    c.clear();
    assert!(c.get(&cache::advice_key("qwen3-4b-instruct-2507", "aaaa")).is_none());
}

#[test]
fn two_models_do_not_share_an_overlay() {
    assert_ne!(
        cache::advice_key("qwen3-4b-instruct-2507", "aaaa"),
        cache::advice_key("gemma-3-4b-it", "aaaa"),
        "one model's prose must never render under the other's name"
    );
}

#[test]
fn a_draft_key_follows_the_file_not_the_ledger() {
    let a = cache::draft_key("qwen3-4b-instruct-2507", "claudemd-trim-1", "file-hash-1");
    let b = cache::draft_key("qwen3-4b-instruct-2507", "claudemd-trim-1", "file-hash-1");
    assert_eq!(a, b, "the whole ledger can move without re-drafting a file");
    let moved = cache::draft_key("qwen3-4b-instruct-2507", "claudemd-trim-1", "file-hash-2");
    assert_ne!(a, moved, "a changed file is a different draft");
}

#[test]
fn the_overlay_names_the_candidates_it_drafted() {
    let o = overlay("aaaa", "qwen3-4b-instruct-2507");
    assert_eq!(o.drafted_candidates(), vec!["claudemd-trim-2222222222222222"]);
}

#[test]
fn the_same_facts_produce_a_byte_identical_advice_list() {
    // Acceptance criterion 6: same facts hash twice, byte-identical list.
    let f = facts();
    let raw = r#"{"picks":[
      {"id":"saver-mix-3333333333333333","why":"Honey has been compared over both arms and moved nothing you can see."},
      {"id":"server-disable-1111111111111111","why":"Nothing has called quiet-server in the window, so every session pays for it."}
    ]}"#;
    let order = |raw: &str| {
        let s = guard::accept_suggestion(raw, &f).unwrap();
        let mut c = candidates();
        advice::apply_llm_order(&mut c, &s.picks);
        c.into_iter().map(|c| c.id).collect::<Vec<_>>()
    };
    let first = order(raw);
    assert_eq!(first, order(raw));
    // The model's two picks come first in its order; the one it ignored keeps
    // its deterministic place behind them.
    assert_eq!(
        first,
        vec![
            "saver-mix-3333333333333333".to_string(),
            "server-disable-1111111111111111".to_string(),
            "claudemd-trim-2222222222222222".to_string(),
        ]
    );
}

#[test]
fn no_picks_means_exactly_the_deterministic_order() {
    let before: Vec<String> = candidates().into_iter().map(|c| c.id).collect();
    let mut c = candidates();
    advice::apply_llm_order(&mut c, &[]);
    assert_eq!(c.into_iter().map(|c| c.id).collect::<Vec<_>>(), before);
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

#[test]
fn the_suggest_prompt_shows_the_example_the_guard_matches() {
    let prompt = prompts::suggest_preamble();
    assert!(prompt.contains(SUGGEST_EXAMPLE_ID));
    assert!(prompt.contains(SUGGEST_EXAMPLE_WHY));
}

#[test]
fn the_draft_prompt_shows_the_sentinels_the_parser_looks_for() {
    let prompt = prompts::draft_preamble("Stacked's CLAUDE.md", 120, prompts::draft_target_lines(120));
    assert!(prompt.contains(DRAFT_OPEN));
    assert!(prompt.contains(DRAFT_CLOSE));
}

#[test]
fn no_prompt_contains_an_em_dash() {
    let built = [
        prompts::SUGGEST_SYSTEM.to_string(),
        prompts::DRAFT_SYSTEM.to_string(),
        prompts::suggest_preamble(),
        prompts::draft_preamble("x", 40, prompts::draft_target_lines(40)),
    ];
    for p in built {
        assert!(!p.contains('\u{2014}'), "em-dash in a prompt: {p}");
        assert!(!p.contains('\u{2013}'), "en-dash in a prompt: {p}");
    }
}

#[test]
fn the_grammar_is_present_and_switched_off() {
    // The decision is a constant, not a comment: flipping it is one line, and
    // the text it would use is verified against the vendored parser.
    // A compile-time assertion, so flipping the constant fails the build rather
    // than one test: the flip needs the manual sampling loop and a live test
    // that survives a rejection, and neither is in this milestone.
    const _: () = assert!(!prompts::GRAMMAR);
    assert!(prompts::SUGGEST_GBNF.contains("root"));
}

// ---------------------------------------------------------------------------
// The tokenizer seam, without weights
// ---------------------------------------------------------------------------

/// A stand-in for the advisor's real tokenizer.
///
/// The seam is a trait precisely so this is three lines: `probe.rs` never links
/// llama, and M5.4's [`piggy_core::advisor::tokenizer::ModelTokenizer`] is the
/// only other implementation of it.
struct FakeTokenizer;

impl piggy_core::probe::SchemaTokenizer for FakeTokenizer {
    fn count(&self, text: &str) -> i64 {
        text.len() as i64 * 2
    }
    fn label(&self) -> String {
        "qwen3-4b-instruct-2507".to_string()
    }
}

#[test]
fn a_swapped_tokenizer_reaches_the_manifest_row() {
    // The fixture MCP servers are node scripts.
    let Some(node) = which_node() else {
        println!("SKIP a_swapped_tokenizer_reaches_the_manifest_row: no `node` on PATH");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let mut store = piggy_core::Store::open(home.path()).unwrap();
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp/ok-server.mjs")
        .to_string_lossy()
        .into_owned();
    let root = serde_json::json!({
        "mcpServers": { "ok": { "command": node, "args": [script], "env": {} } }
    });
    let server = piggy_core::probe::servers_from_root(&root)
        .into_iter()
        .next()
        .expect("one configured server");

    let opts = piggy_core::probe::ProbeOptions {
        timeout: std::time::Duration::from_millis(2_500),
        tokenizer: &FakeTokenizer,
        ..piggy_core::probe::ProbeOptions::default()
    };
    let row = piggy_core::probe::probe(&mut store, &server, &opts)
        .unwrap()
        .expect("stdio servers are measured");

    assert!(row.ok, "probe failed: {:?}", row.error);
    // Both halves land: the count the tokenizer produced, and the label that
    // says which one produced it. The label is what flips the UI from "rough
    // estimate" to "measured manifest", so a tokenizer that lied about its name
    // would relabel every row.
    assert_eq!(row.schema_tokens, row.schema_bytes * 2);
    assert_eq!(row.tokenizer, "qwen3-4b-instruct-2507");
    assert_ne!(row.tokenizer, piggy_core::probe::TOKENIZER_BYTES_ESTIMATE);
}

fn which_node() -> Option<String> {
    let out = std::process::Command::new("which").arg("node").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}
