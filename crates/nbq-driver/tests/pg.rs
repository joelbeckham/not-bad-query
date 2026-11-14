//! Postgres driver against a real server (dev/docker-compose.yml).

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use nbq_driver::{ErrorKind, Execution, Session, connect, display::cell_text};

async fn session() -> Box<dyn Session> {
    let p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    connect(&p).await.expect("connect")
}

#[tokio::test]
async fn wide_results_stop_at_the_batch_byte_target_and_reject_huge_values() {
    let mut s = session().await;
    let Execution::Rows(mut stream) = s.execute("select repeat('x', 65536) from generate_series(1, 100)").await.unwrap() else { panic!("rows") };
    let first = stream.next_batch(100, Duration::from_secs(5)).await.unwrap().unwrap();
    assert!(first.num_rows() > 0 && first.num_rows() <= 64);
    let mut count = first.num_rows();
    while let Some(batch) = stream.next_batch(100, Duration::from_secs(5)).await.unwrap() { count += batch.num_rows(); }
    assert_eq!(count, 100);
    drop(stream);
    let Execution::Rows(mut stream) = s.execute("select repeat('x', 8388609)").await.unwrap() else { panic!("rows") };
    let error = stream.next_batch(1, Duration::from_secs(5)).await.unwrap_err();
    assert!(error.message.contains("8 MiB"));
    s.cancel_handle().cancel().await.unwrap();
    drop(stream);
    assert_eq!(scalar(&mut *s, "select 1").await.as_deref(), Some("1"));
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let Execution::Rows(mut st) = s.execute(sql).await.unwrap() else { panic!("expected rows") };
    let mut out = Vec::new();
    while let Some(b) = st.next_batch(1000, Duration::from_secs(5)).await.unwrap() {
        out.extend(batch_rows(&b));
    }
    out
}

fn batch_rows(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|c| {
                    let mut s = String::new();
                    cell_text(c, r, &mut s).then_some(s)
                })
                .collect()
        })
        .collect()
}

async fn scalar(s: &mut dyn Session, sql: &str) -> Option<String> {
    rows(s, sql).await.remove(0).remove(0)
}

/// Our rendering of each value must equal the type's own output function
/// (`format('%s', x)`; note `x::text` is a cast and differs for inet/bpchar).
#[tokio::test]
async fn values_render_like_postgres_text() {
    let mut s = session().await;
    s.execute("set intervalstyle = 'postgres'").await.unwrap();
    let exprs = [
        "1::int2", "-2::int4", "3::int8", "1.5::float4", "0.1::float4", "1e10::float4", "2.5e100::float8",
        "0.1::float8 + 0.2::float8", "'NaN'::float8", "'-Infinity'::float8", "1e-5::float8",
        "'x'::text", "'hé'::varchar", "'ab'::char(4)", "'a'::name", "1::oid", "'q'::\"char\"",
        "12.340::numeric", "-0.0001::numeric", "1e30::numeric", "'NaN'::numeric", "0::numeric", "1234567.891::numeric(20,5)",
        "'2024-01-02'::date", "'infinity'::date", "'0044-03-15 BC'::date",
        "'2024-01-02 03:04:05.5'::timestamp", "'-infinity'::timestamp", "'13:01:02.25'::time", "'13:01:02+05:30'::timetz",
        "'1 year 2 mons 3 days 04:05:06.7'::interval", "'-1 day'::interval", "'-1 mon -1 year'::interval",
        "'25 hours'::interval", "'1 day -1.5 sec'::interval", "'0'::interval",
        r#"'{"a": [1, 2], "b": null}'::jsonb"#, r#"'{"a":1}'::json"#,
        "'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid", r"'\xdead'::bytea", "''::bytea",
        "array[1,2,null]::int[]", "array['a b','c',null,'','NULL','x\"y']::text[]", "array[[1,2],[3,4]]",
        "'{}'::int[]", "'[0:1]={1,2}'::int[]", "array[1.5::numeric]", "array['2024-01-01'::date]",
        "'192.168.0.1/24'::inet", "'10.0.0.1'::inet", "'10.0.0.0/8'::cidr", "'::1'::inet", "'2001:db8::/32'::cidr",
        "'08:00:2b:01:02:03'::macaddr", "B'10101'", "B'1'::bit(3)",
        "int4range(1,5)", "'empty'::int4range", "numrange(1.5, null)", "tstzrange(null, null)",
        "row(1,'a b',null)", "row()", "'0/16B3748'::pg_lsn",
    ];
    let mut failures = Vec::new();
    for e in exprs {
        let r = rows(&mut *s, &format!("select {e}, format('%s', {e})")).await;
        let (ours, theirs) = (&r[0][0], &r[0][1]);
        if ours != theirs {
            failures.push(format!("{e}: ours={ours:?} pg={theirs:?}"));
        }
    }
    assert!(failures.is_empty(), "mismatches:\n{}", failures.join("\n"));
}

#[tokio::test]
async fn enums_and_domains() {
    let mut s = session().await;
    s.execute("drop schema if exists t_enum cascade").await.unwrap();
    s.execute("create schema t_enum").await.unwrap();
    s.execute("create type t_enum.mood as enum ('happy', 'sad')").await.unwrap();
    s.execute("create domain t_enum.pos as int check (value > 0)").await.unwrap();
    s.execute("create domain t_enum.label as text").await.unwrap();
    let r = rows(&mut *s, "select 'sad'::t_enum.mood, 5::t_enum.pos, 'x'::t_enum.label, array['happy']::t_enum.mood[]").await;
    assert_eq!(r[0], [Some("sad".into()), Some("5".into()), Some("x".into()), Some("{happy}".into())]);
}

#[tokio::test]
async fn schema_metadata_and_nulls() {
    let mut s = session().await;
    let Execution::Rows(st) = s.execute("select 1::int8 as id, null::text as name, 2.5::numeric n").await.unwrap() else {
        panic!()
    };
    let schema = st.schema();
    let f: Vec<_> = schema.fields().iter().map(|f| (f.name().clone(), f.metadata()[nbq_driver::meta::KIND].clone())).collect();
    assert_eq!(f, [("id".into(), "number".into()), ("name".into(), "text".into()), ("n".into(), "number".into())]);
    drop(st);
    assert_eq!(rows(&mut *s, "select null::int4").await[0][0], None);
}

#[tokio::test]
async fn dml_reports_rows_affected() {
    let mut s = session().await;
    s.execute("create temp table t (a int)").await.unwrap();
    match s.execute("insert into t select generate_series(1, 7)").await.unwrap() {
        Execution::Done { rows_affected, command } => {
            assert_eq!(rows_affected, Some(7));
            assert_eq!(command, "INSERT");
        }
        Execution::Rows(_) => panic!("expected Done"),
    }
    // RETURNING produces rows plus a count.
    let Execution::Rows(mut st) = s.execute("update t set a = a + 1 where a < 3 returning a").await.unwrap() else {
        panic!()
    };
    let mut n = 0;
    while let Some(b) = st.next_batch(100, Duration::from_secs(1)).await.unwrap() {
        n += b.num_rows();
    }
    assert_eq!(n, 2);
    assert_eq!(st.rows_affected(), Some(2));
}

#[tokio::test]
async fn errors_carry_code_and_position() {
    let mut s = session().await;
    let e = s.execute("select * from no_such_table").await.err().unwrap();
    assert_eq!(e.kind, ErrorKind::Query);
    assert_eq!(e.code.as_deref(), Some("42P01"));
    assert_eq!(e.position, Some(15));
    // Session stays usable.
    assert_eq!(scalar(&mut *s, "select 1").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn batches_respect_max_rows() {
    let mut s = session().await;
    let Execution::Rows(mut st) = s.execute("select g from generate_series(1, 2500) g").await.unwrap() else { panic!() };
    let mut sizes = Vec::new();
    while let Some(b) = st.next_batch(1000, Duration::from_secs(5)).await.unwrap() {
        sizes.push(b.num_rows());
    }
    assert_eq!(sizes, [1000, 1000, 500]);
}

/// A slow producer still delivers a partial batch at the soft deadline.
#[tokio::test]
async fn soft_deadline_returns_partial_batch() {
    let mut s = session().await;
    // Postgres flushes its output buffer every ~8KB, so produce enough rows to
    // force a flush before the slow row.
    let sql = "select g, repeat('x', 1000), case when g = 50 then pg_sleep(2) end from generate_series(1, 60) g";
    let Execution::Rows(mut st) = s.execute(sql).await.unwrap() else { panic!() };
    let t = Instant::now();
    let b = st.next_batch(1000, Duration::from_millis(200)).await.unwrap().unwrap();
    assert!(b.num_rows() < 60, "got all rows; deadline not honoured");
    assert!(t.elapsed() < Duration::from_millis(1500));
}

async fn activity(observer: &mut dyn Session, app: &str) -> Vec<Vec<Option<String>>> {
    rows(
        observer,
        &format!(
            "select state, wait_event, left(query, 40) from pg_stat_activity \
             where application_name = '{app}' and pid <> pg_backend_pid()"
        ),
    )
    .await
}

async fn session_named(app: &str) -> Box<dyn Session> {
    let mut p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    p.application_name = app.into();
    connect(&p).await.unwrap()
}

/// Not polling the stream leaves the server blocked on `ClientWrite`, i.e.
/// the result is paused on the server, not buffered in our memory.
#[tokio::test]
async fn unpolled_stream_pauses_server() {
    let app = "nbq-pause-test";
    let mut s = session_named(app).await;
    let mut obs = session().await;
    let Execution::Rows(mut st) =
        s.execute("select g, repeat('x', 200) from generate_series(1, 5000000) g").await.unwrap()
    else {
        panic!()
    };
    let b = st.next_batch(100, Duration::from_secs(5)).await.unwrap().unwrap();
    assert_eq!(b.num_rows(), 100);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let a = activity(&mut *obs, app).await;
        if a.iter().any(|r| r[0].as_deref() == Some("active") && r[1].as_deref() == Some("ClientWrite")) {
            break;
        }
        assert!(Instant::now() < deadline, "server never blocked on ClientWrite: {a:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Resuming works and continues where we left off.
    let b2 = st.next_batch(10, Duration::from_secs(5)).await.unwrap().unwrap();
    let mut first = String::new();
    cell_text(b2.column(0), 0, &mut first);
    assert_eq!(first, "101");
}

/// Cancel stops the statement on the server, and the session is reusable.
#[tokio::test]
async fn cancel_running_statement() {
    let app = "nbq-cancel-test";
    let mut s = session_named(app).await;
    let mut obs = session().await;
    let cancel = s.cancel_handle();
    let t = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let err = match s.execute("select pg_sleep(30)").await {
        Ok(Execution::Rows(mut st)) => st.next_batch(10, Duration::from_secs(60)).await.err().unwrap(),
        Ok(_) => panic!("expected rows"),
        Err(e) => e,
    };
    canceller.await.unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    let a = activity(&mut *obs, app).await;
    assert!(a.iter().all(|r| r[0].as_deref() != Some("active")), "still active on server: {a:?}");
    assert_eq!(scalar(&mut *s, "select 42").await.as_deref(), Some("42"));
}

/// Cancelling a paused stream ends it; the session is reusable afterwards.
#[tokio::test]
async fn cancel_paused_stream() {
    let mut s = session().await;
    let cancel: Arc<dyn nbq_driver::CancelHandle> = s.cancel_handle();
    let Execution::Rows(mut st) = s.execute("select g from generate_series(1, 50000000) g").await.unwrap() else {
        panic!()
    };
    st.next_batch(100, Duration::from_secs(5)).await.unwrap().unwrap();
    cancel.cancel().await.unwrap();
    // Drain what was already buffered; must end in a Cancelled error quickly.
    let t = Instant::now();
    let err = loop {
        match st.next_batch(100_000, Duration::from_secs(5)).await {
            Ok(Some(_)) => assert!(t.elapsed() < Duration::from_secs(10), "stream kept going after cancel"),
            Ok(None) => panic!("stream ended without error"),
            Err(e) => break e,
        }
    };
    assert_eq!(err.kind, ErrorKind::Cancelled);
    drop(st);
    assert_eq!(scalar(&mut *s, "select 'ok'").await.as_deref(), Some("ok"));
}
