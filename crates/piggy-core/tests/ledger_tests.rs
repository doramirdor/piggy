//! Context-ledger read model: aggregation, the overhead ratio, and the join
//! shape that quietly inflates message counts if you get it wrong.

use std::collections::BTreeMap;

use piggy_core::{ContextTokens, ModelTokens, Pricing, SessionParse, Store, CTX_CONVERSATION,
                 CTX_FLOOR};

/// Insert a session with an explicit context ledger. `msgs` is the assistant
/// message count the per-project row should report.
fn insert(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    project: &str,
    msgs: u64,
    ctx: &[(&str, u64)],
) {
    let mut models = BTreeMap::new();
    models.insert(
        "claude-sonnet-4-5".to_string(),
        ModelTokens {
            input_tokens: 0,
            output_tokens: 100,
            cache_creation_tokens: ctx.iter().map(|(_, t)| t).sum(),
            cache_creation_1h_tokens: 0,
            cache_read_tokens: 0,
        },
    );
    let context = ctx
        .iter()
        .map(|(k, t)| (k.to_string(), ContextTokens { tokens: *t, n: 1 }))
        .collect();
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "gui".to_string(),
        client: None,
        project_path: Some(project.to_string()),
        git_branch: None,
        first_ts: Some("2026-07-10T00:00:00.000Z".into()),
        last_ts: Some("2026-07-10T01:00:00.000Z".into()),
        models,
        n_assistant_msgs: msgs,
        n_user_msgs: msgs,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        context,
        tasks: Default::default(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, pricing, &format!("/proj/{id}.jsonl"), 1, 1)
        .unwrap();
}

#[test]
fn ledger_aggregates_kinds_and_flags_removable() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "a", "/work", 10,
           &[(CTX_FLOOR, 1000), (CTX_CONVERSATION, 8000), ("hook_success", 1000)]);
    insert(&mut store, &pricing, "b", "/work", 10,
           &[(CTX_FLOOR, 1000), (CTX_CONVERSATION, 8000), ("skill_listing", 500)]);

    let l = store.ledger(None, &pricing).unwrap();
    assert_eq!(l.total_tokens(), 19_500);
    // Largest bucket first.
    assert_eq!(l.rows[0].kind, CTX_CONVERSATION);
    assert_eq!(l.rows[0].tokens, 16_000);
    // Only the attachment kinds are configuration, not the floor or the work.
    assert_eq!(l.removable_tokens(), 1_500);
    assert!(l.rows.iter().find(|r| r.kind == CTX_FLOOR).unwrap().removable() == false);
    assert!(l.rows.iter().find(|r| r.kind == "hook_success").unwrap().removable());
    // 2000 floor of 19500 total.
    assert!((l.overhead() - 2000.0 / 19_500.0).abs() < 1e-9);
}

#[test]
fn per_project_rows_do_not_multiply_message_counts_by_bucket_count() {
    // Regression guard: joining `sessions` straight to `session_context` gives
    // one row per (session, bucket), so SUM(n_msgs) counts each session once
    // per bucket. Session `a` has 3 buckets and `b` has 1; a naive join reports
    // 40 messages for a project that ran 20.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "a", "/work", 10,
           &[(CTX_FLOOR, 500), (CTX_CONVERSATION, 500), ("hook_success", 100)]);
    insert(&mut store, &pricing, "b", "/work", 10, &[(CTX_FLOOR, 500)]);

    let l = store.ledger(None, &pricing).unwrap();
    let p = l.projects.iter().find(|p| p.project == "/work").unwrap();
    assert_eq!(p.sessions, 2);
    assert_eq!(p.msgs, 20, "each session's n_msgs counts once, not once per bucket");
    assert_eq!(p.msgs_per_session(), 10.0);
    assert_eq!(p.floor_tokens, 1000);
    assert_eq!(p.work_tokens, 600);
}

#[test]
fn overhead_separates_a_short_session_project_from_a_working_one() {
    // The headline metric has to tell these apart: same floor per session, but
    // one project does real work in each and the other opens and quits.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    for i in 0..5 {
        insert(&mut store, &pricing, &format!("deep-{i}"), "/deep", 200,
               &[(CTX_FLOOR, 30_000), (CTX_CONVERSATION, 3_000_000)]);
        insert(&mut store, &pricing, &format!("churn-{i}"), "/churn", 1,
               &[(CTX_FLOOR, 30_000), (CTX_CONVERSATION, 2_000)]);
    }

    let l = store.ledger(None, &pricing).unwrap();
    let deep = l.projects.iter().find(|p| p.project == "/deep").unwrap();
    let churn = l.projects.iter().find(|p| p.project == "/churn").unwrap();
    assert!(deep.overhead() < 0.02, "real work amortizes the floor: {}", deep.overhead());
    assert!(churn.overhead() > 0.90, "open-and-quit does not: {}", churn.overhead());
    assert_eq!(deep.msgs_per_session(), 200.0);
    assert_eq!(churn.msgs_per_session(), 1.0);
}

#[test]
fn since_filters_by_session_start() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    insert(&mut store, &pricing, "old", "/work", 5, &[(CTX_FLOOR, 100)]);

    assert_eq!(store.ledger(Some("2026-07-01"), &pricing).unwrap().total_tokens(), 100);
    assert_eq!(store.ledger(Some("2026-08-01"), &pricing).unwrap().total_tokens(), 0);
    // An empty ledger must not divide by zero.
    assert_eq!(store.ledger(Some("2026-08-01"), &pricing).unwrap().overhead(), 0.0);
}

#[test]
fn headroom_is_available_not_achieved_and_stays_quiet_when_trivial() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // 400 of 1000 cache-write tokens are configurable — 40% of WRITES. But the
    // session also spends 100 output tokens, and output prices at 5x input while
    // a 5-minute cache write prices at 1.25x. In cost units:
    //     writes  1000 * 1.25 = 1250
    //     output   100 * 5.00 =  500
    //     total               = 1750, of which removable is 400 * 1.25 = 500
    // so the removable share of the BILL is 500/1750 = 28.6%, not 40%, and the
    // plan multiplier is 1.40x, not 1.67x.
    //
    // Regression: dividing removable writes by total writes ignored output and
    // cache reads entirely and published the result as a plan multiplier. On a
    // real tree, where cache writes were 70.6% of cost, it printed 1.59x for a
    // setup whose honest figure was 1.35x.
    insert(&mut store, &pricing, "a", "/work", 10,
           &[(CTX_FLOOR, 300), (CTX_CONVERSATION, 300), ("floor:skill_listing", 400)]);
    let l = store.ledger(None, &pricing).unwrap();
    assert!((l.removable_share() - 0.4).abs() < 1e-9, "share of writes is still 40%");
    assert!(
        (l.removable_cost_share() - 500.0 / 1750.0).abs() < 1e-9,
        "share of cost is what the multiplier uses, got {}",
        l.removable_cost_share()
    );
    let m = l.headroom().unwrap();
    assert!((m - 1.4).abs() < 0.01, "cost-weighted multiplier, got {m}");
    assert!(m < 1.0 / 0.6, "must be BELOW the token-only figure it replaced");

    // A 2% share is rounding, not a finding: no multiplier at all.
    let home2 = tempfile::tempdir().unwrap();
    let mut store2 = Store::open(home2.path()).unwrap();
    insert(&mut store2, &pricing, "b", "/work", 10,
           &[(CTX_FLOOR, 200), (CTX_CONVERSATION, 780), ("hook_success", 20)]);
    assert_eq!(store2.ledger(None, &pricing).unwrap().headroom(), None);

    // Nothing removable, and an empty ledger: both silent, neither a divide by zero.
    let home3 = tempfile::tempdir().unwrap();
    let mut store3 = Store::open(home3.path()).unwrap();
    insert(&mut store3, &pricing, "c", "/work", 10, &[(CTX_FLOOR, 500), (CTX_CONVERSATION, 500)]);
    assert_eq!(store3.ledger(None, &pricing).unwrap().headroom(), None);
    assert_eq!(store3.ledger(Some("2099-01-01"), &pricing).unwrap().headroom(), None);
}
