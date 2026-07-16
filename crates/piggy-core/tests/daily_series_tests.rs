//! Day-over-day usage series: `Store::daily_series` groups sessions by UTC
//! calendar day, zero-fills gaps, and returns them oldest-first.

use std::collections::BTreeMap;

use piggy_core::stats::Period;
use piggy_core::{ModelTokens, Pricing, SessionParse, Store};

/// Seed one session whose last activity is `day` (an RFC3339 `...T..Z` string),
/// carrying `input`/`output` tokens on a known-priced model.
fn seed_day(store: &mut Store, id: &str, day_ts: &str, input: u64, output: u64) {
    let mut models = BTreeMap::new();
    models.insert(
        "claude-sonnet-5".to_string(),
        ModelTokens {
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: 0,
        },
    );
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some(day_ts.to_string()),
        last_ts: Some(day_ts.to_string()),
        models,
        n_assistant_msgs: 1,
        n_user_msgs: 1,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, &Pricing::embedded(), &format!("/f/{id}"), 1, 1)
        .unwrap();
}

/// A UTC day `n` days before today, at noon, as an RFC3339 string.
fn days_ago(n: i64) -> (String, String) {
    let date = chrono::Utc::now().date_naive() - chrono::Duration::days(n);
    (
        date.format("%Y-%m-%d").to_string(),
        format!("{}T12:00:00.000Z", date.format("%Y-%m-%d")),
    )
}

#[test]
fn week_series_is_seven_days_oldest_first_with_zero_filled_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(&tmp.path().join("piggy")).unwrap();

    let (d_today, ts_today) = days_ago(0);
    let (_d1, ts1) = days_ago(1);
    let (d2, _ts2) = days_ago(2); // deliberately left empty to test zero-fill
    let (d3, ts3) = days_ago(3);

    seed_day(&mut store, "s_today", &ts_today, 100, 40);
    seed_day(&mut store, "s1", &ts1, 50, 10);
    seed_day(&mut store, "s3", &ts3, 200, 60);

    let series = store.daily_series(Period::Week).unwrap();

    // Exactly the last 7 calendar days, including today.
    assert_eq!(series.len(), 7);
    assert_eq!(series.last().unwrap().date, d_today);

    // Oldest-first, contiguous, no duplicate/skipped dates.
    for w in series.windows(2) {
        assert!(w[0].date < w[1].date, "series must be ascending by date");
    }

    let find = |d: &str| series.iter().find(|r| r.date == d).unwrap();

    // Seeded days carry their tokens.
    assert_eq!(find(&d_today).totals.total_tokens(), 140);
    assert_eq!(find(&d3).totals.total_tokens(), 260);

    // The untouched day is a real zero, not missing.
    let gap = find(&d2);
    assert_eq!(gap.totals.total_tokens(), 0);
    assert_eq!(gap.totals.sessions, 0);

    // The series sum matches the whole-window total.
    let series_sum: u64 = series.iter().map(|r| r.totals.total_tokens()).sum();
    assert_eq!(series_sum, store.totals(Period::Week).unwrap().total_tokens());
}

#[test]
fn today_series_is_a_single_day() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(&tmp.path().join("piggy")).unwrap();
    let (d_today, ts_today) = days_ago(0);
    seed_day(&mut store, "s_today", &ts_today, 10, 5);

    let series = store.daily_series(Period::Today).unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].date, d_today);
    assert_eq!(series[0].totals.total_tokens(), 15);
}

#[test]
fn empty_store_returns_zero_filled_window_not_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("piggy")).unwrap();
    let series = store.daily_series(Period::Week).unwrap();
    assert_eq!(series.len(), 7);
    assert!(series.iter().all(|r| r.totals.total_tokens() == 0));
}
