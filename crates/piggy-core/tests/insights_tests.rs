//! Insight detectors: they must fire on real waste and stay silent otherwise.
//! A findings list that always finds something is a list users learn to ignore.

use piggy_core::{insights, Ledger, LedgerRow, ProjectRow, Severity};

/// Cost-weighted denominator for a synthetic ledger. Real ones read it from
/// `session_models`; here we say "cache writes are the whole bill" unless a
/// test wants otherwise, which keeps the old token-share expectations intact.
fn ledger(rows: Vec<LedgerRow>, projects: Vec<ProjectRow>) -> Ledger {
    let writes: u64 = rows.iter().map(|r| r.tokens).sum();
    Ledger { rows, projects, cost_units: writes as f64 * 1.25, write_weight: 1.25 }
}

fn row(kind: &str, tokens: u64) -> LedgerRow {
    LedgerRow { kind: kind.to_string(), tokens, n: 1 }
}

fn project(name: &str, sessions: u64, msgs: u64, floor: u64, work: u64) -> ProjectRow {
    ProjectRow {
        project: name.to_string(),
        sessions,
        msgs,
        floor_tokens: floor,
        work_tokens: work,
    }
}

#[test]
fn a_healthy_setup_produces_no_findings() {
    // Long sessions, small floor, no injections worth naming. The correct
    // output is nothing at all.
    let l = ledger(vec![row("__floor", 1_000), row("__conversation", 99_000)], vec![project("/work", 50, 5_000, 1_000, 99_000)]);
    assert!(insights(&l).is_empty(), "{:?}", insights(&l));
}

#[test]
fn an_empty_ledger_is_silent_not_a_crash() {
    let l = ledger(vec![], vec![]);
    assert!(insights(&l).is_empty());
}

#[test]
fn a_dominant_floor_is_the_loudest_finding() {
    let l = ledger(vec![row("__floor", 80_000), row("__conversation", 20_000)], vec![project("/work", 10, 40, 80_000, 20_000)]);
    let found = insights(&l);
    let top = &found[0];
    assert_eq!(top.id, "floor-dominates");
    assert_eq!(top.severity, Severity::High);
    assert_eq!(top.tokens, 80_000);
    assert!(top.title.contains("80%"), "{}", top.title);
}

#[test]
fn churn_needs_both_short_sessions_and_wasted_tokens() {
    // Same shape twice: many short sessions. Only the one where the floor
    // actually dominates is a finding. A team working in small chunks with a
    // cheap floor is not doing anything wrong.
    let wasteful = ledger(vec![row("__floor", 900_000), row("__conversation", 100_000)], vec![project("/bench", 300, 300, 900_000, 100_000)]);
    assert!(
        insights(&wasteful).iter().any(|i| i.id == "churn:/bench"),
        "300 sessions at 90% overhead must be flagged"
    );

    let fine = ledger(vec![row("__floor", 100_000), row("__conversation", 900_000)], vec![project("/chunks", 300, 300, 100_000, 900_000)]);
    assert!(
        !insights(&fine).iter().any(|i| i.id.starts_with("churn:")),
        "short sessions with a cheap floor are not waste: {:?}",
        insights(&fine)
    );
}

#[test]
fn floor_components_are_reported_per_session_and_only_when_they_matter() {
    let l = ledger(vec![
            row("__floor", 500_000),
            row("__conversation", 400_000),
            row("floor:skill_listing", 400_000), // 4,000/session — worth saying
            row("floor:date_change", 1_000),     // 10/session — noise
        ], vec![project("/work", 100, 4_000, 900_000, 400_000)]);
    let found = insights(&l);
    let skill = found
        .iter()
        .find(|i| i.id == "floor-component:skill_listing")
        .expect("the 4k/session component is a finding");
    assert_eq!(skill.severity, Severity::High);
    assert!(skill.title.contains("4,000"), "{}", skill.title);
    assert!(
        !found.iter().any(|i| i.id.contains("date_change")),
        "a 10-token-per-session component is not worth a finding"
    );
}

#[test]
fn findings_are_ordered_loudest_first() {
    let l = ledger(vec![
            row("__floor", 500_000),
            row("__conversation", 100_000),
            row("floor:skill_listing", 400_000),
        ], vec![project("/bench", 100, 100, 900_000, 100_000)]);
    let found = insights(&l);
    for pair in found.windows(2) {
        assert!(
            pair[0].severity >= pair[1].severity,
            "severity must not increase down the list: {:?}",
            found.iter().map(|i| (i.severity, &i.id)).collect::<Vec<_>>()
        );
    }
}
