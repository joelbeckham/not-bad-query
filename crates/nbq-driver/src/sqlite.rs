//! SQLite runs on a dedicated blocking worker. The worker holds a prepared
//! statement only while its pull-based stream is alive; no rows are prefetched.
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use arrow_array::{ArrayRef, RecordBatch, builder::StringBuilder};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use tokio::sync::oneshot;

use crate::{
    CancelHandle, Capabilities, ColumnKind, ConnectParams, Dialect, DriverError, ErrorKind,
    Execution, Result, RowStream, meta,
};

fn error(e: rusqlite::Error) -> DriverError {
    let kind = if matches!(&e, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::OperationInterrupted)
    {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Query
    };
    let mut out = DriverError::new(kind, e.to_string());
    if let rusqlite::Error::SqliteFailure(e, _) = e {
        out.code = Some(e.extended_code.to_string());
    }
    out
}

fn open(path: &Path) -> Result<Connection> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(DriverError::connection(
            "Choose an absolute database file path",
        ));
    }
    // No CREATE and no URI flag: filenames are literal and missing files fail.
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| DriverError::connection(e.to_string()))?;
    db.busy_timeout(Duration::from_secs(2)).map_err(error)?;
    db.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
        .map_err(|e| DriverError::connection(e.to_string()))?;
    db.pragma_update(None, "foreign_keys", true)
        .map_err(error)?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
        crate::MAX_ROW_BYTES as i32,
    )
    .map_err(error)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_COLUMN, 1024)
        .map_err(error)?;
    Ok(db)
}

pub fn create_database(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(DriverError::connection(
            "Choose an absolute database file path",
        ));
    }
    // Reserve the name atomically, even if another process creates it after the picker.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            DriverError::connection(format!("Could not create {}: {e}", path.display()))
        })?;
    drop(file);
    let result = (|| {
        let db = open(path)?;
        db.execute_batch("VACUUM").map_err(error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

struct Cancel {
    interrupt: rusqlite::InterruptHandle,
    cancelled: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    in_transaction: AtomicBool,
}

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        self.interrupt.interrupt();
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

pub struct SqliteSession {
    commands: mpsc::Sender<Execute>,
    cancel: Arc<Cancel>,
}

impl SqliteSession {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let path = params.file_path.clone();
        let (ready, rx) = oneshot::channel();
        let (commands, work) = mpsc::channel::<Execute>();
        std::thread::Builder::new()
            .name("nbq-sqlite".into())
            .spawn(move || {
                let mut db = match open(&path) {
                    Ok(db) => db,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let cancel = Arc::new(Cancel {
                    interrupt: db.get_interrupt_handle(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    closed: Arc::new(AtomicBool::new(false)),
                    in_transaction: AtomicBool::new(false),
                });
                let cancelled = cancel.cancelled.clone();
                if let Err(e) =
                    db.progress_handler(1000, Some(move || cancelled.load(Ordering::Acquire)))
                {
                    let _ = ready.send(Err(error(e)));
                    return;
                }
                if ready.send(Ok(cancel.clone())).is_err() {
                    return;
                }
                while let Ok((sql, reply)) = work.recv() {
                    if cancel.closed.load(Ordering::Acquire) {
                        break;
                    }
                    // Reset here, after the old stream was dropped and finalized.
                    cancel.cancelled.store(false, Ordering::Release);
                    run(&db, &sql, reply, &cancel);
                }
                cancel.closed.store(true, Ordering::Release);
            })
            .map_err(|e| DriverError::connection(e.to_string()))?;
        let cancel = rx
            .await
            .map_err(|_| DriverError::connection("SQLite worker stopped"))??;
        Ok(Self { commands, cancel })
    }
}

impl Drop for SqliteSession {
    fn drop(&mut self) {
        self.cancel.close_connection();
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { savepoints: true }
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
        let (tx, rx) = oneshot::channel();
        self.commands
            .send((sql.to_owned(), tx))
            .map_err(|_| DriverError::connection("SQLite worker stopped"))?;
        rx.await
            .map_err(|_| DriverError::connection("SQLite worker stopped"))?
    }
}

use crate::Session;

struct Stream {
    schema: SchemaRef,
    pull: mpsc::Sender<Pull>,
    done: bool,
    affected: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl RowStream for Stream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn rows_affected(&self) -> Option<u64> {
        let n = self.affected.load(Ordering::Acquire);
        (n != u64::MAX).then_some(n)
    }
    async fn next_batch(&mut self, max: usize, deadline: Duration) -> Result<Option<RecordBatch>> {
        if self.done {
            return Ok(None);
        }
        let (tx, rx) = oneshot::channel();
        self.pull
            .send((max.max(1), deadline, tx))
            .map_err(|_| DriverError::connection("SQLite stream stopped"))?;
        let result = rx
            .await
            .map_err(|_| DriverError::connection("SQLite worker stopped"))?;
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result
    }
}

fn run(db: &Connection, sql: &str, reply: oneshot::Sender<Result<Execution>>, cancel: &Cancel) {
    let mut stmt = match db.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(Err(error(e)));
            return;
        }
    };
    if stmt.column_count() == 0 {
        let result = stmt
            .execute([])
            .map(|n| Execution::Done {
                rows_affected: Some(n as u64),
                command: sql.split_whitespace().next().unwrap_or("").to_uppercase(),
            })
            .map_err(error);
        cancel
            .in_transaction
            .store(!db.is_autocommit(), Ordering::Release);
        let _ = reply.send(result);
        return;
    }
    let readonly = stmt.readonly();
    // SQLite values can change storage class on every row, even in declared
    // columns. Stable text Arrow columns preserve all values without coercion.
    let schema = Arc::new(Schema::new(
        stmt.columns()
            .map(|c| {
                let ty = c.decl_type().unwrap_or("dynamic");
                let upper = ty.to_ascii_uppercase();
                let kind = if ["INT", "REAL", "FLOA", "DOUB", "NUM", "DEC"]
                    .iter()
                    .any(|t| upper.contains(t))
                {
                    ColumnKind::Number
                } else {
                    ColumnKind::Text
                };
                Field::new(c.name(), DataType::Utf8, true).with_metadata(
                    std::collections::HashMap::<String, String>::from([
                        (meta::DB_TYPE.into(), ty.into()),
                        (meta::KIND.into(), kind.as_str().into()),
                    ]),
                )
            })
            .collect::<Vec<_>>(),
    ));
    let (pull, requests) = mpsc::channel::<Pull>();
    let affected = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    let stream = Stream {
        schema: schema.clone(),
        pull,
        done: false,
        affected: affected.clone(),
    };
    let mut rows = match stmt.query([]) {
        Ok(rows) => rows,
        Err(e) => {
            let _ = reply.send(Err(error(e)));
            return;
        }
    };
    if reply.send(Ok(Execution::Rows(Box::new(stream)))).is_err() {
        return;
    }
    loop {
        if cancel.closed.load(Ordering::Acquire) {
            break;
        }
        let (max, deadline, reply) = match requests.recv_timeout(Duration::from_millis(100)) {
            Ok(pull) => pull,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let started = Instant::now();
        let mut builders: Vec<_> = schema
            .fields()
            .iter()
            .map(|_| StringBuilder::new())
            .collect();
        let mut count = 0;
        let mut bytes = 0;
        let mut ended = false;
        let batch = (|| -> Result<Option<RecordBatch>> {
            while count < max {
                if cancel.cancelled.load(Ordering::Acquire) {
                    return Err(DriverError::new(ErrorKind::Cancelled, "Query cancelled"));
                }
                let Some(row) = rows.next().map_err(error)? else {
                    ended = true;
                    break;
                };
                let mut row_bytes = 0;
                for (i, builder) in builders.iter_mut().enumerate() {
                    let value = row.get_ref(i).map_err(error)?;
                    let raw_len = match value {
                        ValueRef::Blob(v) | ValueRef::Text(v) => v.len(),
                        _ => 8,
                    };
                    if raw_len > crate::MAX_VALUE_BYTES {
                        return Err(DriverError::new(
                            ErrorKind::Query,
                            "SQLite value exceeds 8 MiB",
                        ));
                    }
                    let text = match value {
                        ValueRef::Null => {
                            builder.append_null();
                            continue;
                        }
                        ValueRef::Integer(v) => v.to_string(),
                        ValueRef::Real(v) => v.to_string(),
                        ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                        ValueRef::Blob(v) => {
                            let mut s = String::new();
                            crate::display::push_hex(v, &mut s);
                            s
                        }
                    };
                    row_bytes += text.len();
                    if row_bytes > crate::MAX_ROW_BYTES {
                        return Err(DriverError::new(
                            ErrorKind::Query,
                            "SQLite row exceeds 16 MiB",
                        ));
                    }
                    builder.append_value(text);
                }
                bytes += row_bytes;
                count += 1;
                if bytes >= crate::TARGET_BATCH_BYTES || started.elapsed() >= deadline {
                    break;
                }
            }
            if count == 0 {
                return Ok(None);
            }
            let arrays: Vec<ArrayRef> = builders
                .iter_mut()
                .map(|b| Arc::new(b.finish()) as ArrayRef)
                .collect();
            RecordBatch::try_new(schema.clone(), arrays)
                .map(Some)
                .map_err(|e| DriverError::new(ErrorKind::Query, e.to_string()))
        })();
        let failed = batch.is_err();
        cancel
            .in_transaction
            .store(!db.is_autocommit(), Ordering::Release);
        if ended && !readonly {
            affected.store(db.changes(), Ordering::Release);
        }
        if reply.send(batch).is_err() || failed {
            break;
        }
        if ended {
            // Deliver EOF after the final partial batch.
            if count > 0 {
                while !cancel.closed.load(Ordering::Acquire) {
                    match requests.recv_timeout(Duration::from_millis(100)) {
                        Ok((_, _, reply)) => {
                            let _ = reply.send(Ok(None));
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    }
                }
            }
            break;
        }
    }
}
