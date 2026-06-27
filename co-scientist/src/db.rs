//! Database layer: opens a local SQLite file (via `rusqlite` with the bundled
//! SQLite library), runs schema migrations on first open, exposes the [`Db`]
//! handle. No network, no auth — just a file.
//!
//! `rusqlite` is synchronous. We hold the `Connection` behind a
//! `std::sync::Mutex` and call its methods directly from the async
//! functions — SQLite is single-threaded, fast, and rarely blocks. This
//! avoids the lifetime gymnastics of `spawn_blocking` (which requires
//! `'static` captures and would force every caller to clone strings).

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, Params};

/// Cheap handle to a shared connection. Returned by [`Db::conn`].
/// Mirrors the shape of the old `turso::Connection` API (async
/// `.query`, `.execute`) but is backed by `rusqlite`. The connection
/// is shared — every call serializes through a mutex. For write
/// concurrency, callers should create independent `Db` instances
/// (each with its own `Connection` to the same file).
#[derive(Clone)]
pub struct Conn {
    inner: Arc<Mutex<Connection>>,
}

impl Conn {
    /// Execute a statement that doesn't return rows.
///
/// The `Connection` is held behind a `std::sync::Mutex`. The query runs
/// synchronously inside this async fn — SQLite is single-threaded and
/// fast, so we don't bother with `spawn_blocking`. The future captures
/// `&self` for its lifetime, so callers may pass borrowed `&str` params.
    pub async fn execute<P>(&self, sql: &str, params: P) -> Result<usize>
    where
        P: Params,
    {
        let n = {
            let guard = self.inner.lock().expect("db mutex poisoned");
            guard.execute(sql, params)?
        };
        Ok(n)
    }

    /// Run `sql` and return all rows materialized as owned values.
    /// The returned [`Rows`] supports `.next().await?` to match the
    /// previous turso-shaped API.
    pub async fn query<P>(&self, sql: &str, params: P) -> Result<Rows>
    where
        P: Params,
    {
        let owned = {
            let guard = self.inner.lock().expect("db mutex poisoned");
            let mut stmt = guard.prepare(sql)?;
            let n_cols = stmt.column_count();
            let mut rows = stmt.query(params)?;
            let mut out: Vec<Vec<rusqlite::types::Value>> = Vec::new();
            while let Some(row) = rows.next()? {
                let mut vals = Vec::with_capacity(n_cols);
                for i in 0..n_cols {
                    vals.push(row.get::<_, rusqlite::types::Value>(i)?);
                }
                out.push(vals);
            }
            OwnedRows { rows: out, idx: 0 }
        };
        Ok(Rows { inner: owned })
    }
}

/// Rows hold owned column values, not borrowed rusqlite::Rows.
pub struct Rows {
    inner: OwnedRows,
}

struct OwnedRows {
    rows: Vec<Vec<rusqlite::types::Value>>,
    idx: usize,
}

impl Rows {
    /// Awaitable next. Returns `Ok(None)` on exhaustion.
    pub async fn next(&mut self) -> Result<Option<OwnedRow>, rusqlite::Error> {
        if self.inner.idx >= self.inner.rows.len() {
            return Ok(None);
        }
        let row = OwnedRow {
            values: std::mem::take(&mut self.inner.rows[self.inner.idx]),
        };
        self.inner.idx += 1;
        Ok(Some(row))
    }
}

/// A single row with owned values.
pub struct OwnedRow {
    values: Vec<rusqlite::types::Value>,
}

impl OwnedRow {
    /// Get the column at `idx` as `T`. Mirrors the old turso API: single
    /// generic `T` for the column type, index is always `usize`.
    pub fn get<T: rusqlite::types::FromSql>(&self, idx: usize) -> rusqlite::Result<T> {
        let vref = value_to_ref(&self.values[idx]);
        T::column_result(vref).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Null, Box::new(e))
        })
    }
}

/// Borrow an owned `Value` as a `ValueRef<'_>`.
fn value_to_ref(val: &rusqlite::types::Value) -> rusqlite::types::ValueRef<'_> {
    use rusqlite::types::ValueRef;
    use rusqlite::types::Value as V;
    match val {
        V::Null => ValueRef::Null,
        V::Integer(i) => ValueRef::Integer(*i),
        V::Real(f) => ValueRef::Real(*f),
        V::Text(s) => ValueRef::Text(s.as_bytes()),
        V::Blob(b) => ValueRef::Blob(b.as_slice()),
    }
}

/// Open a local file-backed SQLite database and run schema migrations.
///
/// `path` may be:
///   - a plain filesystem path (`./co_scientist.db`, `/home/you/x.db`)
///   - `:memory:` for an in-memory DB
pub async fn open(path: &str) -> Result<Db> {
    let path_str = path.to_owned();
    let conn = tokio::task::spawn_blocking(move || Connection::open(path_str.as_str()))
        .await
        .context("joining open thread")?
        .with_context(|| format!("opening sqlite db at {path}"))?;
    migrate(&conn)?;
    Ok(Db::new(conn))
}

/// Open an in-memory database (useful for tests and short-lived sessions).
pub async fn open_memory() -> Result<Db> {
    let conn = tokio::task::spawn_blocking(Connection::open_in_memory)
        .await
        .context("joining open_memory thread")?
        .context("opening in-memory sqlite db")?;
    migrate(&conn)?;
    Ok(Db::new(conn))
}

/// Wrapper around a single SQLite connection guarded by a `std::sync::Mutex`.
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Db {
    /// Build a Db from an already-opened Connection.
    pub fn new(conn: Connection) -> Db {
        Db {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    /// Borrow a connection handle for executing queries.
    pub fn conn(&self) -> Conn {
        Conn {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Open a fresh connection to the same file. Returns a raw `Connection`.
    pub async fn connect_fresh(path: &str) -> Result<Connection> {
        let path_str = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || Connection::open(path_str.as_str()))
            .await
            .context("joining connect_fresh thread")?
            .with_context(|| format!("opening fresh sqlite connection at {path}"))?;
        Ok(conn)
    }

    /// Build a Db wrapping an already-opened Connection.
    pub fn with_new_conn(&self, conn: Connection) -> Db {
        Db::new(conn)
    }
}

/// Schema migration. Idempotent — safe to run on every open.
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .context("setting pragmas")?;

    let stmts = [
        r#"
        CREATE TABLE IF NOT EXISTS agents (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            name            TEXT    NOT NULL UNIQUE,
            role            TEXT    NOT NULL,
            system_prompt   TEXT    NOT NULL,
            created_at      TEXT    NOT NULL
        )"#,
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            run_id          TEXT    NOT NULL,
            agent_id        INTEGER NOT NULL,
            started_at      TEXT    NOT NULL,
            ended_at        TEXT,
            PRIMARY KEY (run_id, agent_id),
            FOREIGN KEY (agent_id) REFERENCES agents(id)
        )"#,
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT    NOT NULL,
            agent_id        INTEGER NOT NULL,
            step_index      INTEGER NOT NULL,
            type            TEXT    NOT NULL,
            payload_json    TEXT,
            created_at      TEXT    NOT NULL,
            idempotency_key TEXT,
            FOREIGN KEY (run_id, agent_id) REFERENCES sessions(run_id, agent_id)
        )"#,
        r#"
        CREATE TABLE IF NOT EXISTS semantic_memories (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT    NOT NULL,
            agent_id        INTEGER,
            scope           TEXT    NOT NULL,
            summary         TEXT    NOT NULL,
            details_json    TEXT,
            embedding       BLOB,
            importance      REAL    NOT NULL DEFAULT 1.0,
            archived        INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT    NOT NULL,
            idempotency_key TEXT
        )"#,
        r#"
        CREATE TABLE IF NOT EXISTS behavior_memories (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id        INTEGER NOT NULL,
            pattern         TEXT    NOT NULL,
            notes           TEXT    NOT NULL,
            evidence_json   TEXT,
            created_at      TEXT    NOT NULL,
            FOREIGN KEY (agent_id) REFERENCES agents(id)
        )"#,
        "CREATE INDEX IF NOT EXISTS idx_events_run_agent ON events(run_id, agent_id)",
        "CREATE INDEX IF NOT EXISTS idx_semantic_run ON semantic_memories(run_id)",
        "CREATE INDEX IF NOT EXISTS idx_semantic_scope ON semantic_memories(scope)",
        "CREATE INDEX IF NOT EXISTS idx_behavior_agent ON behavior_memories(agent_id)",
        r#"
        CREATE TABLE IF NOT EXISTS tasks (
            id                TEXT    PRIMARY KEY,
            session_id        TEXT    NOT NULL,
            agent             TEXT    NOT NULL,
            action            TEXT    NOT NULL,
            payload           TEXT    NOT NULL,
            priority          INTEGER NOT NULL DEFAULT 100,
            status            TEXT    NOT NULL DEFAULT 'pending',
            lease_owner       TEXT,
            lease_expires_at  INTEGER,
            attempts          INTEGER NOT NULL DEFAULT 0,
            max_attempts      INTEGER NOT NULL DEFAULT 3,
            created_at        TEXT    NOT NULL,
            started_at        TEXT,
            finished_at       TEXT,
            last_error        TEXT,
            next_retry_at     INTEGER,
            idempotency_key   TEXT
        )"#,
        "CREATE INDEX IF NOT EXISTS idx_tasks_queue ON tasks(session_id, status, priority, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_tasks_lease ON tasks(status, lease_expires_at)",
        "CREATE TABLE IF NOT EXISTS term_index (\
            memory_kind TEXT NOT NULL, \
            memory_id   INTEGER NOT NULL, \
            term        TEXT NOT NULL, \
            PRIMARY KEY (memory_kind, memory_id, term))",
        "CREATE INDEX IF NOT EXISTS idx_term_index_term ON term_index(term)",
        "CREATE INDEX IF NOT EXISTS idx_term_index_kind_id ON term_index(memory_kind, memory_id)",
        r#"
        CREATE TABLE IF NOT EXISTS research_sessions (
            id              TEXT    PRIMARY KEY,
            goal            TEXT    NOT NULL,
            preferences     TEXT,
            status          TEXT    NOT NULL DEFAULT 'running',
            budget_usd      REAL,
            tokens_used     INTEGER NOT NULL DEFAULT 0,
            started_at      TEXT    NOT NULL,
            ended_at        TEXT,
            final_report    TEXT
        )"#,
        r#"
        CREATE TABLE IF NOT EXISTS hypotheses (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id      TEXT    NOT NULL,
            state           TEXT    NOT NULL DEFAULT 'draft',
            elo             REAL    NOT NULL DEFAULT 1200.0,
            parent_ids      TEXT,
            semantic_id     INTEGER,
            matches_played  INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT    NOT NULL
        )"#,
        "CREATE INDEX IF NOT EXISTS idx_hypotheses_session ON hypotheses(session_id)",
        "CREATE INDEX IF NOT EXISTS idx_hypotheses_elo ON hypotheses(session_id, elo DESC)",
        r#"
        CREATE TABLE IF NOT EXISTS tournament_matches (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id      TEXT    NOT NULL,
            hypothesis_a    INTEGER NOT NULL,
            hypothesis_b    INTEGER NOT NULL,
            winner          INTEGER NOT NULL,
            rationale       TEXT,
            created_at      TEXT    NOT NULL
        )"#,
        "CREATE INDEX IF NOT EXISTS idx_matches_session ON tournament_matches(session_id)",
    ];

    for sql in stmts {
        conn.execute(sql, [])
            .with_context(|| format!("migration failed: {sql}"))?;
    }

    try_add_column(conn, "events", "idempotency_key", "TEXT")?;
    try_add_column(conn, "semantic_memories", "idempotency_key", "TEXT")?;
    try_add_column(conn, "behavior_memories", "idempotency_key", "TEXT")?;
    try_add_column(conn, "tasks", "idempotency_key", "TEXT")?;
    try_add_column(conn, "tasks", "next_retry_at", "INTEGER")?;
    try_add_column(conn, "events", "rendered_prompt", "TEXT")?;
    try_add_column(conn, "events", "raw_response", "TEXT")?;
    try_add_column(conn, "events", "marker_op_outcome", "TEXT")?;
    try_add_column(conn, "semantic_memories", "last_accessed_at", "TEXT")?;
    try_add_column(conn, "behavior_memories", "last_accessed_at", "TEXT")?;
    try_add_column(conn, "behavior_memories", "archived", "INTEGER NOT NULL DEFAULT 0")?;

    for sql in [
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_events_idem ON events(idempotency_key)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_semantic_idem ON semantic_memories(idempotency_key)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_behavior_idem ON behavior_memories(idempotency_key)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_idem ON tasks(idempotency_key)",
    ] {
        conn.execute(sql, [])
            .with_context(|| format!("creating index: {sql}"))?;
    }

    Ok(())
}

fn try_add_column(
    conn: &Connection,
    table: &str,
    column: &str,
    col_type: &str,
) -> Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {col_type}");
    match conn.execute(&sql, []) {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.to_string().contains("duplicate column name") {
                Ok(())
            } else {
                Err(anyhow::anyhow!("add column {table}.{column} failed: {e}"))
            }
        }
    }
}