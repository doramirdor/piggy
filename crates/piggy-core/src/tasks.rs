//! The task table: per-project spend with the outcome signal attached.
//!
//! `session_context` answers "what did the tokens buy". This answers the two
//! questions it cannot: *which* of my tasks was expensive, and did it work.
//! The outcome half exists only because [`crate::parser::TaskAgg`] counts
//! `tool_result` blocks flagged `is_error`: the single success signal the
//! session logs carry.

use anyhow::Result;
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};

use crate::stats::Period;
use crate::store::Store;

/// `All` draws at most this many days, matching [`Store::daily_series`]. The
/// sparkline is a chart window, not an all-time record.
const MAX_ALL_DAYS: i64 = 120;

/// One project's task record over a window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskRow {
    pub project: String,
    /// User prompts recorded in this window.
    ///
    /// `0` means **not recorded** (logs predating `promptId` carry no task
    /// boundary) and never "no work happened". Callers must render it as
    /// missing data rather than as an outcome of zero.
    pub tasks: u64,
    /// Assistant turns inside those tasks.
    pub turns: u64,
    /// `tool_use` blocks inside those tasks, every tool and not just the ones
    /// Sweep tracks.
    pub tool_calls: u64,
    /// `tool_result` blocks seen. The denominator [`Self::tool_errors`] is a
    /// share of: without it 2 errors reads the same whether the task made 2
    /// tool calls or 200.
    pub tool_results: u64,
    /// `tool_result` blocks flagged `is_error`. A floor, not a guess: an absent
    /// flag is not counted (see [`crate::parser`]).
    pub tool_errors: u64,
    /// Tasks that hit at least one tool error. Kept separate from
    /// `tool_errors` because ten failures in one task and ten tasks failing
    /// once are different problems with the same error count.
    pub failed_tasks: u64,
    /// Cache-write tokens per day across the window, oldest first, zero-filled
    /// so a quiet day is a real `0` rather than a hole.
    ///
    /// For `Today`/`Week`/`Month` this sums to the total the ledger charges
    /// this project over [`Period::day_cutoff`], so the sparkline is the row's
    /// own history rather than a different measure drawn beside it. Callers
    /// pairing it with a ledger total MUST use that cutoff and not the rolling
    /// [`Period::cutoff`].
    ///
    /// For `All` it does NOT: the series is clamped to the most recent 120 days
    /// (`MAX_ALL_DAYS`), which is a chart window, while the all-time total is
    /// all of history. Anything rendering both has to say so.
    pub daily: Vec<u64>,
    /// The same total over the equal-length window immediately before this one.
    ///
    /// `None` when there is no prior window (`All`) or when it held nothing, so
    /// the UI shows no delta rather than a delta against zero.
    pub prev_tokens: Option<u64>,
}

impl TaskRow {
    /// Cache-write tokens in the display window: the sum of [`Self::daily`].
    pub fn tokens(&self) -> u64 {
        self.daily.iter().sum()
    }

    /// Change against the prior equal-length window, as a fraction. `None`
    /// whenever [`Self::prev_tokens`] is, so a missing comparison stays missing
    /// instead of becoming `+100%`.
    pub fn delta(&self) -> Option<f64> {
        let prev = self.prev_tokens?;
        if prev == 0 {
            return None;
        }
        Some((self.tokens() as f64 - prev as f64) / prev as f64)
    }

    /// Share of this project's tasks that hit at least one tool error, or
    /// `None` when no tasks were recorded (see [`Self::tasks`]).
    pub fn failure_rate(&self) -> Option<f64> {
        if self.tasks == 0 {
            return None;
        }
        Some(self.failed_tasks as f64 / self.tasks as f64)
    }

    /// Assistant turns per task, or `None` when no tasks were recorded.
    pub fn turns_per_task(&self) -> Option<f64> {
        if self.tasks == 0 {
            return None;
        }
        Some(self.turns as f64 / self.tasks as f64)
    }
}

/// The task-side counts for one project in the display window. A named struct
/// rather than a tuple because six anonymous `u64`s in a row is how a turn count
/// ends up in an error column.
#[derive(Debug, Clone, Copy, Default)]
struct TaskCounts {
    tasks: u64,
    turns: u64,
    tool_calls: u64,
    tool_results: u64,
    tool_errors: u64,
    failed_tasks: u64,
}

/// The display window `[start, today]` and the equal-length window before it,
/// as `YYYY-MM-DD` bounds. `None` day count means `All`: no prior window.
fn windows(period: Period, today: chrono::NaiveDate) -> (Option<String>, Option<String>) {
    let days: i64 = match period {
        Period::Today => 1,
        Period::Week => 7,
        Period::Month => 30,
        // No prior window to compare against, and no lower scan bound.
        Period::All => return (None, None),
    };
    let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
    (
        Some(fmt(today - chrono::Duration::days(days - 1))),
        // Doubled window: the display half feeds the sparkline, the half before
        // it is the comparison, and one query serves both.
        Some(fmt(today - chrono::Duration::days(days * 2 - 1))),
    )
}

impl Store {
    /// Per-project task rows for `period`, heaviest first.
    ///
    /// Windows on `started_at` (like [`Store::ledger`]) rather than `ended_at`,
    /// and on calendar days, so for `Today`/`Week`/`Month` a row's sparkline
    /// sums to the total `Store::ledger(period.day_cutoff(), _)` shows for the
    /// same project. `All` is the exception: see [`TaskRow::daily`].
    pub fn task_table(&self, period: Period) -> Result<Vec<TaskRow>> {
        let today = chrono::Utc::now().date_naive();
        let (window_from, scan_from) = windows(period, today);

        // Per-project, per-day cache-write tokens over the doubled window. An
        // undated session cannot be placed on a day at all, so it has no bar to
        // draw; the aggregate query below still counts its tasks.
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(s.project, '(unknown)') AS p,
                    substr(s.started_at, 1, 10) AS d,
                    COALESCE(SUM(c.tokens), 0)
             FROM sessions s
             JOIN session_context c ON c.session_id = s.session_id
             WHERE s.started_at IS NOT NULL
               AND (?1 IS NULL OR substr(s.started_at, 1, 10) >= ?1)
             GROUP BY p, d",
        )?;
        let rows = stmt.query_map(params![scan_from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut by_day: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for row in rows {
            let (project, day, tokens) = row?;
            *by_day
                .entry(project)
                .or_default()
                .entry(day)
                .or_default() += tokens;
        }

        // Task aggregates over the DISPLAY window only. These are counts of
        // what happened, not a series, so the prior half must not leak in.
        //
        // `All` counts undated sessions too, matching `Store::ledger(None, _)`,
        // which is what the row's total comes from: a session Piggy could not
        // date still did the work, and dropping it here alone would report a
        // project's tokens without its outcomes.
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(s.project, '(unknown)') AS p,
                    COUNT(*),
                    COALESCE(SUM(t.n_turns), 0),
                    COALESCE(SUM(t.n_tool_calls), 0),
                    COALESCE(SUM(t.n_tool_results), 0),
                    COALESCE(SUM(t.n_tool_errors), 0),
                    COALESCE(SUM(CASE WHEN t.n_tool_errors > 0 THEN 1 ELSE 0 END), 0)
             FROM tasks t
             JOIN sessions s ON s.session_id = t.session_id
             WHERE ?1 IS NULL
                OR (s.started_at IS NOT NULL AND substr(s.started_at, 1, 10) >= ?1)
             GROUP BY p",
        )?;
        let rows = stmt.query_map(params![window_from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TaskCounts {
                    tasks: r.get::<_, i64>(1)? as u64,
                    turns: r.get::<_, i64>(2)? as u64,
                    tool_calls: r.get::<_, i64>(3)? as u64,
                    tool_results: r.get::<_, i64>(4)? as u64,
                    tool_errors: r.get::<_, i64>(5)? as u64,
                    failed_tasks: r.get::<_, i64>(6)? as u64,
                },
            ))
        })?;
        let mut agg: BTreeMap<String, TaskCounts> = BTreeMap::new();
        for row in rows {
            let (project, counts) = row?;
            agg.insert(project, counts);
        }

        // The window start for zero-filling. `All` has no fixed start, so it
        // begins at the earliest day any project actually recorded, clamped.
        let start = match window_from.as_deref() {
            Some(d) => parse_day(d).unwrap_or(today),
            None => by_day
                .values()
                .filter_map(|days| days.keys().next())
                .filter_map(|d| parse_day(d))
                .min()
                .unwrap_or(today)
                .max(today - chrono::Duration::days(MAX_ALL_DAYS - 1)),
        };

        // A project appears if it spent tokens OR recorded tasks: a row missing
        // from either side is still a real project, and dropping it would hide
        // spend rather than report it.
        let projects: BTreeSet<&String> = by_day.keys().chain(agg.keys()).collect();
        let mut out: Vec<TaskRow> = projects
            .into_iter()
            .cloned()
            .map(|project| {
                let days = by_day.get(&project);
                let mut daily = Vec::new();
                let mut d = start;
                while d <= today {
                    let key = d.format("%Y-%m-%d").to_string();
                    daily.push(days.and_then(|m| m.get(&key)).copied().unwrap_or(0));
                    d += chrono::Duration::days(1);
                }
                // Everything the scan picked up before the display window is,
                // by construction, the equal-length window before it.
                let prev_tokens = window_from.as_deref().map(|w| {
                    days.map(|m| {
                        m.iter()
                            .filter(|(day, _)| day.as_str() < w)
                            .map(|(_, t)| *t)
                            .sum::<u64>()
                    })
                    .unwrap_or(0)
                });
                let c = agg.get(&project).copied().unwrap_or_default();
                TaskRow {
                    project,
                    tasks: c.tasks,
                    turns: c.turns,
                    tool_calls: c.tool_calls,
                    tool_results: c.tool_results,
                    tool_errors: c.tool_errors,
                    failed_tasks: c.failed_tasks,
                    daily,
                    prev_tokens,
                }
            })
            .collect();

        // Heaviest first, ties by name so the table is stable run to run.
        out.sort_by(|a, b| {
            b.tokens()
                .cmp(&a.tokens())
                .then_with(|| a.project.cmp(&b.project))
        });
        Ok(out)
    }
}

fn parse_day(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}
