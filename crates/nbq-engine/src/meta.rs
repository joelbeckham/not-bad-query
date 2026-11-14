//! Database object introspection for the object tree and autocomplete.
//!
//! Runs on a dedicated metadata session per connection (never a tab's
//! session), so browsing objects never interferes with a tab's transaction
//! or a running query.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nbq_driver::{ConnectParams, DriverError, Execution, Session, display::cell_text};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::Event;

/// Where in the object tree to list children.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ObjPath {
    /// Schemas of the database.
    Root,
    /// Object categories of a schema (Tables, Views, ...), with counts.
    Schema(String),
    /// Objects of one kind in a schema.
    Folder { schema: String, kind: ObjKind },
    /// Children of a table/view: columns, indexes, triggers.
    Object { schema: String, name: String, kind: ObjKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjKind {
    Schema,
    Table,
    View,
    MatView,
    Foreign,
    Function,
    Sequence,
    Column,
    Index,
    Trigger,
}

impl ObjKind {
    pub fn plural(self) -> &'static str {
        match self {
            ObjKind::Schema => "Schemas",
            ObjKind::Table => "Tables",
            ObjKind::View => "Views",
            ObjKind::MatView => "Materialized Views",
            ObjKind::Foreign => "Foreign Tables",
            ObjKind::Function => "Functions",
            ObjKind::Sequence => "Sequences",
            ObjKind::Column => "Columns",
            ObjKind::Index => "Indexes",
            ObjKind::Trigger => "Triggers",
        }
    }
    fn relkind(self) -> Option<&'static str> {
        Some(match self {
            ObjKind::Table => "'r','p'",
            ObjKind::View => "'v'",
            ObjKind::MatView => "'m'",
            ObjKind::Foreign => "'f'",
            ObjKind::Sequence => "'S'",
            _ => return None,
        })
    }
    /// Kinds that are listed in a schema.
    pub const IN_SCHEMA: [ObjKind; 6] =
        [ObjKind::Table, ObjKind::View, ObjKind::MatView, ObjKind::Foreign, ObjKind::Function, ObjKind::Sequence];
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjItem {
    pub name: String,
    pub kind: ObjKind,
    /// Secondary text: a column's type, a folder's count, an index definition.
    pub detail: Option<String>,
    /// Column is (part of) the primary key / index is unique.
    pub key: bool,
    /// The item can be expanded.
    pub has_children: bool,
    /// For folder entries: the path to expand.
    pub path: Option<ObjPath>,
}

/// One relation for autocomplete.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTable {
    pub schema: String,
    pub name: String,
    pub kind: ObjKind,
    pub columns: Vec<(String, String)>,
}

pub(crate) fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(crate) fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

async fn strings(s: &mut dyn Session, sql: &str) -> Result<Vec<Vec<Option<String>>>, DriverError> {
    let mut out = Vec::new();
    match s.execute(sql).await? {
        Execution::Done { .. } => {}
        Execution::Rows(mut st) => {
            while let Some(b) = st.next_batch(10_000, Duration::from_secs(5)).await? {
                for r in 0..b.num_rows() {
                    out.push(
                        b.columns()
                            .iter()
                            .map(|c| {
                                let mut t = String::new();
                                cell_text(c, r, &mut t).then_some(t)
                            })
                            .collect(),
                    );
                }
            }
        }
    }
    Ok(out)
}

fn col(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

pub async fn list(s: &mut dyn Session, path: &ObjPath) -> Result<Vec<ObjItem>, DriverError> {
    let item = |name: String, kind, detail: Option<String>, has_children, path| ObjItem {
        name,
        kind,
        detail,
        key: false,
        has_children,
        path,
    };
    match path {
        ObjPath::Root => {
            let rows = strings(
                s,
                "select nspname from pg_namespace \
                 where nspname !~ '^pg_' and nspname <> 'information_schema' order by nspname",
            )
            .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let n = col(r, 0);
                    item(n.clone(), ObjKind::Schema, None, true, Some(ObjPath::Schema(n)))
                })
                .collect())
        }
        ObjPath::Schema(schema) => {
            let q = quote_literal(schema);
            let rows = strings(
                s,
                &format!(
                    "select c.relkind::text, count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace \
                     where n.nspname = {q} and c.relkind in ('r','p','v','m','f','S') and not c.relispartition \
                     group by 1 \
                     union all \
                     select 'fn', count(*) from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
                     where n.nspname = {q} and p.prokind in ('f','p')"
                ),
            )
            .await?;
            let count = |k: ObjKind| -> u64 {
                let codes: &[&str] = match k {
                    ObjKind::Table => &["r", "p"],
                    ObjKind::View => &["v"],
                    ObjKind::MatView => &["m"],
                    ObjKind::Foreign => &["f"],
                    ObjKind::Sequence => &["S"],
                    ObjKind::Function => &["fn"],
                    _ => &[],
                };
                rows.iter()
                    .filter(|r| codes.contains(&col(r, 0).as_str()))
                    .map(|r| col(r, 1).parse::<u64>().unwrap_or(0))
                    .sum()
            };
            Ok(ObjKind::IN_SCHEMA
                .iter()
                .filter_map(|&k| {
                    let n = count(k);
                    // Always show Tables and Views; hide other empty categories.
                    (n > 0 || matches!(k, ObjKind::Table | ObjKind::View)).then(|| {
                        item(
                            k.plural().to_string(),
                            k,
                            Some(n.to_string()),
                            n > 0,
                            Some(ObjPath::Folder { schema: schema.clone(), kind: k }),
                        )
                    })
                })
                .collect())
        }
        ObjPath::Folder { schema, kind } => {
            let q = quote_literal(schema);
            if *kind == ObjKind::Function {
                let rows = strings(
                    s,
                    &format!(
                        "select p.proname, pg_get_function_identity_arguments(p.oid), \
                         case p.prokind when 'p' then 'procedure' else pg_get_function_result(p.oid) end \
                         from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
                         where n.nspname = {q} and p.prokind in ('f','p') order by 1, 2"
                    ),
                )
                .await?;
                return Ok(rows
                    .iter()
                    .map(|r| item(format!("{}({})", col(r, 0), col(r, 1)), ObjKind::Function, r[2].clone(), false, None))
                    .collect());
            }
            let Some(rk) = kind.relkind() else { return Ok(Vec::new()) };
            let rows = strings(
                s,
                &format!(
                    "select c.relname, case when c.reltuples >= 0 then c.reltuples::bigint end \
                     from pg_class c join pg_namespace n on n.oid = c.relnamespace \
                     where n.nspname = {q} and c.relkind in ({rk}) and not c.relispartition order by 1"
                ),
            )
            .await?;
            let expandable = !matches!(kind, ObjKind::Sequence);
            Ok(rows
                .iter()
                .map(|r| {
                    let name = col(r, 0);
                    // Row estimate for tables, e.g. "~1.0M rows".
                    let detail = matches!(kind, ObjKind::Table | ObjKind::MatView)
                        .then(|| r[1].as_deref().and_then(|n| n.parse::<u64>().ok()).map(approx_rows))
                        .flatten();
                    let path = expandable.then(|| ObjPath::Object { schema: schema.clone(), name: name.clone(), kind: *kind });
                    item(name, *kind, detail, expandable, path)
                })
                .collect())
        }
        ObjPath::Object { schema, name, kind } => {
            let rel = quote_literal(&format!("{}.{}", quote_ident(schema), quote_ident(name)));
            let cols = strings(
                s,
                &format!(
                    "select a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                     coalesce((select true from pg_index i where i.indrelid = a.attrelid and i.indisprimary \
                               and a.attnum = any(i.indkey)), false) \
                     from pg_attribute a where a.attrelid = {rel}::regclass and a.attnum > 0 \
                     and not a.attisdropped order by a.attnum"
                ),
            )
            .await?;
            let mut out: Vec<ObjItem> = cols
                .iter()
                .map(|r| {
                    let not_null = col(r, 2) == "true";
                    let ty = col(r, 1);
                    ObjItem {
                        name: col(r, 0),
                        kind: ObjKind::Column,
                        detail: Some(if not_null { ty } else { format!("{ty}?") }),
                        key: col(r, 3) == "true",
                        has_children: false,
                        path: None,
                    }
                })
                .collect();
            if matches!(kind, ObjKind::Table | ObjKind::MatView) {
                let idx = strings(
                    s,
                    &format!(
                        "select c.relname, i.indisunique, pg_get_indexdef(i.indexrelid) from pg_index i \
                         join pg_class c on c.oid = i.indexrelid where i.indrelid = {rel}::regclass order by 1"
                    ),
                )
                .await?;
                out.extend(idx.iter().map(|r| ObjItem {
                    name: col(r, 0),
                    kind: ObjKind::Index,
                    detail: r[2].clone(),
                    key: col(r, 1) == "true",
                    has_children: false,
                    path: None,
                }));
                let trg = strings(
                    s,
                    &format!(
                        "select tgname, pg_get_triggerdef(oid) from pg_trigger \
                         where tgrelid = {rel}::regclass and not tgisinternal order by 1"
                    ),
                )
                .await?;
                out.extend(trg.iter().map(|r| item(col(r, 0), ObjKind::Trigger, r[1].clone(), false, None)));
            }
            Ok(out)
        }
    }
}

fn approx_rows(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} rows"),
        1_000..1_000_000 => format!("~{:.1}K rows", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("~{:.1}M rows", n as f64 / 1e6),
        _ => format!("~{:.1}B rows", n as f64 / 1e9),
    }
}

/// Every user relation with its columns, for autocomplete.
pub async fn catalog(s: &mut dyn Session) -> Result<Vec<CatalogTable>, DriverError> {
    let rows = strings(
        s,
        "select n.nspname, c.relname, c.relkind::text, a.attname, format_type(a.atttypid, a.atttypmod) \
         from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         left join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped \
         where c.relkind in ('r','p','v','m','f') and not c.relispartition \
         and n.nspname !~ '^pg_' and n.nspname <> 'information_schema' \
         order by 1, 2, a.attnum",
    )
    .await?;
    let mut out: Vec<CatalogTable> = Vec::new();
    for r in rows {
        let (schema, name) = (col(&r, 0), col(&r, 1));
        if out.last().is_none_or(|t| t.schema != schema || t.name != name) {
            let kind = match col(&r, 2).as_str() {
                "v" => ObjKind::View,
                "m" => ObjKind::MatView,
                "f" => ObjKind::Foreign,
                _ => ObjKind::Table,
            };
            out.push(CatalogTable { schema, name, kind, columns: Vec::new() });
        }
        if let Some(c) = r[3].clone() {
            out.last_mut().unwrap().columns.push((c, col(&r, 4)));
        }
    }
    Ok(out)
}

/// A per-connection metadata session, connected lazily and reconnected on failure.
pub(crate) struct MetaConn {
    params: ConnectParams,
    session: AsyncMutex<Option<Box<dyn Session>>>,
}

impl MetaConn {
    pub fn new(mut params: ConnectParams) -> Arc<Self> {
        params.application_name = format!("{} (metadata)", params.application_name);
        Arc::new(MetaConn { params, session: AsyncMutex::new(None) })
    }

    pub fn same_params(&self, p: &ConnectParams) -> bool {
        self.params.host == p.host
            && self.params.port == p.port
            && self.params.database == p.database
            && self.params.user == p.user
            && self.params.password == p.password
            && self.params.kind == p.kind
    }

    pub async fn list(&self, path: &ObjPath) -> Result<Vec<ObjItem>, DriverError> {
        let mut g = self.session.lock().await;
        if g.as_ref().is_none_or(|s| s.is_closed()) {
            *g = Some(nbq_driver::connect(&self.params).await?);
        }
        let r = list(&mut **g.as_mut().unwrap(), path).await;
        if r.is_err() && g.as_ref().is_some_and(|s| s.is_closed()) {
            *g = None;
        }
        r
    }

    pub async fn catalog(&self) -> Result<Vec<CatalogTable>, DriverError> {
        let mut g = self.session.lock().await;
        if g.as_ref().is_none_or(|s| s.is_closed()) {
            *g = Some(nbq_driver::connect(&self.params).await?);
        }
        let r = catalog(&mut **g.as_mut().unwrap()).await;
        if r.is_err() && g.as_ref().is_some_and(|s| s.is_closed()) {
            *g = None;
        }
        r
    }
}

#[derive(Default)]
pub(crate) struct MetaPool {
    conns: parking_lot::Mutex<HashMap<String, Arc<MetaConn>>>,
}

impl MetaPool {
    pub fn get(&self, conn_id: &str, params: &ConnectParams) -> Arc<MetaConn> {
        let mut g = self.conns.lock();
        match g.get(conn_id) {
            Some(c) if c.same_params(params) => c.clone(),
            _ => {
                let c = MetaConn::new(params.clone());
                g.insert(conn_id.to_string(), c.clone());
                c
            }
        }
    }

    pub fn forget(&self, conn_id: &str) {
        self.conns.lock().remove(conn_id);
    }
}

pub(crate) fn send(events: &mpsc::UnboundedSender<Event>, e: Event) {
    let _ = events.send(e);
}
