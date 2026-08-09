#![allow(clippy::missing_errors_doc)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use app_model::{
    DomainEvent, ProjectMetadata, SessionKind, SessionMetadata, SessionSnapshot, rebuild,
};
use diagnostics::DiagnosticEvent;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 6;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("storage connection lock poisoned")]
    LockPoisoned,
    #[error("session not found: {0}")]
    SessionNotFound(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub struct Storage {
    connection: Mutex<Connection>,
}

pub struct RecoveredSession {
    pub state: SessionSnapshot,
    pub replayed_events: usize,
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        let storage = Self {
            connection: Mutex::new(connection),
        };
        storage.configure()?;
        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        let storage = Self {
            connection: Mutex::new(connection),
        };
        storage.configure()?;
        storage.migrate()?;
        Ok(storage)
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.connection()?
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn upsert_session(&self, metadata: &SessionMetadata) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO app_sessions (
                id, sdk_session_id, project_path, repository_root, title, kind,
                model, mode, base_ref, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                sdk_session_id = excluded.sdk_session_id,
                project_path = excluded.project_path,
                repository_root = excluded.repository_root,
                title = excluded.title,
                kind = excluded.kind,
                model = excluded.model,
                mode = excluded.mode,
                base_ref = excluded.base_ref,
                updated_at = excluded.updated_at",
            params![
                metadata.id,
                metadata.sdk_session_id,
                metadata.project_path,
                metadata.repository_root,
                metadata.title,
                metadata.kind.as_str(),
                metadata.model,
                metadata.mode,
                metadata.base_ref,
                metadata.created_at,
                metadata.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_project(&self, project: &ProjectMetadata) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO projects (id, path, name, default_branch, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                path = excluded.path,
                name = excluded.name,
                default_branch = excluded.default_branch,
                last_opened_at = excluded.last_opened_at",
            params![
                project.id,
                project.path,
                project.name,
                project.default_branch,
                project.last_opened_at,
            ],
        )?;
        Ok(())
    }

    /// Delete a session and everything derived from it.
    ///
    /// `domain_events` and `snapshots` reference `app_sessions` with
    /// `ON DELETE CASCADE`, and foreign keys are enabled, so the event log and
    /// snapshots go with it. The CLI runtime's own session state under
    /// `~/.copilot` is owned by the runtime and is deliberately left alone.
    /// Reclaim space freed by deletions.
    ///
    /// `SQLite` keeps deleted pages for reuse, so a deleted session's space is
    /// not returned to the filesystem without this.
    pub fn vacuum(&self) -> Result<()> {
        self.connection()?.execute_batch("VACUUM;")?;
        Ok(())
    }

    /// Event ids already recorded for a session.
    ///
    /// The event log is the record of what was seen, so history reconciliation
    /// asks it rather than carrying a copy of the log inside the snapshot.
    pub fn event_ids(&self, session_id: &str) -> Result<HashSet<String>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT event_id FROM domain_events WHERE session_id = ?1")?;
        let rows = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<HashSet<_>, _>>()
            .map_err(Into::into)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let connection = self.connection()?;
        // Diagnostics are not session-scoped by schema -- session_id is
        // nullable, so there is no foreign key to cascade from -- and were
        // being orphaned by every delete.
        connection.execute(
            "DELETE FROM diagnostics WHERE session_id = ?1",
            [session_id],
        )?;
        connection.execute("DELETE FROM app_sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    /// Remove a project row. Sessions are unaffected; they are associated by
    /// `repository_root`, not by a foreign key.
    pub fn remove_project(&self, project_id: &str) -> Result<()> {
        self.connection()?
            .execute("DELETE FROM projects WHERE id = ?1", [project_id])?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectMetadata>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, path, name, default_branch, last_opened_at
             FROM projects ORDER BY last_opened_at DESC, name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ProjectMetadata {
                id: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                default_branch: row.get(3)?,
                last_opened_at: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn set_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        match session_id {
            Some(session_id) => {
                self.connection()?.execute(
                    "INSERT INTO app_state (key, value) VALUES ('selected_session', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [session_id],
                )?;
            }
            None => {
                self.connection()?
                    .execute("DELETE FROM app_state WHERE key = 'selected_session'", [])?;
            }
        }
        Ok(())
    }

    pub fn selected_session(&self) -> Result<Option<String>> {
        self.connection()?
            .query_row(
                "SELECT value FROM app_state WHERE key = 'selected_session'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, sdk_session_id, project_path, repository_root, title, kind, model, mode, base_ref, created_at, updated_at
             FROM app_sessions ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([], metadata_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn append_event(&self, event: &DomainEvent) -> Result<bool> {
        let affected = self.connection()?.execute(
            "INSERT OR IGNORE INTO domain_events (
                session_id, sequence, event_id, event_version, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.session_id,
                event.sequence,
                event.id,
                event.version,
                serde_json::to_string(event)?,
            ],
        )?;
        Ok(affected == 1)
    }

    /// Replace a session's snapshot.
    ///
    /// Only the newest snapshot is ever read, so older ones are removed rather
    /// than accumulated. Keeping every snapshot made storage grow with the
    /// square of a session's length.
    pub fn write_snapshot(&self, snapshot: &SessionSnapshot) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM snapshots WHERE session_id = ?1 AND sequence <> ?2",
            params![snapshot.metadata.id, snapshot.last_sequence],
        )?;
        connection.execute(
            "INSERT INTO snapshots (session_id, sequence, snapshot_version, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, sequence) DO UPDATE SET
                snapshot_version = excluded.snapshot_version,
                payload = excluded.payload",
            params![
                snapshot.metadata.id,
                snapshot.last_sequence,
                snapshot.version,
                serde_json::to_string(snapshot)?,
            ],
        )?;
        Ok(())
    }

    pub fn recover_session(&self, session_id: &str) -> Result<RecoveredSession> {
        let connection = self.connection()?;
        let metadata = connection
            .query_row(
                "SELECT id, sdk_session_id, project_path, repository_root, title, kind, model, mode, base_ref, created_at, updated_at
                 FROM app_sessions WHERE id = ?1",
                [session_id],
                metadata_from_row,
            )
            .optional()?
            .ok_or_else(|| StorageError::SessionNotFound(session_id.to_owned()))?;

        let snapshot_json: Option<String> = connection
            .query_row(
                "SELECT payload FROM snapshots
                 WHERE session_id = ?1 ORDER BY sequence DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let snapshot = match snapshot_json {
            Some(payload) => serde_json::from_str(&payload)?,
            None => SessionSnapshot::new(metadata),
        };
        let mut statement = connection.prepare(
            "SELECT payload FROM domain_events
             WHERE session_id = ?1 AND sequence > ?2 ORDER BY sequence",
        )?;
        let rows = statement.query_map(params![session_id, snapshot.last_sequence], |row| {
            row.get::<_, String>(0)
        })?;
        let events = rows
            .map(|row| {
                let payload = row?;
                serde_json::from_str(&payload).map_err(StorageError::from)
            })
            .collect::<Result<Vec<DomainEvent>>>()?;
        let replayed_events = events.len();
        Ok(RecoveredSession {
            state: rebuild(snapshot, events),
            replayed_events,
        })
    }

    pub fn record_diagnostic(&self, event: &DiagnosticEvent) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO diagnostics (
                timestamp, category, operation, elapsed_ms, session_id, success, details
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.timestamp,
                event.category,
                event.operation,
                event.elapsed_ms,
                event.session_id,
                event.success,
                serde_json::to_string(&event.details)?,
            ],
        )?;
        Ok(())
    }

    fn configure(&self) -> Result<()> {
        self.connection()?.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                default_branch TEXT,
                last_opened_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS app_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS app_sessions (
                id TEXT PRIMARY KEY,
                sdk_session_id TEXT NOT NULL UNIQUE,
                project_path TEXT NOT NULL,
                title TEXT NOT NULL,
                model TEXT,
                mode TEXT,
                base_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS domain_events (
                session_id TEXT NOT NULL REFERENCES app_sessions(id) ON DELETE CASCADE,
                sequence INTEGER NOT NULL,
                event_id TEXT NOT NULL,
                event_version INTEGER NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY(session_id, sequence),
                UNIQUE(session_id, event_id)
             );
             CREATE TABLE IF NOT EXISTS snapshots (
                session_id TEXT NOT NULL REFERENCES app_sessions(id) ON DELETE CASCADE,
                sequence INTEGER NOT NULL,
                snapshot_version INTEGER NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY(session_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS diagnostics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                category TEXT NOT NULL,
                operation TEXT NOT NULL,
                elapsed_ms INTEGER,
                session_id TEXT,
                success INTEGER NOT NULL,
                details TEXT NOT NULL
             );",
        )?;
        // Databases created before schema version 3 predate the changes view
        // and lack `base_ref`. `CREATE TABLE IF NOT EXISTS` does not add
        // columns to an existing table, so columns introduced after a table
        // shipped are added here. Each add is idempotent, which keeps repeated
        // opens and forward migration from older databases working.
        add_column_if_missing(&transaction, "app_sessions", "base_ref", "TEXT")?;
        add_column_if_missing(&transaction, "app_sessions", "repository_root", "TEXT")?;
        add_column_if_missing(&transaction, "app_sessions", "kind", "TEXT")?;
        // Earlier builds kept every snapshot a session ever wrote, and each
        // one embedded the whole event log. Only the newest is ever read, so
        // the rest are discarded on open. One database shrank from 499 MB to
        // 13 MB here.
        let pruned = transaction.execute(
            "DELETE FROM snapshots
             WHERE (session_id, sequence) NOT IN (
                SELECT session_id, max(sequence) FROM snapshots GROUP BY session_id
             )",
            [],
        )?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        // Outside the transaction: VACUUM cannot run inside one, and without
        // it the pages freed above stay allocated to the file.
        if pruned > 0 {
            connection.execute_batch("VACUUM;")?;
        }
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| StorageError::LockPoisoned)
    }
}

/// Whether a rusqlite error reports a duplicate column name.
fn is_duplicate_column(error: &rusqlite::Error) -> bool {
    error.to_string().contains("duplicate column name")
}

/// Add a column to an existing table unless it is already present.
///
/// Used to migrate databases created by earlier schema versions in place, and
/// to keep repeated opens of an already-migrated database working.
fn add_column_if_missing(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<()> {
    let statement = format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}");
    if let Err(error) = transaction.execute(&statement, [])
        && !is_duplicate_column(&error)
    {
        return Err(error.into());
    }
    Ok(())
}

fn metadata_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMetadata> {
    Ok(SessionMetadata {
        id: row.get(0)?,
        sdk_session_id: row.get(1)?,
        project_path: row.get(2)?,
        repository_root: row.get(3)?,
        title: row.get(4)?,
        kind: SessionKind::from_str_or_default(row.get::<_, Option<String>>(5)?.as_deref()),
        model: row.get(6)?,
        mode: row.get(7)?,
        base_ref: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use app_model::DomainEvent;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn metadata() -> SessionMetadata {
        SessionMetadata {
            id: "app-session".to_owned(),
            sdk_session_id: "sdk-session".to_owned(),
            project_path: "/tmp/project".to_owned(),
            repository_root: None,
            title: "Recovered session".to_owned(),
            kind: SessionKind::Project,
            model: Some("test-model".to_owned()),
            mode: Some("interactive".to_owned()),
            base_ref: Some("main".to_owned()),
            created_at: "1".to_owned(),
            updated_at: "2".to_owned(),
        }
    }

    fn event(sequence: u64, event_type: &str) -> DomainEvent {
        DomainEvent::from_sdk_event_for(
            "app-session",
            sequence,
            &json!({
                "id": format!("event-{sequence}"),
                "type": event_type,
                "data": {}
            }),
        )
    }

    #[test]
    fn migration_enables_wal_and_tracks_schema_version() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(directory.path().join("gcabb.db")).unwrap();

        assert_eq!(storage.schema_version().unwrap(), SCHEMA_VERSION);
        let journal_mode: String = storage
            .connection()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
    }

    #[test]
    fn recovery_replays_only_events_after_latest_snapshot() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_session(&metadata()).unwrap();
        let first = event(1, "assistant.turn_start");
        let second = event(2, "session.idle");
        storage.append_event(&first).unwrap();

        let mut snapshot = SessionSnapshot::new(metadata());
        assert_eq!(snapshot.apply(first), app_model::ApplyOutcome::Applied);
        storage.write_snapshot(&snapshot).unwrap();
        storage.append_event(&second).unwrap();
        assert!(!storage.append_event(&second).unwrap());

        let recovered = storage.recover_session("app-session").unwrap();
        assert_eq!(recovered.replayed_events, 1);
        assert_eq!(recovered.state.last_sequence, 2);
        // The event log, not the snapshot, is where the events live.
        assert_eq!(storage.event_ids("app-session").unwrap().len(), 2);
        assert_eq!(recovered.state.status, app_model::SessionStatus::Idle);
    }

    /// A session keeps one snapshot, not one per event. Retaining every
    /// snapshot made storage grow with the square of a session's length:
    /// one real database reached 499 MB of snapshots over 5.5 MB of events.
    #[test]
    fn a_session_keeps_only_its_latest_snapshot() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_session(&metadata()).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata());
        for sequence in 1..=5 {
            let event = event(sequence, "assistant.turn_start");
            storage.append_event(&event).unwrap();
            assert_eq!(snapshot.apply(event), app_model::ApplyOutcome::Applied);
            storage.write_snapshot(&snapshot).unwrap();
        }

        let rows: u64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM snapshots WHERE session_id = 'app-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 1,
            "every snapshot was kept instead of just the latest"
        );

        // The one kept must be the newest, or recovery would replay from a
        // stale point and rebuild state that was already computed.
        let recovered = storage.recover_session("app-session").unwrap();
        assert_eq!(recovered.state.last_sequence, 5);
        assert_eq!(recovered.replayed_events, 0);
    }

    /// The event log is the record of what happened; the snapshot is a
    /// projection of it. Storing the events inside the snapshot duplicated
    /// the log and accounted for 99% of every snapshot written.
    #[test]
    fn a_snapshot_does_not_carry_the_event_log() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_session(&metadata()).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata());
        for sequence in 1..=3 {
            let event = event(sequence, "assistant.turn_start");
            storage.append_event(&event).unwrap();
            assert_eq!(snapshot.apply(event), app_model::ApplyOutcome::Applied);
        }
        storage.write_snapshot(&snapshot).unwrap();

        let payload: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT payload FROM snapshots WHERE session_id = 'app-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let stored: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(
            stored.get("activities").is_none(),
            "the snapshot still carries a copy of the event log"
        );

        // Recovery must still work: state comes from the snapshot, and the
        // events after it are replayed from the log.
        let recovered = storage.recover_session("app-session").unwrap();
        assert_eq!(recovered.state.last_sequence, 3);
        assert_eq!(recovered.replayed_events, 0);
    }

    /// Deleting a session must not leave its rows behind. Events and
    /// snapshots cascade, but diagnostics have no foreign key to cascade
    /// from, so they were orphaned by every delete.
    #[test]
    fn deleting_a_session_removes_everything_belonging_to_it() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_session(&metadata()).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata());
        let event = event(1, "assistant.turn_start");
        storage.append_event(&event).unwrap();
        assert_eq!(snapshot.apply(event), app_model::ApplyOutcome::Applied);
        storage.write_snapshot(&snapshot).unwrap();
        storage
            .record_diagnostic(&DiagnosticEvent {
                timestamp: "1".to_owned(),
                category: "session".to_owned(),
                operation: "test".to_owned(),
                elapsed_ms: None,
                session_id: Some("app-session".to_owned()),
                success: true,
                details: serde_json::Value::Null,
            })
            .unwrap();

        storage.delete_session("app-session").unwrap();

        let count = |table: &str| -> u64 {
            storage
                .connection()
                .unwrap()
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE session_id = 'app-session'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("domain_events"), 0, "events outlived the session");
        assert_eq!(count("snapshots"), 0, "snapshots outlived the session");
        assert_eq!(
            count("diagnostics"),
            0,
            "diagnostics outlived the session they describe"
        );
    }

    /// A database written by an earlier build carries every snapshot a
    /// session ever wrote. Opening it discards all but the newest, which is
    /// the only one that is ever read.
    #[test]
    fn opening_an_old_database_prunes_superseded_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gcabb.db");
        {
            let storage = Storage::open(&path).unwrap();
            storage.upsert_session(&metadata()).unwrap();
            let connection = storage.connection().unwrap();
            for sequence in 1..=4 {
                connection
                    .execute(
                        "INSERT INTO snapshots (session_id, sequence, snapshot_version, payload)
                         VALUES ('app-session', ?1, 1, '{}')",
                        [sequence],
                    )
                    .unwrap();
            }
        }

        let storage = Storage::open(&path).unwrap();
        let remaining: Vec<u64> = {
            let connection = storage.connection().unwrap();
            let mut statement = connection
                .prepare("SELECT sequence FROM snapshots WHERE session_id = 'app-session'")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, u64>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            remaining,
            vec![4],
            "superseded snapshots survived the upgrade"
        );
    }

    #[test]
    fn project_and_selected_session_state_round_trip() {
        let storage = Storage::open_in_memory().unwrap();
        let project = ProjectMetadata {
            id: "project-1".to_owned(),
            path: "/tmp/project".to_owned(),
            name: "Project".to_owned(),
            default_branch: Some("main".to_owned()),
            last_opened_at: "1".to_owned(),
        };

        storage.upsert_project(&project).unwrap();
        storage.set_selected_session(Some("session-1")).unwrap();

        assert_eq!(storage.list_projects().unwrap(), vec![project]);
        assert_eq!(
            storage.selected_session().unwrap().as_deref(),
            Some("session-1")
        );
        storage.set_selected_session(None).unwrap();
        assert!(storage.selected_session().unwrap().is_none());
    }

    #[test]
    fn version_one_database_migrates_forward() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gcabb.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE app_sessions (
                    id TEXT PRIMARY KEY,
                    sdk_session_id TEXT NOT NULL UNIQUE,
                    project_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    model TEXT,
                    mode TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 INSERT INTO app_sessions VALUES (
                    'legacy', 'legacy-sdk', '/tmp/legacy', 'Legacy', NULL, NULL, '1', '2'
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&path).unwrap();

        assert_eq!(storage.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(storage.list_projects().unwrap().is_empty());
        assert!(storage.selected_session().unwrap().is_none());

        // The pre-existing row must survive the added column and read back
        // with an absent base ref rather than failing the query.
        let sessions = storage.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy");
        assert!(sessions[0].base_ref.is_none());
    }

    #[test]
    fn repeated_open_does_not_duplicate_base_ref_column() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gcabb.db");
        {
            let storage = Storage::open(&path).unwrap();
            storage.upsert_session(&metadata()).unwrap();
        }
        // Opening again re-runs the migration; the duplicate-column error must
        // be tolerated rather than failing startup.
        let storage = Storage::open(&path).unwrap();
        let sessions = storage.list_sessions().unwrap();
        assert_eq!(sessions[0].base_ref.as_deref(), Some("main"));
    }
}
