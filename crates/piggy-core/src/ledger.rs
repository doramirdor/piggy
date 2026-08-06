//! The context ledger: where a session's cache-write tokens came from.
//!
//! This is the accounting half of Piggy, and it is deliberately not statistics.
//! [`crate::attribution`] answers "does this saver help?", which needs
//! randomization, a holdout, and weeks of sessions before it can say anything.
//! The ledger answers "what is in my context and what did it cost?", which is a
//! receipt: every cache-write token is charged to the thing that caused it
//! during parsing, so the numbers are exact on day one with no holdout, no
//! confidence interval, and nothing to wait for.
//!
//! Three kinds of row, from [`crate::parser`]:
//!
//! * [`CTX_FLOOR`]: system prompt, tool definitions, memory. Paid once per
//!   session before the user types. The number that makes short sessions
//!   expensive.
//! * [`CTX_CONVERSATION`] is the work itself: prompts, tool results, file reads.
//! * Everything else is an attachment type (`hook_success`, `skill_listing`,
//!   `deferred_tools_delta`, …). These are **removable by configuration**, which
//!   is what makes them the actionable rows.

use anyhow::Result;

use crate::parser::{CTX_CONVERSATION, CTX_FLOOR, CTX_FLOOR_PREFIX};
use crate::pricing::Pricing;
use crate::store::Store;

/// One bucket of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// [`CTX_FLOOR`], [`CTX_CONVERSATION`], or an attachment type.
    pub kind: String,
    pub tokens: u64,
    /// How many assistant messages this bucket was charged on.
    pub n: u64,
}

impl LedgerRow {
    /// Whether this row is part of what a session pays before the user types:
    /// the unexplained floor residual, or a named floor component.
    pub fn is_floor(&self) -> bool {
        self.kind == CTX_FLOOR || self.kind.starts_with(CTX_FLOOR_PREFIX)
    }

    /// Whether a user could configure this away. Named floor components count
    /// (turning off a skill shrinks the floor) and so do per-turn injections.
    /// The floor *residual* does not: it is the system prompt and tool schemas,
    /// which no setting removes.
    pub fn removable(&self) -> bool {
        self.kind != CTX_FLOOR && self.kind != CTX_CONVERSATION
    }

    /// Whether the figure is a bounded estimate rather than a measured write.
    /// Floor residual and conversation are exact; everything charged by content
    /// size is not, and the UI must not present the two as equally precise.
    pub fn estimated(&self) -> bool {
        self.removable()
    }

    /// A human label. The reserved buckets get prose; attachment types are
    /// already their own best name.
    ///
    /// Floor components are suffixed `(startup)`. The same attachment type can
    /// appear twice (`hook_success` is loaded at startup AND re-injected during
    /// a session), and two rows with one name read as a duplicate rather than
    /// as the two different costs they are.
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            CTX_FLOOR => "session floor (system prompt + tool schemas)".to_string(),
            CTX_CONVERSATION => "conversation growth".to_string(),
            other => match other.strip_prefix(CTX_FLOOR_PREFIX) {
                Some(component) => format!("{component} (startup)"),
                None => other.to_string(),
            },
        }
    }
}

/// One project's split between what it paid to *open* sessions and what it paid
/// to do work.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    pub project: String,
    pub sessions: u64,
    pub msgs: u64,
    /// Cache-write tokens charged to [`CTX_FLOOR`].
    pub floor_tokens: u64,
    /// Everything else: conversation growth plus injections.
    pub work_tokens: u64,
}

impl ProjectRow {
    /// Assistant messages per session. The shape number: a project averaging
    /// ~1 is paying a full context floor per unit of work.
    pub fn msgs_per_session(&self) -> f64 {
        if self.sessions == 0 {
            0.0
        } else {
            self.msgs as f64 / self.sessions as f64
        }
    }

    /// Share of cache-write tokens that bought session startup rather than
    /// work. **The headline metric.** Measured on a real tree it separated
    /// long-running project work (1.5%) from a benchmark harness spawning
    /// thousands of one-message sessions (79%), which no per-turn average
    /// could distinguish.
    pub fn overhead(&self) -> f64 {
        let total = self.floor_tokens + self.work_tokens;
        if total == 0 {
            0.0
        } else {
            self.floor_tokens as f64 / total as f64
        }
    }
}

/// Below this removable share, [`Ledger::headroom`] reports nothing: a 1.02x
/// multiplier is not a finding, it is rounding dressed as one.
const HEADROOM_MIN_SHARE: f64 = 0.05;

/// The whole ledger: buckets globally, plus the per-project overhead split.
#[derive(Debug, Clone)]
pub struct Ledger {
    /// Buckets, largest first.
    pub rows: Vec<LedgerRow>,
    /// Per-project split, heaviest total first.
    pub projects: Vec<ProjectRow>,
    /// Total spend for the window in **input-token equivalents**, across every
    /// stream, including the output tokens and cache reads the ledger itself
    /// does not bucket.
    ///
    /// [`Self::headroom`] needs this. Removing configurable context shrinks
    /// cache writes only, and cache writes are not the whole bill: on a real
    /// tree they were 70.6% of cost, with output at 7.4% and cache reads at
    /// 22.0%. Dividing removable writes by total *writes* answered "what share
    /// of my writes is configurable" and then printed it as a **plan**
    /// multiplier, overstating 1.35x as 1.59x.
    pub cost_units: f64,
    /// Blended cost weight of one cache-write token in this window (between the
    /// 1.25x 5-minute and 2.0x 1-hour rates).
    pub write_weight: f64,
}

impl Ledger {
    pub fn total_tokens(&self) -> u64 {
        self.rows.iter().map(|r| r.tokens).sum()
    }

    /// Tokens in rows a user could configure away.
    pub fn removable_tokens(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.removable())
            .map(|r| r.tokens)
            .sum()
    }

    /// Share of all cache writes that bought session startup.
    pub fn overhead(&self) -> f64 {
        let total = self.total_tokens();
        if total == 0 {
            return 0.0;
        }
        let floor: u64 = self
            .rows
            .iter()
            .filter(|r| r.is_floor())
            .map(|r| r.tokens)
            .sum();
        floor as f64 / total as f64
    }

    /// How much further the same plan would go with the configurable context
    /// removed: `total / (total - removable)`.
    ///
    /// This is **available headroom, not achieved savings**, and the two must
    /// never be conflated in the UI. It is exact token arithmetic over cache
    /// writes (no pricing table, no holdout, no confidence interval), which is
    /// why it can be shown on day one while [`crate::attribution`] is still
    /// gathering. What it does NOT claim is that a saver already delivered it.
    ///
    /// `None` when there is nothing worth claiming: an empty ledger, or a
    /// removable share under [`HEADROOM_MIN_SHARE`], where the multiplier
    /// rounds to 1.0x and printing it just adds noise.
    pub fn headroom(&self) -> Option<f64> {
        let removable = self.removable_tokens();
        if removable == 0 || self.cost_units <= 0.0 {
            return None;
        }
        // Cost-weighted, NOT token-weighted: the denominator is the whole bill,
        // not just the part of it the ledger buckets.
        let share = (removable as f64 * self.write_weight) / self.cost_units;
        if share < HEADROOM_MIN_SHARE || share >= 1.0 {
            return None;
        }
        Some(1.0 / (1.0 - share))
    }

    /// Removable share of **total cost**, 0.0-1.0. The number behind
    /// [`Self::headroom`]; use this in copy, not [`Self::removable_share`],
    /// which is a share of cache writes only.
    pub fn removable_cost_share(&self) -> f64 {
        if self.cost_units <= 0.0 {
            return 0.0;
        }
        (self.removable_tokens() as f64 * self.write_weight / self.cost_units).min(1.0)
    }

    /// Removable share of all cache writes, 0.0-1.0. The number behind
    /// [`Self::headroom`], exposed so a UI can state it rather than only the
    /// multiplier derived from it.
    pub fn removable_share(&self) -> f64 {
        let total = self.total_tokens();
        if total == 0 {
            0.0
        } else {
            self.removable_tokens() as f64 / total as f64
        }
    }

    /// `tokens / total` for one row, 0.0 when the ledger is empty.
    pub fn share(&self, row: &LedgerRow) -> f64 {
        let total = self.total_tokens();
        if total == 0 {
            0.0
        } else {
            row.tokens as f64 / total as f64
        }
    }
}

impl Store {
    /// Build the ledger over every indexed session.
    ///
    /// `since` filters on `sessions.started_at` (an RFC3339 prefix compares
    /// correctly as a string); `None` covers all history.
    pub fn ledger(&self, since: Option<&str>, pricing: &Pricing) -> Result<Ledger> {
        let cutoff = since.unwrap_or("");
        let mut stmt = self.conn.prepare(
            "SELECT c.kind, SUM(c.tokens), SUM(c.n)
             FROM session_context c
             JOIN sessions s ON s.session_id = c.session_id
             WHERE COALESCE(s.started_at, '') >= ?1
             GROUP BY c.kind",
        )?;
        let mut rows: Vec<LedgerRow> = stmt
            .query_map([cutoff], |r| {
                Ok(LedgerRow {
                    kind: r.get(0)?,
                    tokens: r.get::<_, i64>(1)? as u64,
                    n: r.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        // Largest first, ties by name so the display is stable run to run.
        rows.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.kind.cmp(&b.kind)));

        // The inner GROUP BY collapses each session to one row FIRST. Joining
        // sessions to session_context directly would multiply `n_msgs` by the
        // number of ledger buckets that session has.
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(s.project, '(unknown)'), COUNT(*), SUM(s.n_msgs),
                    SUM(f.floor_tok), SUM(f.work_tok)
             FROM sessions s
             JOIN (SELECT session_id,
                          SUM(CASE WHEN kind = ?2 OR kind LIKE ?3 THEN tokens ELSE 0 END) AS floor_tok,
                          SUM(CASE WHEN kind <> ?2 AND kind NOT LIKE ?3 THEN tokens ELSE 0 END) AS work_tok
                   FROM session_context GROUP BY session_id) f
               ON f.session_id = s.session_id
             WHERE COALESCE(s.started_at, '') >= ?1
             GROUP BY COALESCE(s.project, '(unknown)')",
        )?;
        let mut projects: Vec<ProjectRow> = stmt
            .query_map(
                rusqlite::params![cutoff, CTX_FLOOR, format!("{CTX_FLOOR_PREFIX}%")],
                |r| {
                    Ok(ProjectRow {
                        project: r.get(0)?,
                        sessions: r.get::<_, i64>(1)? as u64,
                        msgs: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                        floor_tokens: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                        work_tokens: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                    })
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        projects.sort_by(|a, b| {
            (b.floor_tokens + b.work_tokens)
                .cmp(&(a.floor_tokens + a.work_tokens))
                .then_with(|| a.project.cmp(&b.project))
        });

        // Cost weights come from every stream in the window, not just the ones
        // the ledger buckets, so the headroom denominator is the real bill.
        let mut stmt = self.conn.prepare(
            "SELECT sm.model, COALESCE(SUM(sm.input_tokens), 0), COALESCE(SUM(sm.output_tokens), 0),
                    COALESCE(SUM(sm.cache_creation_tokens), 0),
                    COALESCE(SUM(sm.cache_creation_1h_tokens), 0),
                    COALESCE(SUM(sm.cache_read_tokens), 0)
             FROM session_models sm
             JOIN sessions s ON s.session_id = sm.session_id
             WHERE COALESCE(s.started_at, '') >= ?1
             GROUP BY sm.model",
        )?;
        let mut cost_units = 0.0f64;
        let mut all = crate::ModelTokens::default();
        let per_model = stmt.query_map([cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::ModelTokens {
                    input_tokens: r.get::<_, i64>(1)? as u64,
                    output_tokens: r.get::<_, i64>(2)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(3)? as u64,
                    cache_creation_1h_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_tokens: r.get::<_, i64>(5)? as u64,
                },
            ))
        })?;
        for row in per_model {
            let (model, t) = row?;
            cost_units += pricing.cost_units(&model, &t);
            all.input_tokens += t.input_tokens;
            all.output_tokens += t.output_tokens;
            all.cache_creation_tokens += t.cache_creation_tokens;
            all.cache_creation_1h_tokens += t.cache_creation_1h_tokens;
            all.cache_read_tokens += t.cache_read_tokens;
        }
        let write_weight = Pricing::write_blend(&all);

        Ok(Ledger {
            rows,
            projects,
            cost_units,
            write_weight,
        })
    }
}
