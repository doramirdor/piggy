//! Query layer over the stored aggregates.
//!
//! Time windows filter on a session's last-activity timestamp (`ended_at`).
//! Windows use UTC boundaries for determinism. Everything here is an additional
//! inherent `impl Store`, so callers use `store.totals(period)` etc.

use anyhow::Result;
use rusqlite::params;

use crate::store::Store;

/// A reporting time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Week,
    Month,
    All,
}

impl Period {
    /// Human label for tables.
    pub fn label(&self) -> &'static str {
        match self {
            Period::Today => "Today",
            Period::Week => "Last 7 days",
            Period::Month => "Last 30 days",
            Period::All => "All time",
        }
    }

    /// Inclusive lower bound as an ISO8601-Z string comparable to stored
    /// timestamps, or `None` for all-time (no lower bound).
    pub fn cutoff(&self) -> Option<String> {
        let now = chrono::Utc::now();
        let naive = match self {
            Period::Today => now.date_naive().and_hms_opt(0, 0, 0).unwrap(),
            Period::Week => (now - chrono::Duration::days(7)).naive_utc(),
            Period::Month => (now - chrono::Duration::days(30)).naive_utc(),
            Period::All => return None,
        };
        Some(naive.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
    }
}

/// Aggregated token/cost figures for a window or group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub sessions: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_read_tokens: u64,
    /// Sum of estimated cost over models that have pricing.
    pub cost_usd_est: f64,
    /// Tokens belonging to models with no pricing (excluded from `cost_usd_est`).
    pub unpriced_tokens: u64,
}

impl Totals {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }

    /// Whether every contributing token was priced.
    pub fn fully_priced(&self) -> bool {
        self.unpriced_tokens == 0
    }
}

/// One row of a grouped breakdown (by project or model).
#[derive(Debug, Clone)]
pub struct GroupRow {
    pub key: String,
    pub totals: Totals,
}

const AGG_COLS: &str = "COUNT(DISTINCT s.session_id),
     COALESCE(SUM(sm.input_tokens),0),
     COALESCE(SUM(sm.output_tokens),0),
     COALESCE(SUM(sm.cache_creation_tokens),0),
     COALESCE(SUM(sm.cache_creation_1h_tokens),0),
     COALESCE(SUM(sm.cache_read_tokens),0),
     COALESCE(SUM(sm.cost_usd_est),0.0),
     COALESCE(SUM(CASE WHEN sm.cost_usd_est IS NULL
         THEN sm.input_tokens+sm.output_tokens+sm.cache_creation_tokens+sm.cache_read_tokens
         ELSE 0 END),0)";

fn row_to_totals(r: &rusqlite::Row, base: usize) -> rusqlite::Result<Totals> {
    Ok(Totals {
        sessions: r.get(base)?,
        input_tokens: r.get(base + 1)?,
        output_tokens: r.get(base + 2)?,
        cache_creation_tokens: r.get(base + 3)?,
        cache_creation_1h_tokens: r.get(base + 4)?,
        cache_read_tokens: r.get(base + 5)?,
        cost_usd_est: r.get(base + 6)?,
        unpriced_tokens: r.get(base + 7)?,
    })
}

impl Store {
    /// Overall totals for a window.
    pub fn totals(&self, period: Period) -> Result<Totals> {
        let cutoff = period.cutoff();
        let sql = format!(
            "SELECT {AGG_COLS}
             FROM sessions s JOIN session_models sm ON sm.session_id = s.session_id
             WHERE (?1 IS NULL OR s.ended_at >= ?1)"
        );
        let t = self
            .conn
            .query_row(&sql, params![cutoff], |r| row_to_totals(r, 0))?;
        Ok(t)
    }

    /// Breakdown grouped by project (`cwd`), largest first.
    pub fn by_project(&self, period: Period) -> Result<Vec<GroupRow>> {
        self.grouped(period, "COALESCE(s.project, '(unknown)')")
    }

    /// Breakdown grouped by model, largest first.
    pub fn by_model(&self, period: Period) -> Result<Vec<GroupRow>> {
        self.grouped(period, "sm.model")
    }

    fn grouped(&self, period: Period, key_expr: &str) -> Result<Vec<GroupRow>> {
        let cutoff = period.cutoff();
        // `key_expr` is a fixed internal literal, never user input.
        let sql = format!(
            "SELECT {key_expr} AS k, {AGG_COLS}
             FROM sessions s JOIN session_models sm ON sm.session_id = s.session_id
             WHERE (?1 IS NULL OR s.ended_at >= ?1)
             GROUP BY k
             ORDER BY COALESCE(SUM(sm.input_tokens+sm.output_tokens+
                 sm.cache_creation_tokens+sm.cache_read_tokens),0) DESC, k ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![cutoff], |r| {
            let key: String = r.get(0)?;
            Ok(GroupRow {
                key,
                totals: row_to_totals(r, 1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// `(matched_tokens, total_tokens)` where matched tokens belong to models
    /// that have a pricing entry. Used for pricing-coverage diagnostics.
    pub fn pricing_coverage(&self) -> Result<(u64, u64)> {
        let sql = "SELECT
            COALESCE(SUM(CASE WHEN cost_usd_est IS NOT NULL
                THEN input_tokens+output_tokens+cache_creation_tokens+cache_read_tokens
                ELSE 0 END),0),
            COALESCE(SUM(input_tokens+output_tokens+cache_creation_tokens+cache_read_tokens),0)
            FROM session_models";
        let pair = self
            .conn
            .query_row(sql, [], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?)))?;
        Ok(pair)
    }

    /// Sum of `parse_errors` across all sessions.
    pub fn total_parse_errors(&self) -> Result<u64> {
        let n = self.conn.query_row(
            "SELECT COALESCE(SUM(parse_errors),0) FROM sessions",
            [],
            |r| r.get::<_, u64>(0),
        )?;
        Ok(n)
    }
}
