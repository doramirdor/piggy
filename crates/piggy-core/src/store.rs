//! SQLite persistence of per-session token aggregates.
//!
//! Database lives at `<home>/piggy.db` (WAL mode). Query methods live in
//! [`crate::stats`] as additional `impl Store` blocks. All writes go through a
//! transaction.

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::parser::SessionParse;
use crate::pricing::Pricing;

const SCHEMA_VERSION: i64 = 3;

/// How a session's saver assignment came to be, stored in `session_savers.source`.
/// `rotation`/`holdout` are Piggy's A/B scheduler; `manual` is a user toggle;
/// `pre_install` marks sessions that predate Piggy (observational baseline).
pub mod source {
    pub const ROTATION: &str = "rotation";
    pub const MANUAL: &str = "manual";
    pub const HOLDOUT: &str = "holdout";
    pub const PRE_INSTALL: &str = "pre_install";
}

/// One `(saver_id, enabled, source)` fact snapshotted for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaverTag {
    pub saver_id: String,
    pub enabled: bool,
    pub source: String,
}

impl SaverTag {
    pub fn new(saver_id: impl Into<String>, enabled: bool, source: impl Into<String>) -> Self {
        SaverTag {
            saver_id: saver_id.into(),
            enabled,
            source: source.into(),
        }
    }
}

/// Handle to the Piggy SQLite database.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database under `home`. Ensures the parent
    /// directory exists, enables WAL, and applies the schema.
    pub fn open(home: &Path) -> Result<Store> {
        std::fs::create_dir_all(home)?;
        let conn = Connection::open(home.join("piggy.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE IF NOT EXISTS sessions (
                session_id   TEXT PRIMARY KEY,
                project      TEXT,
                git_branch   TEXT,
                started_at   TEXT,
                ended_at     TEXT,
                n_msgs       INTEGER NOT NULL DEFAULT 0,
                n_user_msgs  INTEGER NOT NULL DEFAULT 0,
                parse_errors INTEGER NOT NULL DEFAULT 0,
                indexed_at   TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_models (
                session_id               TEXT NOT NULL,
                model                    TEXT NOT NULL,
                input_tokens             INTEGER NOT NULL DEFAULT 0,
                output_tokens            INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens    INTEGER NOT NULL DEFAULT 0,
                cache_creation_1h_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens        INTEGER NOT NULL DEFAULT 0,
                cost_usd_est             REAL,
                PRIMARY KEY (session_id, model)
            );
            CREATE TABLE IF NOT EXISTS files (
                path         TEXT PRIMARY KEY,
                size         INTEGER NOT NULL,
                mtime_ns     INTEGER NOT NULL,
                offset_bytes INTEGER NOT NULL DEFAULT 0,
                session_id   TEXT
            );
            CREATE TABLE IF NOT EXISTS session_tools (
                session_id TEXT NOT NULL,
                tool_name  TEXT NOT NULL,
                n          INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, tool_name)
            );
            CREATE TABLE IF NOT EXISTS session_savers (
                session_id TEXT NOT NULL,
                saver_id   TEXT NOT NULL,
                enabled    INTEGER NOT NULL DEFAULT 0,
                source     TEXT NOT NULL,
                PRIMARY KEY (session_id, saver_id)
            );
            CREATE TABLE IF NOT EXISTS rotation_state (
                id           INTEGER PRIMARY KEY CHECK (id = 0),
                block_pos    INTEGER NOT NULL DEFAULT 0,
                planned_next TEXT,
                updated_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_ended ON sessions(ended_at);
            CREATE INDEX IF NOT EXISTS idx_session_models_model ON session_models(model);
            CREATE INDEX IF NOT EXISTS idx_session_tools_name ON session_tools(tool_name);
            CREATE INDEX IF NOT EXISTS idx_session_savers_saver ON session_savers(saver_id);",
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// The `(size, mtime_ns)` last recorded for `path`, if any. Used to skip
    /// unchanged files during incremental indexing.
    pub fn file_state(&self, path: &str) -> Result<Option<(i64, i64)>> {
        let row = self
            .conn
            .query_row(
                "SELECT size, mtime_ns FROM files WHERE path = ?1",
                [path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Persist one parsed session (replacing any prior rows for it) plus its
    /// file bookkeeping, atomically. `size`/`mtime_ns` describe the source file
    /// on disk so a later index run can detect changes.
    pub fn upsert_session(
        &mut self,
        parse: &SessionParse,
        pricing: &Pricing,
        path: &str,
        size: i64,
        mtime_ns: i64,
    ) -> Result<()> {
        let indexed_at = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO sessions
             (session_id, project, git_branch, started_at, ended_at,
              n_msgs, n_user_msgs, parse_errors, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                parse.session_id,
                parse.project_path,
                parse.git_branch,
                parse.first_ts,
                parse.last_ts,
                parse.n_assistant_msgs,
                parse.n_user_msgs,
                parse.parse_errors,
                indexed_at,
            ],
        )?;
        tx.execute(
            "DELETE FROM session_models WHERE session_id = ?1",
            params![parse.session_id],
        )?;
        for (model, tok) in &parse.models {
            let cost = pricing.cost_usd(model, tok);
            tx.execute(
                "INSERT OR REPLACE INTO session_models
                 (session_id, model, input_tokens, output_tokens,
                  cache_creation_tokens, cache_creation_1h_tokens,
                  cache_read_tokens, cost_usd_est)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    parse.session_id,
                    model,
                    tok.input_tokens,
                    tok.output_tokens,
                    tok.cache_creation_tokens,
                    tok.cache_creation_1h_tokens,
                    tok.cache_read_tokens,
                    cost,
                ],
            )?;
        }
        tx.execute(
            "DELETE FROM session_tools WHERE session_id = ?1",
            params![parse.session_id],
        )?;
        for (tool, n) in &parse.tool_use_counts {
            tx.execute(
                "INSERT OR REPLACE INTO session_tools (session_id, tool_name, n)
                 VALUES (?1, ?2, ?3)",
                params![parse.session_id, tool, n],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime_ns, offset_bytes, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![path, size, mtime_ns, size, parse.session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Summed `tool_use` counts across the most recent `n_sessions` sessions
    /// (by last-activity time). Keys are full tool names (`mcp__<server>__<tool>`
    /// / `Skill`). Backs Sweep's usage cross-reference.
    pub fn recent_tool_usage(
        &self,
        n_sessions: usize,
    ) -> Result<std::collections::BTreeMap<String, u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool_name, SUM(n)
             FROM session_tools
             WHERE session_id IN (
                 SELECT session_id FROM sessions
                 ORDER BY ended_at DESC LIMIT ?1
             )
             GROUP BY tool_name",
        )?;
        let rows = stmt.query_map(params![n_sessions as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })?;
        let mut out = std::collections::BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// Number of sessions considered by [`Self::recent_tool_usage`] (capped at
    /// `n_sessions`).
    pub fn recent_session_count(&self, n_sessions: usize) -> Result<u64> {
        let n = self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT session_id FROM sessions ORDER BY ended_at DESC LIMIT ?1)",
            params![n_sessions as i64],
            |r| r.get::<_, u64>(0),
        )?;
        Ok(n)
    }

    /// Total number of session rows.
    pub fn session_count(&self) -> Result<u64> {
        let n = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, u64>(0))?;
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Session tagging (session_savers) — M3 ground truth for A/B attribution
    // -----------------------------------------------------------------------

    /// Replace the saver-set snapshot for `session_id` with `tags`, atomically.
    /// Passing an empty slice clears the session's tags.
    pub fn set_session_savers(&mut self, session_id: &str, tags: &[SaverTag]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM session_savers WHERE session_id = ?1",
            params![session_id],
        )?;
        for t in tags {
            tx.execute(
                "INSERT OR REPLACE INTO session_savers (session_id, saver_id, enabled, source)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, t.saver_id, t.enabled as i64, t.source],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The saver-set snapshot recorded for `session_id` (empty if untagged).
    pub fn session_savers(&self, session_id: &str) -> Result<Vec<SaverTag>> {
        let mut stmt = self.conn.prepare(
            "SELECT saver_id, enabled, source FROM session_savers
             WHERE session_id = ?1 ORDER BY saver_id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(SaverTag {
                saver_id: r.get::<_, String>(0)?,
                enabled: r.get::<_, i64>(1)? != 0,
                source: r.get::<_, String>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Distinct saver ids that appear in any session snapshot (i.e. have
    /// attribution data), sorted. Backs `piggy report` when nothing is currently
    /// installed but historical A/B data exists.
    pub fn tagged_saver_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT saver_id FROM session_savers ORDER BY saver_id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Whether `session_id` already has a saver-set snapshot.
    pub fn has_session_savers(&self, session_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_savers WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Tag every **untagged** session that started before `cutoff` (an RFC3339
    /// install-anchor timestamp) as the `pre_install` baseline: one `enabled=0`
    /// row per id in `saver_ids`. Returns the number of sessions newly tagged.
    ///
    /// A session with a NULL `started_at` is left alone (we cannot prove it
    /// predates Piggy). Idempotent: re-running skips sessions that already have
    /// any tag (e.g. ones the watcher snapshotted live).
    pub fn tag_pre_install(&mut self, cutoff: &str, saver_ids: &[String]) -> Result<usize> {
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT s.session_id FROM sessions s
                 WHERE s.started_at IS NOT NULL AND s.started_at < ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM session_savers ss WHERE ss.session_id = s.session_id
                   )",
            )?;
            let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row?);
            }
            v
        };
        let tags: Vec<SaverTag> = saver_ids
            .iter()
            .map(|id| SaverTag::new(id.clone(), false, source::PRE_INSTALL))
            .collect();
        for id in &ids {
            self.set_session_savers(id, &tags)?;
        }
        Ok(ids.len())
    }

    // -----------------------------------------------------------------------
    // Rotation state
    // -----------------------------------------------------------------------

    /// Load `(block_pos, planned_next_json)`; defaults to `(0, None)` if unset.
    pub fn rotation_state(&self) -> Result<(i64, Option<String>)> {
        let row = self
            .conn
            .query_row(
                "SELECT block_pos, planned_next FROM rotation_state WHERE id = 0",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(row.unwrap_or((0, None)))
    }

    /// Persist the rotation cursor and the JSON of the next planned set.
    pub fn set_rotation_state(&mut self, block_pos: i64, planned_next: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO rotation_state (id, block_pos, planned_next, updated_at)
             VALUES (0, ?1, ?2, ?3)",
            params![block_pos, planned_next, now],
        )?;
        Ok(())
    }

    /// Verify the database is writable (used by `piggy doctor`).
    pub fn write_test(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _piggy_write_test (x INTEGER);
             DROP TABLE _piggy_write_test;",
        )?;
        Ok(())
    }
}
