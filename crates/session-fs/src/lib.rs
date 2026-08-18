#![allow(clippy::missing_errors_doc)]

//! Host-owned session filesystem.
//!
//! When GCABB registers this provider the runtime stops touching the session
//! directory itself and asks GCABB to do it instead. That matters for one
//! reason above the rest: the agent's own task list lives in the per-session
//! `SQLite` database, and the agent reaches it through the `sql` tool. With the
//! database hosted here, those writes arrive as calls GCABB serves, so the
//! agent's list becomes shared state rather than the runtime's private state.
//!
//! Files are kept on the real filesystem rather than in memory. The runtime
//! and its subprocesses expect a session directory that behaves like a
//! directory, and nothing is gained by simulating one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use github_copilot_sdk::session_fs::{
    DirEntry, DirEntryKind, FileInfo, FsError, FsErrorKind, SessionFsProvider,
    SessionFsSqliteProvider, SessionFsSqliteQueryResult, SessionFsSqliteQueryType,
    SessionFsSqliteTransactionError, SessionFsSqliteTransactionStatement,
};
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::Value;
use tokio::sync::Mutex;

mod sqlite;

pub use sqlite::SqliteStore;

/// A session filesystem rooted at a directory GCABB owns.
///
/// The runtime is configured with one session-state path for the whole
/// client, but each session registers its own provider. Serving that shared
/// logical path from a per-session directory is what keeps two sessions from
/// writing over each other. Paths outside it — the project files the agent
/// reads and edits — are passed through untouched.
pub struct HostSessionFs {
    logical_root: PathBuf,
    real_root: PathBuf,
    database: Arc<SqliteStore>,
}

impl HostSessionFs {
    /// Serve `logical_root` from `real_root`.
    ///
    /// The session's `SQLite` database lives inside `real_root`, and is opened
    /// lazily so a session that never runs SQL does not create a file for it.
    #[must_use]
    pub fn new(logical_root: impl Into<PathBuf>, real_root: impl Into<PathBuf>) -> Self {
        let real_root = real_root.into();
        Self {
            logical_root: logical_root.into(),
            database: Arc::new(SqliteStore::new(real_root.join("session.db"))),
            real_root,
        }
    }

    /// The `SQLite` store behind this session, for reading and writing the
    /// agent's task list from the app.
    #[must_use]
    pub fn database(&self) -> Arc<SqliteStore> {
        self.database.clone()
    }

    /// The directory this session's state is actually written to.
    #[must_use]
    pub fn real_root(&self) -> &Path {
        &self.real_root
    }

    /// Map a path the runtime asked for onto the path GCABB serves.
    ///
    /// Anything outside the session-state root is a project path and must be
    /// left alone: the agent reads and edits the repository through this same
    /// provider, and rewriting those would send its edits somewhere else.
    fn resolve(&self, path: &str) -> PathBuf {
        let requested = Path::new(path);
        requested.strip_prefix(&self.logical_root).map_or_else(
            |_| requested.to_path_buf(),
            |relative| self.real_root.join(relative),
        )
    }
}

/// Timestamps the runtime expects as ISO 8601.
fn iso8601(time: std::io::Result<std::time::SystemTime>) -> String {
    time.map_or_else(
        |_| String::new(),
        |time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339(),
    )
}

async fn ensure_parent(path: &Path) -> Result<(), FsError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

#[async_trait]
impl SessionFsProvider for HostSessionFs {
    async fn read_file(&self, path: &str) -> Result<String, FsError> {
        Ok(tokio::fs::read_to_string(self.resolve(path)).await?)
    }

    async fn write_file(
        &self,
        path: &str,
        content: &str,
        _mode: Option<i64>,
    ) -> Result<(), FsError> {
        let path = self.resolve(path);
        ensure_parent(&path).await?;
        Ok(tokio::fs::write(path, content).await?)
    }

    async fn append_file(
        &self,
        path: &str,
        content: &str,
        _mode: Option<i64>,
    ) -> Result<(), FsError> {
        use tokio::io::AsyncWriteExt as _;
        let path = self.resolve(path);
        ensure_parent(&path).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(content.as_bytes()).await?;
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool, FsError> {
        // A missing path is a false answer, not a failure.
        Ok(tokio::fs::metadata(self.resolve(path)).await.is_ok())
    }

    async fn stat(&self, path: &str) -> Result<FileInfo, FsError> {
        let metadata = tokio::fs::metadata(self.resolve(path)).await?;
        Ok(FileInfo::new(
            metadata.is_file(),
            metadata.is_dir(),
            i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            iso8601(metadata.modified()),
            iso8601(metadata.created()),
        ))
    }

    async fn mkdir(&self, path: &str, recursive: bool, _mode: Option<i64>) -> Result<(), FsError> {
        let path = self.resolve(path);
        if recursive {
            tokio::fs::create_dir_all(path).await?;
        } else {
            tokio::fs::create_dir(path).await?;
        }
        Ok(())
    }

    async fn readdir(&self, path: &str) -> Result<Vec<String>, FsError> {
        Ok(self
            .readdir_with_types(path)
            .await?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    async fn readdir_with_types(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let mut entries = tokio::fs::read_dir(self.resolve(path)).await?;
        let mut listed = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let kind = if entry.file_type().await?.is_dir() {
                DirEntryKind::Directory
            } else {
                DirEntryKind::File
            };
            listed.push(DirEntry::new(entry.file_name().to_string_lossy(), kind));
        }
        // The runtime does not promise an order, but a stable one keeps
        // directory listings reproducible between calls.
        listed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(listed)
    }

    async fn rm(&self, path: &str, recursive: bool, force: bool) -> Result<(), FsError> {
        let target = self.resolve(path);
        let metadata = match tokio::fs::symlink_metadata(&target).await {
            Ok(metadata) => metadata,
            // `force` exists so removing something already gone is a success.
            Err(error) if force && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let result = if metadata.is_dir() && recursive {
            tokio::fs::remove_dir_all(&target).await
        } else if metadata.is_dir() {
            tokio::fs::remove_dir(&target).await
        } else {
            tokio::fs::remove_file(&target).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) if force && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn rename(&self, src: &str, dest: &str) -> Result<(), FsError> {
        let destination = self.resolve(dest);
        ensure_parent(&destination).await?;
        Ok(tokio::fs::rename(self.resolve(src), destination).await?)
    }

    fn sqlite(&self) -> Option<&dyn SessionFsSqliteProvider> {
        Some(self)
    }
}

#[async_trait]
impl SessionFsSqliteProvider for HostSessionFs {
    async fn sqlite_query(
        &self,
        query_type: SessionFsSqliteQueryType,
        query: &str,
        params: Option<&HashMap<String, Value>>,
    ) -> Result<Option<SessionFsSqliteQueryResult>, FsError> {
        self.database
            .query(query_type, query, params)
            .await
            .map(Some)
    }

    async fn sqlite_transaction(
        &self,
        statements: &[SessionFsSqliteTransactionStatement],
    ) -> Result<Vec<SessionFsSqliteQueryResult>, SessionFsSqliteTransactionError> {
        self.database.transaction(statements).await
    }

    async fn sqlite_exists(&self) -> Result<bool, FsError> {
        Ok(self.database.exists().await)
    }
}

/// Translate one `SQLite` value into the JSON the wire carries.
pub(crate) fn json_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(integer) => Value::from(integer),
        ValueRef::Real(real) => {
            serde_json::Number::from_f64(real).map_or(Value::Null, Value::Number)
        }
        ValueRef::Text(text) => Value::from(String::from_utf8_lossy(text).into_owned()),
        // Blobs have no JSON representation; their length is more useful than
        // a lossy string would be.
        ValueRef::Blob(blob) => Value::from(format!("<blob {} bytes>", blob.len())),
    }
}

/// Bind JSON parameters to a statement by name.
pub(crate) fn bind_named(
    statement: &mut rusqlite::Statement<'_>,
    params: Option<&HashMap<String, Value>>,
) -> rusqlite::Result<()> {
    let Some(params) = params else { return Ok(()) };
    for (name, value) in params {
        // The runtime sends bare names; SQLite expects the sigil.
        let key = if name.starts_with([':', '@', '$']) {
            name.clone()
        } else {
            format!(":{name}")
        };
        let Some(index) = statement.parameter_index(&key)? else {
            continue;
        };
        match value {
            Value::Null => statement.raw_bind_parameter(index, rusqlite::types::Null)?,
            Value::Bool(flag) => statement.raw_bind_parameter(index, flag)?,
            Value::Number(number) => {
                if let Some(integer) = number.as_i64() {
                    statement.raw_bind_parameter(index, integer)?;
                } else if let Some(real) = number.as_f64() {
                    statement.raw_bind_parameter(index, real)?;
                }
            }
            Value::String(text) => statement.raw_bind_parameter(index, text.as_str())?,
            // Arrays and objects have no SQLite type; their JSON encoding is
            // the only lossless option.
            other => statement.raw_bind_parameter(index, other.to_string())?,
        }
    }
    Ok(())
}

pub(crate) fn fs_error(error: &rusqlite::Error) -> FsError {
    FsError::with_message(FsErrorKind::Other, error.to_string())
}

/// Open a connection with the pragmas the session database needs.
pub(crate) fn open_database(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(connection)
}

/// Guards a lazily opened connection.
pub(crate) type ConnectionCell = Mutex<Option<Connection>>;
