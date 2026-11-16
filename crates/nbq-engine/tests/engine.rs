//! Headless end-to-end tests of the engine against the dev Postgres.

use std::sync::Arc;
use std::time::Duration;

use nbq_driver::{ConnectParams, Execution, Session};
use nbq_engine::{
    ConnState, CsvOptions, Engine, EngineOptions, Event, Outcome, QueryId, Spool, SpoolStatus, TabId, TxnState,
};
use tokio::sync::mpsc::UnboundedReceiver;

const T: Duration = Duration::from_secs(15);

struct H {
    engine: Arc<Engine>,
    rx: UnboundedReceiver<Event>,
    tab: TabId,
    params: ConnectParams,
    dir: tempfile::TempDir,
    /// Latest transaction state seen in any event.
    txn: Option<TxnState>,
    /// Latest fetch state per query: (progress, paused).
    fetch: std::collections::HashMap<QueryId, (nbq_engine::Progress, bool)>,
}

impl H {
    async fn sqlite(autocommit: bool) -> H {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sqlite-engine-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let params = ConnectParams::file(nbq_driver::DbKind::Sqlite, dir.path().join("local.db"));
        nbq_driver::create_database(params.kind, &params.file_path).unwrap();
        let (engine, rx) = Engine::new(EngineOptions::new(dir.path().join("spool")));
        let tab = engine.open_tab();
        engine.set_connection(tab, Some(params.clone()), autocommit);
        let mut h = H { engine, rx, tab, params, dir, txn: None, fetch: Default::default() };
        h.wait(|e| matches!(e, Event::Conn { state: ConnState::Connected, .. }).then_some(())).await;
        h
    }
    async fn new(app: &str, autocommit: bool) -> H {
        Self::with_timeout(app, autocommit, Duration::from_secs(900)).await
    }

    async fn with_timeout(app: &str, autocommit: bool, timeout: Duration) -> H {
        let mut params = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
        params.application_name = app.into();
        let dir = tempfile::tempdir().unwrap();
        let mut opts = EngineOptions::new(dir.path().join("spool"));
        opts.transaction_idle_timeout = timeout;
        let (engine, rx) = Engine::new(opts);
        let tab = engine.open_tab();
        engine.set_connection(tab, Some(params.clone()), autocommit);
        let mut h = H { engine, rx, tab, params, dir, txn: None, fetch: Default::default() };
        h.wait(|e| matches!(e, Event::Conn { state: ConnState::Connected, .. }).then_some(())).await;
        h
    }

    async fn wait<R>(&mut self, mut f: impl FnMut(&Event) -> Option<R>) -> R {
        let deadline = tokio::time::Instant::now() + T;
        loop {
            let e = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .expect("timed out waiting for event")
                .expect("engine closed");
            if let Event::Txn { state, .. } = &e {
                self.txn = Some(*state);
            }
            if let Event::Fetch { query, progress, paused, .. } = &e {
                self.fetch.insert(*query, (progress.clone(), *paused));
            }
            if let Some(r) = f(&e) {
                return r;
            }
        }
    }

    /// Execute and collect events until the query is done.
    async fn run_all(&mut self, stmts: &[&str]) -> (QueryId, Vec<Event>) {
        let q = self.engine.execute(self.tab, stmts.iter().map(|s| s.to_string()).collect());
        let mut evs = Vec::new();
        self.wait(|e| {
            evs.push(e.clone());
            matches!(e, Event::QueryDone { query, .. } if *query == q).then_some(())
        })
        .await;
        (q, evs)
    }

    async fn run(&mut self, sql: &str) -> (QueryId, Vec<Event>) {
        self.run_all(&[sql]).await
    }

    async fn ok(&mut self, sql: &str) {
        let (_, evs) = self.run(sql).await;
        if let Some(o) = outcomes(&evs).into_iter().find(|o| !matches!(o, Outcome::Done { .. } | Outcome::Rows)) {
            panic!("{sql}: {o:?}");
        }
    }

    async fn paused_at(&mut self, q: QueryId) -> u64 {
        if let Some((p, true)) = self.fetch.get(&q) {
            return p.rows;
        }
        self.wait(|e| match e {
            Event::Fetch { query, progress, paused: true, .. } if *query == q => Some(progress.rows),
            _ => None,
        })
        .await
    }

    async fn ended(&mut self, q: QueryId) -> (u64, SpoolStatus) {
        if let Some((p, _)) = self.fetch.get(&q).filter(|(p, _)| p.status != SpoolStatus::Open) {
            return (p.rows, p.status.clone());
        }
        self.wait(|e| match e {
            Event::Fetch { query, progress, .. } if *query == q && progress.status != SpoolStatus::Open => {
                Some((progress.rows, progress.status.clone()))
            }
            _ => None,
        })
        .await
    }

    /// Wait until the transaction state satisfies `pred` (may already).
    async fn txn_until(&mut self, pred: impl Fn(&TxnState) -> bool) -> TxnState {
        if let Some(t) = self.txn.filter(|t| pred(t)) {
            return t;
        }
        self.wait(|e| match e {
            Event::Txn { state, .. } if pred(state) => Some(*state),
            _ => None,
        })
        .await
    }

    async fn commit(&mut self) {
        self.engine.commit(self.tab);
        self.txn = None;
        self.txn_until(|t| !t.in_txn).await;
    }

    /// An independent autocommit connection for checking server state.
    async fn observer(&self) -> Box<dyn Session> {
        let mut p = self.params.clone();
        p.application_name = "nbq-observer".into();
        nbq_driver::connect(&p).await.unwrap()
    }
}

#[tokio::test]
async fn sqlite_transactions_pause_cancel_export_and_reconnect() {
    let mut h = H::sqlite(false).await;
    h.ok("create table t(id integer primary key)").await;
    h.ok("insert into t values (1)").await;
    let (_, events) = h.run("insert into t values (1)").await;
    assert!(outcomes(&events).iter().any(|o| matches!(o, Outcome::Failed(_))));
    h.ok("insert into t values (2)").await;
    h.commit().await;
    let (q, events) = h.run("with recursive n(x) as (values(1) union all select x+1 from n where x<1000000) select x from n").await;
    assert!(h.paused_at(q).await < 10_000);
    let spool = spool_of(&events);
    h.engine.cancel(h.tab);
    h.ended(q).await;
    assert!(!spool.read(0..1).unwrap().is_empty());
    h.ok("insert into t values (3)").await;
    h.engine.rollback(h.tab);
    h.txn = None;
    h.txn_until(|t| !t.in_txn).await;
    let mut obs = h.observer().await;
    assert_eq!(scalar(&mut *obs, "select count(*) from t").await, "2");
    let (q, events) = h.run("select id from t order by id").await;
    let spool = spool_of(&events);
    let path = h.dir.path().join("out.csv");
    h.engine.export_csv(h.tab, q, spool, path.clone(), CsvOptions::default());
    h.wait(|e| matches!(e, Event::Export { done: Some(Ok(())), .. }).then_some(())).await;
    assert_eq!(std::fs::read_to_string(path).unwrap().replace('\r', ""), "id\n1\n2\n");
    h.engine.set_connection(h.tab, None, true);
    h.wait(|e| matches!(e, Event::Conn { state: ConnState::Disconnected, .. }).then_some(())).await;
    h.engine.set_connection(h.tab, Some(h.params.clone()), true);
    h.wait(|e| matches!(e, Event::Conn { state: ConnState::Connected, .. }).then_some(())).await;
    h.ok("select * from t").await;
}

#[tokio::test]
async fn sqlite_object_browser_and_catalog_quote_identifiers() {
    use nbq_engine::meta::{self, ObjKind, ObjPath};
    let h = H::sqlite(true).await;
    let mut s = h.observer().await;
    for sql in ["create table \"odd ' table\" (id integer primary key, value text, doubled text generated always as (value || value) virtual)", "create index idx_value on \"odd ' table\"(value)", "create view v as select id from \"odd ' table\"", "create trigger tr after insert on \"odd ' table\" begin select 1; end"] { s.execute(sql).await.unwrap(); }
    let roots = meta::list(&mut *s, &ObjPath::Root).await.unwrap();
    assert!(roots.iter().any(|i| i.name == "main"));
    let folders = meta::list(&mut *s, &ObjPath::Schema("main".into())).await.unwrap();
    assert_eq!(folders.len(), 2);
    let tables = meta::list(&mut *s, &ObjPath::Folder { schema: "main".into(), kind: ObjKind::Table }).await.unwrap();
    assert_eq!(tables.len(), 1);
    let children = meta::list(&mut *s, tables[0].path.as_ref().unwrap()).await.unwrap();
    assert!(children.iter().any(|i| i.name == "id" && i.key));
    assert!(children.iter().any(|i| i.name == "doubled" && i.kind == ObjKind::Column));
    assert!(children.iter().any(|i| i.name == "idx_value" && i.kind == ObjKind::Index));
    assert!(children.iter().any(|i| i.name == "tr" && i.kind == ObjKind::Trigger));
    let catalog = meta::catalog(&mut *s).await.unwrap();
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog.iter().find(|t| t.name == "odd ' table").unwrap().columns.len(), 3);
}

#[tokio::test]
async fn sqlite_automatic_rollback_updates_engine_transaction_state() {
    let mut h = H::sqlite(false).await;
    h.ok("create table t(id integer primary key)").await;
    h.commit().await;
    h.ok("insert into t values (1)").await;
    assert!(h.txn.unwrap().in_txn);
    let (_, events) = h.run("insert or rollback into t values (1)").await;
    assert!(outcomes(&events).iter().any(|o| matches!(o, Outcome::Failed(_))));
    assert!(!h.txn.unwrap().in_txn);
    assert_eq!(h.txn.unwrap().pending_writes, 0);
    h.ok("insert into t values (2)").await;
    h.commit().await;
    let mut obs = h.observer().await;
    assert_eq!(scalar(&mut *obs, "select sum(id) from t").await, "2");
}

fn outcomes(evs: &[Event]) -> Vec<Outcome> {
    evs.iter()
        .filter_map(|e| match e {
            Event::StatementFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn spool_of(evs: &[Event]) -> Arc<Spool> {
    evs.iter()
        .find_map(|e| match e {
            Event::ResultOpened { spool, .. } => Some(spool.clone()),
            _ => None,
        })
        .expect("no result opened")
}

async fn scalar(s: &mut dyn Session, sql: &str) -> String {
    let Execution::Rows(mut st) = s.execute(sql).await.unwrap() else { panic!("no rows") };
    let b = st.next_batch(10, Duration::from_secs(5)).await.unwrap().unwrap();
    let mut out = String::new();
    nbq_driver::display::cell_text(b.column(0), 0, &mut out);
    out
}

async fn active_queries(obs: &mut dyn Session, app: &str) -> String {
    scalar(
        obs,
        &format!(
            "select count(*) from pg_stat_activity where application_name = '{app}' and state = 'active'"
        ),
    )
    .await
}

#[tokio::test]
async fn result_pauses_then_follows_scrolling() {
    let mut h = H::new("nbq-e-scroll", true).await;
    let (q, evs) = h.run("select g, md5(g::text) from generate_series(1, 100000) g").await;
    assert!(matches!(outcomes(&evs)[..], [Outcome::Rows]), "{:?}", outcomes(&evs));
    let spool = spool_of(&evs);
    let rows = h.paused_at(q).await;
    assert_eq!(rows, 500, "initial fetch");
    assert_eq!(spool.rows(), 500);

    // User scrolls to row 10k: fetch up to there plus lookahead, then pause.
    h.fetch.remove(&q);
    h.engine.ensure_rows(h.tab, q, 10_000);
    let rows = h.paused_at(q).await;
    assert!((12_000..12_000 + 8192).contains(&rows), "rows {rows}");

    // The server is still holding the rest.
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "nbq-e-scroll").await, "1");
}

#[tokio::test]
async fn export_fetches_the_rest_of_a_paused_result() {
    let mut h = H::new("nbq-e-export", true).await;
    let (q, evs) = h.run("select g, 'x' || g from generate_series(1, 50000) g").await;
    let spool = spool_of(&evs);
    h.paused_at(q).await;
    let path = h.dir.path().join("out.csv");
    h.engine.export_csv(h.tab, q, spool.clone(), path.clone(), CsvOptions::default());
    let (rows, done) = h
        .wait(|e| match e {
            Event::Export { rows, done: Some(d), .. } => Some((*rows, d.clone())),
            _ => None,
        })
        .await;
    assert_eq!(done, Ok(()));
    assert_eq!(rows, 50_000);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 50_001);
    assert_eq!(text.lines().last().unwrap(), "50000,x50000");
    assert_eq!(spool.progress().status, SpoolStatus::Complete);
}

#[tokio::test]
async fn cancel_running_statement_on_server() {
    let mut h = H::new("nbq-e-cancel", true).await;
    let q = h.engine.execute(h.tab, vec!["select pg_sleep(30)".into()]);
    h.wait(|e| matches!(e, Event::StatementStarted { query, .. } if *query == q).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    h.engine.cancel(h.tab);
    let o = h
        .wait(|e| match e {
            Event::StatementFinished { query, outcome, .. } if *query == q => Some(outcome.clone()),
            _ => None,
        })
        .await;
    assert!(matches!(o, Outcome::Cancelled), "{o:?}");
    assert!(t.elapsed() < Duration::from_secs(3));
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "nbq-e-cancel").await, "0");
    h.ok("select 1").await;
}

#[tokio::test]
async fn cancel_long_ddl() {
    let mut h = H::new("nbq-e-cancel-ddl", true).await;
    let q = h.engine.execute(h.tab, vec!["create temp table slow as select pg_sleep(30)::text".into()]);
    h.wait(|e| matches!(e, Event::StatementStarted { query, .. } if *query == q).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.engine.cancel(h.tab);
    let o = h
        .wait(|e| match e {
            Event::StatementFinished { query, outcome, .. } if *query == q => Some(outcome.clone()),
            _ => None,
        })
        .await;
    assert!(matches!(o, Outcome::Cancelled), "{o:?}");
}

#[tokio::test]
async fn stop_paused_result_and_run_next() {
    let mut h = H::new("nbq-e-stop", true).await;
    let (q, _) = h.run("select g from generate_series(1, 10000000) g").await;
    h.paused_at(q).await;
    h.engine.cancel(h.tab);
    let (rows, status) = h.ended(q).await;
    assert_eq!(rows, 500, "keeps what was loaded");
    assert_eq!(status, SpoolStatus::Stopped("closed".into()));
    h.ok("select 2").await;
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "nbq-e-stop").await, "0");
}

#[tokio::test]
async fn new_query_closes_paused_result_on_server() {
    let mut h = H::new("nbq-e-replace", true).await;
    let (q, _) = h.run("select g from generate_series(1, 10000000) g").await;
    h.paused_at(q).await;
    h.ok("select 'next'").await;
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "nbq-e-replace").await, "0");
}

#[tokio::test]
async fn manual_commit_and_rollback() {
    let mut h = H::new("nbq-e-txn", false).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_txn").await.unwrap();
    obs.execute("create table e_txn (a int)").await.unwrap();

    h.ok("insert into e_txn values (1)").await;
    let t = h.txn.unwrap();
    assert!(t.in_txn && !t.autocommit && t.since.is_some());
    assert_eq!(t.pending_writes, 1);
    // A read doesn't count as a pending change.
    h.ok("select 1").await;
    assert_eq!(h.txn.unwrap().pending_writes, 1);
    assert_eq!(scalar(&mut *obs, "select count(*) from e_txn").await, "0", "uncommitted is invisible");

    h.engine.rollback(h.tab);
    let t = h.txn_until(|t| !t.in_txn).await;
    assert_eq!(t.pending_writes, 0);
    assert_eq!(scalar(&mut *obs, "select count(*) from e_txn").await, "0");

    h.ok("insert into e_txn values (2)").await;
    h.commit().await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_txn").await, "1");
}

/// A failing statement inside a manual transaction doesn't abort it.
#[tokio::test]
async fn error_does_not_abort_open_transaction() {
    let mut h = H::new("nbq-e-sp", false).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_sp").await.unwrap();
    obs.execute("create table e_sp (a int)").await.unwrap();
    h.ok("insert into e_sp values (1)").await;
    let (_, evs) = h.run("select * from no_such_table").await;
    assert!(matches!(outcomes(&evs)[..], [Outcome::Failed(_)]));
    h.ok("insert into e_sp values (2)").await;
    h.commit().await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_sp").await, "2");
}

/// Committing while a big select is paused closes it without losing the transaction.
#[tokio::test]
async fn paused_result_in_transaction_then_commit() {
    let mut h = H::new("nbq-e-sp-paused", false).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_spp").await.unwrap();
    obs.execute("create table e_spp (a int)").await.unwrap();
    h.ok("insert into e_spp values (1)").await;
    let (q, _) = h.run("select g from generate_series(1, 10000000) g").await;
    h.paused_at(q).await;
    h.commit().await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_spp").await, "1");
}

#[tokio::test]
async fn script_stops_at_first_error() {
    let mut h = H::new("nbq-e-script", true).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_script").await.unwrap();
    obs.execute("create table e_script (a int)").await.unwrap();
    let (_, evs) = h
        .run_all(&["insert into e_script values (1)", "insert into nope values (1)", "insert into e_script values (3)"])
        .await;
    let o = outcomes(&evs);
    assert_eq!(o.len(), 2, "{o:?}");
    assert!(matches!(o[0], Outcome::Done { rows_affected: Some(1), .. }));
    assert!(matches!(&o[1], Outcome::Failed(e) if e.code.as_deref() == Some("42P01")));
    assert_eq!(scalar(&mut *obs, "select count(*) from e_script").await, "1");
}

/// Writes that return rows are drained so the statement completes.
#[tokio::test]
async fn returning_is_fully_drained() {
    let mut h = H::new("nbq-e-returning", true).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_ret").await.unwrap();
    obs.execute("create table e_ret (a int)").await.unwrap();
    let (q, evs) = h.run("insert into e_ret select generate_series(1, 5000) returning a").await;
    let spool = spool_of(&evs);
    assert_eq!(spool.progress().status, SpoolStatus::Complete);
    assert_eq!(spool.rows(), 5000);
    let _ = q;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_ret").await, "5000");
}

#[tokio::test]
async fn reconnects_after_connection_loss() {
    let mut h = H::new("nbq-e-reconnect", true).await;
    h.ok("select 1").await;
    let mut obs = h.observer().await;
    scalar(
        &mut *obs,
        "select count(pg_terminate_backend(pid)) from pg_stat_activity where application_name = 'nbq-e-reconnect'",
    )
    .await;
    // First attempt may fail on the dead socket; the next one reconnects.
    let (_, evs) = h.run("select 1").await;
    if !matches!(outcomes(&evs)[..], [Outcome::Rows]) {
        h.ok("select 1").await;
    }
}

#[tokio::test]
async fn connection_failure_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().to_path_buf()));
    let tab = engine.open_tab();
    let mut p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    p.password = Some("wrong".into());
    engine.set_connection(tab, Some(p), true);
    let err = loop {
        match tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap() {
            Event::Conn { state: ConnState::Failed(e), .. } => break e,
            _ => {}
        }
    };
    assert_eq!(err.code.as_deref(), Some("28P01"), "{err:?}");
}

#[tokio::test]
async fn closing_a_running_tab_cancels_instead_of_waiting_for_the_statement() {
    let mut h = H::new("nbq-e-close-running", true).await;
    let mut observer = h.observer().await;
    let query = h.engine.execute(h.tab, vec!["select pg_sleep(30)".into()]);
    h.wait(|event| matches!(event, Event::StatementStarted { query: q, .. } if *q == query).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.engine.close_tab(h.tab);
    h.wait(|event| matches!(event, Event::QueryDone { query: q, .. } if *q == query).then_some(())).await;
    assert_eq!(active_queries(&mut *observer, "nbq-e-close-running").await, "0");
}

#[tokio::test]
async fn disconnect_cancels_a_pending_tab_connect_without_waiting_for_timeout() {
    let mut p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    let real = p.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    p.host = "127.0.0.1".into();
    p.port = listener.local_addr().unwrap().port();
    p.ssl = nbq_driver::SslMode::Disable;
    p.connect_timeout = Duration::from_secs(30);
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().into()));
    let tab = engine.open_tab();
    engine.set_connection(tab, Some(p), true);
    let (_socket, _) = listener.accept().await.unwrap();
    engine.set_connection(tab, None, true);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await.unwrap() {
                Event::Conn { state: ConnState::Disconnected, .. } => break,
                Event::Conn { state: ConnState::Connected | ConnState::Failed(_), .. } => panic!("cancelled connect published a result"),
                _ => {}
            }
        }
    }).await.expect("disconnect waited for connect timeout");
    engine.set_connection(tab, Some(real), true);
    loop {
        if let Event::Conn { state: ConnState::Connected, .. } = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap() { break; }
    }
}

#[tokio::test]
async fn disconnect_releases_a_paused_result_and_keeps_fetched_rows() {
    let mut h = H::new("nbq-e-disconnect-paused", true).await;
    let mut observer = h.observer().await;
    let (_, events) = h.run("select i, repeat('x', 1000) from generate_series(1, 100000) i").await;
    let spool = spool_of(&events);
    assert!(spool.rows() > 0);
    h.engine.set_connection(h.tab, None, true);
    h.wait(|e| matches!(e, Event::Conn { state: ConnState::Disconnected, .. }).then_some(())).await;
    assert!(matches!(spool.progress().status, SpoolStatus::Stopped(_)));
    assert!(!spool.read(0..1).unwrap().is_empty());
    assert_eq!(active_queries(&mut *observer, "nbq-e-disconnect-paused").await, "0");
}

async fn no_backends(observer: &mut dyn Session, app: &str) {
    let sql = format!("select count(*) from pg_stat_activity where application_name like '{app}%'");
    tokio::time::timeout(Duration::from_secs(5), async {
        while scalar(observer, &sql).await != "0" { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.expect("backend/transaction remained open");
}

#[tokio::test]
async fn idle_transaction_rolls_back_pending_writes_and_reconnects_cleanly() {
    let mut h = H::with_timeout("nbq-e-idle-expire", false, Duration::from_millis(500)).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_idle_expire").await.unwrap();
    obs.execute("create table e_idle_expire (a int)").await.unwrap();
    h.ok("insert into e_idle_expire values (1)").await;
    assert!(h.txn.unwrap().in_txn);
    h.wait(|e| matches!(e, Event::TransactionExpired { .. }).then_some(())).await;
    assert!(!h.txn.unwrap().in_txn);
    assert_eq!(h.txn.unwrap().pending_writes, 0);
    no_backends(&mut *obs, "nbq-e-idle-expire").await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_idle_expire").await, "0");
    h.ok("insert into e_idle_expire values (2)").await;
    h.commit().await;
    assert_eq!(scalar(&mut *obs, "select sum(a) from e_idle_expire").await, "2");
}

#[tokio::test]
async fn database_activity_resets_idle_limit_and_long_running_queries_are_allowed() {
    let mut h = H::with_timeout("nbq-e-idle-active", false, Duration::from_millis(600)).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_idle_active").await.unwrap();
    obs.execute("create table e_idle_active (a int)").await.unwrap();
    h.ok("insert into e_idle_active values (1)").await;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.ok("select 1").await;
    }
    h.ok("select pg_sleep(1)").await; // Active query exceeds the idle limit.
    h.commit().await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_idle_active").await, "1");
}

#[tokio::test]
async fn paused_result_expires_without_server_idle_timeout_and_keeps_fetched_rows() {
    let mut h = H::with_timeout("nbq-e-idle-paused", false, Duration::from_millis(700)).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_idle_paused").await.unwrap();
    obs.execute("create table e_idle_paused (a int)").await.unwrap();
    h.ok("insert into e_idle_paused values (1)").await;
    h.ok("set idle_in_transaction_session_timeout = 0").await; // Exercise app-side protection alone.
    let (q, events) = h.run("select i, repeat('x', 1000) from generate_series(1, 1000000) i").await;
    h.paused_at(q).await;
    let spool = spool_of(&events);
    h.wait(|e| matches!(e, Event::TransactionExpired { .. }).then_some(())).await;
    assert!(!h.txn.unwrap().in_txn);
    assert!(matches!(spool.progress().status, SpoolStatus::Stopped(_)));
    assert!(!spool.read(0..1).unwrap().is_empty());
    no_backends(&mut *obs, "nbq-e-idle-paused").await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_idle_paused").await, "0");
}

#[tokio::test]
async fn redshift_mode_idle_cleanup_cancels_instead_of_draining_an_abandoned_result() {
    let mut h = H::with_timeout("nbq-e-idle-redshift", false, Duration::from_millis(700)).await;
    h.params.kind = nbq_driver::DbKind::Redshift;
    h.engine.set_connection(h.tab, Some(h.params.clone()), false);
    h.wait(|e| matches!(e, Event::Conn { state: ConnState::Connected, .. }).then_some(())).await;
    let mut obs = h.observer().await;
    let (q, events) = h.run("select i, repeat('x', 1000) from generate_series(1, 10000000) i").await;
    h.paused_at(q).await;
    let spool = spool_of(&events);
    tokio::time::timeout(Duration::from_secs(4), h.wait(|e| matches!(e, Event::TransactionExpired { .. }).then_some(())))
        .await.expect("idle cleanup tried to drain a Redshift result");
    no_backends(&mut *obs, "nbq-e-idle-redshift").await;
    assert!(matches!(spool.progress().status, SpoolStatus::Stopped(_)));
}

#[tokio::test]
async fn abandoned_autocommit_result_releases_its_implicit_transaction() {
    let mut h = H::with_timeout("nbq-e-idle-autocommit", true, Duration::from_millis(700)).await;
    let mut obs = h.observer().await;
    let (q, events) = h.run("select i, repeat('x', 1000) from generate_series(1, 10000000) i").await;
    h.paused_at(q).await;
    let spool = spool_of(&events);
    h.wait(|e| matches!(e, Event::TransactionExpired { had_transaction: false, .. }).then_some(())).await;
    no_backends(&mut *obs, "nbq-e-idle-autocommit").await;
    assert!(matches!(spool.progress().status, SpoolStatus::Stopped(_)));
    assert!(!spool.read(0..1).unwrap().is_empty());
}

#[tokio::test]
async fn server_idle_timeout_updates_transaction_state_without_another_query() {
    let mut h = H::with_timeout("nbq-e-idle-server", false, Duration::from_secs(30)).await;
    h.ok("select 1").await;
    h.ok("set idle_in_transaction_session_timeout = '150ms'").await;
    tokio::time::timeout(Duration::from_secs(3), h.wait(|e| matches!(e, Event::TransactionExpired { .. }).then_some(())))
        .await.expect("UI state was not updated when the server terminated an idle transaction");
    assert!(!h.txn.unwrap().in_txn);
}

#[tokio::test]
async fn disconnect_cancels_a_running_query_and_rolls_back_the_transaction() {
    let mut h = H::new("nbq-e-disconnect-txn", false).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_disconnect_txn").await.unwrap();
    obs.execute("create table e_disconnect_txn (a int)").await.unwrap();
    h.ok("insert into e_disconnect_txn values (1)").await;
    let query = h.engine.execute(h.tab, vec!["select pg_sleep(30)".into()]);
    h.wait(|e| matches!(e, Event::StatementStarted { query: q, .. } if *q == query).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.engine.set_connection(h.tab, None, false);
    tokio::time::timeout(Duration::from_secs(4), h.wait(|e| matches!(e, Event::Conn { state: ConnState::Disconnected, .. }).then_some(())))
        .await.expect("disconnect waited for the running query");
    assert!(!h.txn.unwrap().in_txn);
    no_backends(&mut *obs, "nbq-e-disconnect-txn").await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_disconnect_txn").await, "0");
}

#[tokio::test]
async fn shutdown_waits_for_rollback_and_closes_tabs_and_metadata() {
    let mut h = H::new("nbq-e-shutdown-txn", false).await;
    let mut obs = h.observer().await;
    obs.execute("drop table if exists e_shutdown_txn").await.unwrap();
    obs.execute("create table e_shutdown_txn (a int)").await.unwrap();
    h.ok("insert into e_shutdown_txn values (1)").await;
    h.engine.connect_profile("c1", &h.params);
    h.wait(|e| matches!(e, Event::MetadataConn { state: ConnState::Connected, .. }).then_some(())).await;
    let second = h.engine.open_tab();
    h.engine.set_connection(second, Some(h.params.clone()), true);
    h.wait(|e| matches!(e, Event::Conn { tab, state: ConnState::Connected } if *tab == second).then_some(())).await;
    let q = h.engine.execute(second, vec!["select pg_sleep(30)".into()]);
    h.wait(|e| matches!(e, Event::StatementStarted { query, .. } if *query == q).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.engine.close_tab(second); // Shutdown must also wait for an already-closing tab.
    tokio::time::timeout(Duration::from_secs(6), h.engine.shutdown()).await.unwrap().unwrap();
    no_backends(&mut *obs, "nbq-e-shutdown-txn").await;
    assert_eq!(scalar(&mut *obs, "select count(*) from e_shutdown_txn").await, "0");
}
