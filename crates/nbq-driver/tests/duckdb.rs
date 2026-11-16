use nbq_driver::{
    ConnectParams, DbKind, ErrorKind, Execution, Session, connect, create_database,
    display::cell_text,
};
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, ConnectParams) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/duckdb-tests");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let params = ConnectParams::file(DbKind::DuckDb, dir.path().join("data ü.duckdb"));
    (dir, params)
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let Execution::Rows(mut stream) = s.execute(sql).await.unwrap() else {
        panic!("expected rows")
    };
    let mut out = Vec::new();
    while let Some(batch) = stream.next_batch(2, Duration::from_secs(1)).await.unwrap() {
        for row in 0..batch.num_rows() {
            out.push(
                batch
                    .columns()
                    .iter()
                    .map(|a| {
                        let mut value = String::new();
                        cell_text(a, row, &mut value).then_some(value)
                    })
                    .collect(),
            );
        }
    }
    out
}

#[tokio::test]
async fn explicit_creation_open_validation_no_clobber_and_reopen() {
    let (_dir, p) = fixture();
    assert!(connect(&p).await.is_err());
    assert!(!p.file_path.exists());
    create_database(p.kind, &p.file_path).unwrap();
    let before = std::fs::read(&p.file_path).unwrap();
    assert_eq!(&before[8..12], b"DUCK");
    assert!(create_database(p.kind, &p.file_path).is_err());
    assert_eq!(before, std::fs::read(&p.file_path).unwrap());
    let mut s = connect(&p).await.unwrap();
    s.execute("create table t(id integer)").await.unwrap();
    s.execute("insert into t values (42)").await.unwrap();
    drop(s);
    let mut s = connect(&p).await.unwrap();
    assert_eq!(
        rows(&mut *s, "select * from t").await[0][0].as_deref(),
        Some("42")
    );
    let invalid = p.file_path.with_extension("invalid");
    std::fs::write(&invalid, b"not a DuckDB database").unwrap();
    let mut p = p;
    p.file_path = invalid.clone();
    assert!(connect(&p).await.is_err());
    assert_eq!(std::fs::read(invalid).unwrap(), b"not a DuckDB database");
}

#[tokio::test]
async fn independent_sessions_share_database_and_transactions_stay_local() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut first = connect(&p).await.unwrap();
    first.execute("create table t(id integer)").await.unwrap();
    let mut alias = p.clone();
    alias.file_path = p
        .file_path
        .parent()
        .unwrap()
        .join(".")
        .join(p.file_path.file_name().unwrap());
    let mut second = connect(&alias).await.unwrap();
    first.execute("begin").await.unwrap();
    first.execute("insert into t values (1)").await.unwrap();
    assert_eq!(
        rows(&mut *first, "select count(*) from t").await[0][0].as_deref(),
        Some("1")
    );
    assert_eq!(
        rows(&mut *second, "select count(*) from t").await[0][0].as_deref(),
        Some("0")
    );
    first.execute("commit").await.unwrap();
    assert_eq!(
        rows(&mut *second, "select count(*) from t").await[0][0].as_deref(),
        Some("1")
    );
    first.execute("begin").await.unwrap();
    first.execute("insert into t values (2)").await.unwrap();
    drop(first);
    assert_eq!(
        rows(&mut *second, "select count(*) from t").await[0][0].as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn types_nulls_empty_results_returning_and_counts() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    let Execution::Rows(types) = s
        .execute("select 12.34::decimal(10,2) as amount, [1,2,3] as items")
        .await
        .unwrap()
    else {
        panic!()
    };
    let schema = types.schema();
    assert_eq!(
        schema
            .field(0)
            .metadata()
            .get(nbq_driver::meta::DB_TYPE)
            .unwrap(),
        "DECIMAL(10,2)"
    );
    assert_eq!(
        schema
            .field(1)
            .metadata()
            .get(nbq_driver::meta::DB_TYPE)
            .unwrap(),
        "INTEGER[]"
    );
    drop(types);
    let values = rows(&mut *s, "select 18446744073709551615::ubigint, 12.34::decimal(10,2), date '2026-11-15', [1,2,3], {'label':'hi'}, true, null::integer").await;
    assert_eq!(values[0][0].as_deref(), Some("18446744073709551615"));
    assert_eq!(values[0][1].as_deref(), Some("12.34"));
    assert_eq!(values[0][2].as_deref(), Some("2026-11-15"));
    assert_eq!(values[0][3].as_deref(), Some("[1, 2, 3]"));
    assert!(values[0][4].as_ref().unwrap().contains("hi"));
    assert_eq!(values[0][5].as_deref(), Some("true"));
    assert_eq!(values[0][6], None);
    s.execute("create table t(id integer)").await.unwrap();
    assert!(matches!(
        s.execute("insert into t values (1),(2)").await.unwrap(),
        Execution::Done {
            rows_affected: Some(2),
            ..
        }
    ));
    assert_eq!(
        rows(&mut *s, "update t set id = id + 10 returning id")
            .await
            .len(),
        2
    );
    assert!(
        rows(&mut *s, "select * from t where false")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn paused_stream_cancels_without_draining_and_session_is_reusable() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    s.execute("begin").await.unwrap();
    let Execution::Rows(mut stream) = s.execute("select * from range(1000000000)").await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        stream
            .next_batch(3, Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap()
            .num_rows(),
        3
    );
    s.cancel_handle().cancel().await.unwrap();
    assert_eq!(
        stream
            .next_batch(3, Duration::from_secs(1))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    drop(stream);
    assert_eq!(rows(&mut *s, "select 7").await[0][0].as_deref(), Some("7"));
    assert_eq!(s.transaction_active(), Some(true));
    s.execute("rollback").await.unwrap();
    let Execution::Rows(stream) = s.execute("select * from range(1000000000)").await.unwrap()
    else {
        panic!()
    };
    drop(stream);
    assert_eq!(rows(&mut *s, "select 8").await[0][0].as_deref(), Some("8"));
}

#[tokio::test]
async fn interrupt_busy_query_and_recover_aborted_transaction() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    let cancel = s.cancel_handle();
    let task = tokio::spawn(async move {
        let result = match s
            .execute("select sum(a.i * b.i) from range(1000000) a(i), range(1000000) b(i)")
            .await
        {
            Ok(Execution::Rows(mut stream)) => stream
                .next_batch(1, Duration::from_secs(1))
                .await
                .map(|_| ()),
            Ok(Execution::Done { .. }) => Ok(()),
            Err(error) => Err(error),
        };
        (s, result)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel().await.unwrap();
    let (mut s, result) = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    s.execute("create table t(id integer primary key)")
        .await
        .unwrap();
    s.execute("begin").await.unwrap();
    s.execute("insert into t values (1)").await.unwrap();
    assert!(s.execute("select (").await.is_err());
    assert_eq!(s.transaction_active(), Some(true));
    let error = s.execute("insert into t values (1)").await.err().unwrap();
    assert_eq!(error.kind, ErrorKind::Query);
    assert_eq!(s.transaction_active(), Some(false));
    assert_eq!(
        rows(&mut *s, "select count(*) from t").await[0][0].as_deref(),
        Some("0")
    );
}

#[tokio::test]
async fn value_limits_fail_cleanly_and_preserve_session() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    let Execution::Rows(mut stream) = s
        .execute("select repeat('x', 9 * 1024 * 1024)")
        .await
        .unwrap()
    else {
        panic!()
    };
    let error = stream
        .next_batch(1, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.message.contains("8 MiB"));
    drop(stream);
    assert_eq!(rows(&mut *s, "select 9").await[0][0].as_deref(), Some("9"));
}
