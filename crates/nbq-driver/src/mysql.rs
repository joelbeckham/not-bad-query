//! MySQL text-protocol sessions. A worker owns the socket while a result is open
//! and reads rows only in response to a fetch request.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arrow_array::builder::{BinaryBuilder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use mysql_async::consts::{ColumnFlags, ColumnType, StatusFlags};
use mysql_async::prelude::Queryable;
use mysql_async::{
    Column, Conn, Opts, OptsBuilder, QueryResult, Row, SslOpts, TextProtocol, Value,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{Instant, timeout, timeout_at};

use crate::{
    CancelHandle, ColumnKind, ConnectParams, Dialect, DriverError, ErrorKind, Execution,
    MAX_ROW_BYTES, MAX_VALUE_BYTES, Result, RowStream, Session, SslMode, TARGET_BATCH_BYTES, meta,
};

const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

fn mysql_error(error: mysql_async::Error) -> DriverError {
    if let mysql_async::Error::Server(error) = error {
        let kind = match error.code {
            1317 => ErrorKind::Cancelled,
            1053 | 1927 | 4031 => ErrorKind::Connection,
            _ => ErrorKind::Query,
        };
        let mut out = DriverError::new(kind, error.to_string());
        out.code = Some(error.code.to_string());
        out
    } else {
        DriverError::connection(error.to_string())
    }
}

fn options(params: ConnectParams) -> Result<Opts> {
    let ssl = match params.ssl {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        SslMode::VerifyFull => {
            static SYSTEM_ROOTS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
            let system = SYSTEM_ROOTS.get_or_init(|| {
                let mut roots = rustls::RootCertStore::empty();
                rustls_native_certs::load_native_certs()
                    .certs
                    .into_iter()
                    .filter(|cert| roots.add(cert.clone()).is_ok())
                    .map(|cert| cert.to_vec())
                    .collect()
            });
            let mut roots: Vec<_> = system.iter().cloned().map(Into::into).collect();
            if !params.ssl_root_cert.is_empty() {
                roots.extend(
                    crate::pg::validate_ca_bundle(&params.ssl_root_cert)?
                        .into_iter()
                        .map(|cert| cert.to_vec().into()),
                );
            }
            Some(SslOpts::default().with_root_certs(roots))
        }
    };
    Ok(OptsBuilder::default()
        .ip_or_hostname(params.host)
        .tcp_port(params.port)
        .user(Some(params.user))
        .pass(params.password)
        .db_name(Some(params.database).filter(|name| !name.is_empty()))
        .prefer_socket(false)
        .ssl_opts(ssl)
        .max_allowed_packet(Some(MAX_ROW_BYTES + 64 * 1024))
        .connect_attributes([("program_name".into(), params.application_name)].into())
        .into())
}

struct State {
    // Serialize the start of a statement with KILL QUERY so a late cancellation
    // cannot hit the next statement.
    gate: Mutex<()>,
    active: AtomicU64,
    in_transaction: AtomicBool,
}

struct MySqlCancel {
    opts: Opts,
    id: u32,
    state: Arc<State>,
    shutdown: watch::Sender<bool>,
}

#[async_trait]
impl CancelHandle for MySqlCancel {
    async fn cancel(&self) -> Result<()> {
        let generation = self.state.active.load(Ordering::Acquire);
        if generation == 0 {
            return Ok(());
        }
        let _guard = self.state.gate.lock().await;
        if self.state.active.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let killed = timeout(CANCEL_TIMEOUT, async {
            let mut conn = Conn::new(self.opts.clone()).await?;
            conn.query_drop(format!("KILL QUERY {}", self.id)).await
        })
        .await;
        match killed {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.close_connection();
                Err(mysql_error(error))
            }
            Err(_) => {
                self.close_connection();
                Err(DriverError::connection("MySQL cancellation timed out"))
            }
        }
    }

    fn close_connection(&self) {
        self.shutdown.send_replace(true);
    }
}

struct Statement {
    sql: String,
    reply: oneshot::Sender<Result<Execution>>,
}

pub(crate) struct MySqlSession {
    requests: mpsc::Sender<Statement>,
    cancel: Arc<MySqlCancel>,
}

impl MySqlSession {
    pub(crate) async fn connect(params: &ConnectParams) -> Result<Self> {
        let (mode, limit) = (params.ssl, params.connect_timeout);
        let params = params.clone();
        let opts = tokio::task::spawn_blocking(move || options(params))
            .await
            .map_err(|error| DriverError::connection(error.to_string()))??;
        let conn = timeout(limit, async {
            match Conn::new(opts.clone()).await {
                Err(mysql_async::Error::Driver(
                    mysql_async::DriverError::NoClientSslFlagFromServer,
                )) if mode == SslMode::Prefer => {
                    Conn::new(OptsBuilder::from_opts(opts).ssl_opts(None)).await
                }
                result => result,
            }
        })
        .await
        .map_err(|_| DriverError::connection("MySQL connection timed out"))?
        .map_err(|error| DriverError::connection(error.to_string()))?;
        let state = Arc::new(State {
            gate: Mutex::new(()),
            active: AtomicU64::new(0),
            in_transaction: AtomicBool::new(false),
        });
        let (shutdown, mut stop) = watch::channel(false);
        let (requests, mut commands) = mpsc::channel::<Statement>(1);
        let cancel = Arc::new(MySqlCancel {
            opts: conn.opts().clone(),
            id: conn.id(),
            state: state.clone(),
            shutdown,
        });
        tokio::spawn(async move {
            let mut conn = conn;
            tokio::select! {
                _ = stop.wait_for(|stop| *stop) => {}
                _ = run(&mut conn, &mut commands, &state) => {}
            }
            state.active.store(0, Ordering::Release);
            commands.close();
            let _ = timeout(CANCEL_TIMEOUT, conn.disconnect()).await;
        });
        Ok(Self { requests, cancel })
    }
}

impl Drop for MySqlSession {
    fn drop(&mut self) {
        self.cancel.close_connection();
    }
}

#[async_trait]
impl Session for MySqlSession {
    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }

    fn capabilities(&self) -> crate::Capabilities {
        // MySQL can implicitly commit DDL, which releases all savepoints.
        crate::Capabilities { savepoints: false }
    }

    fn can_cancel_in_transaction(&self) -> bool {
        true
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    fn is_closed(&self) -> bool {
        self.requests.is_closed() || *self.cancel.shutdown.borrow()
    }

    fn transaction_active(&self) -> Option<bool> {
        Some(self.cancel.state.in_transaction.load(Ordering::Acquire))
    }

    async fn execute(&mut self, sql: &str) -> Result<Execution> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(Statement {
                sql: sql.into(),
                reply,
            })
            .await
            .map_err(|_| closed())?;
        result.await.map_err(|_| closed())?
    }
}

fn closed() -> DriverError {
    DriverError::connection("MySQL connection closed")
}

async fn run(conn: &mut Conn, requests: &mut mpsc::Receiver<Statement>, state: &State) {
    let mut generation = 0;
    while let Some(statement) = requests.recv().await {
        {
            let _guard = state.gate.lock().await;
            generation += 1;
            state.active.store(generation, Ordering::Release);
        }
        let end = execute(conn, statement).await;
        state.active.store(0, Ordering::Release);
        let error = end.error();
        let fatal = error.is_some_and(|error| error.kind == ErrorKind::Connection)
            || conn.is_disconnected();
        // ERR packets carry no transaction status. Get a fresh OK packet to
        // detect deadlocks or administrator-configured full lock-timeout rollback.
        if !fatal && error.is_some() && state.in_transaction.load(Ordering::Acquire) {
            let _ = timeout(CANCEL_TIMEOUT, conn.query_drop("DO 0")).await;
        }
        if let Some(ok) = conn.last_ok_packet() {
            state.in_transaction.store(
                ok.status_flags()
                    .contains(StatusFlags::SERVER_STATUS_IN_TRANS),
                Ordering::Release,
            );
        }
        let closed = fatal || conn.is_disconnected();
        if closed {
            requests.close();
        }
        end.reply();
        if closed {
            return;
        }
    }
}

type Batch = Result<Option<RecordBatch>>;
struct Fetch {
    max_rows: usize,
    soft_deadline: Duration,
    reply: oneshot::Sender<Batch>,
}

struct Rows {
    schema: SchemaRef,
    requests: mpsc::Sender<Fetch>,
    done: bool,
}

#[async_trait]
impl RowStream for Rows {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn rows_affected(&self) -> Option<u64> {
        None
    }

    async fn next_batch(&mut self, max_rows: usize, soft_deadline: Duration) -> Batch {
        if self.done {
            return Ok(None);
        }
        let (reply, result) = oneshot::channel();
        self.requests
            .send(Fetch {
                max_rows,
                soft_deadline,
                reply,
            })
            .await
            .map_err(|_| closed())?;
        let batch = result.await.map_err(|_| closed())?;
        self.done = !matches!(batch, Ok(Some(_)));
        batch
    }
}

// Reply after updating the session's server transaction status.
enum End {
    Statement(oneshot::Sender<Result<Execution>>, Result<Execution>),
    Rows(Option<oneshot::Sender<Batch>>, Result<()>),
}

impl End {
    fn error(&self) -> Option<&DriverError> {
        match self {
            Self::Statement(_, result) => result.as_ref().err(),
            Self::Rows(_, result) => result.as_ref().err(),
        }
    }

    fn reply(self) {
        match self {
            Self::Statement(reply, result) => {
                let _ = reply.send(result);
            }
            Self::Rows(Some(reply), result) => {
                let _ = reply.send(result.map(|()| None));
            }
            Self::Rows(None, _) => {}
        }
    }
}

async fn execute(conn: &mut Conn, statement: Statement) -> End {
    let Statement { sql, reply } = statement;
    let mut result = match conn.query_iter(&sql).await {
        Ok(result) => result,
        Err(error) => return End::Statement(reply, Err(mysql_error(error))),
    };
    let columns = result.columns_ref();
    if columns.is_empty() {
        let rows_affected = Some(result.affected_rows());
        let command = nbq_sql::words(&sql, Dialect::MySql)
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let done = result
            .drop_result()
            .await
            .map_err(mysql_error)
            .map(|()| Execution::Done {
                rows_affected,
                command,
            });
        return End::Statement(reply, done);
    }
    if columns.len() > 1024 {
        // Abandon the session rather than consuming a potentially massive result.
        return End::Statement(
            reply,
            Err(DriverError::connection(
                "MySQL results with more than 1024 columns are not supported",
            )),
        );
    }
    let schema = Arc::new(Schema::new(columns.iter().map(field).collect::<Vec<_>>()));
    let (requests, mut fetches) = mpsc::channel(1);
    if reply
        .send(Ok(Execution::Rows(Box::new(Rows {
            schema: schema.clone(),
            requests,
            done: false,
        }))))
        .is_err()
    {
        return End::Rows(None, Err(closed()));
    }
    let (reply, fetched) = fetch_rows(&mut result, &mut fetches, &schema).await;
    // Finish other sets (such as the trailing OK from CALL) before allowing
    // another statement. A dropped result must settle within a bounded time.
    let finished = match (reply.is_some(), fetched) {
        (true, Ok(())) => result.drop_result().await.map_err(mysql_error),
        (_, fetched) => match timeout(CANCEL_TIMEOUT, result.drop_result()).await {
            Ok(Ok(())) => fetched,
            Ok(Err(error)) if !error.is_fatal() => fetched.and(Err(mysql_error(error))),
            Ok(Err(error)) => Err(mysql_error(error)),
            Err(_) => Err(DriverError::connection("MySQL result cleanup timed out")),
        },
    };
    End::Rows(reply, finished)
}

async fn fetch_rows(
    result: &mut QueryResult<'_, '_, TextProtocol>,
    requests: &mut mpsc::Receiver<Fetch>,
    schema: &SchemaRef,
) -> (Option<oneshot::Sender<Batch>>, Result<()>) {
    let mut stream = match result.stream::<Row>().await {
        Ok(Some(stream)) => stream,
        Ok(None) => return (requests.recv().await.map(|r| r.reply), Ok(())),
        Err(error) => {
            return (
                requests.recv().await.map(|r| r.reply),
                Err(mysql_error(error)),
            );
        }
    };
    let mut pending = None;
    while let Some(Fetch {
        max_rows,
        soft_deadline,
        reply,
    }) = requests.recv().await
    {
        match batch(&mut stream, &mut pending, schema, max_rows, soft_deadline).await {
            Ok(Some(batch)) => {
                if reply.send(Ok(Some(batch))).is_err() {
                    break;
                }
            }
            result => return (Some(reply), result.map(|_| ())),
        }
    }
    (None, Ok(()))
}

async fn batch(
    stream: &mut (impl Stream<Item = mysql_async::Result<Row>> + Unpin),
    pending: &mut Option<(Vec<Value>, usize)>,
    schema: &SchemaRef,
    max_rows: usize,
    soft_deadline: Duration,
) -> Batch {
    let deadline = Instant::now() + soft_deadline;
    let mut rows = Vec::new();
    let mut bytes = 0;
    for _ in 0..max_rows.max(1) {
        let (values, size) = if let Some(row) = pending.take() {
            row
        } else {
            let next = if rows.is_empty() {
                stream.next().await
            } else {
                match timeout_at(deadline, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => break,
                }
            };
            let Some(row) = next else { break };
            let values = row.map_err(mysql_error)?.unwrap();
            let size = row_bytes(&values)?;
            (values, size)
        };
        if !rows.is_empty() && bytes + size > TARGET_BATCH_BYTES {
            *pending = Some((values, size));
            break;
        }
        bytes += size;
        rows.push(values);
        if bytes >= TARGET_BATCH_BYTES || Instant::now() >= deadline {
            break;
        }
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let arrays = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, field)| array(&rows, i, field.data_type()))
        .collect();
    RecordBatch::try_new(schema.clone(), arrays)
        .map(Some)
        .map_err(|error| DriverError::new(ErrorKind::Query, error.to_string()))
}

fn row_bytes(values: &[Value]) -> Result<usize> {
    let mut size = values.len() * 8;
    for value in values {
        if let Value::Bytes(bytes) = value {
            if bytes.len() > MAX_VALUE_BYTES {
                return Err(DriverError::new(
                    ErrorKind::Query,
                    "MySQL value exceeds the 8 MiB limit",
                ));
            }
            size += bytes.len();
        }
    }
    if size > MAX_ROW_BYTES {
        return Err(DriverError::new(
            ErrorKind::Query,
            "MySQL row exceeds the 16 MiB limit",
        ));
    }
    Ok(size)
}

fn field(column: &Column) -> Field {
    use ColumnType::*;
    let ty = column.column_type();
    let kind = match ty {
        MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_DATE
        | MYSQL_TYPE_TIME
        | MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_NEWDATE
        | MYSQL_TYPE_TIMESTAMP2
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIME2 => ColumnKind::Temporal,
        MYSQL_TYPE_JSON => ColumnKind::Json,
        MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR => ColumnKind::Binary,
        ty if ty.is_numeric_type() || ty == MYSQL_TYPE_YEAR => ColumnKind::Number,
        ty if column.character_set() == 63
            && (ty.is_character_type()
                || matches!(
                    ty,
                    MYSQL_TYPE_TINY_BLOB
                        | MYSQL_TYPE_MEDIUM_BLOB
                        | MYSQL_TYPE_LONG_BLOB
                        | MYSQL_TYPE_BLOB
                )) =>
        {
            ColumnKind::Binary
        }
        _ => ColumnKind::Text,
    };
    let binary = kind == ColumnKind::Binary;
    let db_type = match ty {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_VECTOR => "vector",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_STRING if column.flags().contains(ColumnFlags::ENUM_FLAG) => "enum",
        MYSQL_TYPE_STRING if column.flags().contains(ColumnFlags::SET_FLAG) => "set",
        MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        MYSQL_TYPE_TINY_BLOB => {
            if binary {
                "tinyblob"
            } else {
                "tinytext"
            }
        }
        MYSQL_TYPE_MEDIUM_BLOB => {
            if binary {
                "mediumblob"
            } else {
                "mediumtext"
            }
        }
        MYSQL_TYPE_LONG_BLOB => {
            if binary {
                "longblob"
            } else {
                "longtext"
            }
        }
        MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        _ => "unknown",
    };
    let db_type =
        if kind == ColumnKind::Number && column.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
            format!("{db_type} unsigned")
        } else {
            db_type.to_string()
        };
    let data_type = if binary {
        DataType::Binary
    } else {
        DataType::Utf8
    };
    Field::new(column.name_str(), data_type, true).with_metadata([
        (meta::DB_TYPE, db_type),
        (meta::KIND, kind.as_str().to_string()),
    ])
}

fn array(rows: &[Vec<Value>], i: usize, data_type: &DataType) -> ArrayRef {
    if *data_type == DataType::Binary {
        let mut builder = BinaryBuilder::new();
        for row in rows {
            match &row[i] {
                Value::Bytes(bytes) => builder.append_value(bytes),
                Value::NULL => builder.append_null(),
                value => builder.append_value(value.as_sql(false)),
            }
        }
        Arc::new(builder.finish())
    } else {
        let mut builder = StringBuilder::new();
        for row in rows {
            match &row[i] {
                Value::Bytes(bytes) => builder.append_value(String::from_utf8_lossy(bytes)),
                Value::NULL => builder.append_null(),
                value => builder.append_value(value.as_sql(false)),
            }
        }
        Arc::new(builder.finish())
    }
}
