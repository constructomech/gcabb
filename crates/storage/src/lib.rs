#![allow(clippy::missing_errors_doc)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use app_model::{
    DomainEvent, OutputMetadata, OutputStreamKind, OutputStreamUpdate, ProjectMetadata,
    SessionKind, SessionMetadata, SessionSnapshot, TitleSource, ToolActivity, rebuild,
};
use diagnostics::DiagnosticEvent;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 8;
/// Initial restored output window. Older chunks stay in `SQLite` and can be
/// prepended through `read_output` without inflating every restored snapshot.
pub const RESTORED_OUTPUT_CHUNKS: u64 = 64;

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
    #[error(
        "output stream {kind}/{identity} is incomplete: expected {expected} chunks, read {actual}"
    )]
    OutputIncomplete {
        kind: &'static str,
        identity: String,
        expected: u64,
        actual: u64,
    },
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub struct Storage {
    connection: Mutex<Connection>,
}

pub struct RecoveredSession {
    pub state: SessionSnapshot,
    pub replayed_events: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputRange {
    pub start_chunk: u64,
    pub max_chunks: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputRead {
    pub content: String,
    pub metadata: OutputMetadata,
    pub next_chunk: u64,
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
                id, sdk_session_id, project_path, repository_root, title, title_source,
                kind, model, mode, base_ref, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                sdk_session_id = excluded.sdk_session_id,
                project_path = excluded.project_path,
                repository_root = excluded.repository_root,
                title = excluded.title,
                title_source = excluded.title_source,
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
                metadata.title_source.as_str(),
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
            "SELECT id, sdk_session_id, project_path, repository_root, title, title_source, kind, model, mode, base_ref, created_at, updated_at
             FROM app_sessions ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([], metadata_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn append_event(&self, event: &DomainEvent) -> Result<bool> {
        self.append_event_with_output(event, &[])
    }

    pub fn append_event_with_output(
        &self,
        event: &DomainEvent,
        updates: &[OutputStreamUpdate],
    ) -> Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let affected = transaction.execute(
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
        if affected == 0 {
            transaction.commit()?;
            return Ok(false);
        }
        for update in updates {
            Self::apply_output_update(&transaction, &event.session_id, event.sequence, update)?;
        }
        transaction.commit()?;
        Ok(true)
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
                "SELECT id, sdk_session_id, project_path, repository_root, title, title_source, kind, model, mode, base_ref, created_at, updated_at
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
        let mut state = rebuild(snapshot, events);
        drop(statement);
        drop(connection);
        self.hydrate_output(&mut state);
        Ok(RecoveredSession {
            state,
            replayed_events,
        })
    }

    pub fn read_output(
        &self,
        session_id: &str,
        kind: OutputStreamKind,
        identity: &str,
        range: OutputRange,
    ) -> Result<OutputRead> {
        let connection = self.connection()?;
        let metadata = connection
            .query_row(
                "SELECT chunk_count, byte_count, complete
                 FROM output_streams
                 WHERE session_id = ?1 AND stream_kind = ?2 AND stream_id = ?3",
                params![session_id, kind.as_str(), identity],
                |row| {
                    Ok(OutputMetadata {
                        chunk_count: row.get(0)?,
                        byte_count: row.get(1)?,
                        complete: row.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        let mut statement = connection.prepare(
            "SELECT chunk_index, content FROM output_chunks
             WHERE session_id = ?1 AND stream_kind = ?2 AND stream_id = ?3
               AND chunk_index >= ?4
             ORDER BY chunk_index
             LIMIT ?5",
        )?;
        let chunks = statement.query_map(
            params![
                session_id,
                kind.as_str(),
                identity,
                range.start_chunk,
                range.max_chunks
            ],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut content = String::new();
        let mut read = 0u64;
        for chunk in chunks {
            let (chunk_index, chunk) = chunk?;
            if chunk_index != range.start_chunk + read {
                return Err(StorageError::OutputIncomplete {
                    kind: kind.as_str(),
                    identity: identity.to_owned(),
                    expected: metadata
                        .chunk_count
                        .saturating_sub(range.start_chunk)
                        .min(range.max_chunks),
                    actual: read,
                });
            }
            content.push_str(&chunk);
            read += 1;
        }
        let expected = metadata
            .chunk_count
            .saturating_sub(range.start_chunk)
            .min(range.max_chunks);
        if read != expected {
            return Err(StorageError::OutputIncomplete {
                kind: kind.as_str(),
                identity: identity.to_owned(),
                expected,
                actual: read,
            });
        }
        Ok(OutputRead {
            content,
            metadata,
            next_chunk: range.start_chunk + read,
        })
    }

    fn hydrate_output(&self, snapshot: &mut SessionSnapshot) {
        let session_id = snapshot.metadata.id.clone();
        let invocations: Vec<String> = snapshot
            .tool_activity
            .invocations
            .iter()
            .map(|invocation| invocation.call_id.clone())
            .collect();
        let terminals: Vec<String> = snapshot
            .tool_activity
            .terminals
            .iter()
            .map(|terminal| terminal.shell_id.clone())
            .collect();
        for (kind, identities) in [
            (OutputStreamKind::Invocation, invocations),
            (OutputStreamKind::Terminal, terminals),
        ] {
            for identity in identities {
                let output = self
                    .read_output(
                        &session_id,
                        kind,
                        &identity,
                        OutputRange {
                            start_chunk: 0,
                            max_chunks: 0,
                        },
                    )
                    .and_then(|metadata| {
                        let start_chunk = metadata
                            .metadata
                            .chunk_count
                            .saturating_sub(RESTORED_OUTPUT_CHUNKS);
                        self.read_output(
                            &session_id,
                            kind,
                            &identity,
                            OutputRange {
                                start_chunk,
                                max_chunks: RESTORED_OUTPUT_CHUNKS,
                            },
                        )
                        .map(|read| (read.content, read.metadata, start_chunk))
                    })
                    .map_err(|error| error.to_string());
                snapshot.tool_activity.set_output(kind, &identity, output);
            }
        }
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

    #[allow(
        clippy::too_many_lines,
        reason = "schema creation and backfill must remain in one migration transaction"
    )]
    fn migrate(&self) -> Result<()> {
        let mut connection = self.connection()?;
        let previous_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
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
                title_source TEXT NOT NULL DEFAULT 'manual',
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
             CREATE TABLE IF NOT EXISTS output_streams (
                session_id TEXT NOT NULL REFERENCES app_sessions(id) ON DELETE CASCADE,
                stream_kind TEXT NOT NULL,
                stream_id TEXT NOT NULL,
                chunk_count INTEGER NOT NULL DEFAULT 0,
                byte_count INTEGER NOT NULL DEFAULT 0,
                complete INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(session_id, stream_kind, stream_id)
             );
             CREATE TABLE IF NOT EXISTS output_chunks (
                session_id TEXT NOT NULL,
                stream_kind TEXT NOT NULL,
                stream_id TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                event_sequence INTEGER NOT NULL,
                byte_count INTEGER NOT NULL,
                content TEXT NOT NULL,
                PRIMARY KEY(session_id, stream_kind, stream_id, chunk_index),
                FOREIGN KEY(session_id, stream_kind, stream_id)
                    REFERENCES output_streams(session_id, stream_kind, stream_id)
                    ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS output_chunks_event
                ON output_chunks(session_id, event_sequence);
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
        add_column_if_missing(
            &transaction,
            "app_sessions",
            "title_source",
            "TEXT NOT NULL DEFAULT 'manual'",
        )?;
        if previous_version < 8 {
            Self::backfill_output_streams(&transaction)?;
        }
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

    fn apply_output_update(
        transaction: &rusqlite::Transaction<'_>,
        session_id: &str,
        event_sequence: u64,
        update: &OutputStreamUpdate,
    ) -> Result<()> {
        transaction.execute(
            "INSERT INTO output_streams (
                session_id, stream_kind, stream_id, chunk_count, byte_count, complete
             ) VALUES (?1, ?2, ?3, 0, 0, ?4)
             ON CONFLICT(session_id, stream_kind, stream_id) DO UPDATE SET
                complete = output_streams.complete OR excluded.complete",
            params![
                session_id,
                update.kind.as_str(),
                update.identity,
                update.complete
            ],
        )?;
        if let Some(chunk) = update.chunk.as_deref() {
            let chunk_index: u64 = transaction.query_row(
                "SELECT chunk_count FROM output_streams
                 WHERE session_id = ?1 AND stream_kind = ?2 AND stream_id = ?3",
                params![session_id, update.kind.as_str(), update.identity],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO output_chunks (
                    session_id, stream_kind, stream_id, chunk_index,
                    event_sequence, byte_count, content
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    update.kind.as_str(),
                    update.identity,
                    chunk_index,
                    event_sequence,
                    chunk.len() as u64,
                    chunk
                ],
            )?;
            transaction.execute(
                "UPDATE output_streams
                 SET chunk_count = chunk_count + 1, byte_count = byte_count + ?4
                 WHERE session_id = ?1 AND stream_kind = ?2 AND stream_id = ?3",
                params![
                    session_id,
                    update.kind.as_str(),
                    update.identity,
                    chunk.len() as u64
                ],
            )?;
        }
        Ok(())
    }

    fn backfill_output_streams(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
        let session_ids = {
            let mut statement = transaction.prepare("SELECT id FROM app_sessions ORDER BY id")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for session_id in session_ids {
            let payloads = {
                let mut statement = transaction.prepare(
                    "SELECT payload FROM domain_events
                     WHERE session_id = ?1 ORDER BY sequence",
                )?;
                statement
                    .query_map([&session_id], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut activity = ToolActivity::default();
            for payload in payloads {
                let event: DomainEvent = serde_json::from_str(&payload)?;
                let updates = app_model::tools::output_updates(&activity, &event);
                for update in &updates {
                    Self::apply_output_update(transaction, &session_id, event.sequence, update)?;
                }
                app_model::tools::project(&mut activity, &event);
            }
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
        title_source: TitleSource::from_str_or_default(row.get::<_, Option<String>>(5)?.as_deref()),
        kind: SessionKind::from_str_or_default(row.get::<_, Option<String>>(6)?.as_deref()),
        model: row.get(7)?,
        mode: row.get(8)?,
        base_ref: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use app_model::DomainEvent;
    use serde_json::json;
    use tempfile::tempdir;
    use test_harness::{LargeSessionConfig, large_session_events};

    use super::*;

    fn metadata() -> SessionMetadata {
        SessionMetadata {
            id: "app-session".to_owned(),
            sdk_session_id: "sdk-session".to_owned(),
            project_path: "/tmp/project".to_owned(),
            repository_root: None,
            title: "Recovered session".to_owned(),
            title_source: TitleSource::Manual,
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

    fn append_raw_events(
        storage: &Storage,
        snapshot: &mut SessionSnapshot,
        events: impl IntoIterator<Item = serde_json::Value>,
    ) {
        for raw in events {
            let event =
                DomainEvent::from_sdk_event_for("app-session", snapshot.last_sequence + 1, &raw);
            let updates = app_model::tools::output_updates(&snapshot.tool_activity, &event);
            assert!(storage.append_event_with_output(&event, &updates).unwrap());
            assert_eq!(snapshot.apply(event), app_model::ApplyOutcome::Applied);
        }
    }

    fn assert_output_range(
        storage: &Storage,
        kind: OutputStreamKind,
        identity: &str,
        range: OutputRange,
        expected: &str,
        next_chunk: u64,
    ) {
        let output = storage
            .read_output("app-session", kind, identity, range)
            .unwrap();
        assert_eq!(output.content, expected);
        assert_eq!(output.next_chunk, next_chunk);
    }

    fn repeated_output_events() -> [serde_json::Value; 7] {
        [
            json!({
                "id": "start",
                "type": "tool.execution_start",
                "timestamp": "1",
                "data": {
                    "toolCallId": "call-1",
                    "toolName": "bash",
                    "arguments": {"command": "printf repeated", "shellId": "shell-1"}
                }
            }),
            json!({
                "id": "partial-1",
                "type": "tool.execution_partial_result",
                "timestamp": "2",
                "data": {"toolCallId": "call-1", "partialOutput": "same\n"}
            }),
            json!({
                "id": "read-start",
                "type": "tool.execution_start",
                "timestamp": "3",
                "data": {
                    "toolCallId": "call-2",
                    "toolName": "read_bash",
                    "arguments": {"shellId": "shell-1"}
                }
            }),
            json!({
                "id": "read-partial",
                "type": "tool.execution_partial_result",
                "timestamp": "4",
                "data": {"toolCallId": "call-2", "partialOutput": "middle\n"}
            }),
            json!({
                "id": "partial-delayed-redelivery",
                "type": "tool.execution_partial_result",
                "timestamp": "2",
                "data": {"toolCallId": "call-1", "partialOutput": "same\n"}
            }),
            json!({
                "id": "partial-legitimate-repeat",
                "type": "tool.execution_partial_result",
                "timestamp": "5",
                "data": {"toolCallId": "call-1", "partialOutput": "same\n"}
            }),
            json!({
                "id": "partial-tail",
                "type": "tool.execution_partial_result",
                "timestamp": "6",
                "data": {"toolCallId": "call-1", "partialOutput": "tail\n"}
            }),
        ]
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

    #[test]
    fn redelivered_output_is_not_persisted_but_repeated_chunks_remain_addressable() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_session(&metadata()).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata());
        append_raw_events(&storage, &mut snapshot, repeated_output_events());
        storage.write_snapshot(&snapshot).unwrap();

        for (kind, identity, expected, expected_chunks) in [
            (
                OutputStreamKind::Invocation,
                "call-1",
                "same\nsame\ntail\n",
                3,
            ),
            (
                OutputStreamKind::Terminal,
                "shell-1",
                "same\nmiddle\nsame\ntail\n",
                4,
            ),
        ] {
            let metadata = storage
                .read_output(
                    "app-session",
                    kind,
                    identity,
                    OutputRange {
                        start_chunk: 0,
                        max_chunks: 10,
                    },
                )
                .unwrap();
            assert_eq!(metadata.content, expected);
            assert_eq!(metadata.metadata.chunk_count, expected_chunks);
            assert_eq!(metadata.next_chunk, expected_chunks);
            let expected_tail = match kind {
                OutputStreamKind::Invocation => "same\ntail\n",
                OutputStreamKind::Terminal => "middle\nsame\ntail\n",
            };
            assert_output_range(
                &storage,
                kind,
                identity,
                OutputRange {
                    start_chunk: 1,
                    max_chunks: 10,
                },
                expected_tail,
                expected_chunks,
            );
        }

        let recovered = storage.recover_session("app-session").unwrap();
        let invocation = recovered.state.tool_activity.invocation("call-1").unwrap();
        assert_eq!(invocation.output, "same\nsame\ntail\n");
        assert_eq!(invocation.output_metadata.chunk_count, 3);
        let terminal = recovered.state.tool_activity.terminal("shell-1").unwrap();
        assert_eq!(terminal.output, "same\nmiddle\nsame\ntail\n");
        assert_eq!(terminal.output_metadata.chunk_count, 4);
    }

    #[test]
    fn complete_large_output_survives_reopen_and_supports_chunk_ranges() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gcabb.db");
        let chunk_bytes = 1_024;
        let chunk_count = 300;
        {
            let storage = Storage::open(&path).unwrap();
            storage.upsert_session(&metadata()).unwrap();
            let mut snapshot = SessionSnapshot::new(metadata());
            for raw in large_session_events(LargeSessionConfig {
                turns: 1,
                output_chunks_per_turn: chunk_count,
                output_chunk_bytes: chunk_bytes,
            }) {
                let event = DomainEvent::from_sdk_event_for(
                    "app-session",
                    snapshot.last_sequence + 1,
                    &raw,
                );
                let updates = app_model::tools::output_updates(&snapshot.tool_activity, &event);
                assert!(storage.append_event_with_output(&event, &updates).unwrap());
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
            assert!(
                payload.len() < 32 * 1_024,
                "snapshot embedded the large output: {} bytes",
                payload.len()
            );
            assert!(!payload.contains(&"x".repeat(chunk_bytes)));
        }

        let storage = Storage::open(&path).unwrap();
        let restored_chunks = usize::try_from(RESTORED_OUTPUT_CHUNKS).unwrap();
        let first = storage
            .read_output(
                "app-session",
                OutputStreamKind::Invocation,
                "call-0",
                OutputRange {
                    start_chunk: 0,
                    max_chunks: 17,
                },
            )
            .unwrap();
        assert_eq!(first.content.len(), 17 * chunk_bytes);
        assert_eq!(first.next_chunk, 17);
        assert_eq!(first.metadata.chunk_count, chunk_count as u64);
        assert_eq!(
            first.metadata.byte_count,
            (chunk_count * chunk_bytes) as u64
        );
        assert!(first.metadata.complete);

        let recovered = storage.recover_session("app-session").unwrap();
        let invocation = recovered.state.tool_activity.invocation("call-0").unwrap();
        assert_eq!(invocation.output.len(), restored_chunks * chunk_bytes);
        assert_eq!(
            invocation.output_start_chunk,
            chunk_count as u64 - RESTORED_OUTPUT_CHUNKS
        );
        assert_eq!(invocation.output_metadata, first.metadata);
        assert!(invocation.output_load_error.is_none());
        assert!(
            !invocation.output.contains('\n'),
            "fixture should exercise one long logical line"
        );
        let terminal = recovered.state.tool_activity.terminal("shell-0").unwrap();
        assert_eq!(terminal.output, invocation.output);
        assert_eq!(terminal.output_metadata, invocation.output_metadata);
        assert_eq!(terminal.state, app_model::TerminalState::Exited);

        assert_missing_output_chunk_is_reported(&storage, chunk_bytes);
    }

    fn assert_missing_output_chunk_is_reported(storage: &Storage, chunk_bytes: usize) {
        storage
            .connection()
            .unwrap()
            .execute(
                "DELETE FROM output_chunks
                 WHERE session_id = 'app-session'
                   AND stream_kind = 'invocation'
                   AND stream_id = 'call-0'
                   AND chunk_index = 42",
                [],
            )
            .unwrap();
        let recovered = storage.recover_session("app-session").unwrap();
        let invocation = recovered.state.tool_activity.invocation("call-0").unwrap();
        assert_eq!(
            invocation.output.len(),
            usize::try_from(RESTORED_OUTPUT_CHUNKS).unwrap() * chunk_bytes
        );
        let error = storage
            .read_output(
                "app-session",
                OutputStreamKind::Invocation,
                "call-0",
                OutputRange {
                    start_chunk: 0,
                    max_chunks: RESTORED_OUTPUT_CHUNKS,
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("expected 64 chunks, read 42"),
            "missing requested chunks were not surfaced: {error}"
        );
    }

    #[test]
    fn completion_only_shell_output_survives_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gcabb.db");
        {
            let storage = Storage::open(&path).unwrap();
            storage.upsert_session(&metadata()).unwrap();
            let mut snapshot = SessionSnapshot::new(metadata());
            for raw in [
                json!({
                    "id": "start",
                    "type": "tool.execution_start",
                    "data": {
                        "toolCallId": "call",
                        "toolName": "bash",
                        "arguments": {"command": "printf complete"},
                        "shellToolInfo": {"displayCommand": "printf complete"}
                    }
                }),
                json!({
                    "id": "complete",
                    "type": "tool.execution_complete",
                    "data": {
                        "toolCallId": "call",
                        "success": true,
                        "result": {
                            "detailedContent": "complete output\n",
                            "contents": [{
                                "type": "shell_exit",
                                "shellId": "shell",
                                "exitCode": 0,
                                "outputPreview": ""
                            }]
                        }
                    }
                }),
            ] {
                let event = DomainEvent::from_sdk_event_for(
                    "app-session",
                    snapshot.last_sequence + 1,
                    &raw,
                );
                let updates = app_model::tools::output_updates(&snapshot.tool_activity, &event);
                storage.append_event_with_output(&event, &updates).unwrap();
                assert_eq!(snapshot.apply(event), app_model::ApplyOutcome::Applied);
            }
            storage.write_snapshot(&snapshot).unwrap();
        }

        let storage = Storage::open(&path).unwrap();
        let recovered = storage.recover_session("app-session").unwrap();
        let invocation = recovered.state.tool_activity.invocation("call").unwrap();
        let terminal = recovered.state.tool_activity.terminal("shell").unwrap();
        assert_eq!(invocation.output, "complete output\n");
        assert_eq!(terminal.output, invocation.output);
        assert!(invocation.output_metadata.complete);
        assert!(terminal.output_metadata.complete);
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
        assert_eq!(sessions[0].title_source, TitleSource::Manual);
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
