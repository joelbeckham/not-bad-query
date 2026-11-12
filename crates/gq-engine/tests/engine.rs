//! Headless end-to-end tests of the engine against the dev Postgres.

use std::sync::Arc;
use std::time::Duration;

use gq_driver::{ConnectParams, Execution, Session};
use gq_engine::{
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
    fetch: std::collections::HashMap<QueryId, (gq_engine::Progress, bool)>,
}

impl H {
    async fn new(app: &str, autocommit: bool) -> H {
        let mut params = tokio::task::spawn_blocking(gq_testkit::pg).await.unwrap();
        params.application_name = app.into();
        let dir = tempfile::tempdir().unwrap();
        let (engine, rx) = Engine::new(EngineOptions::new(dir.path().join("spool")));
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
            if let Event::Conn { state: ConnState::Failed(err), .. } = &e {
                panic!("connection failed: {err}");
            }
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
        p.application_name = "gq-observer".into();
        gq_driver::connect(&p).await.unwrap()
    }
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
    gq_driver::display::cell_text(b.column(0), 0, &mut out);
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
    let mut h = H::new("gq-e-scroll", true).await;
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
    assert_eq!(active_queries(&mut *obs, "gq-e-scroll").await, "1");
}

#[tokio::test]
async fn export_fetches_the_rest_of_a_paused_result() {
    let mut h = H::new("gq-e-export", true).await;
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
    let mut h = H::new("gq-e-cancel", true).await;
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
    assert_eq!(active_queries(&mut *obs, "gq-e-cancel").await, "0");
    h.ok("select 1").await;
}

#[tokio::test]
async fn cancel_long_ddl() {
    let mut h = H::new("gq-e-cancel-ddl", true).await;
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
    let mut h = H::new("gq-e-stop", true).await;
    let (q, _) = h.run("select g from generate_series(1, 10000000) g").await;
    h.paused_at(q).await;
    h.engine.cancel(h.tab);
    let (rows, status) = h.ended(q).await;
    assert_eq!(rows, 500, "keeps what was loaded");
    assert_eq!(status, SpoolStatus::Stopped("closed".into()));
    h.ok("select 2").await;
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "gq-e-stop").await, "0");
}

#[tokio::test]
async fn new_query_closes_paused_result_on_server() {
    let mut h = H::new("gq-e-replace", true).await;
    let (q, _) = h.run("select g from generate_series(1, 10000000) g").await;
    h.paused_at(q).await;
    h.ok("select 'next'").await;
    let mut obs = h.observer().await;
    assert_eq!(active_queries(&mut *obs, "gq-e-replace").await, "0");
}

#[tokio::test]
async fn manual_commit_and_rollback() {
    let mut h = H::new("gq-e-txn", false).await;
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
    let mut h = H::new("gq-e-sp", false).await;
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
    let mut h = H::new("gq-e-sp-paused", false).await;
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
    let mut h = H::new("gq-e-script", true).await;
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
    let mut h = H::new("gq-e-returning", true).await;
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
    let mut h = H::new("gq-e-reconnect", true).await;
    h.ok("select 1").await;
    let mut obs = h.observer().await;
    scalar(
        &mut *obs,
        "select count(pg_terminate_backend(pid)) from pg_stat_activity where application_name = 'gq-e-reconnect'",
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
    let mut p = tokio::task::spawn_blocking(gq_testkit::pg).await.unwrap();
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
