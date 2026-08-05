//! The task table read model: window splitting, the prior-period comparison,
//! and the outcome counts.
//!
//! The window maths is the part that rots silently: a row whose sparkline
//! quietly includes the comparison half still renders, it just lies.

use std::collections::BTreeMap;

use piggy_core::parser::TaskAgg;
use piggy_core::{run_index, ContextTokens, ModelTokens, Period, Pricing, SessionParse, Store,
                 CTX_CONVERSATION, CTX_FLOOR};

/// One session log, with the `promptId` boundary a task row needs. Written to
/// disk so the incremental indexer sees a real (size, mtime) pair.
fn session_log() -> String {
    let today = chrono::Utc::now().date_naive().format("%Y-%m-%d");
    format!(
        concat!(
            r#"{{"type":"user","promptId":"p1","timestamp":"{0}T09:00:00.000Z","cwd":"/work/proj","message":{{"content":"do a thing"}}}}"#,
            "\n",
            r#"{{"type":"assistant","requestId":"r1","timestamp":"{0}T09:00:01.000Z","cwd":"/work/proj","message":{{"model":"claude-sonnet-4-5","usage":{{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":100}},"content":[{{"type":"tool_use","name":"Read","input":{{}}}}]}}}}"#,
            "\n",
        ),
        today
    )
}

/// Insert a session at midday `days_ago` days back, with a context ledger and
/// tasks. Each task tuple is `(prompt_id, turns, tool_errors)`.
fn insert(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    project: &str,
    days_ago: i64,
    ctx: &[(&str, u64)],
    tasks: &[(&str, u64, u64)],
) {
    insert_at(store, pricing, id, project, days_ago, "12:00:00", ctx, tasks);
}

/// [`insert`], but at a chosen time of day: the window bugs all live at the
/// edges of a day, so a test that only ever writes midday cannot see them.
#[allow(clippy::too_many_arguments)]
fn insert_at(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    project: &str,
    days_ago: i64,
    hms: &str,
    ctx: &[(&str, u64)],
    tasks: &[(&str, u64, u64)],
) {
    let day = chrono::Utc::now().date_naive() - chrono::Duration::days(days_ago);
    let ts = format!("{}T{hms}.000Z", day.format("%Y-%m-%d"));
    insert_ts(store, pricing, id, project, Some(ts), ctx, tasks);
}

/// [`insert`] with no timestamps at all: a log Piggy could not date. The ledger
/// still counts it, so anything sitting on the same row has to.
fn insert_undated(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    project: &str,
    ctx: &[(&str, u64)],
    tasks: &[(&str, u64, u64)],
) {
    insert_ts(store, pricing, id, project, None, ctx, tasks);
}

#[allow(clippy::too_many_arguments)]
fn insert_ts(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    project: &str,
    ts: Option<String>,
    ctx: &[(&str, u64)],
    tasks: &[(&str, u64, u64)],
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
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "gui".to_string(),
        client: None,
        project_path: Some(project.to_string()),
        git_branch: None,
        first_ts: ts.clone(),
        last_ts: ts,
        models,
        n_assistant_msgs: 1,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        context: ctx
            .iter()
            .map(|(k, t)| (k.to_string(), ContextTokens { tokens: *t, n: 1 }))
            .collect(),
        tasks: tasks
            .iter()
            .map(|(pid, turns, errs)| {
                (
                    pid.to_string(),
                    TaskAgg {
                        n_turns: *turns,
                        n_tool_errors: *errs,
                        ..Default::default()
                    },
                )
            })
            .collect(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, pricing, &format!("/proj/{id}.jsonl"), 1, 1)
        .unwrap();
}

fn row<'a>(rows: &'a [piggy_core::TaskRow], project: &str) -> &'a piggy_core::TaskRow {
    rows.iter()
        .find(|r| r.project == project)
        .unwrap_or_else(|| panic!("no row for {project}"))
}

/// The total the task table's consumers put on the row: the ledger's spend for
/// one project over the same window the row is built for.
fn ledger_total(store: &Store, pricing: &Pricing, period: Period, project: &str) -> u64 {
    store
        .ledger(period.day_cutoff().as_deref(), pricing)
        .unwrap()
        .projects
        .iter()
        .find(|p| p.project == project)
        .map(|p| p.floor_tokens + p.work_tokens)
        .unwrap_or(0)
}

#[test]
fn the_comparison_half_stays_out_of_the_display_window() {
    // The whole point of the doubled scan: 10 days back is the PRIOR week, and
    // must feed `prev_tokens` without ever appearing in the sparkline.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "now", "/work", 1, &[(CTX_CONVERSATION, 3_000)], &[]);
    insert(&mut store, &pricing, "prev", "/work", 10, &[(CTX_CONVERSATION, 1_000)], &[]);

    let rows = store.task_table(Period::Week).unwrap();
    let r = row(&rows, "/work");
    assert_eq!(r.tokens(), 3_000, "sparkline must cover the display week only");
    assert_eq!(r.prev_tokens, Some(1_000));
    // 3000 vs 1000 is a tripling.
    assert!((r.delta().unwrap() - 2.0).abs() < 1e-9);
    // Zero-filled to one point per day, and it sums to the row total.
    assert_eq!(r.daily.len(), 7);
    assert_eq!(r.daily.iter().sum::<u64>(), r.tokens());
}

#[test]
fn an_empty_prior_window_yields_no_delta_rather_than_plus_infinity() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "a", "/fresh", 0, &[(CTX_CONVERSATION, 5_000)], &[]);

    let r = &store.task_table(Period::Week).unwrap()[0];
    assert_eq!(r.prev_tokens, Some(0));
    assert_eq!(r.delta(), None, "a delta against zero is not a measurement");
}

#[test]
fn all_time_has_no_prior_window_to_compare_against() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "a", "/work", 3, &[(CTX_CONVERSATION, 5_000)], &[]);

    let r = &store.task_table(Period::All).unwrap()[0];
    assert_eq!(r.prev_tokens, None);
    assert_eq!(r.delta(), None);
    assert_eq!(r.tokens(), 5_000);
}

#[test]
fn tool_errors_are_counted_per_project_and_per_task() {
    // Ten failures in one task and ten tasks failing once share an error count
    // and are different problems, so both numbers are carried.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "a", "/noisy", 1, &[(CTX_FLOOR, 2_000)],
           &[("t1", 4, 3), ("t2", 2, 0), ("t3", 1, 1)]);
    insert(&mut store, &pricing, "b", "/clean", 1, &[(CTX_FLOOR, 9_000)],
           &[("t4", 5, 0)]);

    let rows = store.task_table(Period::Week).unwrap();
    // Heaviest first: /clean spent more even though /noisy failed more.
    assert_eq!(rows[0].project, "/clean");

    let noisy = row(&rows, "/noisy");
    assert_eq!(noisy.tasks, 3);
    assert_eq!(noisy.turns, 7);
    assert_eq!(noisy.tool_errors, 4, "every flagged block counts");
    assert_eq!(noisy.failed_tasks, 2, "but only two tasks were affected");
    assert!((noisy.failure_rate().unwrap() - 2.0 / 3.0).abs() < 1e-9);

    let clean = row(&rows, "/clean");
    assert_eq!(clean.tool_errors, 0);
    assert_eq!(clean.failure_rate(), Some(0.0));
}

#[test]
fn a_project_with_no_recorded_tasks_reports_missing_not_zero() {
    // Logs predating `promptId` produce spend with no task rows. That must read
    // as "not recorded", never as a clean run of zero failures.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "old", "/legacy", 2, &[(CTX_CONVERSATION, 7_000)], &[]);

    let r = &store.task_table(Period::Week).unwrap()[0];
    assert_eq!(r.tasks, 0);
    assert_eq!(r.failure_rate(), None);
    assert_eq!(r.turns_per_task(), None);
    assert_eq!(r.tokens(), 7_000, "the spend is still real and still shown");
}

#[test]
fn the_row_total_and_its_sparkline_cover_the_same_days() {
    // Every consumer draws the ledger total, the delta and the sparkline on one
    // row as though they were one measurement. The ledger window was a rolling
    // instant and the series calendar days, so a session late on the day before
    // the window opened was inside the total and outside every bar under it:
    // 6,000 tokens over a sparkline summing to 1,000.
    for (period, boundary_days_ago, days) in [
        (Period::Today, 1, 1),
        (Period::Week, 7, 7),
        (Period::Month, 30, 30),
    ] {
        let home = tempfile::tempdir().unwrap();
        let pricing = Pricing::embedded();
        let mut store = Store::open(home.path()).unwrap();

        // The last second of the day before the window opens: inside `now - Nd`
        // whatever hour the test runs at, outside the calendar window either way.
        insert_at(&mut store, &pricing, "edge", "/work", boundary_days_ago, "23:59:59",
                  &[(CTX_CONVERSATION, 5_000)], &[]);
        insert_at(&mut store, &pricing, "now", "/work", 0, "09:00:00",
                  &[(CTX_CONVERSATION, 1_000)], &[]);

        let rows = store.task_table(period).unwrap();
        let r = row(&rows, "/work");
        let total = ledger_total(&store, &pricing, period, "/work");
        assert_eq!(r.daily.len(), days, "{period:?}: one point per calendar day");
        assert_eq!(
            r.daily.iter().sum::<u64>(),
            total,
            "{period:?}: the sparkline must account for the total on its own row"
        );
        assert_eq!(total, 1_000, "{period:?}: the boundary session is last window's");
        assert_eq!(r.prev_tokens, Some(5_000), "{period:?}: and it belongs to the delta");
    }
}

#[test]
fn all_time_counts_sessions_the_ledger_counts_even_when_they_are_undated() {
    // `Store::ledger(None, ..)` includes a session with no `started_at`, so the
    // row's total does. Requiring a timestamp on the task side alone reported a
    // project's tokens with none of its outcomes.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "dated", "/work", 2, &[(CTX_FLOOR, 1_000)], &[("t1", 1, 0)]);
    insert_undated(&mut store, &pricing, "undated", "/work", &[(CTX_FLOOR, 500)], &[("t2", 4, 2)]);

    let rows = store.task_table(Period::All).unwrap();
    let r = row(&rows, "/work");
    assert_eq!(r.tasks, 2, "the undated session's task still happened");
    assert_eq!(r.turns, 5);
    assert_eq!(r.tool_errors, 2);
}

#[test]
fn a_schema_upgrade_reparses_logs_the_incremental_skip_would_freeze() {
    // Incremental indexing skips any file whose (size, mtime) still match, and a
    // finished session log never changes again. Ship a schema that records
    // something new and every existing install keeps the old parse forever: v6
    // added `tasks` and, on real databases, never filled it.
    let home = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let proj = projects.path().join("-work-proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("s1.jsonl"), session_log()).unwrap();

    let pricing = Pricing::embedded();
    let current = {
        let mut store = Store::open(home.path()).unwrap();
        let rep = run_index(&mut store, &pricing, projects.path(), false).unwrap();
        assert_eq!(rep.updated, 1);
        store
            .schema_version()
            .unwrap()
            .expect("a migrated database records the version that wrote it")
    };

    // A database written by the previous schema: the sessions are indexed, the
    // new table is not filled.
    {
        let conn = rusqlite::Connection::open(home.path().join("piggy.db")).unwrap();
        conn.execute("DELETE FROM tasks", []).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [(current - 1).to_string()],
        )
        .unwrap();
    }

    let mut store = Store::open(home.path()).unwrap();
    let rep = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(rep.updated, 1, "an upgraded schema must re-read the logs");
    assert_eq!(rep.skipped, 0);
    assert_eq!(row(&store.task_table(Period::All).unwrap(), "/work/proj").tasks, 1);

    // And exactly once: a database already at the current version pays nothing.
    let rep = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(rep.updated, 0, "no reindex without a version change");
    assert_eq!(rep.skipped, 1);
}

#[test]
fn a_fresh_database_does_not_reindex_what_it_just_indexed() {
    // The invalidation keys off a version that moved. A brand new database has
    // no stored version at all, and must not be read as "older than current".
    let home = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let proj = projects.path().join("-work-proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("s1.jsonl"), session_log()).unwrap();

    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(run_index(&mut store, &pricing, projects.path(), false).unwrap().updated, 1);
    drop(store);

    // Reopening runs the migration again, on a database that is already current.
    let mut store = Store::open(home.path()).unwrap();
    let rep = run_index(&mut store, &pricing, projects.path(), false).unwrap();
    assert_eq!(rep.updated, 0);
    assert_eq!(rep.skipped, 1, "the file bookkeeping survived the reopen");
}

#[test]
fn task_counts_ignore_the_comparison_half() {
    // `prev_tokens` needs the doubled scan; the task COUNTS must not see it.
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    insert(&mut store, &pricing, "now", "/work", 1, &[(CTX_FLOOR, 100)], &[("t1", 1, 1)]);
    insert(&mut store, &pricing, "old", "/work", 10, &[(CTX_FLOOR, 100)], &[("t2", 9, 9)]);

    let r = &store.task_table(Period::Week).unwrap()[0];
    assert_eq!(r.tasks, 1);
    assert_eq!(r.turns, 1);
    assert_eq!(r.tool_errors, 1, "last week's failures are not this week's");
}
