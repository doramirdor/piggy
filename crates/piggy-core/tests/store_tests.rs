//! Store round-trip and incremental-index tests using a temp PIGGY_HOME.

use std::fs;
use std::path::PathBuf;

use piggy_core::{parse_file, run_index, Period, Pricing, Store};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn upsert_round_trip_and_pricing() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    let parse = parse_file(&fixture("basic.jsonl")).unwrap();
    store
        .upsert_session(&parse, &pricing, "/fake/basic.jsonl", 3242, 111)
        .unwrap();

    let totals = store.totals(Period::All).unwrap();
    // opus (120+55+40+200) + sonnet (200+80) tokens.
    assert_eq!(totals.input_tokens, 320);
    assert_eq!(totals.output_tokens, 135);
    assert_eq!(totals.cache_creation_tokens, 40);
    assert_eq!(totals.cache_read_tokens, 200);
    assert_eq!(totals.sessions, 1);
    assert!(totals.fully_priced());
    assert!(totals.cost_usd_est > 0.0);

    let by_model = store.by_model(Period::All).unwrap();
    assert_eq!(by_model.len(), 2);

    // Re-upserting the same session must not double-count (session_models
    // replaced, not appended).
    store
        .upsert_session(&parse, &pricing, "/fake/basic.jsonl", 3242, 111)
        .unwrap();
    let totals2 = store.totals(Period::All).unwrap();
    assert_eq!(totals2.input_tokens, 320);
    assert_eq!(totals2.sessions, 1);

    let (matched, total) = store.pricing_coverage().unwrap();
    assert_eq!(matched, total);
    assert!(total > 0);
}

#[test]
fn incremental_index_skips_unchanged_files() {
    let home = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();

    // A project subdirectory with one session file copied from a fixture.
    let proj_dir = projects.path().join("-Users-dev-proj");
    fs::create_dir_all(&proj_dir).unwrap();
    fs::copy(fixture("basic.jsonl"), proj_dir.join("basic.jsonl")).unwrap();

    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    let r1 = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(r1.scanned, 1);
    assert_eq!(r1.updated, 1);
    assert_eq!(r1.skipped, 0);
    assert_eq!(r1.sessions, 1);

    // Second run: file unchanged -> skipped.
    let r2 = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(r2.scanned, 1);
    assert_eq!(r2.updated, 0);
    assert_eq!(r2.skipped, 1);

    // --full forces a re-parse.
    let r3 = run_index(&mut store, &pricing, projects.path(), true).unwrap();
    assert_eq!(r3.updated, 1);
    assert_eq!(r3.skipped, 0);
}
