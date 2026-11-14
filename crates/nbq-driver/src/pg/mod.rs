//! Postgres and Redshift (which speaks the Postgres wire protocol).
//!
//! Pausing: a `tokio_postgres::RowStream` that is not polled makes the
//! connection task stop reading the socket, so the server blocks on send.
//! That gives a server-side pause without cursors or transactions.

mod decode;
mod tls;

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float32Builder, Float64Builder, Int16Builder, Int32Builder, Int64Builder,
    StringBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use futures::StreamExt;
use tokio_postgres::types::{FromSql, ToSql, Type};
use tokio_postgres::{CancelToken, Client, Statement};

use crate::{
    Capabilities, CancelHandle, ConnectParams, DbKind, Dialect, DriverError, ErrorKind, Execution, Result, RowStream,
    Session, SslMode, meta,
};
pub use decode::{Plan, format_value, kind_of, plan};
pub use tls::validate_ca_bundle;

pub struct PgSession {
    client: Client,
    kind: DbKind,
    cancel: Arc<PgCancel>,
}

struct PgCancel {
    token: CancelToken,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
}

#[async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> Result<()> {
        let r = match &self.tls {
            Some(tls) => self.token.cancel_query(tls.clone()).await,
            None => self.token.cancel_query(tokio_postgres::NoTls).await,
        };
        r.map_err(|e| DriverError::connection(format!("cancel failed: {e}")))
    }
}

impl PgSession {
    pub async fn connect(p: &ConnectParams) -> Result<Self> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&p.host)
            .port(p.port)
            .dbname(&p.database)
            .user(&p.user)
            .application_name(&p.application_name)
            .connect_timeout(p.connect_timeout)
            .ssl_mode(match p.ssl {
                SslMode::Disable => tokio_postgres::config::SslMode::Disable,
                SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
                SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
            });
        if let Some(pw) = &p.password {
            cfg.password(pw);
        }
        let connector = if p.ssl == SslMode::Disable { None } else { Some(tls::make_tls(p)?) };
        let connect = async {
            if p.ssl == SslMode::Disable {
                let (client, conn) = cfg.connect(tokio_postgres::NoTls).await?;
                spawn_conn(conn);
                Ok::<_, tokio_postgres::Error>((client, None))
            } else {
                let tls = connector.expect("TLS enabled");
                let (client, conn) = cfg.connect(tls.clone()).await?;
                spawn_conn(conn);
                Ok((client, Some(tls)))
            }
        };
        let (client, tls) = tokio::time::timeout(p.connect_timeout + Duration::from_secs(1), connect)
            .await
            .map_err(|_| DriverError::connection("connection timed out"))?
            .map_err(|e| {
                let mut d = map_err(e);
                d.kind = ErrorKind::Connection;
                d
            })?;
        let cancel = Arc::new(PgCancel { token: client.cancel_token(), tls });
        Ok(PgSession { client, kind: p.kind, cancel })
    }
}

fn spawn_conn<F>(conn: F)
where
    F: std::future::Future<Output = std::result::Result<(), tokio_postgres::Error>> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("postgres connection closed: {e}");
        }
    });
}

pub(crate) fn map_err(e: tokio_postgres::Error) -> DriverError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code().to_string();
        let kind = sqlstate_kind(&code);
        return DriverError {
            message: db.message().to_string(),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
            detail: db.detail().map(str::to_string),
            hint: db.hint().map(str::to_string),
            code: Some(code),
            kind,
        };
    }
    let kind = if e.is_closed() { ErrorKind::Connection } else { ErrorKind::Query };
    // tokio-postgres error Display is terse; include the source chain.
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    DriverError::new(kind, msg)
}

fn sqlstate_kind(code: &str) -> ErrorKind {
    if code == "57014" { ErrorKind::Cancelled }
    else if code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03") { ErrorKind::Connection }
    else { ErrorKind::Query }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    #[test]
    fn fatal_server_errors_require_a_new_session() {
        for code in ["08006", "08003", "57P01", "57P02", "57P03"] { assert_eq!(sqlstate_kind(code), ErrorKind::Connection); }
        assert_eq!(sqlstate_kind("57014"), ErrorKind::Cancelled);
        assert_eq!(sqlstate_kind("23505"), ErrorKind::Query);
    }
}

fn no_params() -> impl ExactSizeIterator<Item = &'static (dyn ToSql + Sync)> {
    std::iter::empty()
}

fn command_tag(sql: &str, d: Dialect) -> String {
    let w: Vec<String> = nbq_sql::words(sql, d).take(2).map(str::to_ascii_uppercase).collect();
    match w.first().map(String::as_str) {
        // DML: the verb alone ("UPDATE", not "UPDATE SHOP").
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE" | "SELECT" | "COPY" | "CALL" | "TRUNCATE") => w[0].clone(),
        _ => w.join(" "),
    }
}

#[async_trait]
impl Session for PgSession {
    fn dialect(&self) -> Dialect {
        self.kind.dialect()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { savepoints: self.kind == DbKind::Postgres }
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    async fn execute(&mut self, sql: &str) -> Result<Execution> {
        let stmt = self.client.prepare(sql).await.map_err(map_err)?;
        if stmt.columns().len() > 1024 {
            return Err(DriverError::new(ErrorKind::Query, "Results with more than 1024 columns are not supported"));
        }
        if stmt.columns().is_empty() {
            let n = self.client.execute(&stmt, &[]).await.map_err(map_err)?;
            return Ok(Execution::Done { rows_affected: Some(n), command: command_tag(sql, self.dialect()) });
        }
        let stream = self.client.query_raw(&stmt, no_params()).await.map_err(map_err)?;
        Ok(Execution::Rows(Box::new(PgRowStream::new(stmt, stream))))
    }
}

pub struct PgRowStream {
    _stmt: Statement,
    stream: Pin<Box<tokio_postgres::RowStream>>,
    schema: SchemaRef,
    plans: Vec<Plan>,
    done: bool,
    rows_affected: Option<u64>,
}

impl PgRowStream {
    fn new(stmt: Statement, stream: tokio_postgres::RowStream) -> Self {
        let mut fields = Vec::new();
        let mut plans = Vec::new();
        for c in stmt.columns() {
            let p = plan(c.type_());
            let dt = match &p {
                Plan::Bool => DataType::Boolean,
                Plan::I16 => DataType::Int16,
                Plan::I32 => DataType::Int32,
                Plan::I64 => DataType::Int64,
                Plan::F32 => DataType::Float32,
                Plan::F64 => DataType::Float64,
                Plan::Bytes => DataType::Binary,
                Plan::Text(_) => DataType::Utf8,
            };
            let md = [
                (meta::DB_TYPE.to_string(), c.type_().name().to_string()),
                (meta::KIND.to_string(), kind_of(c.type_()).as_str().to_string()),
            ];
            fields.push(Field::new(c.name(), dt, true).with_metadata(md.into_iter().collect::<std::collections::HashMap<_, _>>()));
            plans.push(p);
        }
        PgRowStream {
            _stmt: stmt,
            stream: Box::pin(stream),
            schema: Arc::new(Schema::new(fields)),
            plans,
            done: false,
            rows_affected: None,
        }
    }
}

/// Raw wire bytes of one cell.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }
    fn from_sql_null(_: &Type) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(None))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

enum Col {
    Bool(BooleanBuilder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Bytes(BinaryBuilder),
    Text(StringBuilder, Type),
}

impl Col {
    fn new(p: &Plan, cap: usize) -> Col {
        match p {
            Plan::Bool => Col::Bool(BooleanBuilder::with_capacity(cap)),
            Plan::I16 => Col::I16(Int16Builder::with_capacity(cap)),
            Plan::I32 => Col::I32(Int32Builder::with_capacity(cap)),
            Plan::I64 => Col::I64(Int64Builder::with_capacity(cap)),
            Plan::F32 => Col::F32(Float32Builder::with_capacity(cap)),
            Plan::F64 => Col::F64(Float64Builder::with_capacity(cap)),
            Plan::Bytes => Col::Bytes(BinaryBuilder::with_capacity(cap, cap * 16)),
            Plan::Text(t) => Col::Text(StringBuilder::with_capacity(cap, cap * 16), t.clone()),
        }
    }

    fn push(&mut self, raw: Option<&[u8]>, scratch: &mut String) -> Result<usize> {
        let mut bytes = raw.map_or(1, |r| r.len());
        macro_rules! fixed {
            ($b:expr, $t:ty) => {
                match raw.and_then(|r| <[u8; size_of::<$t>()]>::try_from(r).ok()) {
                    Some(a) => $b.append_value(<$t>::from_be_bytes(a)),
                    None => $b.append_null(),
                }
            };
        }
        match self {
            Col::Bool(b) => match raw {
                Some(r) if !r.is_empty() => b.append_value(r[0] != 0),
                _ => b.append_null(),
            },
            Col::I16(b) => fixed!(b, i16),
            Col::I32(b) => fixed!(b, i32),
            Col::I64(b) => fixed!(b, i64),
            Col::F32(b) => fixed!(b, f32),
            Col::F64(b) => fixed!(b, f64),
            Col::Bytes(b) => match raw {
                Some(r) => b.append_value(r),
                None => b.append_null(),
            },
            Col::Text(b, ty) => match raw {
                Some(r) => {
                    scratch.clear();
                    format_value(ty, r, scratch);
                    bytes = scratch.len();
                    if bytes > crate::MAX_VALUE_BYTES {
                        return Err(DriverError::new(ErrorKind::Query, "A formatted value exceeds the 8 MiB result limit"));
                    }
                    b.append_value(&*scratch);
                }
                None => b.append_null(),
            },
        }
        Ok(bytes)
    }

    fn finish(self) -> ArrayRef {
        match self {
            Col::Bool(mut b) => Arc::new(b.finish()),
            Col::I16(mut b) => Arc::new(b.finish()),
            Col::I32(mut b) => Arc::new(b.finish()),
            Col::I64(mut b) => Arc::new(b.finish()),
            Col::F32(mut b) => Arc::new(b.finish()),
            Col::F64(mut b) => Arc::new(b.finish()),
            Col::Bytes(mut b) => Arc::new(b.finish()),
            Col::Text(mut b, _) => Arc::new(b.finish()),
        }
    }
}

#[async_trait]
impl RowStream for PgRowStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    async fn next_batch(&mut self, max_rows: usize, soft_deadline: Duration) -> Result<Option<RecordBatch>> {
        if self.done {
            return Ok(None);
        }
        let cap = max_rows.min(1024);
        let mut cols: Vec<Col> = self.plans.iter().map(|p| Col::new(p, cap)).collect();
        let mut scratch = String::new();
        let deadline = Instant::now() + soft_deadline;
        let mut n = 0;
        let mut bytes = 0;
        while n < max_rows {
            let next = if n == 0 {
                self.stream.next().await
            } else {
                // `StreamExt::next` is cancel-safe, so timing out loses nothing.
                match tokio::time::timeout_at(deadline.into(), self.stream.next()).await {
                    Ok(x) => x,
                    Err(_) => break,
                }
            };
            match next {
                None => {
                    self.done = true;
                    self.rows_affected = self.stream.rows_affected();
                    break;
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Err(map_err(e));
                }
                Some(Ok(row)) => {
                    let mut wire_bytes = 0usize;
                    for i in 0..cols.len() {
                        let Raw(raw) = row.try_get::<_, Raw>(i).map_err(map_err)?;
                        let len = raw.map_or(1, |r| r.len());
                        if len > crate::MAX_VALUE_BYTES {
                            return Err(DriverError::new(ErrorKind::Query, "A value exceeds the 8 MiB result limit"));
                        }
                        wire_bytes += len;
                    }
                    if wire_bytes > crate::MAX_ROW_BYTES {
                        return Err(DriverError::new(ErrorKind::Query, "A row exceeds the 16 MiB result limit"));
                    }
                    let mut row_bytes = 0;
                    for (i, c) in cols.iter_mut().enumerate() {
                        let Raw(raw) = row.try_get::<_, Raw>(i).map_err(map_err)?;
                        row_bytes += c.push(raw, &mut scratch)?;
                        if row_bytes > crate::MAX_ROW_BYTES {
                            return Err(DriverError::new(ErrorKind::Query, "A formatted row exceeds the 16 MiB result limit"));
                        }
                    }
                    n += 1;
                    bytes += row_bytes;
                    if bytes >= crate::TARGET_BATCH_BYTES { break; }
                }
            }
        }
        if n == 0 {
            return Ok(None);
        }
        let arrays = cols.into_iter().map(Col::finish).collect();
        RecordBatch::try_new(self.schema.clone(), arrays)
            .map(Some)
            .map_err(|e| DriverError::new(ErrorKind::Query, format!("building batch: {e}")))
    }
}
