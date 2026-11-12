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

pub mod display;
pub mod pg;

pub use gq_sql::Dialect;

/// Arrow field metadata keys set by drivers on every result column.
pub mod meta {
    /// Database type name, e.g. `int4`, `timestamptz`, `my_enum`.
    pub const DB_TYPE: &str = "gq.db_type";
    /// A [`super::ColumnKind`] name; drives alignment/colour in the grid.
    pub const KIND: &str = "gq.kind";
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
}

impl DbKind {
    pub fn dialect(self) -> Dialect {
        match self {
            DbKind::Postgres => Dialect::Postgres,
            DbKind::Redshift => Dialect::Redshift,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
}

/// Everything needed to open a session. Passwords are resolved from the OS
/// keyring by the caller and never persisted here.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub ssl: SslMode,
    pub connect_timeout: Duration,
    /// Shown in `pg_stat_activity.application_name` and similar.
    pub application_name: String,
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
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;
    /// Execute exactly one statement. While a returned [`RowStream`] is alive
    /// the session must not be used for anything else; the engine enforces
    /// this by closing streams before issuing new statements.
    async fn execute(&mut self, sql: &str) -> Result<Execution>;
    fn is_closed(&self) -> bool;
}

/// Open a session for `params`.
pub async fn connect(params: &ConnectParams) -> Result<Box<dyn Session>> {
    match params.kind {
        DbKind::Postgres | DbKind::Redshift => Ok(Box::new(pg::PgSession::connect(params).await?)),
    }
}
