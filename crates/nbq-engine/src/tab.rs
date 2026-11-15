//! Per-tab actor: owns the tab's database session and serializes everything
//! that touches it. Cancellation and fetch demand bypass the actor's queue
//! through [`TabShared`], because the actor may be busy awaiting the server.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use nbq_driver::{CancelHandle, ConnectParams, DriverError, ErrorKind, Execution, Session};
use nbq_results::Spool;
use nbq_sql::StatementKind;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::fetch::{FetchCtl, FetchEnd, FetchTask, StopPolicy};
use crate::{ConnState, EngineOptions, Event, Outcome, QueryId, TabId, TxnState};

pub(crate) enum Cmd {
    SetConnection { params: Option<ConnectParams>, autocommit: bool, revision: u64 },
    SetAutocommit(bool),
    Execute { query: QueryId, statements: Vec<String> },
    Commit,
    Rollback,
    Shutdown,
}

pub(crate) struct TabShared {
    inner: Mutex<SharedInner>,
    connection_revision: watch::Sender<u64>,
}

impl Default for TabShared {
    fn default() -> Self {
        Self { inner: Default::default(), connection_revision: watch::channel(0).0 }
    }
}

#[derive(Default)]
struct SharedInner {
    cancel: Option<Arc<dyn CancelHandle>>,
    /// A statement is executing (not merely a paused result).
    running: bool,
    fetch: Option<FetchCtl>,
    fetching: bool,
    last_activity: Option<Instant>,
}

impl TabShared {
    pub fn change_connection(&self) -> u64 {
        self.connection_revision.send_modify(|r| *r += 1);
        *self.connection_revision.borrow()
    }

    pub fn set_running(&self, running: bool) {
        let mut inner = self.inner.lock();
        inner.running = running;
        inner.last_activity = Some(Instant::now());
    }

    pub fn set_fetching(&self, fetching: bool) {
        let mut inner = self.inner.lock();
        if inner.fetching != fetching {
            inner.fetching = fetching;
            inner.last_activity = Some(Instant::now());
        }
    }

    fn touch(&self) { self.inner.lock().last_activity = Some(Instant::now()); }

    fn idle_for(&self) -> Option<Duration> {
        let inner = self.inner.lock();
        if inner.running || inner.fetching { return None; }
        inner.last_activity.map(|at| at.elapsed())
    }

    /// Terminal cancellation may abort the whole transaction (including Redshift).
    pub async fn force_cancel(&self) {
        let handle = self.inner.lock().cancel.clone();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle.cancel()).await;
        }
    }

    /// Run outside the actor queue: even a stalled Execute must release its
    /// socket promptly after an intentional disconnect/close.
    pub async fn terminate(&self) {
        let (handle, running) = { let inner = self.inner.lock(); (inner.cancel.clone(), inner.running) };
        if let Some(handle) = handle {
            if running {
                let _ = tokio::time::timeout(Duration::from_secs(1), handle.cancel()).await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            handle.close_connection();
        }
    }

    fn set_cancel(&self, c: Option<Arc<dyn CancelHandle>>) {
        self.inner.lock().cancel = c;
    }

    fn set_fetch(&self, f: Option<FetchCtl>) {
        self.inner.lock().fetch = f;
    }

    pub async fn cancel(&self) {
        let (handle, running, fetch) = {
            let g = self.inner.lock();
            (g.cancel.clone(), g.running, g.fetch.clone())
        };
        if running {
            if let Some(h) = handle {
                let _ = h.cancel().await;
            }
        } else if let Some(f) = fetch {
            f.stop(handle).await;
        }
    }

    pub fn demand(&self, query: QueryId, rows: u64) {
        let mut g = self.inner.lock();
        let mut changed = false;
        if let Some(f) = g.fetch.as_ref().filter(|f| f.query == query) {
            f.demand.send_if_modified(|v| {
                if rows > *v {
                    *v = rows;
                    changed = true;
                    true
                } else {
                    false
                }
            });
        }
        if changed { g.last_activity = Some(Instant::now()); }
    }
}

struct ActiveFetch {
    ctl: FetchCtl,
    join: JoinHandle<FetchEnd>,
    /// The statement runs inside `SAVEPOINT nbq_sp`, to be released or rolled back.
    savepoint: bool,
    write: bool,
}

pub(crate) struct TabActor {
    id: TabId,
    rx: mpsc::UnboundedReceiver<Cmd>,
    shared: Arc<TabShared>,
    events: mpsc::UnboundedSender<Event>,
    opts: EngineOptions,
    params: Option<ConnectParams>,
    connection_revision: u64,
    session: Option<Box<dyn Session>>,
    txn: TxnState,
    fetch: Option<ActiveFetch>,
    idle_timeout: Duration,
}

const SAVEPOINT: &str = "SAVEPOINT nbq_sp";
const RELEASE: &str = "RELEASE SAVEPOINT nbq_sp";
const ROLLBACK_TO: &str = "ROLLBACK TO SAVEPOINT nbq_sp";

impl TabActor {
    pub fn new(
        id: TabId,
        rx: mpsc::UnboundedReceiver<Cmd>,
        shared: Arc<TabShared>,
        events: mpsc::UnboundedSender<Event>,
        opts: EngineOptions,
    ) -> Self {
        let idle_timeout = opts.transaction_idle_timeout.max(Duration::from_millis(1));
        TabActor {
            id,
            rx,
            shared,
            events,
            opts,
            params: None,
            connection_revision: 0,
            session: None,
            txn: TxnState { autocommit: true, in_txn: false, pending_writes: 0, since: None },
            fetch: None,
            idle_timeout,
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn emit_txn(&self) {
        self.emit(Event::Txn { tab: self.id, state: self.txn });
    }

    fn reset_txn(&mut self) {
        self.txn.in_txn = false;
        self.txn.pending_writes = 0;
        self.txn.since = None;
    }

    pub async fn run(mut self) {
        let mut poll = tokio::time::interval(self.idle_timeout.min(Duration::from_millis(250)).max(Duration::from_millis(10)));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cmd = tokio::select! {
                biased;
                cmd = self.rx.recv() => { let Some(cmd) = cmd else { break }; cmd },
                _ = poll.tick() => { self.maintain().await; continue; }
            };
            match cmd {
                Cmd::SetConnection { params, autocommit, revision } => {
                    if revision != *self.shared.connection_revision.borrow() { continue; }
                    self.connection_revision = revision;
                    self.terminate_session("disconnected").await;
                    self.txn.autocommit = autocommit;
                    self.params = params;
                    self.emit_txn();
                    if self.params.is_some() {
                        self.connect().await;
                    } else {
                        self.emit(Event::Conn { tab: self.id, state: ConnState::Disconnected });
                    }
                }
                Cmd::SetAutocommit(a) => {
                    if a && self.txn.in_txn {
                        // Like DBeaver: switching to auto-commit commits the open transaction.
                        self.end_txn("COMMIT").await;
                    }
                    self.txn.autocommit = a;
                    self.emit_txn();
                }
                Cmd::Execute { query, statements } => {
                    self.shared.touch();
                    self.execute(query, statements).await;
                    self.emit(Event::QueryDone { tab: self.id, query });
                }
                Cmd::Commit => self.end_txn("COMMIT").await,
                Cmd::Rollback => self.end_txn("ROLLBACK").await,
                Cmd::Shutdown => break,
            }
        }
        // Tab closed: never leave a transaction or a paused query on the server.
        self.terminate_session("tab closed").await;
    }

    /// Check local state only: polling must not send keepalives that reset the
    /// server's idle timeout. Paused ClientWrite results need the app timer.
    async fn maintain(&mut self) {
        if self.fetch.as_ref().is_some_and(|f| f.join.is_finished()) {
            self.close_fetch().await;
        }
        if self.session.as_ref().is_some_and(|s| s.is_closed()) {
            let expired = self.session.as_ref().and_then(|s| s.closed_error())
                .is_some_and(|e| e.code.as_deref() == Some("25P03"));
            let error = self.session.as_ref().and_then(|s| s.closed_error())
                .unwrap_or_else(|| DriverError::connection("Database connection closed; uncommitted changes were rolled back."));
            let idle_for = self.shared.idle_for().unwrap_or(self.idle_timeout);
            self.terminate_session("connection closed").await;
            if expired {
                self.emit(Event::Conn { tab: self.id, state: ConnState::Disconnected });
                self.emit(Event::TransactionExpired { tab: self.id, idle_for, had_transaction: true });
            } else {
                self.emit(Event::Conn { tab: self.id, state: ConnState::Failed(error) });
            }
        } else if (self.txn.in_txn || self.fetch.as_ref().is_some_and(|f| f.ctl.is_open()))
            && self.shared.idle_for().is_some_and(|idle| idle >= self.idle_timeout) {
            let idle_for = self.shared.idle_for().unwrap_or(self.idle_timeout);
            let had_transaction = self.txn.in_txn;
            self.terminate_session("transaction idle timeout").await;
            self.emit(Event::Conn { tab: self.id, state: ConnState::Disconnected });
            self.emit(Event::TransactionExpired { tab: self.id, idle_for, had_transaction });
        }
    }

    /// Terminal cleanup must not drain an abandoned Redshift result forever.
    /// Cancel, attempt ROLLBACK with a deadline, then close the owning socket.
    async fn terminate_session(&mut self, reason: &str) {
        if let Some(fetch) = self.fetch.take() {
            fetch.join.abort();
            let _ = fetch.join.await;
            if fetch.ctl.is_open() {
                fetch.ctl.spool.finish(nbq_results::Status::Stopped(reason.into()));
                if self.session.as_ref().is_some_and(|s| !s.is_closed()) {
                    self.shared.force_cancel().await;
                }
            }
            self.emit(Event::Fetch { tab: self.id, query: fetch.ctl.query, progress: fetch.ctl.spool.progress(), paused: false });
        }
        self.shared.set_fetch(None);
        self.shared.set_fetching(false);
        self.shared.set_running(false);
        // Also flush cancellation and cover transactions opened by SQL forms
        // the client has not classified. ROLLBACK outside a txn is harmless.
        if self.session.as_ref().is_some_and(|s| !s.is_closed()) {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.simple("ROLLBACK")).await;
        }
        self.drop_session();
        self.reset_txn();
        self.emit_txn();
    }

    fn drop_session(&mut self) {
        self.session = None;
        self.shared.set_cancel(None);
    }

    async fn connect(&mut self) -> bool {
        let Some(p) = self.params.clone() else { return false };
        self.emit(Event::Conn { tab: self.id, state: ConnState::Connecting });
        let mut revision = self.shared.connection_revision.subscribe();
        let result = tokio::select! {
            biased;
            _ = revision.wait_for(|r| *r != self.connection_revision) => return false,
            result = nbq_driver::connect(&p) => result,
        };
        match result {
            Ok(mut s) => {
                let configured = tokio::select! {
                    biased;
                    _ = revision.wait_for(|r| *r != self.connection_revision) => return false,
                    result = tokio::time::timeout(p.connect_timeout,
                        s.configure_transaction_timeout(self.opts.transaction_idle_timeout.max(Duration::from_millis(1)))) => {
                        result.unwrap_or_else(|_| Err(DriverError::connection("Timed out configuring transaction inactivity protection.")))
                    },
                };
                match configured {
                    Ok(timeout) => self.idle_timeout = timeout,
                    Err(error) => {
                        self.emit(Event::Conn { tab: self.id, state: ConnState::Failed(error) });
                        return false;
                    }
                }
                self.shared.set_cancel(Some(s.cancel_handle()));
                self.session = Some(s);
                self.shared.touch();
                self.emit(Event::Conn { tab: self.id, state: ConnState::Connected });
                true
            }
            Err(e) => {
                self.emit(Event::Conn { tab: self.id, state: ConnState::Failed(e) });
                false
            }
        }
    }

    /// Ensure a live session, reconnecting if the old one died.
    async fn ensure_session(&mut self) -> Result<(), DriverError> {
        if self.session.as_ref().is_some_and(|s| s.is_closed()) {
            self.drop_session();
            if self.txn.in_txn {
                self.reset_txn();
                self.emit_txn();
            }
            self.emit(Event::Conn { tab: self.id, state: ConnState::Disconnected });
        }
        if self.session.is_none() && !self.connect().await {
            return Err(DriverError::connection(if self.params.is_none() {
                "no connection selected"
            } else {
                "could not connect"
            }));
        }
        Ok(())
    }

    /// Run a statement whose result we don't care about.
    async fn simple(&mut self, sql: &str) -> Result<(), DriverError> {
        let s = self.session.as_mut().ok_or_else(|| DriverError::connection("not connected"))?;
        match s.execute(sql).await? {
            Execution::Done { .. } => Ok(()),
            Execution::Rows(mut st) => {
                while st.next_batch(65_536, std::time::Duration::from_secs(1)).await?.is_some() {}
                Ok(())
            }
        }
    }

    fn on_error(&mut self, e: &DriverError) {
        if e.kind == ErrorKind::Connection || self.session.as_ref().is_some_and(|s| s.is_closed()) {
            self.drop_session();
            self.emit(Event::Conn { tab: self.id, state: ConnState::Failed(e.clone()) });
            if self.txn.in_txn {
                self.reset_txn();
                self.emit_txn();
            }
        }
    }

    /// Stop and finish the open result, if any, and settle its savepoint.
    async fn close_fetch(&mut self) {
        let Some(f) = self.fetch.take() else { return };
        if f.ctl.is_open() {
            let cancel = self.session.as_ref().map(|s| s.cancel_handle());
            f.ctl.stop(cancel).await;
        }
        let end = f.join.await.unwrap_or(FetchEnd { complete: false, error: None });
        self.shared.set_fetch(None);
        self.settle(f.savepoint, f.write, &end).await;
    }

    async fn settle(&mut self, savepoint: bool, write: bool, end: &FetchEnd) {
        if let Some(e) = &end.error {
            self.on_error(e);
        }
        if self.session.is_none() {
            return;
        }
        if savepoint {
            let ok = end.complete && end.error.is_none();
            let _ = self.simple(if ok { RELEASE } else { ROLLBACK_TO }).await;
        }
        if write && end.complete && self.txn.in_txn {
            self.txn.pending_writes += 1;
            self.emit_txn();
        }
    }

    async fn end_txn(&mut self, sql: &str) {
        self.close_fetch().await;
        if !self.txn.in_txn || self.session.is_none() {
            return;
        }
        if let Err(e) = self.simple(sql).await {
            self.on_error(&e);
            tracing::warn!("{sql} failed: {e}");
        }
        self.reset_txn();
        self.emit_txn();
    }

    async fn execute(&mut self, query: QueryId, statements: Vec<String>) {
        self.close_fetch().await;
        let n = statements.len();
        for (index, sql) in statements.into_iter().enumerate() {
            let at = Instant::now();
            self.emit(Event::StatementStarted { tab: self.id, query, index, sql: sql.clone(), at });
            let finished = |me: &Self, outcome: Outcome| {
                me.emit(Event::StatementFinished { tab: me.id, query, index, elapsed: at.elapsed(), outcome })
            };
            if let Err(e) = self.ensure_session().await {
                finished(self, Outcome::Failed(e));
                return;
            }
            let dialect = self.session.as_ref().unwrap().dialect();
            let savepoints = self.session.as_ref().unwrap().capabilities().savepoints;
            let kind = nbq_sql::classify(&sql, dialect);
            let txn_control = matches!(kind, StatementKind::Begin | StatementKind::Commit | StatementKind::Rollback);

            self.shared.set_running(true);
            if !self.txn.autocommit && !self.txn.in_txn && !txn_control {
                if let Err(e) = self.simple("BEGIN").await {
                    self.shared.set_running(false);
                    self.on_error(&e);
                    finished(self, Outcome::Failed(e));
                    return;
                }
                self.txn.in_txn = true;
                self.txn.since = Some(Instant::now());
                self.emit_txn();
            }
            let use_sp = self.txn.in_txn && savepoints && !txn_control;
            if use_sp {
                if let Err(e) = self.simple(SAVEPOINT).await {
                    self.shared.set_running(false);
                    self.on_error(&e);
                    finished(self, Outcome::Failed(e));
                    return;
                }
            }

            let result = self.session.as_mut().unwrap().execute(&sql).await;
            match result {
                Err(e) => {
                    self.shared.set_running(false);
                    if use_sp {
                        let _ = self.simple(ROLLBACK_TO).await;
                    }
                    self.on_error(&e);
                    let outcome = if e.kind == ErrorKind::Cancelled { Outcome::Cancelled } else { Outcome::Failed(e) };
                    finished(self, outcome);
                    return;
                }
                Ok(Execution::Done { rows_affected, command }) => {
                    self.shared.set_running(false);
                    if use_sp {
                        let _ = self.simple(RELEASE).await;
                    }
                    match kind {
                        StatementKind::Begin => {
                            self.txn.in_txn = true;
                            self.txn.since = Some(Instant::now());
                        }
                        StatementKind::Commit | StatementKind::Rollback => self.reset_txn(),
                        StatementKind::Write if self.txn.in_txn => self.txn.pending_writes += 1,
                        _ => {}
                    }
                    if kind != StatementKind::Query {
                        self.emit_txn();
                    }
                    finished(self, Outcome::Done { rows_affected, command });
                }
                Ok(Execution::Rows(stream)) => {
                    let spool = match Spool::create_with_limits(&self.opts.spool_dir, stream.schema(), self.opts.spool_limits.clone()) {
                        Ok(s) => s,
                        Err(e) => {
                            // Can't spool: close the stream on the server and report.
                            self.shared.set_running(false);
                            let c = self.session.as_ref().unwrap().cancel_handle();
                            let _ = c.cancel().await;
                            drop(stream);
                            if use_sp {
                                let _ = self.simple(ROLLBACK_TO).await;
                            }
                            finished(self, Outcome::Failed(DriverError::new(ErrorKind::Query, e.to_string())));
                            return;
                        }
                    };
                    self.emit(Event::ResultOpened { tab: self.id, query, index, spool: spool.clone() });
                    let write = kind == StatementKind::Write;
                    // Earlier statements in a script and anything that writes
                    // must run to completion.
                    let drain = index + 1 < n || write;
                    let initial = if drain { u64::MAX } else { self.opts.initial_rows };
                    let (demand_tx, demand_rx) = watch::channel(initial);
                    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                    let ctl = FetchCtl {
                        query,
                        demand: Arc::new(demand_tx),
                        discard: Arc::new(AtomicBool::new(false)),
                        policy: if self.txn.in_txn && !use_sp { StopPolicy::Drain } else { StopPolicy::Cancel },
                        spool,
                    };
                    self.shared.set_fetch(Some(ctl.clone()));
                    let task = FetchTask {
                        tab: self.id,
                        query,
                        index,
                        stream,
                        ctl: ctl.clone(),
                        demand: demand_rx,
                        events: self.events.clone(),
                        started: at,
                        shared: self.shared.clone(),
                        first_done: Some(first_tx),
                    };
                    let join = tokio::spawn(task.run());
                    if drain {
                        let end = join.await.unwrap_or(FetchEnd { complete: false, error: None });
                        self.shared.set_fetch(None);
                        let ok = end.complete && end.error.is_none();
                        self.settle(use_sp, write, &end).await;
                        if !ok {
                            return;
                        }
                    } else {
                        // The query is done (for the UI) once its first rows are in.
                        let _ = first_rx.await;
                        self.fetch = Some(ActiveFetch { ctl, join, savepoint: use_sp, write });
                    }
                }
            }
        }
    }
}
