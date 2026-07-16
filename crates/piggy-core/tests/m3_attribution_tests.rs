//! M3 measurement tests that need no environment sandbox: attribution on a
//! synthesized DB (the honesty tests), the bootstrap/PRNG primitives, session
//! tagging round-trips, and discovery parsing against fixture JSON.
//!
//! These operate on an explicit `Store` opened in a `tempdir` (never the real
//! `~/.piggy`) and touch no process-global env vars, so they run in parallel
//! with everything else.

use std::collections::BTreeMap;
use std::path::PathBuf;

use piggy_core::attribution::{self, median, Badge, Stream};
use piggy_core::rng::XorShift64;
use piggy_core::store::source;
use piggy_core::{discovery, Catalog, ModelTokens, Pricing, SaverTag, SessionParse, Store};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Insert a synthetic session with a single model's token totals and `turns`
/// deduped assistant messages.
#[allow(clippy::too_many_arguments)]
fn insert_session(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    model: &str,
    turns: u64,
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
) {
    let mut models = BTreeMap::new();
    models.insert(
        model.to_string(),
        ModelTokens {
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: cache_read,
        },
    );
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some("2026-01-01T00:00:00.000Z".into()),
        last_ts: Some("2026-01-01T00:10:00.000Z".into()),
        models,
        n_assistant_msgs: turns,
        n_user_msgs: turns,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, pricing, &format!("/proj/{id}.jsonl"), 1, 1)
        .unwrap();
}

/// Uniform in `[0.0, 1.0)` from the deterministic PRNG.
fn unit(rng: &mut XorShift64) -> f64 {
    rng.next_u64() as f64 / (u64::MAX as f64 + 1.0)
}

// ---------------------------------------------------------------------------
// The key honesty test: a planted 20% output reduction must be recovered.
// ---------------------------------------------------------------------------

#[test]
fn planted_effect_is_recovered_within_ci() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // 100 ON + 100 OFF sessions. OFF sessions output ~1000 tokens/turn; ON
    // sessions ~800 tokens/turn — a true 20% reduction — with symmetric
    // multiplicative noise (median 1.0) so the medians land on 800 / 1000.
    let mut rng = XorShift64::new(0xC0FF_EE12_3456_789A);
    let n = 100;
    for i in 0..n {
        // OFF
        let turns = 5 + (rng.below(20) as u64);
        let noise = 0.7 + unit(&mut rng) * 0.6; // [0.7, 1.3), median 1.0
        let out = (1000.0 * noise * turns as f64).round() as u64;
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();

        // ON
        let turns = 5 + (rng.below(20) as u64);
        let noise = 0.7 + unit(&mut rng) * 0.6;
        let out = (800.0 * noise * turns as f64).round() as u64;
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x1234_5678).unwrap();
    let out = a.output().expect("output stream present");

    assert_eq!(out.n_on, 100);
    assert_eq!(out.n_off, 100);
    assert_eq!(
        out.badge,
        Badge::Measured,
        "a real 20% effect at n=100/side must badge measured: {out:?}"
    );
    let (lo, hi) = out.ci.expect("a CI is computed");
    assert!(
        lo <= 0.20 && 0.20 <= hi,
        "90% CI [{lo:.3}, {hi:.3}] must contain the planted 0.20"
    );
    assert!(lo > 0.0, "a measured badge means the CI excludes zero");
    let d = out.delta.unwrap();
    assert!(
        (d - 0.20).abs() < 0.06,
        "recovered delta {d:.3} should be near the planted 0.20"
    );
}

// ---------------------------------------------------------------------------
// Null test: no planted effect → CI crosses zero → never a point claim.
// ---------------------------------------------------------------------------

#[test]
fn null_effect_never_makes_a_point_claim() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // Both groups drawn from the *same* 1000-tokens/turn distribution.
    let mut rng = XorShift64::new(0x0BAD_F00D_1357_9BDF);
    for i in 0..100 {
        for (enabled, tag) in [(false, "off"), (true, "on")] {
            let turns = 5 + (rng.below(20) as u64);
            let noise = 0.6 + unit(&mut rng) * 0.8; // wide noise, median 1.0
            let out = (1000.0 * noise * turns as f64).round() as u64;
            let id = format!("{tag}-{i}");
            insert_session(
                &mut store,
                &pricing,
                &id,
                "claude-sonnet-4-5",
                turns,
                400,
                out,
                0,
                0,
            );
            store
                .set_session_savers(&id, &[SaverTag::new("rtk", enabled, source::ROTATION)])
                .unwrap();
        }
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x99).unwrap();
    let out = a.output().unwrap();
    assert_eq!(
        out.badge,
        Badge::Measuring,
        "no real effect must never badge measured: delta={:?} ci={:?}",
        out.delta,
        out.ci
    );
    let (lo, hi) = out.ci.unwrap();
    assert!(
        lo <= 0.0 && hi >= 0.0,
        "the null CI [{lo:.3}, {hi:.3}] should straddle zero"
    );
}

// ---------------------------------------------------------------------------
// Small-n: a real effect but too few sessions → measuring, never a number.
// ---------------------------------------------------------------------------

#[test]
fn small_n_stays_measuring() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    for i in 0..5 {
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            10_000,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            8_000,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x7).unwrap();
    let out = a.output().unwrap();
    assert_eq!(out.n_on, 5);
    assert_eq!(
        out.badge,
        Badge::Measuring,
        "below the {}-session floor must stay measuring even with a clear effect",
        attribution::MIN_GROUP
    );
}

// ---------------------------------------------------------------------------
// Subagent sub-sessions are excluded from attribution groups.
// ---------------------------------------------------------------------------

#[test]
fn subagent_sessions_are_excluded_from_groups() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // One normal session and one subagent sub-session, both tagged ON for rtk.
    insert_session(
        &mut store,
        &pricing,
        "main",
        "claude-sonnet-4-5",
        10,
        400,
        5000,
        0,
        0,
    );
    store
        .set_session_savers("main", &[SaverTag::new("rtk", true, source::ROTATION)])
        .unwrap();

    // A subagent file (path contains /subagents/) — upsert with that path.
    let mut models = BTreeMap::new();
    models.insert(
        "claude-sonnet-4-5".to_string(),
        ModelTokens {
            input_tokens: 400,
            output_tokens: 9999,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: 0,
        },
    );
    let parse = SessionParse {
        session_id: "sub".into(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some("2026-01-01T00:00:00.000Z".into()),
        last_ts: Some("2026-01-01T00:10:00.000Z".into()),
        models,
        n_assistant_msgs: 10,
        n_user_msgs: 10,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, &pricing, "/proj/subagents/sub.jsonl", 1, 1)
        .unwrap();
    store
        .set_session_savers("sub", &[SaverTag::new("rtk", true, source::ROTATION)])
        .unwrap();

    let rates = store.session_rate_map(&pricing).unwrap();
    assert!(rates.contains_key("main"));
    assert!(
        !rates.contains_key("sub"),
        "subagent sub-session must be excluded from attribution rates"
    );
}

// ---------------------------------------------------------------------------
// Statistics + PRNG primitives.
// ---------------------------------------------------------------------------

#[test]
fn median_of_odd_and_even() {
    assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    assert_eq!(median(&[]), 0.0);
}

#[test]
fn xorshift_is_deterministic() {
    let mut a = XorShift64::new(42);
    let mut b = XorShift64::new(42);
    for _ in 0..1000 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
    // `below` stays in range.
    let mut r = XorShift64::new(7);
    for _ in 0..1000 {
        assert!(r.below(13) < 13);
    }
}

#[test]
fn bootstrap_is_reproducible_for_a_fixed_seed() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(1);
    for i in 0..40 {
        let turns = 8 + (rng.below(10) as u64);
        let out = (1000.0 * (0.8 + unit(&mut rng) * 0.4) * turns as f64) as u64;
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            300,
            out,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        let out = (700.0 * (0.8 + unit(&mut rng) * 0.4) * turns as f64) as u64;
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            300,
            out,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    let a1 = attribution::attribute(&store, &pricing, "rtk", 0xABCD).unwrap();
    let a2 = attribution::attribute(&store, &pricing, "rtk", 0xABCD).unwrap();
    let (o1, o2) = (a1.output().unwrap(), a2.output().unwrap());
    assert_eq!(o1.ci, o2.ci, "same seed → byte-identical CI");
}

// ---------------------------------------------------------------------------
// Headline: full-on vs holdout multiplier, and pre-install fallback.
// ---------------------------------------------------------------------------

#[test]
fn headline_uses_holdout_and_reports_multiplier() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // Full-on sessions spend ~half of holdout sessions per turn.
    for i in 0..20 {
        let id = format!("fullon-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            500,
            500,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
        let id = format!("holdout-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            1000,
            1000,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x5EED).unwrap();
    assert_eq!(hl.baseline, attribution::HeadlineBaseline::Holdout);
    assert_eq!(hl.n_full_on, 20);
    assert_eq!(hl.n_baseline, 20);
    let m = hl.multiplier.expect("a multiplier is computable");
    assert!(
        (m - 2.0).abs() < 0.1,
        "holdout spends ~2× full-on per turn → ~2.0× longer, got {m:.2}"
    );
}

#[test]
fn headline_falls_back_to_pre_install_when_no_holdout() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    for i in 0..12 {
        let id = format!("fullon-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            500,
            500,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
        // Pre-install baseline (all-off, observational).
        let id = format!("pre-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            900,
            900,
            0,
            0,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::PRE_INSTALL)])
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x11).unwrap();
    assert_eq!(
        hl.baseline,
        attribution::HeadlineBaseline::PreInstall,
        "with no holdout, the headline uses observational pre-install history"
    );
    assert!(hl.multiplier.unwrap() > 1.0);
}

// ---------------------------------------------------------------------------
// Store: session_savers + rotation_state + pre-install tagging round-trips.
// ---------------------------------------------------------------------------

#[test]
fn session_savers_and_rotation_state_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();

    store
        .set_session_savers(
            "s1",
            &[
                SaverTag::new("rtk", true, source::ROTATION),
                SaverTag::new("caveman", false, source::MANUAL),
            ],
        )
        .unwrap();
    assert!(store.has_session_savers("s1").unwrap());
    // Rows come back ordered by saver_id.
    assert_eq!(
        store.session_savers("s1").unwrap(),
        vec![
            SaverTag::new("caveman", false, source::MANUAL),
            SaverTag::new("rtk", true, source::ROTATION),
        ]
    );
    assert_eq!(store.tagged_saver_ids().unwrap(), vec!["caveman", "rtk"]);

    // Replacement, not append.
    store
        .set_session_savers("s1", &[SaverTag::new("rtk", false, source::HOLDOUT)])
        .unwrap();
    assert_eq!(store.session_savers("s1").unwrap().len(), 1);

    // Rotation state.
    assert_eq!(store.rotation_state().unwrap(), (0, None));
    store.set_rotation_state(3, Some("{\"rtk\":true}")).unwrap();
    let (pos, planned) = store.rotation_state().unwrap();
    assert_eq!(pos, 3);
    assert_eq!(planned.as_deref(), Some("{\"rtk\":true}"));
}

#[test]
fn tag_pre_install_only_touches_untagged_old_sessions() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // Two old sessions (before the cutoff) and one new one (after).
    let older = |i: &str| format!("2026-01-0{i}T00:00:00.000Z");
    for (id, ts) in [("old-a", older("1")), ("old-b", older("2"))] {
        insert_at(&mut store, &pricing, id, &ts);
    }
    insert_at(&mut store, &pricing, "new-c", "2026-02-01T00:00:00.000Z");
    // Pre-tag one of the old ones (as if the watcher already snapshotted it).
    store
        .set_session_savers("old-b", &[SaverTag::new("rtk", true, source::ROTATION)])
        .unwrap();

    let cutoff = "2026-01-15T00:00:00.000Z";
    let n = store
        .tag_pre_install(cutoff, &["rtk".into(), "caveman".into()])
        .unwrap();
    assert_eq!(n, 1, "only the untagged old session gets tagged");

    // old-a now carries an all-off pre_install snapshot for both savers.
    let tags = store.session_savers("old-a").unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags
        .iter()
        .all(|t| !t.enabled && t.source == source::PRE_INSTALL));
    // old-b's existing tag is untouched; new-c stays untagged.
    assert_eq!(
        store.session_savers("old-b").unwrap()[0].source,
        source::ROTATION
    );
    assert!(!store.has_session_savers("new-c").unwrap());
}

fn insert_at(store: &mut Store, pricing: &Pricing, id: &str, ts: &str) {
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some(ts.to_string()),
        last_ts: Some(ts.to_string()),
        models: BTreeMap::new(),
        n_assistant_msgs: 1,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, pricing, &format!("/proj/{id}.jsonl"), 1, 1)
        .unwrap();
}

// ---------------------------------------------------------------------------
// Discovery: parse fixture GitHub JSON, merge/dedup/filter against the catalog.
// ---------------------------------------------------------------------------

#[test]
fn discovery_parses_and_filters_against_catalog() {
    let json = std::fs::read_to_string(fixture("github_search.json")).unwrap();
    let parsed = discovery::parse_search_response(&json).unwrap();
    assert_eq!(parsed.len(), 3);

    // A second batch re-lists token-saver with a higher star count (dedup keeps max).
    let dup = vec![piggy_core::DiscoveredRepo {
        full_name: "someone/token-saver".into(),
        description: Some("dupe".into()),
        stars: 999,
        url: "https://github.com/someone/token-saver".into(),
        topics: vec![],
        listed_only: false,
        exclusion_reason: None,
    }];

    let catalog = Catalog::embedded();
    let merged = discovery::merge_and_filter(vec![parsed, dup], &catalog);

    // rtk-ai/rtk is curated in the catalog → filtered out of discovery.
    assert!(
        !merged
            .iter()
            .any(|r| r.full_name == "rtk-ai/rtk" && !r.listed_only),
        "curated repos must not be re-suggested"
    );
    // Dedup kept the higher star count.
    let ts = merged
        .iter()
        .find(|r| r.full_name == "someone/token-saver")
        .unwrap();
    assert_eq!(ts.stars, 999);
    // Sorted by stars desc among github hits: claude-tokens (1200) before token-saver (999).
    let names: Vec<&str> = merged
        .iter()
        .filter(|r| !r.listed_only)
        .map(|r| r.full_name.as_str())
        .collect();
    assert_eq!(names, vec!["other/claude-tokens", "someone/token-saver"]);
    // The listed-only catalog entry appears with its exclusion reason.
    let listed = merged
        .iter()
        .find(|r| r.listed_only)
        .expect("listed_only entry surfaced");
    assert_eq!(listed.full_name, "token-optimizer-mcp");
    assert!(listed
        .exclusion_reason
        .as_ref()
        .unwrap()
        .contains("uninstall"));
}

#[test]
fn attribution_stream_labels_are_stable() {
    // Guard the four streams and their order (the report depends on it).
    assert_eq!(
        Stream::ALL.iter().map(|s| s.label()).collect::<Vec<_>>(),
        vec!["input", "output", "cache write", "cache read"]
    );
}

// ---------------------------------------------------------------------------
// Regression: sessions with zero assistant messages have no session_models
// rows; the LEFT JOIN in session_rate_map must COALESCE their token columns
// instead of failing on NULL (seen on real data: `piggy report` crashed).
// ---------------------------------------------------------------------------

#[test]
fn session_without_model_rows_does_not_break_rate_map() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::embedded();

    // A normal session…
    insert_session(
        &mut store,
        &pricing,
        "s-normal",
        "claude-opus-4-8",
        10,
        1000,
        500,
        0,
        0,
    );
    // …and an empty one: parsed file with zero assistant messages → no models.
    let parse = SessionParse {
        session_id: "s-empty".to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: None,
        last_ts: None,
        models: BTreeMap::new(),
        n_assistant_msgs: 0,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, &pricing, "/proj/s-empty.jsonl", 1, 1)
        .unwrap();

    let map = store.session_rate_map(&pricing).unwrap();
    assert!(map.contains_key("s-normal"));
    let empty = &map["s-empty"];
    assert_eq!(empty.input, 0);
    assert_eq!(empty.output, 0);
    assert_eq!(empty.turns, 0);
}
