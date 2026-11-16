//! File-backed DuckDB sessions. A shared database instance owns independent
//! connections; blocking work and lazy Arrow fetching stay on session workers.
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use ::duckdb::{Config, Connection};
use arrow_array::RecordBatch;
use arrow_schema::{DataType, Schema, SchemaRef};
use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::{
    CancelHandle, Capabilities, ColumnKind, ConnectParams, Dialect, DriverError, ErrorKind,
    Execution, Result, RowStream, Session, meta,
};

type Database = Arc<Mutex<Connection>>;
type Instances = HashMap<PathBuf, Weak<Mutex<Connection>>>;
static INSTANCES: OnceLock<Mutex<Instances>> = OnceLock::new();

fn connection_error(e: impl std::fmt::Display) -> DriverError {
    DriverError::connection(e.to_string())
}

fn open(path: &Path) -> Result<(Database, Connection)> {
    use std::io::Read;
    if !path.is_absolute() {
        return Err(connection_error("Choose an absolute database file path"));
    }
    let path = path.canonicalize().map_err(connection_error)?;
    let mut instances = INSTANCES
        .get_or_init(Default::default)
        .lock()
        .map_err(connection_error)?;
    instances.retain(|_, db| db.strong_count() > 0);
    let database = if let Some(db) = instances.get(&path).and_then(Weak::upgrade) {
        db
    } else {
        // DuckDB takes an exclusive file handle on Windows. Check an existing
        // file only when opening a new instance, and release the probe first.
        let mut file = std::fs::File::open(&path).map_err(connection_error)?;
        let mut header = [0; 12];
        file.read_exact(&mut header).map_err(connection_error)?;
        if &header[8..12] != b"DUCK" {
            return Err(connection_error("This file is not a DuckDB database"));
        }
        drop(file);
        let config = Config::default()
            .with("memory_limit", "512MB")
            .map_err(connection_error)?;
        let db = Connection::open_with_flags(&path, config).map_err(connection_error)?;
        let db = Arc::new(Mutex::new(db));
        instances.insert(path, Arc::downgrade(&db));
        db
    };
    let connection = database
        .lock()
        .map_err(connection_error)?
        .try_clone()
        .map_err(connection_error)?;
    Ok((database, connection))
}

pub fn create_database(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(connection_error("Choose an absolute database file path"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| connection_error("Choose a database filename"))?;
    // DuckDB rejects an empty reserved file. Initialize privately in the same
    // directory, then publish with an atomic no-clobber rename.
    let staging = tempfile::tempdir_in(parent).map_err(connection_error)?;
    let staged = staging.path().join("new.duckdb");
    let db = Connection::open(&staged).map_err(connection_error)?;
    db.execute_batch("CHECKPOINT").map_err(connection_error)?;
    drop(db);
    let staged = tempfile::TempPath::try_from_path(staged).map_err(connection_error)?;
    staged.persist_noclobber(path).map_err(connection_error)?;
    Ok(())
}

struct Cancel {
    interrupt: Arc<::duckdb::InterruptHandle>,
    cancelled: AtomicBool,
    closed: AtomicBool,
    busy: AtomicBool,
    in_transaction: AtomicBool,
}

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        // A paused result can be finalized without aborting its transaction.
        if self.busy.load(Ordering::Acquire) {
            self.interrupt.interrupt();
        }
        Ok(())
    }
    fn close_connection(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        self.interrupt.interrupt();
    }
}

type Pull = (
    usize,
    Duration,
    oneshot::Sender<Result<Option<RecordBatch>>>,
);
type Execute = (String, oneshot::Sender<Result<Execution>>);

pub struct DuckDbSession {
    commands: mpsc::Sender<Execute>,
    cancel: Arc<Cancel>,
}

impl DuckDbSession {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let path = params.file_path.clone();
        let (ready, rx) = oneshot::channel();
        let (commands, work) = mpsc::channel::<Execute>();
        std::thread::Builder::new()
            .name("nbq-duckdb".into())
            .spawn(move || {
                let (_database, db) = match open(&path) {
                    Ok(db) => db,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let cancel = Arc::new(Cancel {
                    interrupt: db.interrupt_handle(),
                    cancelled: AtomicBool::new(false),
                    closed: AtomicBool::new(false),
                    busy: AtomicBool::new(false),
                    in_transaction: AtomicBool::new(false),
                });
                if ready.send(Ok(cancel.clone())).is_err() {
                    close_instance(_database, db);
                    return;
                }
                while let Ok((sql, reply)) = work.recv() {
                    if cancel.closed.load(Ordering::Acquire) {
                        break;
                    }
                    run(&db, &sql, reply, &cancel);
                }
                // Keep the cache's owner alive until this worker's connection
                // is closed, including while a concurrent session reconnects.
                close_instance(_database, db);
                cancel.closed.store(true, Ordering::Release);
            })
            .map_err(connection_error)?;
        let cancel = rx.await.map_err(connection_error)??;
        Ok(Self { commands, cancel })
    }
}

fn close_instance(database: Database, connection: Connection) {
    drop(connection);
    // Weak::upgrade can fail while the last owner's destructor is still
    // closing the exclusive Windows handle. Serialize that final close with open.
    let _closing = INSTANCES.get().and_then(|instances| instances.lock().ok());
    drop(database);
}

impl Drop for DuckDbSession {
    fn drop(&mut self) {
        self.cancel.close_connection();
    }
}

#[async_trait]
impl Session for DuckDbSession {
    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { savepoints: false }
    }
    fn can_cancel_in_transaction(&self) -> bool {
        true
    }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }
    fn is_closed(&self) -> bool {
        self.cancel.closed.load(Ordering::Acquire)
    }
    fn transaction_active(&self) -> Option<bool> {
        Some(self.cancel.in_transaction.load(Ordering::Acquire))
    }
    async fn execute(&mut self, sql: &str) -> Result<Execution> {
        self.cancel.cancelled.store(false, Ordering::Release);
        let (tx, rx) = oneshot::channel();
        self.commands
            .send((sql.into(), tx))
            .map_err(connection_error)?;
        rx.await.map_err(connection_error)?
    }
}

struct Stream {
    schema: SchemaRef,
    pull: mpsc::Sender<Pull>,
    done: bool,
}

#[async_trait]
impl RowStream for Stream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn rows_affected(&self) -> Option<u64> {
        None
    }
    async fn next_batch(&mut self, max: usize, deadline: Duration) -> Result<Option<RecordBatch>> {
        if self.done {
            return Ok(None);
        }
        let (tx, rx) = oneshot::channel();
        self.pull
            .send((max.max(1), deadline, tx))
            .map_err(connection_error)?;
        let result = rx.await.map_err(connection_error)?;
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }
}

fn cancelled() -> DriverError {
    DriverError::new(ErrorKind::Cancelled, "Query cancelled")
}

fn query_error(db: &Connection, cancel: &Cancel, e: ::duckdb::Error) -> DriverError {
    let message = e.to_string();
    let mut error = DriverError::new(
        if cancel.cancelled.load(Ordering::Acquire) || message.contains("INTERRUPT") {
            ErrorKind::Cancelled
        } else {
            ErrorKind::Query
        },
        message,
    );
    // DuckDB has no savepoints. Binding/parser errors may leave a transaction
    // usable; execution errors can abort it. Resolve only aborted transactions.
    if cancel.in_transaction.load(Ordering::Acquire)
        && db
            .query_row("SELECT 1", [], |r| r.get::<_, i32>(0))
            .is_err()
    {
        if db.execute_batch("ROLLBACK").is_err() {
            cancel.close_connection();
        }
        cancel.in_transaction.store(false, Ordering::Release);
        error.detail = Some(
            "DuckDB aborted this transaction. Its uncommitted changes were rolled back.".into(),
        );
    }
    error
}

fn row_statement(sql: &str) -> bool {
    let kind = nbq_sql::classify(sql, Dialect::DuckDb);
    if kind == nbq_sql::StatementKind::Query {
        return true;
    }
    let mut words = nbq_sql::words(sql, Dialect::DuckDb);
    let first = words.next().unwrap_or_default().to_ascii_uppercase();
    matches!(
        first.as_str(),
        "WITH" | "CALL" | "SUMMARIZE" | "FROM" | "PIVOT" | "UNPIVOT"
    ) || words.any(|w| w.eq_ignore_ascii_case("returning"))
}

fn kind(ty: &DataType) -> ColumnKind {
    if ty.is_numeric() {
        return ColumnKind::Number;
    }
    if ty.is_temporal() {
        return ColumnKind::Temporal;
    }
    match ty {
        DataType::Boolean => ColumnKind::Bool,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            ColumnKind::Binary
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => ColumnKind::Text,
        _ => ColumnKind::Other,
    }
}

fn sql_type(ty: &DataType) -> String {
    use DataType::*;
    match ty {
        Boolean => "BOOLEAN".into(),
        Int8 => "TINYINT".into(),
        Int16 => "SMALLINT".into(),
        Int32 => "INTEGER".into(),
        Int64 => "BIGINT".into(),
        UInt8 => "UTINYINT".into(),
        UInt16 => "USMALLINT".into(),
        UInt32 => "UINTEGER".into(),
        UInt64 => "UBIGINT".into(),
        Float16 | Float32 => "FLOAT".into(),
        Float64 => "DOUBLE".into(),
        Decimal32(p, s) | Decimal64(p, s) | Decimal128(p, s) | Decimal256(p, s) => {
            format!("DECIMAL({p},{s})")
        }
        Utf8 | LargeUtf8 | Utf8View => "VARCHAR".into(),
        Binary | LargeBinary | BinaryView | FixedSizeBinary(_) => "BLOB".into(),
        Date32 | Date64 => "DATE".into(),
        Timestamp(_, Some(_)) => "TIMESTAMPTZ".into(),
        Timestamp(_, None) => "TIMESTAMP".into(),
        Time32(_) | Time64(_) => "TIME".into(),
        Interval(_) | Duration(_) => "INTERVAL".into(),
        List(f) | LargeList(f) | ListView(f) | LargeListView(f) => {
            format!("{}[]", sql_type(f.data_type()))
        }
        FixedSizeList(f, n) => format!("{}[{n}]", sql_type(f.data_type())),
        Struct(_) => "STRUCT".into(),
        Map(_, _) => "MAP".into(),
        Union(_, _) => "UNION".into(),
        Dictionary(_, _) => "ENUM".into(),
        Null => "NULL".into(),
        _ => ty.to_string(),
    }
}

struct ValueSize(usize);
impl std::fmt::Write for ValueSize {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.0 = self.0.saturating_add(value.len());
        if self.0 > crate::MAX_VALUE_BYTES {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn run(db: &Connection, sql: &str, reply: oneshot::Sender<Result<Execution>>, cancel: &Cancel) {
    cancel.busy.store(true, Ordering::Release);
    if cancel.cancelled.load(Ordering::Acquire) {
        cancel.busy.store(false, Ordering::Release);
        let _ = reply.send(Err(cancelled()));
        return;
    }
    let mut stmt = match db.prepare(sql) {
        Ok(stmt) => stmt,
        Err(e) => {
            cancel.busy.store(false, Ordering::Release);
            let _ = reply.send(Err(query_error(db, cancel, e)));
            return;
        }
    };
    if !row_statement(sql) {
        let result = stmt
            .execute([])
            .map(|n| Execution::Done {
                rows_affected: Some(n as u64),
                command: nbq_sql::words(sql, Dialect::DuckDb)
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase(),
            })
            .map_err(|e| query_error(db, cancel, e));
        if result.is_ok() {
            // duckdb-rs's is_autocommit currently returns a constant. Track
            // successful explicit transaction commands and aborted recovery.
            match nbq_sql::classify(sql, Dialect::DuckDb) {
                nbq_sql::StatementKind::Begin => {
                    cancel.in_transaction.store(true, Ordering::Release);
                }
                nbq_sql::StatementKind::Commit | nbq_sql::StatementKind::Rollback => {
                    cancel.in_transaction.store(false, Ordering::Release);
                }
                _ => {}
            }
        }
        cancel.busy.store(false, Ordering::Release);
        let _ = reply.send(result);
        return;
    }
    let mut rows = match stmt.stream_arrow([]) {
        Ok(rows) => rows,
        Err(e) => {
            cancel.busy.store(false, Ordering::Release);
            let _ = reply.send(Err(query_error(db, cancel, e)));
            return;
        }
    };
    cancel.busy.store(false, Ordering::Release);
    let source_schema = rows.get_schema();
    if source_schema.fields().len() > 1024 {
        let _ = reply.send(Err(DriverError::new(
            ErrorKind::Query,
            "DuckDB result exceeds 1,024 columns",
        )));
        return;
    }
    let schema = Arc::new(Schema::new(
        source_schema
            .fields()
            .iter()
            .map(|f| {
                let mut metadata = f.metadata().clone();
                metadata.insert(meta::DB_TYPE, sql_type(f.data_type()));
                metadata.insert(meta::KIND, kind(f.data_type()).as_str());
                f.as_ref().clone().with_metadata(metadata)
            })
            .collect::<Vec<_>>(),
    ));
    let (pull, requests) = mpsc::channel::<Pull>();
    if reply
        .send(Ok(Execution::Rows(Box::new(Stream {
            schema: schema.clone(),
            pull,
            done: false,
        }))))
        .is_err()
    {
        return;
    }
    let mut pending: Option<RecordBatch> = None;
    let mut offset = 0;
    loop {
        if cancel.closed.load(Ordering::Acquire) {
            break;
        }
        let (max, deadline, reply) = match requests.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let result = (|| -> Result<Option<RecordBatch>> {
            if cancel.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            if pending.is_none() {
                cancel.busy.store(true, Ordering::Release);
                if cancel.cancelled.load(Ordering::Acquire) {
                    cancel.busy.store(false, Ordering::Release);
                    return Err(cancelled());
                }
                let next = rows.next();
                cancel.busy.store(false, Ordering::Release);
                pending = next.transpose().map_err(|e| query_error(db, cancel, e))?;
                offset = 0;
            }
            let Some(batch) = pending.as_ref() else {
                return Ok(None);
            };
            let options = Default::default();
            let formatters = batch
                .columns()
                .iter()
                .map(|a| arrow_cast::display::ArrayFormatter::try_new(a.as_ref(), &options))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| DriverError::new(ErrorKind::Query, e.to_string()))?;
            let start = offset;
            let started = Instant::now();
            let mut bytes = 0;
            while offset < batch.num_rows() && offset - start < max {
                if cancel.cancelled.load(Ordering::Acquire) {
                    return Err(cancelled());
                }
                let mut row_bytes = 0;
                for formatter in &formatters {
                    let mut value = ValueSize(0);
                    write!(&mut value, "{}", formatter.value(offset)).map_err(|_| {
                        DriverError::new(ErrorKind::Query, "DuckDB value exceeds 8 MiB")
                    })?;
                    row_bytes += value.0;
                }
                if row_bytes > crate::MAX_ROW_BYTES {
                    return Err(DriverError::new(
                        ErrorKind::Query,
                        "DuckDB row exceeds 16 MiB",
                    ));
                }
                bytes += row_bytes;
                offset += 1;
                if bytes >= crate::TARGET_BATCH_BYTES || started.elapsed() >= deadline {
                    break;
                }
            }
            let count = offset - start;
            RecordBatch::try_new(
                schema.clone(),
                batch
                    .columns()
                    .iter()
                    .map(|a| a.slice(start, count))
                    .collect(),
            )
            .map(Some)
            .map_err(|e| DriverError::new(ErrorKind::Query, e.to_string()))
        })();
        let end = !matches!(result, Ok(Some(_)));
        if pending
            .as_ref()
            .is_some_and(|batch| offset == batch.num_rows())
        {
            pending = None;
        }
        if reply.send(result).is_err() || end {
            break;
        }
    }
}
