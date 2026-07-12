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

const SCHEMA_VERSION: i64 = 1;

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
            CREATE INDEX IF NOT EXISTS idx_sessions_ended ON sessions(ended_at);
            CREATE INDEX IF NOT EXISTS idx_session_models_model ON session_models(model);",
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
            "INSERT OR REPLACE INTO files (path, size, mtime_ns, offset_bytes, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![path, size, mtime_ns, size, parse.session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Total number of session rows.
    pub fn session_count(&self) -> Result<u64> {
        let n = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, u64>(0))?;
        Ok(n)
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
