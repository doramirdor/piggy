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

// 6: added `tasks`, the per-prompt unit. Everything before it was keyed by
// session (a container) or normalised per turn (a denominator); neither can
// answer "which of my tasks was expensive" or "did it work".
// 7: `tasks.n_tool_results`, the denominator `n_tool_errors` never had. Also the
// first version anything READS: v6 shipped the table but not the re-index that
// fills it, so every v6 database in the wild has session rows and no task rows.
// Bumping past 6 is what gets those databases their one-time re-parse.
// 8: M5's three advisor tables (`mcp_manifests`, `claudemd_files`, `advice`).
// Purely additive - nothing existing is rewritten, and none of the three is
// filled by the parser: they hold what the probe measured, what the CLAUDE.md
// scanner inventoried, and the lifecycle of each suggestion.
const SCHEMA_VERSION: i64 = 8;

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

/// `mcp_manifests.scope` for a server configured at the top level of
/// `~/.claude.json` (user scope, loaded by every session). A project-scoped
/// server stores its project path instead, mirroring
/// [`crate::sweep::SweepItem::source`] (whose `None` is this same case).
pub const SCOPE_USER: &str = "";

/// One measured MCP tool-schema manifest, as written by the probe.
///
/// `config_hash` is the fingerprint of the server's configured command, args
/// and env at measurement time: a caller compares it against the current config
/// and treats a mismatch as "not measured" rather than trusting the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpManifest {
    /// Server name as it appears in `~/.claude.json`.
    pub server_key: String,
    /// The project path the server is configured under, or [`SCOPE_USER`].
    pub scope: String,
    pub config_hash: String,
    pub tool_count: i64,
    pub schema_bytes: i64,
    pub schema_tokens: i64,
    /// Which tokenizer produced `schema_tokens` (the advisor's, or the
    /// bytes/3.5 fallback), so the UI can label a real count as measured and an
    /// approximation as estimated.
    pub tokenizer: String,
    pub measured_at: String,
    /// False when the probe could not measure this server; `error` says why.
    pub ok: bool,
    pub error: Option<String>,
}

/// One CLAUDE.md-family file in the inventory. Contents are never stored: only
/// the size, the estimated token cost, and the hash/mtime that detect a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudemdFile {
    pub path: String,
    /// The project this file belongs to, or `None` for the global files under
    /// `~/.claude`.
    pub project: Option<String>,
    pub bytes: i64,
    pub est_tokens: i64,
    pub hash: String,
    pub mtime_ns: i64,
    pub last_scanned: String,
}

/// Lifecycle values for [`AdviceRow::status`].
pub mod advice_status {
    /// Generated and awaiting the user.
    pub const OPEN: &str = "open";
    /// Applied, with `applied_at` and a `restore_ref` for one-click Undo.
    pub const APPLIED: &str = "applied";
    /// "Not for me": suppressed for this target until the evidence moves.
    pub const DISMISSED: &str = "dismissed";
    /// The evidence it was drafted against changed; never applyable.
    pub const STALE: &str = "stale";
}

/// One candidate action and where it got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdviceRow {
    /// Stable hash of kind + target + evidence inputs.
    pub id: String,
    pub kind: String,
    pub target: String,
    pub created_at: String,
    /// Hash of the facts this candidate was generated from.
    pub facts_hash: Option<String>,
    pub est_tokens_month: i64,
    /// One of [`advice_status`].
    pub status: String,
    /// Kind-specific evidence and parameters, serialized by the advice engine.
    pub payload_json: Option<String>,
    pub applied_at: Option<String>,
    /// How to undo the apply (a file-snapshot or sweep record handle).
    pub restore_ref: Option<String>,
    pub dismiss_note: Option<String>,
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
            // busy_timeout lets a concurrent open wait for the background
            // indexer's write lock instead of failing instantly with
            // SQLITE_BUSY — otherwise a command-thread open during a heavy
            // reindex errors out and the UI misreads it as "no sessions yet".
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        // The CREATE TABLEs run first so `meta` exists to be read; the version
        // it holds is only overwritten at the very end of this function.
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
                indexed_at   TEXT NOT NULL,
                source       TEXT NOT NULL DEFAULT 'claude-code',
                interface    TEXT NOT NULL DEFAULT 'unknown',
                client       TEXT
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
            -- One row per user prompt: the thing a person actually asked for.
            -- `n_tool_errors` is the outcome signal, and the reason this table
            -- exists as much as the token columns are: on real data a task that
            -- hits a tool error costs several times a clean one, which no
            -- session-level or per-turn figure can see.
            CREATE TABLE IF NOT EXISTS tasks (
                session_id    TEXT NOT NULL,
                prompt_id     TEXT NOT NULL,
                spend_tokens  INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                n_turns       INTEGER NOT NULL DEFAULT 0,
                n_tool_calls  INTEGER NOT NULL DEFAULT 0,
                n_tool_results INTEGER NOT NULL DEFAULT 0,
                n_tool_errors INTEGER NOT NULL DEFAULT 0,
                started_at    TEXT,
                ended_at      TEXT,
                PRIMARY KEY (session_id, prompt_id)
            );
            CREATE TABLE IF NOT EXISTS session_savers (
                session_id TEXT NOT NULL,
                saver_id   TEXT NOT NULL,
                enabled    INTEGER NOT NULL DEFAULT 0,
                source     TEXT NOT NULL,
                PRIMARY KEY (session_id, saver_id)
            );
            CREATE TABLE IF NOT EXISTS session_context (
                session_id TEXT NOT NULL,
                kind       TEXT NOT NULL,
                tokens     INTEGER NOT NULL DEFAULT 0,
                n          INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, kind)
            );
            CREATE TABLE IF NOT EXISTS rotation_state (
                id           INTEGER PRIMARY KEY CHECK (id = 0),
                block_pos    INTEGER NOT NULL DEFAULT 0,
                planned_next TEXT,
                updated_at   TEXT
            );
            -- One measured MCP tool-schema manifest per (server, scope). The
            -- `config_hash` stamps what was measured, so a changed
            -- command/args/env reads as unmeasured rather than quietly
            -- reporting a stale figure; `ok = 0` rows record a failed probe
            -- (with its `error`) so a server that cannot be measured is visible
            -- instead of missing.
            CREATE TABLE IF NOT EXISTS mcp_manifests (
                server_key    TEXT NOT NULL,
                scope         TEXT NOT NULL,
                config_hash   TEXT NOT NULL,
                tool_count    INTEGER NOT NULL DEFAULT 0,
                schema_bytes  INTEGER NOT NULL DEFAULT 0,
                schema_tokens INTEGER NOT NULL DEFAULT 0,
                tokenizer     TEXT NOT NULL,
                measured_at   TEXT NOT NULL,
                ok            INTEGER NOT NULL DEFAULT 0,
                error         TEXT,
                PRIMARY KEY (server_key, scope)
            );
            -- Inventory of the CLAUDE.md files loaded into every session, one
            -- row per file. Sizes and hashes only: contents are read at scan
            -- time and never stored.
            CREATE TABLE IF NOT EXISTS claudemd_files (
                path         TEXT PRIMARY KEY,
                project      TEXT,
                bytes        INTEGER NOT NULL DEFAULT 0,
                est_tokens   INTEGER NOT NULL DEFAULT 0,
                hash         TEXT NOT NULL,
                mtime_ns     INTEGER NOT NULL DEFAULT 0,
                last_scanned TEXT NOT NULL
            );
            -- The suggestion ledger: one row per candidate action, carrying its
            -- lifecycle (open -> applied | dismissed | stale). `id` is a stable
            -- hash of kind + target + evidence, so regenerating the same
            -- candidate finds its own row and inherits its history.
            CREATE TABLE IF NOT EXISTS advice (
                id               TEXT PRIMARY KEY,
                kind             TEXT NOT NULL,
                target           TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                facts_hash       TEXT,
                est_tokens_month INTEGER NOT NULL DEFAULT 0,
                status           TEXT NOT NULL,
                payload_json     TEXT,
                applied_at       TEXT,
                restore_ref      TEXT,
                dismiss_note     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_ended ON sessions(ended_at);
            CREATE INDEX IF NOT EXISTS idx_session_models_model ON session_models(model);
            CREATE INDEX IF NOT EXISTS idx_session_tools_name ON session_tools(tool_name);
            -- Ranking tasks by cost is the primary read, and correlating error
            -- count with spend is the second.
            CREATE INDEX IF NOT EXISTS idx_tasks_spend ON tasks(spend_tokens DESC);
            CREATE INDEX IF NOT EXISTS idx_tasks_errors ON tasks(n_tool_errors);
            CREATE INDEX IF NOT EXISTS idx_session_savers_saver ON session_savers(saver_id);
            CREATE INDEX IF NOT EXISTS idx_files_session ON files(session_id);
            CREATE INDEX IF NOT EXISTS idx_session_context_kind ON session_context(kind);
            -- The advice surface reads one status at a time (open for the Spend
            -- section, applied for Undo).
            CREATE INDEX IF NOT EXISTS idx_advice_status ON advice(status);",
        )?;
        let stored = self.schema_version()?;
        // v3 → v4: sessions grew source/interface/client (multi-tool
        // observability). ALTERs run before the index that uses the columns;
        // pre-existing rows are Claude Code by construction (the only source
        // v3 indexed), with an unknown surface until their next re-index.
        if !self.column_exists("sessions", "source")? {
            self.conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN source TEXT NOT NULL DEFAULT 'claude-code';
                 ALTER TABLE sessions ADD COLUMN interface TEXT NOT NULL DEFAULT 'unknown';
                 ALTER TABLE sessions ADD COLUMN client TEXT;",
            )?;
        }
        // v6 → v7: tasks grew `n_tool_results`. Additive, like the columns above.
        if !self.column_exists("tasks", "n_tool_results")? {
            self.conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN n_tool_results INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source, interface);",
        )?;
        // A new schema can only fill its new columns by re-reading the logs, and
        // incremental indexing skips every file whose (size, mtime_ns) still
        // match, which is every finished session, forever. Poisoning the pair is
        // what makes the next run re-parse: without it v6's `tasks` table stayed
        // empty on every existing install and the UI blamed logs that do carry
        // `promptId`.
        //
        // An UPDATE rather than DELETE because the rows carry more than the skip
        // check: `session_rate_map` reads `files.path` to keep subagent
        // sub-sessions out of attribution, and dropping them would widen that
        // population until the re-index finished writing them back.
        //
        // `None` is a database this function has never run on: nothing indexed,
        // so nothing to invalidate.
        if matches!(stored, Some(v) if v < SCHEMA_VERSION) {
            self.conn
                .execute("UPDATE files SET size = -1, mtime_ns = -1", [])?;
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// The schema version this database was last written by, or `None` before
    /// any migration has run against it.
    pub fn schema_version(&self) -> Result<Option<i64>> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v.and_then(|s| s.parse().ok()))
    }

    /// Whether `table` already has a column named `col` (SQLite pragma probe,
    /// used for additive migrations).
    fn column_exists(&self, table: &str, col: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for n in names {
            if n? == col {
                return Ok(true);
            }
        }
        Ok(false)
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
              n_msgs, n_user_msgs, parse_errors, indexed_at,
              source, interface, client)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                parse.source,
                parse.interface,
                parse.client,
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
        // Replace wholesale, like every other per-session table: a re-parse of
        // the same file is authoritative over whatever a partial earlier pass
        // wrote.
        tx.execute(
            "DELETE FROM tasks WHERE session_id = ?1",
            params![parse.session_id],
        )?;
        for (prompt_id, t) in &parse.tasks {
            tx.execute(
                "INSERT OR REPLACE INTO tasks
                   (session_id, prompt_id, spend_tokens, cache_read_tokens,
                    n_turns, n_tool_calls, n_tool_results, n_tool_errors,
                    started_at, ended_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    parse.session_id,
                    prompt_id,
                    t.spend_tokens,
                    t.cache_read_tokens,
                    t.n_turns,
                    t.n_tool_calls,
                    t.n_tool_results,
                    t.n_tool_errors,
                    t.first_ts,
                    t.last_ts
                ],
            )?;
        }
        tx.execute(
            "DELETE FROM session_context WHERE session_id = ?1",
            params![parse.session_id],
        )?;
        for (kind, c) in &parse.context {
            tx.execute(
                "INSERT OR REPLACE INTO session_context (session_id, kind, tokens, n)
                 VALUES (?1, ?2, ?3, ?4)",
                params![parse.session_id, kind, c.tokens, c.n],
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
    /// (by last-activity time), split by the project each session ran in. Keys
    /// are full tool names (`mcp__<server>__<tool>` / `Skill`), then project
    /// path; a session with no recorded project lands under `""`.
    ///
    /// Split rather than totalled because a total cannot tell a tool that earns
    /// its place everywhere from one that earns it in a single project and is
    /// loaded by every other session for nothing. Backs Sweep's usage
    /// cross-reference and its scope call.
    pub fn recent_tool_usage(
        &self,
        n_sessions: usize,
    ) -> Result<std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tool_name, COALESCE(s.project, ''), SUM(t.n)
             FROM session_tools t
             JOIN sessions s ON s.session_id = t.session_id
             WHERE t.session_id IN (
                 SELECT session_id FROM sessions
                 ORDER BY ended_at DESC LIMIT ?1
             )
             GROUP BY t.tool_name, s.project",
        )?;
        let rows = stmt.query_map(params![n_sessions as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
            ))
        })?;
        let mut out: std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (tool, project, n) = row?;
            out.entry(tool).or_default().insert(project, n);
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

    // -----------------------------------------------------------------------
    // M5 advisor tables (mcp_manifests / claudemd_files / advice)
    // -----------------------------------------------------------------------

    /// Record (or replace) the manifest measurement for one server+scope.
    pub fn upsert_mcp_manifest(&mut self, m: &McpManifest) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO mcp_manifests
               (server_key, scope, config_hash, tool_count, schema_bytes,
                schema_tokens, tokenizer, measured_at, ok, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.server_key,
                m.scope,
                m.config_hash,
                m.tool_count,
                m.schema_bytes,
                m.schema_tokens,
                m.tokenizer,
                m.measured_at,
                m.ok as i64,
                m.error,
            ],
        )?;
        Ok(())
    }

    /// The measurement recorded for `server_key` in `scope`, if any.
    ///
    /// Returns whatever was last measured, stale or not: only the caller knows
    /// the server's *current* config hash, so freshness is its call to make
    /// (compare [`McpManifest::config_hash`]).
    pub fn mcp_manifest(&self, server_key: &str, scope: &str) -> Result<Option<McpManifest>> {
        let row = self
            .conn
            .query_row(
                "SELECT server_key, scope, config_hash, tool_count, schema_bytes,
                        schema_tokens, tokenizer, measured_at, ok, error
                 FROM mcp_manifests WHERE server_key = ?1 AND scope = ?2",
                params![server_key, scope],
                mcp_manifest_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Every recorded manifest measurement, in stable (server, scope) order.
    pub fn mcp_manifests(&self) -> Result<Vec<McpManifest>> {
        let mut stmt = self.conn.prepare(
            "SELECT server_key, scope, config_hash, tool_count, schema_bytes,
                    schema_tokens, tokenizer, measured_at, ok, error
             FROM mcp_manifests ORDER BY server_key, scope",
        )?;
        let rows = stmt.query_map([], mcp_manifest_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record (or replace) one CLAUDE.md inventory row.
    pub fn upsert_claudemd_file(&mut self, f: &ClaudemdFile) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO claudemd_files
               (path, project, bytes, est_tokens, hash, mtime_ns, last_scanned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                f.path,
                f.project,
                f.bytes,
                f.est_tokens,
                f.hash,
                f.mtime_ns,
                f.last_scanned,
            ],
        )?;
        Ok(())
    }

    /// The whole CLAUDE.md inventory, in path order.
    pub fn claudemd_files(&self) -> Result<Vec<ClaudemdFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, project, bytes, est_tokens, hash, mtime_ns, last_scanned
             FROM claudemd_files ORDER BY path",
        )?;
        let rows = stmt.query_map([], claudemd_file_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Insert a freshly generated candidate, returning whether it was new.
    ///
    /// An id already in the table is left **exactly** as it is: the id is a hash
    /// of the candidate's own evidence, so a regenerated candidate is the same
    /// suggestion, and overwriting would resurrect a dismissed row as open or
    /// drop an applied row's `restore_ref` (its only route back). Moving a row
    /// on is [`Self::set_advice_status`]'s job.
    pub fn insert_advice(&mut self, a: &AdviceRow) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO advice
               (id, kind, target, created_at, facts_hash, est_tokens_month,
                status, payload_json, applied_at, restore_ref, dismiss_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                a.id,
                a.kind,
                a.target,
                a.created_at,
                a.facts_hash,
                a.est_tokens_month,
                a.status,
                a.payload_json,
                a.applied_at,
                a.restore_ref,
                a.dismiss_note,
            ],
        )?;
        Ok(n > 0)
    }

    /// One advice row by id.
    pub fn advice(&self, id: &str) -> Result<Option<AdviceRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, target, created_at, facts_hash, est_tokens_month,
                        status, payload_json, applied_at, restore_ref, dismiss_note
                 FROM advice WHERE id = ?1",
                params![id],
                advice_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Every advice row with `status`, biggest estimated saving first.
    ///
    /// Ties break on `id` so the same facts always produce the same list in the
    /// same order: the UI takes the top few, and that slice must not shuffle
    /// between reads.
    pub fn advice_by_status(&self, status: &str) -> Result<Vec<AdviceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, target, created_at, facts_hash, est_tokens_month,
                    status, payload_json, applied_at, restore_ref, dismiss_note
             FROM advice WHERE status = ?1
             ORDER BY est_tokens_month DESC, id",
        )?;
        let rows = stmt.query_map(params![status], advice_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Move `id` to `status`, writing the three lifecycle stamps alongside it.
    ///
    /// All three are written as given rather than merged, because they belong to
    /// the status: an apply passes `applied_at` + `restore_ref`, a dismissal
    /// passes `dismiss_note`, and an Undo passing `open` with three `None`s
    /// leaves no stamp from a state the row is no longer in.
    ///
    /// Returns false when no such row exists, so a caller can report a missing
    /// id instead of assuming the write landed.
    pub fn set_advice_status(
        &mut self,
        id: &str,
        status: &str,
        applied_at: Option<&str>,
        restore_ref: Option<&str>,
        dismiss_note: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE advice
             SET status = ?2, applied_at = ?3, restore_ref = ?4, dismiss_note = ?5
             WHERE id = ?1",
            params![id, status, applied_at, restore_ref, dismiss_note],
        )?;
        Ok(n > 0)
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

// Row mappers for the M5 tables, shared by the by-key and list-all reads so the
// column order is written down once per table.

fn mcp_manifest_from_row(r: &rusqlite::Row) -> rusqlite::Result<McpManifest> {
    Ok(McpManifest {
        server_key: r.get(0)?,
        scope: r.get(1)?,
        config_hash: r.get(2)?,
        tool_count: r.get(3)?,
        schema_bytes: r.get(4)?,
        schema_tokens: r.get(5)?,
        tokenizer: r.get(6)?,
        measured_at: r.get(7)?,
        ok: r.get::<_, i64>(8)? != 0,
        error: r.get(9)?,
    })
}

fn claudemd_file_from_row(r: &rusqlite::Row) -> rusqlite::Result<ClaudemdFile> {
    Ok(ClaudemdFile {
        path: r.get(0)?,
        project: r.get(1)?,
        bytes: r.get(2)?,
        est_tokens: r.get(3)?,
        hash: r.get(4)?,
        mtime_ns: r.get(5)?,
        last_scanned: r.get(6)?,
    })
}

fn advice_from_row(r: &rusqlite::Row) -> rusqlite::Result<AdviceRow> {
    Ok(AdviceRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        target: r.get(2)?,
        created_at: r.get(3)?,
        facts_hash: r.get(4)?,
        est_tokens_month: r.get(5)?,
        status: r.get(6)?,
        payload_json: r.get(7)?,
        applied_at: r.get(8)?,
        restore_ref: r.get(9)?,
        dismiss_note: r.get(10)?,
    })
}
