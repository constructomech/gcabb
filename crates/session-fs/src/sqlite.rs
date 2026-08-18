//! The per-session `SQLite` database, owned by the host.
//!
//! The runtime asks for queries through the session filesystem, and the app
//! reads and writes the same tables directly. Both go through one connection
//! behind one lock, so an edit made in the app and a write made by the agent
//! cannot interleave halfway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use github_copilot_sdk::session_fs::{
    FsError, SessionFsSqliteQueryResult, SessionFsSqliteQueryType, SessionFsSqliteTransactionError,
    SessionFsSqliteTransactionStatement,
};
use rusqlite::Connection;
use serde_json::Value;

use crate::{ConnectionCell, bind_named, fs_error, json_value, open_database};

/// A session's `SQLite` database.
pub struct SqliteStore {
    path: PathBuf,
    connection: ConnectionCell,
}

impl SqliteStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            connection: ConnectionCell::default(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the database has been created yet.
    pub async fn exists(&self) -> bool {
        self.connection.lock().await.is_some() || self.path.exists()
    }

    /// Run a statement, opening the database if this is the first use.
    pub async fn query(
        &self,
        query_type: SessionFsSqliteQueryType,
        query: &str,
        params: Option<&HashMap<String, Value>>,
    ) -> Result<SessionFsSqliteQueryResult, FsError> {
        let mut guard = self.connection.lock().await;
        let connection = match guard.as_mut() {
            Some(connection) => connection,
            None => guard.insert(open_database(&self.path).map_err(|error| fs_error(&error))?),
        };
        tracing::debug!(?query_type, query, "session sqlite query");
        run_statement(connection, &query_type, query, params).map_err(|error| {
            tracing::debug!(%error, query, "session sqlite query failed");
            fs_error(&error)
        })
    }

    /// Run several statements atomically.
    ///
    /// A failure rolls the whole batch back, since the runtime treats a
    /// transaction as all-or-nothing.
    pub async fn transaction(
        &self,
        statements: &[SessionFsSqliteTransactionStatement],
    ) -> Result<Vec<SessionFsSqliteQueryResult>, SessionFsSqliteTransactionError> {
        let mut guard = self.connection.lock().await;
        let connection = match guard.as_mut() {
            Some(connection) => connection,
            None => guard.insert(
                open_database(&self.path)
                    .map_err(|error| SessionFsSqliteTransactionError::fatal(error.to_string()))?,
            ),
        };
        let transaction = connection
            .transaction()
            .map_err(|error| transaction_error(&error))?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            let result = run_statement(
                &transaction,
                &statement.query_type,
                &statement.query,
                statement.params.as_ref(),
            )
            .map_err(|error| transaction_error(&error))?;
            results.push(result);
        }
        transaction
            .commit()
            .map_err(|error| transaction_error(&error))?;
        Ok(results)
    }

    /// Read rows from the database for the app's own use.
    pub async fn read(
        &self,
        query: &str,
        params: Option<&HashMap<String, Value>>,
    ) -> Result<SessionFsSqliteQueryResult, FsError> {
        self.query(SessionFsSqliteQueryType::Query, query, params)
            .await
    }

    /// Write to the database for the app's own use.
    pub async fn write(
        &self,
        query: &str,
        params: Option<&HashMap<String, Value>>,
    ) -> Result<SessionFsSqliteQueryResult, FsError> {
        self.query(SessionFsSqliteQueryType::Run, query, params)
            .await
    }

    /// Apply a batch that may contain several statements, such as a schema.
    ///
    /// [`Self::write`] prepares a single statement and rejects anything after
    /// it, which is the wrong shape for schema bootstraps.
    pub async fn exec(&self, query: &str) -> Result<SessionFsSqliteQueryResult, FsError> {
        self.query(SessionFsSqliteQueryType::Exec, query, None)
            .await
    }
}

/// A busy or locked database is worth retrying; anything else is not.
fn transaction_error(error: &rusqlite::Error) -> SessionFsSqliteTransactionError {
    let message = error.to_string();
    if message.contains("database is locked") || message.contains("database table is locked") {
        SessionFsSqliteTransactionError::busy_or_locked(message)
    } else {
        SessionFsSqliteTransactionError::fatal(message)
    }
}

fn run_statement(
    connection: &Connection,
    query_type: &SessionFsSqliteQueryType,
    query: &str,
    params: Option<&HashMap<String, Value>>,
) -> rusqlite::Result<SessionFsSqliteQueryResult> {
    match query_type {
        SessionFsSqliteQueryType::Query => select(connection, query, params),
        // `exec` carries schema bootstraps, which arrive as several statements
        // in one string. Preparing that rejects everything after the first, so
        // the runtime's todos and todo_deps tables would never be created.
        SessionFsSqliteQueryType::Exec => exec_batch(connection, query, params),
        // `run` is a single statement whose result is a row count, as is
        // anything the runtime adds later.
        _ => execute(connection, query, params),
    }
}

fn exec_batch(
    connection: &Connection,
    query: &str,
    params: Option<&HashMap<String, Value>>,
) -> rusqlite::Result<SessionFsSqliteQueryResult> {
    // A batch cannot carry bindings, so a parameterised `exec` is really a
    // single statement and has to keep the prepared path.
    if params.is_some_and(|params| !params.is_empty()) {
        return execute(connection, query, params);
    }
    connection.execute_batch(query)?;
    Ok(SessionFsSqliteQueryResult {
        columns: Vec::new(),
        last_insert_rowid: None,
        rows: Vec::new(),
        rows_affected: i64::try_from(connection.changes()).unwrap_or(i64::MAX),
    })
}

fn select(
    connection: &Connection,
    query: &str,
    params: Option<&HashMap<String, Value>>,
) -> rusqlite::Result<SessionFsSqliteQueryResult> {
    let mut statement = connection.prepare(query)?;
    bind_named(&mut statement, params)?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = Vec::new();
    let mut cursor = statement.raw_query();
    while let Some(row) = cursor.next()? {
        let mut mapped = HashMap::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            mapped.insert(column.clone(), json_value(row.get_ref(index)?));
        }
        rows.push(mapped);
    }
    Ok(SessionFsSqliteQueryResult {
        columns,
        last_insert_rowid: None,
        rows,
        rows_affected: 0,
    })
}

fn execute(
    connection: &Connection,
    query: &str,
    params: Option<&HashMap<String, Value>>,
) -> rusqlite::Result<SessionFsSqliteQueryResult> {
    let mut statement = connection.prepare(query)?;
    bind_named(&mut statement, params)?;
    let rows_affected = statement.raw_execute()?;
    Ok(SessionFsSqliteQueryResult {
        columns: Vec::new(),
        last_insert_rowid: Some(connection.last_insert_rowid()),
        rows: Vec::new(),
        rows_affected: i64::try_from(rows_affected).unwrap_or(i64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store(directory: &Path) -> SqliteStore {
        SqliteStore::new(directory.join("session.db"))
    }

    #[tokio::test]
    async fn the_database_is_only_created_on_first_use() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        assert!(!store.exists().await);

        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY)", None)
            .await
            .expect("create");

        assert!(store.exists().await);
    }

    #[tokio::test]
    async fn rows_round_trip_with_their_column_names() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY, title TEXT)", None)
            .await
            .expect("create");
        store
            .write("INSERT INTO todos (id, title) VALUES ('a', 'First')", None)
            .await
            .expect("insert");

        let read = store
            .read("SELECT id, title FROM todos", None)
            .await
            .expect("select");

        assert_eq!(read.columns, vec!["id".to_owned(), "title".to_owned()]);
        assert_eq!(read.rows.len(), 1);
        assert_eq!(read.rows[0]["title"], Value::from("First"));
    }

    #[tokio::test]
    async fn named_parameters_bind_with_or_without_a_sigil() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY, title TEXT)", None)
            .await
            .expect("create");

        let mut bare = HashMap::new();
        bare.insert("id".to_owned(), Value::from("a"));
        bare.insert("title".to_owned(), Value::from("Bare"));
        store
            .write(
                "INSERT INTO todos (id, title) VALUES (:id, :title)",
                Some(&bare),
            )
            .await
            .expect("insert bare");

        let mut sigil = HashMap::new();
        sigil.insert(":id".to_owned(), Value::from("b"));
        sigil.insert(":title".to_owned(), Value::from("Sigil"));
        store
            .write(
                "INSERT INTO todos (id, title) VALUES (:id, :title)",
                Some(&sigil),
            )
            .await
            .expect("insert sigil");

        let read = store
            .read("SELECT title FROM todos ORDER BY id", None)
            .await
            .expect("select");
        let titles: Vec<_> = read
            .rows
            .iter()
            .map(|row| row["title"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(titles, vec!["Bare", "Sigil"]);
    }

    #[tokio::test]
    async fn writes_report_affected_rows_and_selects_report_none() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY)", None)
            .await
            .expect("create");
        let inserted = store
            .write("INSERT INTO todos (id) VALUES ('a'), ('b')", None)
            .await
            .expect("insert");
        assert_eq!(inserted.rows_affected, 2);
        assert!(inserted.last_insert_rowid.is_some());

        let read = store
            .read("SELECT id FROM todos", None)
            .await
            .expect("read");
        assert_eq!(read.rows_affected, 0);
        assert!(read.last_insert_rowid.is_none());
    }

    #[tokio::test]
    async fn a_failed_statement_rolls_the_whole_transaction_back() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY)", None)
            .await
            .expect("create");

        let statements = vec![
            SessionFsSqliteTransactionStatement {
                params: None,
                query: "INSERT INTO todos (id) VALUES ('a')".to_owned(),
                query_type: SessionFsSqliteQueryType::Run,
            },
            SessionFsSqliteTransactionStatement {
                params: None,
                query: "INSERT INTO nonexistent (id) VALUES ('b')".to_owned(),
                query_type: SessionFsSqliteQueryType::Run,
            },
        ];
        let error = store
            .transaction(&statements)
            .await
            .expect_err("transaction fails");
        assert!(!error.to_string().is_empty());

        // The first insert must not survive the failure of the second.
        let read = store
            .read("SELECT id FROM todos", None)
            .await
            .expect("read");
        assert!(read.rows.is_empty());
    }

    #[tokio::test]
    async fn a_successful_transaction_commits_every_statement() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .write("CREATE TABLE todos (id TEXT PRIMARY KEY)", None)
            .await
            .expect("create");

        let statements = vec![
            SessionFsSqliteTransactionStatement {
                params: None,
                query: "INSERT INTO todos (id) VALUES ('a')".to_owned(),
                query_type: SessionFsSqliteQueryType::Run,
            },
            SessionFsSqliteTransactionStatement {
                params: None,
                query: "SELECT id FROM todos".to_owned(),
                query_type: SessionFsSqliteQueryType::Query,
            },
        ];
        let results = store.transaction(&statements).await.expect("transaction");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].rows_affected, 1);
        assert_eq!(results[1].rows.len(), 1);
    }

    /// The schema the runtime sends to bootstrap its task list, verbatim.
    ///
    /// It arrives as one `exec` containing two statements, which is the case
    /// that a prepared-statement path silently rejects.
    const RUNTIME_TODO_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS todos (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        description TEXT,
        status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'in_progress', 'done', 'blocked')),
        created_at TEXT DEFAULT (datetime('now')),
        updated_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS todo_deps (
        todo_id TEXT NOT NULL,
        depends_on TEXT NOT NULL,
        PRIMARY KEY (todo_id, depends_on),
        FOREIGN KEY (todo_id) REFERENCES todos(id),
        FOREIGN KEY (depends_on) REFERENCES todos(id)
    );
";

    #[tokio::test]
    async fn the_runtimes_multi_statement_schema_is_accepted() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());

        store
            .query(SessionFsSqliteQueryType::Exec, RUNTIME_TODO_SCHEMA, None)
            .await
            .expect("schema applies");

        // Both tables have to exist: stopping at the first statement would
        // leave the runtime unable to record dependencies.
        store
            .read("SELECT id FROM todos", None)
            .await
            .expect("todos exists");
        store
            .read("SELECT todo_id FROM todo_deps", None)
            .await
            .expect("todo_deps exists");
    }

    #[tokio::test]
    async fn a_parameterised_exec_still_binds_its_values() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        store
            .query(SessionFsSqliteQueryType::Exec, RUNTIME_TODO_SCHEMA, None)
            .await
            .expect("schema");

        let mut params = HashMap::new();
        params.insert("id".to_owned(), Value::from("bound"));
        params.insert("title".to_owned(), Value::from("Bound title"));
        store
            .query(
                SessionFsSqliteQueryType::Exec,
                "INSERT INTO todos (id, title) VALUES (:id, :title)",
                Some(&params),
            )
            .await
            .expect("parameterised exec");

        let read = store
            .read("SELECT title FROM todos WHERE id = 'bound'", None)
            .await
            .expect("read");
        assert_eq!(read.rows[0]["title"], Value::from("Bound title"));
    }

    #[tokio::test]
    async fn a_multi_statement_schema_needs_exec_rather_than_write() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());

        // `write` prepares one statement, so a schema handed to it is
        // rejected rather than silently half-applied.
        assert!(store.write(RUNTIME_TODO_SCHEMA, None).await.is_err());

        store.exec(RUNTIME_TODO_SCHEMA).await.expect("exec applies");
        store
            .read("SELECT todo_id FROM todo_deps", None)
            .await
            .expect("todo_deps exists");
    }

    #[tokio::test]
    async fn the_agent_and_the_app_see_one_another_through_the_same_store() {
        let directory = tempdir().expect("tempdir");
        let store = store(directory.path());
        // Standing in for the agent's `sql` tool arriving over the wire.
        store
            .query(
                SessionFsSqliteQueryType::Run,
                "CREATE TABLE todos (id TEXT PRIMARY KEY, title TEXT, status TEXT)",
                None,
            )
            .await
            .expect("agent creates");
        store
            .query(
                SessionFsSqliteQueryType::Run,
                "INSERT INTO todos VALUES ('agent', 'Agent task', 'pending')",
                None,
            )
            .await
            .expect("agent inserts");

        // The app editing the agent's list.
        store
            .write("UPDATE todos SET status = 'done' WHERE id = 'agent'", None)
            .await
            .expect("app updates");

        let read = store
            .query(
                SessionFsSqliteQueryType::Query,
                "SELECT status FROM todos WHERE id = 'agent'",
                None,
            )
            .await
            .expect("agent reads");
        assert_eq!(read.rows[0]["status"], Value::from("done"));
    }
}
