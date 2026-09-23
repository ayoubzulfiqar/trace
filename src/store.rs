//! Phase 4: Execution Memory & the persistent structural index.
//!
//! SQLite-backed persistence for:
//! - the structural index (per-file fingerprint, content hash and facts), so
//!   a restarted server re-parses only what changed while it was down
//! - sessions (session_id, timestamp, agent_name, summary)
//! - touched files (session_id, file_path, change_reason)
//! - events (free-form execution log)
//! - a decisions cache (Phase 3 metadata)
//!
//! The database runs in WAL mode with a busy timeout so a daemon, inline
//! servers and CLI invocations can share it safely.

use crate::humanize::now_ms;
use crate::model::{HistoricalEvent, SessionRecord, TouchedFileRecord};
use crate::structural::{FileFacts, IndexedFile, STRUCTURAL_EXTRACTOR_VERSION};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use std::time::Duration;

/// Rows encoded/decoded per parallel batch when persisting the index.
const INDEX_CHUNK: usize = 1024;

/// Schema version stored in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i32 = 2;

/// Version 1: the original schema (kept verbatim so old databases upgrade).
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    timestamp   INTEGER NOT NULL,
    agent_name  TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    detail      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id  TEXT PRIMARY KEY,
    timestamp   INTEGER NOT NULL,
    agent_name  TEXT NOT NULL,
    summary     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS touched_files (
    session_id  TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    change_reason TEXT NOT NULL,
    PRIMARY KEY (session_id, file_path)
);

CREATE TABLE IF NOT EXISTS decisions (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    status      TEXT NOT NULL,
    context     TEXT NOT NULL,
    "decision"  TEXT NOT NULL,
    consequences TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    author      TEXT NOT NULL,
    supersedes  TEXT
);
"#;

/// Version 2: persistent structural index; drop the never-used v1 caches;
/// indexes for history queries.
const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS file_index (
    rel_path          TEXT PRIMARY KEY,
    mtime_ns          INTEGER NOT NULL,
    size              INTEGER NOT NULL,
    hash              INTEGER NOT NULL,
    extractor_version INTEGER NOT NULL,
    facts             TEXT NOT NULL
) WITHOUT ROWID;

DROP TABLE IF EXISTS scan_cache;
DROP TABLE IF EXISTS fingerprints;

CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_sessions_timestamp ON sessions(timestamp);
CREATE INDEX IF NOT EXISTS idx_touched_files_path ON touched_files(file_path);
"#;

/// Does this error mean the file on disk is not a usable database?
fn is_corrupt(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(
                e.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            )
    )
}

/// Move a damaged database (and its WAL sidecars) out of the way.
fn quarantine(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let to = path.with_file_name(format!("{name}.corrupt-{}", now_ms()));
    let renamed = std::fs::rename(path, &to).is_ok();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{name}{suffix}")));
    }
    renamed.then_some(to)
}

/// Run `op`, retrying while SQLite reports the database busy/locked
/// (up to ~5 s with backoff).
fn retry_busy<T>(mut op: impl FnMut() -> SqlResult<T>) -> SqlResult<T> {
    let mut delay = Duration::from_millis(5);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match op() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(200));
            }
            other => return other,
        }
    }
}

/// A single trace store — wraps a SQLite connection.
pub struct TraceStore {
    pub conn: Connection,
}

/// One session with the files it touched.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionWithFiles {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub touched_files: Vec<TouchedFileRecord>,
}

impl TraceStore {
    /// Open (or create) the SQLite database at `path`, migrating it to the
    /// current schema.
    /// A damaged database is moved aside and replaced, so a project never
    /// gets stuck without persistence.
    pub fn open(path: &std::path::Path) -> SqlResult<Self> {
        match Self::open_at(path) {
            Err(e) if is_corrupt(&e) => {
                let moved = quarantine(path);
                eprintln!(
                    "trace: {} is not a usable database ({e}){}; starting a new one",
                    path.display(),
                    match &moved {
                        Some(to) => format!("; moved it to {}", to.display()),
                        None => String::new(),
                    }
                );
                Self::open_at(path)
            }
            other => other,
        }
    }

    fn open_at(path: &std::path::Path) -> SqlResult<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // WAL: concurrent readers + one writer across daemon/CLI processes.
        // Switching a fresh file to WAL takes an exclusive lock that the busy
        // handler does not always wait for, so contention is retried here.
        retry_busy(|| {
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        })?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        let mut store = TraceStore { conn };
        retry_busy(|| store.migrate())?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (fallback when file-based DB is unavailable).
    pub fn open_in_memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;
        let mut store = TraceStore { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> SqlResult<()> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        // IMMEDIATE takes the write lock up front (a deferred transaction
        // upgrading to write fails with BUSY without waiting); re-read the
        // version under the lock in case another process just migrated.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        if version < 1 {
            tx.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            tx.execute_batch(SCHEMA_V2)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()
    }

    // ── Structural index ──────────────────────────────────────────────

    /// Load every cached file produced by the current extractor version.
    /// Rows from older extractors (or undecodable rows) are skipped, which
    /// makes the next refresh re-parse those files. Rows are decoded in
    /// parallel, a chunk at a time, so peak memory stays near the final size.
    pub fn load_index(&self) -> SqlResult<Vec<(String, IndexedFile)>> {
        // Rows from other extractor versions can never be used again.
        let _ = self.conn.execute(
            "DELETE FROM file_index WHERE extractor_version != ?1",
            params![STRUCTURAL_EXTRACTOR_VERSION],
        );
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, mtime_ns, size, hash, facts FROM file_index
             WHERE extractor_version = ?1",
        )?;
        let mut rows = stmt.query_map(params![STRUCTURAL_EXTRACTOR_VERSION], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        loop {
            let chunk: Vec<_> = rows.by_ref().take(INDEX_CHUNK).collect::<SqlResult<_>>()?;
            if chunk.is_empty() {
                break;
            }
            out.par_extend(chunk.into_par_iter().filter_map(
                |(rel, mtime_ns, size, hash, facts)| {
                    let mut facts: FileFacts = serde_json::from_str(&facts).ok()?;
                    facts.set_path(&rel);
                    Some((
                        rel,
                        IndexedFile {
                            mtime_ns,
                            size: size as u64,
                            hash: hash as u64,
                            facts,
                        },
                    ))
                },
            ));
        }
        Ok(out)
    }

    /// Persist an index delta atomically (facts are encoded in parallel, a
    /// chunk at a time).
    pub fn save_index_changes(
        &mut self,
        upserts: &[(String, IndexedFile)],
        touched: &[(String, i64, u64)],
        removed: &[String],
    ) -> SqlResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT OR REPLACE INTO file_index
                 (rel_path, mtime_ns, size, hash, extractor_version, facts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for chunk in upserts.chunks(INDEX_CHUNK) {
                let encoded: Vec<String> = chunk
                    .par_iter()
                    .map(|(_, entry)| {
                        serde_json::to_string(&entry.facts).unwrap_or_else(|_| "{}".into())
                    })
                    .collect();
                for ((rel, entry), facts) in chunk.iter().zip(&encoded) {
                    upsert.execute(params![
                        rel,
                        entry.mtime_ns,
                        entry.size as i64,
                        entry.hash as i64,
                        STRUCTURAL_EXTRACTOR_VERSION,
                        facts
                    ])?;
                }
            }
            let mut touch = tx.prepare_cached(
                "UPDATE file_index SET mtime_ns = ?2, size = ?3 WHERE rel_path = ?1",
            )?;
            for (rel, mtime_ns, size) in touched {
                touch.execute(params![rel, mtime_ns, *size as i64])?;
            }
            let mut delete = tx.prepare_cached("DELETE FROM file_index WHERE rel_path = ?1")?;
            for rel in removed {
                delete.execute(params![rel])?;
            }
        }
        tx.commit()
    }

    /// Rewrite the database file, releasing pages freed by removed files.
    /// Must not run inside a transaction.
    pub fn vacuum(&self) -> SqlResult<()> {
        self.conn.execute_batch("VACUUM")
    }

    /// Drop the whole structural index cache.
    pub fn clear_index(&self) -> SqlResult<()> {
        self.conn.execute("DELETE FROM file_index", [])?;
        Ok(())
    }

    /// Number of cached files (any extractor version).
    pub fn index_size(&self) -> SqlResult<usize> {
        self.conn
            .query_row("SELECT COUNT(*) FROM file_index", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n as usize)
    }

    // ── Phase 4: Sessions ─────────────────────────────────────────────

    /// Record (or replace) a session summary.
    pub fn save_session(&self, session: &SessionRecord) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (session_id, timestamp, agent_name, summary)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session.session_id,
                session.timestamp_ms,
                session.agent_name,
                session.summary
            ],
        )?;
        Ok(())
    }

    /// Record a session and the files it touched in one transaction. Files
    /// already logged for the session get their reason updated.
    pub fn record_session(
        &mut self,
        session: &SessionRecord,
        touched: &[TouchedFileRecord],
    ) -> SqlResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO sessions (session_id, timestamp, agent_name, summary)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session.session_id,
                session.timestamp_ms,
                session.agent_name,
                session.summary
            ],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO touched_files (session_id, file_path, change_reason)
                 VALUES (?1, ?2, ?3)",
            )?;
            for file in touched {
                stmt.execute(params![
                    session.session_id,
                    file.file_path,
                    file.change_reason
                ])?;
            }
        }
        tx.commit()
    }

    /// Recent sessions, most recent first.
    pub fn get_recent_history(&self, limit: usize) -> SqlResult<Vec<SessionRecord>> {
        self.recent_sessions(limit, None)
    }

    /// Recent sessions, optionally restricted to one agent.
    pub fn recent_sessions(
        &self,
        limit: usize,
        agent: Option<&str>,
    ) -> SqlResult<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp, agent_name, summary
             FROM sessions
             WHERE (?2 IS NULL OR agent_name = ?2)
             ORDER BY timestamp DESC, rowid DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64, agent], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                timestamp_ms: row.get(1)?,
                agent_name: row.get(2)?,
                summary: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Recent sessions with their touched files.
    pub fn recent_history_with_files(
        &self,
        limit: usize,
        agent: Option<&str>,
    ) -> SqlResult<Vec<SessionWithFiles>> {
        self.recent_sessions(limit, agent)?
            .into_iter()
            .map(|session| {
                let touched_files = self.get_touched_files(&session.session_id)?;
                Ok(SessionWithFiles {
                    session,
                    touched_files,
                })
            })
            .collect()
    }

    /// Look up one session.
    pub fn get_session(&self, session_id: &str) -> SqlResult<Option<SessionRecord>> {
        self.conn
            .query_row(
                "SELECT session_id, timestamp, agent_name, summary FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionRecord {
                        session_id: row.get(0)?,
                        timestamp_ms: row.get(1)?,
                        agent_name: row.get(2)?,
                        summary: row.get(3)?,
                    })
                },
            )
            .optional()
    }

    /// Total number of recorded sessions.
    pub fn session_count(&self) -> SqlResult<usize> {
        self.conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n as usize)
    }

    // ── Phase 4: Touched files ────────────────────────────────────────

    /// Log that `file_path` was touched in `session_id` for `reason`.
    pub fn log_touched_file(
        &self,
        session_id: &str,
        file_path: &str,
        reason: &str,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO touched_files (session_id, file_path, change_reason)
             VALUES (?1, ?2, ?3)",
            params![session_id, file_path, reason],
        )?;
        Ok(())
    }

    /// All files touched in a session.
    pub fn get_touched_files(&self, session_id: &str) -> SqlResult<Vec<TouchedFileRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT session_id, file_path, change_reason
             FROM touched_files
             WHERE session_id = ?1
             ORDER BY file_path",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(TouchedFileRecord {
                session_id: row.get(0)?,
                file_path: row.get(1)?,
                change_reason: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Sessions that touched `file_path`, most recent first, with the reason.
    pub fn file_history(
        &self,
        file_path: &str,
        limit: usize,
    ) -> SqlResult<Vec<(SessionRecord, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, s.timestamp, s.agent_name, s.summary, t.change_reason
             FROM touched_files t
             JOIN sessions s ON s.session_id = t.session_id
             WHERE t.file_path = ?1
             ORDER BY s.timestamp DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![file_path, limit as i64], |row| {
            Ok((
                SessionRecord {
                    session_id: row.get(0)?,
                    timestamp_ms: row.get(1)?,
                    agent_name: row.get(2)?,
                    summary: row.get(3)?,
                },
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.collect()
    }

    // ── Phase 4: General events ───────────────────────────────────────

    /// Append a historical event (file change, task step, bug note…).
    pub fn log_event(&self, event: &HistoricalEvent) -> SqlResult<()> {
        let detail_json = serde_json::to_string(&event.detail).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "INSERT INTO events (session_id, timestamp, agent_name, event_type, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &event.session_id,
                event.timestamp_ms,
                &event.agent_name,
                &event.event_type,
                detail_json
            ],
        )?;
        Ok(())
    }

    /// Events for a session, most recent first.
    pub fn get_events(&self, session_id: &str, limit: usize) -> SqlResult<Vec<HistoricalEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp, agent_name, event_type, detail
             FROM events
             WHERE session_id = ?1
             ORDER BY timestamp DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            let detail_str: String = row.get(4)?;
            let detail: serde_json::Value =
                serde_json::from_str(&detail_str).unwrap_or(serde_json::Value::Null);
            Ok(HistoricalEvent {
                session_id: row.get(0)?,
                timestamp_ms: row.get(1)?,
                agent_name: row.get(2)?,
                event_type: row.get(3)?,
                detail,
            })
        })?;
        rows.collect()
    }

    // ── Phase 3: Decisions cache ──────────────────────────────────────

    /// Cache a Decision for fast lookup.
    pub fn save_decision_cached(&self, dec: &crate::model::Decision) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO decisions
             (id, title, status, context, decision, consequences, created_at, author, supersedes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &dec.id,
                &dec.title,
                dec.status.to_string(),
                &dec.context,
                &dec.decision,
                &dec.consequences,
                dec.created_at_ms,
                &dec.author,
                &dec.supersedes
            ],
        )?;
        Ok(())
    }

    /// Search cached decisions by keyword (literal substring, case-insensitive).
    pub fn search_cached_decisions(&self, query: &str) -> SqlResult<Vec<crate::model::Decision>> {
        let escaped = query
            .to_lowercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, context, decision, consequences, created_at, author, supersedes
             FROM decisions
             WHERE lower(title) LIKE ?1 ESCAPE '\\'
                OR lower(context) LIKE ?1 ESCAPE '\\'
                OR lower(decision) LIKE ?1 ESCAPE '\\'
                OR lower(consequences) LIKE ?1 ESCAPE '\\'
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![&pattern], |row| {
            Ok(crate::model::Decision {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get::<_, String>(2)?.parse().unwrap_or_default(),
                context: row.get(3)?,
                decision: row.get(4)?,
                consequences: row.get(5)?,
                created_at_ms: row.get(6)?,
                author: row.get(7)?,
                supersedes: row.get(8)?,
                tags: Vec::new(),
                links: Vec::new(),
            })
        })?;
        rows.collect()
    }

    /// Delete sessions, their touched files, and events older than
    /// `max_age_ms`. Returns the number of sessions removed.
    pub fn prune_history(&mut self, max_age_ms: i64) -> SqlResult<usize> {
        let cutoff = now_ms() - max_age_ms;
        let tx = self.conn.transaction()?;
        let count = tx.execute("DELETE FROM sessions WHERE timestamp < ?1", params![cutoff])?;
        tx.execute(
            "DELETE FROM touched_files WHERE session_id NOT IN (SELECT session_id FROM sessions)",
            [],
        )?;
        tx.execute("DELETE FROM events WHERE timestamp < ?1", params![cutoff])?;
        tx.commit()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Decision, DecisionStatus};
    use crate::structural::extract_file;

    fn session(id: &str, ts: i64, agent: &str) -> SessionRecord {
        SessionRecord {
            session_id: id.to_string(),
            timestamp_ms: ts,
            agent_name: agent.to_string(),
            summary: format!("summary of {id}"),
        }
    }

    fn touched(session: &str, path: &str, reason: &str) -> TouchedFileRecord {
        TouchedFileRecord {
            session_id: session.to_string(),
            file_path: path.to_string(),
            change_reason: reason.to_string(),
        }
    }

    #[test]
    fn open_creates_tables_and_sets_schema_version() {
        let store = TraceStore::open_in_memory().unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for table in [
            "sessions",
            "touched_files",
            "events",
            "decisions",
            "file_index",
        ] {
            assert!(tables.contains(&table.to_string()), "missing {table}");
        }
        assert!(!tables.contains(&"scan_cache".to_string()));
        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn file_database_uses_wal_and_migrates_v1_databases() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.db");
        {
            // A database written by an old release: v1 tables, user_version 0.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(
                "CREATE TABLE scan_cache (rel_path TEXT, mtime_ms INTEGER, size INTEGER, symbols_json TEXT);
                 INSERT INTO sessions VALUES ('old', 1, 'agent', 'kept');",
            )
            .unwrap();
        }
        let store = TraceStore::open(&path).unwrap();
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        assert_eq!(store.get_recent_history(10).unwrap()[0].summary, "kept");
        assert_eq!(store.index_size().unwrap(), 0);
        drop(store);
        // Re-opening an up-to-date database is a no-op.
        assert!(TraceStore::open(&path).is_ok());
    }

    #[test]
    fn concurrent_opens_of_a_fresh_database_all_succeed() {
        for _ in 0..20 {
            let dir = tempfile::TempDir::new().unwrap();
            let path = std::sync::Arc::new(dir.path().join("trace.db"));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    let path = std::sync::Arc::clone(&path);
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        TraceStore::open(&path).map(|_| ())
                    })
                })
                .collect();
            for h in handles {
                h.join()
                    .unwrap()
                    .expect("open must not fail with database is locked");
            }
        }
    }

    #[test]
    fn a_corrupt_database_is_replaced_and_usable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.db");
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();
        std::fs::write(dir.path().join("trace.db-wal"), b"stale wal").unwrap();

        let mut store = TraceStore::open(&path).expect("a corrupt database is replaced");
        store
            .record_session(&session("s1", 1, "agent"), &[touched("s1", "a.rs", "why")])
            .unwrap();
        assert_eq!(store.get_recent_history(5).unwrap().len(), 1);

        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "the old file is kept for inspection");
        // A fresh WAL belongs to the new database; the stale one is gone.
        let wal = std::fs::read(dir.path().join("trace.db-wal")).unwrap_or_default();
        assert_ne!(wal, b"stale wal", "the stale sidecar was discarded");

        // Re-opening the fresh database must not quarantine it again.
        drop(store);
        assert!(TraceStore::open(&path).is_ok());
        let still: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(still.len(), 1);
    }

    #[test]
    fn vacuum_releases_space_after_removals() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.db");
        let mut store = TraceStore::open(&path).unwrap();
        let entry = |rel: &str| IndexedFile {
            mtime_ns: 1,
            size: 1,
            hash: 1,
            facts: extract_file(rel, &"pub fn padding_to_make_rows_large() {}\n".repeat(40)),
        };
        let upserts: Vec<(String, IndexedFile)> = (0..300)
            .map(|i| (format!("src/f{i}.rs"), entry(&format!("src/f{i}.rs"))))
            .collect();
        store.save_index_changes(&upserts, &[], &[]).unwrap();
        let removed: Vec<String> = upserts.iter().skip(5).map(|(rel, _)| rel.clone()).collect();
        store.save_index_changes(&[], &[], &removed).unwrap();
        // In WAL mode the pages live in the sidecar until a checkpoint.
        let checkpoint = |store: &TraceStore| {
            store
                .conn
                .pragma_update(None, "wal_checkpoint", "TRUNCATE")
                .unwrap();
            std::fs::metadata(&path).unwrap().len()
        };
        let before = checkpoint(&store);
        store.vacuum().unwrap();
        let after = checkpoint(&store);
        assert!(after < before, "vacuum should shrink {before} -> {after}");
        assert_eq!(store.load_index().unwrap().len(), 5);
    }

    #[test]
    fn index_round_trip_and_delta() {
        let mut store = TraceStore::open_in_memory().unwrap();
        let entry = |text: &str| IndexedFile {
            mtime_ns: 10,
            size: text.len() as u64,
            hash: 42,
            facts: extract_file("src/a.rs", text),
        };
        store
            .save_index_changes(
                &[
                    ("src/a.rs".into(), entry("pub fn a() {}")),
                    ("src/b.rs".into(), entry("pub fn b() {}")),
                ],
                &[],
                &[],
            )
            .unwrap();
        store
            .save_index_changes(&[], &[("src/a.rs".into(), 99, 7)], &["src/b.rs".into()])
            .unwrap();
        let loaded = store.load_index().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "src/a.rs");
        assert_eq!(loaded[0].1.mtime_ns, 99);
        assert_eq!(loaded[0].1.size, 7);
        assert_eq!(loaded[0].1.hash, 42);
        assert_eq!(loaded[0].1.facts.symbols[0].name, "a");
        assert_eq!(
            &*loaded[0].1.facts.symbols[0].file, "src/a.rs",
            "path restored on load"
        );
        let row: String = store
            .conn
            .query_row("SELECT facts FROM file_index", [], |r| r.get(0))
            .unwrap();
        assert!(
            !row.contains("src/a.rs"),
            "per-item paths are not persisted: {row}"
        );
    }

    #[test]
    fn stale_extractor_rows_are_ignored_and_purged() {
        let store = TraceStore::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO file_index VALUES ('old.rs', 1, 1, 1, 0, '{}')",
                [],
            )
            .unwrap();
        assert!(store.load_index().unwrap().is_empty());
        assert_eq!(store.index_size().unwrap(), 0);
    }

    #[test]
    fn record_session_with_files_and_history() {
        let mut store = TraceStore::open_in_memory().unwrap();
        store
            .record_session(
                &session("s1", 1_000, "claude"),
                &[
                    touched("s1", "src/lib.rs", "added fn"),
                    touched("s1", "src/main.rs", "wired"),
                ],
            )
            .unwrap();
        store
            .record_session(
                &session("s2", 2_000, "cursor"),
                &[touched("s2", "src/lib.rs", "refactor")],
            )
            .unwrap();

        let history = store.recent_history_with_files(10, None).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].session.session_id, "s2");
        assert_eq!(history[1].touched_files.len(), 2);

        let claude_only = store.recent_sessions(10, Some("claude")).unwrap();
        assert_eq!(claude_only.len(), 1);

        let file = store.file_history("src/lib.rs", 10).unwrap();
        assert_eq!(file.len(), 2);
        assert_eq!(file[0].0.session_id, "s2");
        assert_eq!(file[0].1, "refactor");
        assert_eq!(store.session_count().unwrap(), 2);
        assert!(store.get_session("s1").unwrap().is_some());
        assert!(store.get_session("nope").unwrap().is_none());
    }

    #[test]
    fn log_and_retrieve_touched_files() {
        let store = TraceStore::open_in_memory().unwrap();
        store
            .log_touched_file("sess-001", "src/lib.rs", "added new function")
            .unwrap();
        store
            .log_touched_file("sess-001", "src/main.rs", "updated imports")
            .unwrap();
        let files = store.get_touched_files("sess-001").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file_path, "src/lib.rs");
    }

    #[test]
    fn log_and_retrieve_events() {
        let store = TraceStore::open_in_memory().unwrap();
        let event = HistoricalEvent {
            session_id: "sess-001".to_string(),
            timestamp_ms: 2000,
            agent_name: "hermes".to_string(),
            event_type: "task_complete".to_string(),
            detail: serde_json::json!({"task": "scan_repo"}),
        };
        store.log_event(&event).unwrap();
        let events = store.get_events("sess-001", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "task_complete");
        assert_eq!(events[0].detail["task"], "scan_repo");
    }

    #[test]
    fn save_and_search_decisions_escapes_wildcards() {
        let store = TraceStore::open_in_memory().unwrap();
        let dec = Decision {
            id: "0001".to_string(),
            title: "Use SQLite".to_string(),
            status: DecisionStatus::Accepted,
            context: "Need embedded storage".to_string(),
            decision: "Adopt rusqlite".to_string(),
            consequences: "No external server needed".to_string(),
            created_at_ms: 1000,
            author: "hermes".to_string(),
            supersedes: None,
            tags: vec![],
            links: vec![],
        };
        store.save_decision_cached(&dec).unwrap();
        assert_eq!(store.search_cached_decisions("sqlite").unwrap().len(), 1);
        assert_eq!(store.search_cached_decisions("embedded").unwrap().len(), 1);
        assert_eq!(
            store
                .search_cached_decisions("external server")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.search_cached_decisions("nonexistent").unwrap().len(),
            0
        );
        assert_eq!(
            store.search_cached_decisions("%").unwrap().len(),
            0,
            "% is literal"
        );
    }

    #[test]
    fn prune_removes_old_sessions_and_orphaned_files() {
        let mut store = TraceStore::open_in_memory().unwrap();
        store
            .record_session(&session("old", 1000, "a"), &[touched("old", "x.rs", "r")])
            .unwrap();
        store
            .record_session(
                &session("fresh", now_ms(), "a"),
                &[touched("fresh", "y.rs", "r")],
            )
            .unwrap();
        assert_eq!(store.prune_history(3_600_000).unwrap(), 1);
        let history = store.get_recent_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].session_id, "fresh");
        assert!(store.get_touched_files("old").unwrap().is_empty());
        assert_eq!(store.get_touched_files("fresh").unwrap().len(), 1);
    }
}
