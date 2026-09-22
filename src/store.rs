//! Phase 4: Execution Memory & Session Drift Recovery.
//!
//! SQLite-backed persistence for:
//! - Events (structural facts, file fingerprints) — the AST index cache
//! - Sessions (session_id, timestamp, agent_name, summary)
//! - Touched files (session_id, file_path, change_reason)
//! - Decisions (Phase 3 ADR metadata cache)
//!
//! On startup, `get_recent_history` lets agents reconstruct context after
//! context compression without re-reading thousands of prompt tokens.

use crate::humanize::now_ms;
use crate::model::{HistoricalEvent, SessionRecord, TouchedFileRecord};
use rusqlite::{params, Connection, Result as SqlResult};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    timestamp   INTEGER NOT NULL,
    agent_name  TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    detail      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS fingerprints (
    rel_path    TEXT PRIMARY KEY,
    mtime_ms    INTEGER NOT NULL,
    size        INTEGER NOT NULL
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

CREATE TABLE IF NOT EXISTS scan_cache (
    rel_path     TEXT NOT NULL,
    mtime_ms     INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    symbols_json TEXT NOT NULL,
    PRIMARY KEY (rel_path, mtime_ms)
);
"#;

/// A single trace store — wraps a SQLite connection.
pub struct TraceStore {
    pub conn: Connection,
}

impl TraceStore {
    /// Open (or create) the SQLite database at `path`.
    pub fn open(path: &std::path::Path) -> SqlResult<Self> {
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new("."))).ok();
        let conn = Connection::open(path)?;
        // WAL mode for concurrent read/write access from multiple agent shims
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        conn.pragma_update(None, "synchronous", &"NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(TraceStore { conn })
    }

    /// Open an in-memory SQLite database (fallback when file-based DB is unavailable).
    pub fn open_in_memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(TraceStore { conn })
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

    /// Recent sessions, most recent first.
    pub fn get_recent_history(&self, limit: usize) -> SqlResult<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp, agent_name, summary
             FROM sessions
             ORDER BY timestamp DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                timestamp_ms: row.get(1)?,
                agent_name: row.get(2)?,
                summary: row.get(3)?,
            })
        })?;
        rows.collect()
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
        let mut stmt = self.conn.prepare(
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

    /// All events for a session.
    pub fn get_events(&self, session_id: &str, limit: usize) -> SqlResult<Vec<HistoricalEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp, agent_name, event_type, detail
             FROM events
             WHERE session_id = ?1
             ORDER BY timestamp DESC
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

    // ── Phase 1: AST index cache (fingerprints + symbols) ─────────────

    /// Store a file fingerprint; returns true if it differs from what's stored.
    pub fn fingerprint_changed(&self, rel: &str, mtime_ms: i64, size: u64) -> bool {
        let changed: bool = match self.conn.query_row(
            "SELECT mtime_ms, size FROM fingerprints WHERE rel_path = ?1",
            params![rel],
            |row| {
                let mtime: i64 = row.get(0)?;
                let sz: i64 = row.get(1)?;
                Ok((mtime, sz) == (mtime_ms, size as i64))
            },
        ) {
            Ok(same) => !same,
            Err(rusqlite::Error::QueryReturnedNoRows) => true,
            Err(_) => true,
        };
        if changed {
            self.conn.execute(
                "INSERT OR REPLACE INTO fingerprints (rel_path, mtime_ms, size) VALUES (?1, ?2, ?3)",
                params![rel, mtime_ms, size as i64],
            ).ok();
        }
        changed
    }

    /// Cache extracted symbols for a file (used by incremental scans).
    pub fn cache_symbols(
        &self,
        rel: &str,
        mtime_ms: i64,
        size: u64,
        symbols_json: &str,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO scan_cache (rel_path, mtime_ms, size, symbols_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![rel, mtime_ms, size as i64, symbols_json],
        )?;
        Ok(())
    }

    /// Retrieve cached symbols for a file if its fingerprint matches.
    pub fn get_cached_symbols(&self, rel: &str, mtime_ms: i64, size: u64) -> Option<String> {
        self.conn.query_row(
            "SELECT symbols_json FROM scan_cache WHERE rel_path = ?1 AND mtime_ms = ?2 AND size = ?3",
            params![rel, mtime_ms, size as i64],
            |row| row.get::<_, String>(0),
        ).ok()
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

    /// Search cached decisions by keyword.
    pub fn search_cached_decisions(&self, query: &str) -> SqlResult<Vec<crate::model::Decision>> {
        let pattern = format!("%{}%", query.to_lowercase());
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, context, decision, consequences, created_at, author, supersedes
             FROM decisions
             WHERE lower(title) LIKE ?1 OR lower(context) LIKE ?1 OR lower(decision) LIKE ?1"
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

    /// Delete old sessions and events older than `max_age_ms`.
    pub fn prune_history(&self, max_age_ms: i64) -> SqlResult<usize> {
        let cutoff = now_ms() - max_age_ms;
        let count = self
            .conn
            .execute("DELETE FROM sessions WHERE timestamp < ?1", params![cutoff])?;
        self.conn
            .execute("DELETE FROM events WHERE timestamp < ?1", params![cutoff])?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Decision, DecisionStatus, HistoricalEvent, SessionRecord, TouchedFileRecord,
    };

    #[test]
    fn open_creates_tables() {
        let store = TraceStore::open_in_memory().unwrap();
        // Verify tables exist
        let mut stmt = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"touched_files".to_string()));
        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"decisions".to_string()));
        assert!(tables.contains(&"scan_cache".to_string()));
    }

    #[test]
    fn save_and_retrieve_session() {
        let store = TraceStore::open_in_memory().unwrap();
        let session = SessionRecord {
            session_id: "sess-001".to_string(),
            timestamp_ms: 1000,
            agent_name: "hermes".to_string(),
            summary: "fixed invariant parser".to_string(),
        };
        store.save_session(&session).unwrap();
        let history = store.get_recent_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].session_id, "sess-001");
        assert_eq!(history[0].agent_name, "hermes");
        assert_eq!(history[0].summary, "fixed invariant parser");
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
        assert!(files.iter().any(|f| f.file_path == "src/lib.rs"));
        assert!(files.iter().any(|f| f.file_path == "src/main.rs"));
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
    }

    #[test]
    fn fingerprint_changed_detects_modification() {
        let store = TraceStore::open_in_memory().unwrap();
        assert!(store.fingerprint_changed("src/main.rs", 1000, 100));
        assert!(!store.fingerprint_changed("src/main.rs", 1000, 100));
        assert!(store.fingerprint_changed("src/main.rs", 1001, 100));
    }

    #[test]
    fn save_and_search_decisions() {
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
            tags: vec!["storage".to_string()],
            links: vec!["src/store.rs".to_string()],
        };
        store.save_decision_cached(&dec).unwrap();
        let results = store.search_cached_decisions("sqlite").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Use SQLite");
        let results2 = store.search_cached_decisions("embedded").unwrap();
        assert_eq!(results2.len(), 1);
        let results3 = store.search_cached_decisions("nonexistent").unwrap();
        assert_eq!(results3.len(), 0);
    }

    #[test]
    fn get_recent_history_returns_empty_for_empty_db() {
        let store = TraceStore::open_in_memory().unwrap();
        let history = store.get_recent_history(10).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn prune_old_sessions() {
        let store = TraceStore::open_in_memory().unwrap();
        let old = SessionRecord {
            session_id: "sess-old".to_string(),
            timestamp_ms: 1000,
            agent_name: "hermes".to_string(),
            summary: "old session".to_string(),
        };
        let fresh = SessionRecord {
            session_id: "sess-fresh".to_string(),
            timestamp_ms: now_ms(),
            agent_name: "hermes".to_string(),
            summary: "fresh session".to_string(),
        };
        store.save_session(&old).unwrap();
        store.save_session(&fresh).unwrap();
        // Prune entries older than 1 hour
        store.prune_history(3600_000).unwrap();
        let history = store.get_recent_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].session_id, "sess-fresh");
    }
}
