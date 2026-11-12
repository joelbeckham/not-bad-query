//! The headless engine.
//!
//! The UI talks to the engine only through the non-blocking methods on
//! [`Engine`] (commands in) and the [`Event`] channel (events out). The
//! engine owns a tokio runtime; each query tab gets an actor task that owns
//! its database session, and each result set gets a fetch task that streams
//! rows into a [`Spool`] on demand.

mod fetch;
pub mod meta;
mod tab;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gq_driver::{ConnectParams, DriverError};
pub use gq_results::{CsvOptions, Progress, Spool, Status as SpoolStatus};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use meta::{CatalogTable, MetaPool, ObjItem, ObjPath};
use tab::{Cmd, TabShared};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub u64);

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Where result spools live. Cleared of stale spools at startup.
    pub spool_dir: PathBuf,
    /// Rows fetched immediately when a result opens.
    pub initial_rows: u64,
    /// Extra rows fetched beyond what the grid asked for.
    pub lookahead_rows: u64,
}

impl EngineOptions {
    pub fn new(spool_dir: PathBuf) -> Self {
        EngineOptions { spool_dir, initial_rows: 500, lookahead_rows: 2000 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
    Failed(DriverError),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Statement produced rows; they are (being) fetched into `spool`.
    Rows,
    Done { rows_affected: Option<u64>, command: String },
    Failed(DriverError),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnState {
    pub autocommit: bool,
    pub in_txn: bool,
    /// Data-changing statements executed in the open transaction.
    pub pending_writes: u32,
    pub since: Option<Instant>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Conn { tab: TabId, state: ConnState },
    Txn { tab: TabId, state: TxnState },
    /// A statement started executing (the running timer starts here).
    StatementStarted { tab: TabId, query: QueryId, index: usize, sql: String, at: Instant },
    /// A statement that returns rows; the spool fills as rows are fetched.
    ResultOpened { tab: TabId, query: QueryId, index: usize, spool: Arc<Spool> },
    /// The statement finished executing (for row results: first rows are in,
    /// or it ended). The running timer stops here.
    StatementFinished { tab: TabId, query: QueryId, index: usize, elapsed: Duration, outcome: Outcome },
    /// Rows were added to a result, or its fetch state changed.
    Fetch { tab: TabId, query: QueryId, progress: Progress, paused: bool },
    /// The whole submitted batch of statements is done; the tab is idle.
    QueryDone { tab: TabId, query: QueryId },
    Export { tab: TabId, query: QueryId, path: PathBuf, rows: u64, done: Option<Result<(), String>> },
    /// Children of an object-tree node (`conn` is the caller's connection id).
    Objects { conn: String, path: ObjPath, items: Result<Vec<ObjItem>, String> },
    /// All relations and columns of a connection, for autocomplete.
    Catalog { conn: String, tables: Result<Arc<Vec<CatalogTable>>, String> },
}

struct TabEntry {
    cmd: mpsc::UnboundedSender<Cmd>,
    shared: Arc<TabShared>,
}

pub struct Engine {
    rt: Option<tokio::runtime::Runtime>,
    events: mpsc::UnboundedSender<Event>,
    opts: EngineOptions,
    tabs: Mutex<HashMap<TabId, TabEntry>>,
    next_id: AtomicU64,
    meta: Arc<MetaPool>,
}

impl Engine {
    pub fn new(opts: EngineOptions) -> (Arc<Engine>, mpsc::UnboundedReceiver<Event>) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("gq-engine")
            .enable_all()
            .build()
            .expect("tokio runtime");
        clean_spool_dir(&opts.spool_dir);
        let (tx, rx) = mpsc::unbounded_channel();
        let e = Arc::new(Engine { rt: Some(rt), events: tx, opts, tabs: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1), meta: Default::default() });
        (e, rx)
    }

    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("runtime alive")
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn open_tab(&self) -> TabId {
        let id = TabId(self.id());
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(TabShared::default());
        let actor = tab::TabActor::new(id, rx, shared.clone(), self.events.clone(), self.opts.clone());
        self.rt().spawn(actor.run());
        self.tabs.lock().insert(id, TabEntry { cmd: tx, shared });
        id
    }

    fn send(&self, tab: TabId, cmd: Cmd) {
        if let Some(t) = self.tabs.lock().get(&tab) {
            let _ = t.cmd.send(cmd);
        }
    }

    /// Close a tab. An open transaction is rolled back.
    pub fn close_tab(&self, tab: TabId) {
        if let Some(t) = self.tabs.lock().remove(&tab) {
            let _ = t.cmd.send(Cmd::Shutdown);
        }
    }

    /// Point a tab at a connection (or none). Connects eagerly in the
    /// background. Any open transaction on the previous connection is rolled back.
    pub fn set_connection(&self, tab: TabId, params: Option<ConnectParams>, autocommit: bool) {
        self.send(tab, Cmd::SetConnection { params, autocommit });
    }

    pub fn set_autocommit(&self, tab: TabId, autocommit: bool) {
        self.send(tab, Cmd::SetAutocommit(autocommit));
    }

    /// Run statements in order, stopping at the first error. The last
    /// statement's rows are fetched on demand; earlier ones are drained.
    pub fn execute(&self, tab: TabId, statements: Vec<String>) -> QueryId {
        let q = QueryId(self.id());
        self.send(tab, Cmd::Execute { query: q, statements });
        q
    }

    /// Cancel the running statement on the server, or stop fetching a paused result.
    pub fn cancel(&self, tab: TabId) {
        let shared = self.tabs.lock().get(&tab).map(|t| t.shared.clone());
        if let Some(s) = shared {
            self.rt().spawn(async move { s.cancel().await });
        }
    }

    /// Make sure at least `rows` rows of `query`'s result are fetched (the
    /// grid calls this as the user scrolls). Adds lookahead.
    pub fn ensure_rows(&self, tab: TabId, query: QueryId, rows: u64) {
        if let Some(t) = self.tabs.lock().get(&tab) {
            t.shared.demand(query, rows.saturating_add(self.opts.lookahead_rows));
        }
    }

    pub fn commit(&self, tab: TabId) {
        self.send(tab, Cmd::Commit);
    }

    pub fn rollback(&self, tab: TabId) {
        self.send(tab, Cmd::Rollback);
    }

    /// List the children of `path` in the object tree of connection `conn`.
    pub fn load_objects(&self, conn: &str, params: &ConnectParams, path: ObjPath) {
        let mc = self.meta.get(conn, params);
        let (events, conn) = (self.events.clone(), conn.to_string());
        self.rt().spawn(async move {
            let items = mc.list(&path).await.map_err(|e| e.message);
            meta::send(&events, Event::Objects { conn, path, items });
        });
    }

    /// Load every relation and its columns (for autocomplete).
    pub fn load_catalog(&self, conn: &str, params: &ConnectParams) {
        let mc = self.meta.get(conn, params);
        let (events, conn) = (self.events.clone(), conn.to_string());
        self.rt().spawn(async move {
            let tables = mc.catalog().await.map(Arc::new).map_err(|e| e.message);
            meta::send(&events, Event::Catalog { conn, tables });
        });
    }

    /// Drop the metadata session of a connection (e.g. after editing it).
    pub fn forget_connection(&self, conn: &str) {
        self.meta.forget(conn);
    }

    /// Export the full result of `query` to CSV, fetching the rest of it if needed.
    pub fn export_csv(&self, tab: TabId, query: QueryId, spool: Arc<Spool>, path: PathBuf, opts: CsvOptions) {
        if let Some(t) = self.tabs.lock().get(&tab) {
            t.shared.demand(query, u64::MAX);
        }
        let events = self.events.clone();
        self.rt().spawn_blocking(move || {
            let mut last = Instant::now();
            let r = gq_results::export_csv(&spool, &path, &opts, |rows| {
                if last.elapsed() > Duration::from_millis(150) {
                    last = Instant::now();
                    let _ = events.send(Event::Export { tab, query, path: path.clone(), rows, done: None });
                }
                true
            });
            let (rows, done) = match r {
                Ok(Some((gq_results::Status::Complete, n))) => (n, Ok(())),
                Ok(Some((gq_results::Status::Stopped(m), n))) => (n, Err(format!("result incomplete: {m}"))),
                Ok(Some((gq_results::Status::Open, n))) => (n, Err("result still open".into())),
                Ok(None) => (0, Err("export cancelled".into())),
                Err(e) => (0, Err(e.to_string())),
            };
            let _ = events.send(Event::Export { tab, query, path, rows, done: Some(done) });
        });
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        for (_, t) in self.tabs.lock().drain() {
            let _ = t.cmd.send(Cmd::Shutdown);
        }
        // Give tabs a moment to roll back and close cursors, without
        // blocking (dropping a runtime from async code would panic).
        if let Some(rt) = self.rt.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(Duration::from_millis(500));
            }
        }
    }
}

fn clean_spool_dir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with("gq-spool-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}
