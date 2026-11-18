//! Driver abstraction.
//!
//! A [`Session`] is one database connection owned by one query tab. Executing
//! a statement yields either a row count or a [`RowStream`] of Arrow record
//! batches. Streams are *pull-based*: rows are only read from the network when
//! the engine asks for the next batch, which is how results stay paused on the
//! server until the user scrolls.
//!
//! Cancellation goes through a [`CancelHandle`], which can be used from any
//! task while the session is busy and cancels the statement on the server.

use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use async_trait::async_trait;

pub const MAX_VALUE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ROW_BYTES: usize = 16 * 1024 * 1024;
pub const TARGET_BATCH_BYTES: usize = 4 * 1024 * 1024;

pub mod display;
pub mod pg;
pub mod sqlite;
pub mod duckdb;
mod mysql;
pub mod csv_import;

pub use nbq_sql::Dialect;

/// Arrow field metadata keys set by drivers on every result column.
pub mod meta {
    /// Database type name, e.g. `int4`, `timestamptz`, `my_enum`.
    pub const DB_TYPE: &str = "nbq.db_type";
    /// A [`super::ColumnKind`] name; drives alignment/colour in the grid.
    pub const KIND: &str = "nbq.kind";
}

/// Coarse classification of a column for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnKind {
    Number,
    Text,
    Bool,
    Temporal,
    Json,
    Uuid,
    Binary,
    Other,
}

impl ColumnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnKind::Number => "number",
            ColumnKind::Text => "text",
            ColumnKind::Bool => "bool",
            ColumnKind::Temporal => "temporal",
            ColumnKind::Json => "json",
            ColumnKind::Uuid => "uuid",
            ColumnKind::Binary => "binary",
            ColumnKind::Other => "other",
        }
    }
    pub fn parse(s: &str) -> ColumnKind {
        match s {
            "number" => ColumnKind::Number,
            "text" => ColumnKind::Text,
            "bool" => ColumnKind::Bool,
            "temporal" => ColumnKind::Temporal,
            "json" => ColumnKind::Json,
            "uuid" => ColumnKind::Uuid,
            "binary" => ColumnKind::Binary,
            _ => ColumnKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Redshift,
    MySql,
    Sqlite,
    DuckDb,
}

impl DbKind {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Redshift => 5439,
            Self::MySql => 3306,
            Self::Sqlite | Self::DuckDb => 0,
        }
    }

    pub fn default_database(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Redshift => "dev",
            _ => "",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            DbKind::Postgres => Dialect::Postgres,
            DbKind::Redshift => Dialect::Redshift,
            DbKind::MySql => Dialect::MySql,
            DbKind::Sqlite => Dialect::Sqlite,
            DbKind::DuckDb => Dialect::DuckDb,
        }
    }

    /// File-backed engines share the connection dialog and path configuration.
    pub fn is_file_based(self) -> bool { matches!(self, Self::Sqlite | Self::DuckDb) }

    pub fn file_extensions(self) -> &'static [&'static str] {
        match self { Self::Sqlite => &["db", "sqlite", "sqlite3"], Self::DuckDb => &["duckdb", "ddb", "db"], _ => &[] }
    }

    pub fn default_file_name(self) -> &'static str {
        match self { Self::DuckDb => "database.duckdb", _ => "database.db" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    #[serde(rename = "verify-full")]
    VerifyFull,
}

/// Everything needed to open a session. Passwords are resolved from the OS
/// keyring by the caller and never persisted here.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub kind: DbKind,
    /// Absolute path for file-backed engines. Opening never creates a file.
    pub file_path: std::path::PathBuf,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub ssl: SslMode,
    /// Optional public PEM roots, portable with a project. No private keys.
    pub ssl_root_cert: String,
    pub connect_timeout: Duration,
    /// Shown in `pg_stat_activity.application_name` and similar.
    pub application_name: String,
}

impl ConnectParams {
    pub fn file(kind: DbKind, path: impl Into<std::path::PathBuf>) -> Self {
        Self { kind, file_path: path.into(), host: String::new(), port: 0, database: String::new(), user: String::new(), password: None, ssl: SslMode::Disable, ssl_root_cert: String::new(), connect_timeout: Duration::from_secs(10), application_name: "not-bad-query".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DriverError {
    pub message: String,
    /// SQLSTATE or vendor error code.
    pub code: Option<String>,
    /// 1-based character position in the statement, when the server reports one.
    pub position: Option<u32>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Network/auth failure; the session is unusable.
    Connection,
    /// The statement was cancelled by the user.
    Cancelled,
    /// Any other server-reported error; the session is still usable.
    Query,
}

impl DriverError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        DriverError { message: message.into(), code: None, position: None, detail: None, hint: None, kind }
    }
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Connection, message)
    }
}

pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// What executing one statement produced.
pub enum Execution {
    /// A statement that returns a result set. Rows are pulled lazily.
    Rows(Box<dyn RowStream>),
    /// A statement without a result set.
    Done { rows_affected: Option<u64>, command: String },
}

/// A lazily-read result set.
#[async_trait]
pub trait RowStream: Send {
    fn schema(&self) -> SchemaRef;
    /// Read up to `max_rows` rows. Returns early with a partial batch once
    /// `soft_deadline` has elapsed and at least one row was read, so slow
    /// producers still show rows promptly. `None` at end of stream.
    async fn next_batch(&mut self, max_rows: usize, soft_deadline: Duration) -> Result<Option<RecordBatch>>;
    /// Rows affected as reported on completion (e.g. `UPDATE ... RETURNING`).
    fn rows_affected(&self) -> Option<u64>;
}

/// Cancels whatever the owning session is currently running, on the server.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
    /// Terminal fallback: close the owning socket if protocol cancellation or
    /// graceful shutdown cannot complete. Never used for ordinary query Stop.
    fn close_connection(&self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Savepoints can be used to recover a transaction after an error.
    pub savepoints: bool,
}

#[async_trait]
pub trait Session: Send {
    fn dialect(&self) -> Dialect;
    fn capabilities(&self) -> Capabilities;
    /// Drivers without savepoints may still safely stop a paused stream.
    fn can_cancel_in_transaction(&self) -> bool { self.capabilities().savepoints }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;
    /// Execute exactly one statement. While a returned [`RowStream`] is alive
    /// the session must not be used for anything else; the engine enforces
    /// this by closing streams before issuing new statements.
    async fn execute(&mut self, sql: &str) -> Result<Execution>;
    fn is_closed(&self) -> bool;
    /// Some embedded engines may roll back a transaction after interruption.
    fn transaction_active(&self) -> Option<bool> { None }
    /// Last asynchronous connection error, if the driver can report it.
    fn closed_error(&self) -> Option<DriverError> { None }
    /// Install a server-side idle-transaction backstop where supported. Return
    /// the effective limit, preserving a shorter administrator-supplied limit.
    async fn configure_transaction_timeout(&mut self, timeout: Duration) -> Result<Duration> { Ok(timeout) }
}

/// Open a session for `params`.
pub async fn connect(params: &ConnectParams) -> Result<Box<dyn Session>> {
    match params.kind {
        DbKind::Postgres | DbKind::Redshift => Ok(Box::new(pg::PgSession::connect(params).await?)),
        DbKind::MySql => Ok(Box::new(mysql::MySqlSession::connect(params).await?)),
        DbKind::Sqlite => Ok(Box::new(sqlite::SqliteSession::connect(params).await?)),
        DbKind::DuckDb => Ok(Box::new(duckdb::DuckDbSession::connect(params).await?)),
    }
}

/// Explicit file creation, separate from connecting so a typo cannot create a DB.
pub fn create_database(kind: DbKind, path: &std::path::Path) -> Result<()> {
    match kind {
        DbKind::Sqlite => sqlite::create_database(path),
        DbKind::DuckDb => duckdb::create_database(path),
        _ => Err(DriverError::connection("This engine does not use a database file")),
    }
}
