#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use app_model::{DomainEvent, ProjectMetadata, SessionMetadata, SessionSnapshot, rebuild};
use diagnostics::DiagnosticEvent;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 2;

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
                id, sdk_session_id, project_path, title, model, mode, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                sdk_session_id = excluded.sdk_session_id,
                project_path = excluded.project_path,
                title = excluded.title,
                model = excluded.model,
                mode = excluded.mode,
                updated_at = excluded.updated_at",
            params![
                metadata.id,
                metadata.sdk_session_id,
                metadata.project_path,
                metadata.title,
                metadata.model,
                metadata.mode,
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
            "SELECT id, sdk_session_id, project_path, title, model, mode, created_at, updated_at
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

    pub fn write_snapshot(&self, snapshot: &SessionSnapshot) -> Result<()> {
        self.connection()?.execute(
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
                "SELECT id, sdk_session_id, project_path, title, model, mode, created_at, updated_at
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
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| StorageError::LockPoisoned)
    }
}

fn metadata_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMetadata> {
    Ok(SessionMetadata {
        id: row.get(0)?,
        sdk_session_id: row.get(1)?,
        project_path: row.get(2)?,
        title: row.get(3)?,
        model: row.get(4)?,
        mode: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
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
            title: "Recovered session".to_owned(),
            model: Some("test-model".to_owned()),
            mode: Some("interactive".to_owned()),
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
        assert_eq!(recovered.state.activities.len(), 2);
        assert_eq!(recovered.state.status, app_model::SessionStatus::Idle);
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
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&path).unwrap();

        assert_eq!(storage.schema_version().unwrap(), 2);
        assert!(storage.list_projects().unwrap().is_empty());
        assert!(storage.selected_session().unwrap().is_none());
    }
}
