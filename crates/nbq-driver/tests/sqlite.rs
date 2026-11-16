use nbq_driver::{
    ConnectParams, DbKind, ErrorKind, Execution, Session, connect, create_database,
    display::cell_text,
};
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, ConnectParams) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sqlite-tests");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let p = ConnectParams::file(DbKind::Sqlite, dir.path().join("data ü.db"));
    (dir, p)
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
                        let mut v = String::new();
                        cell_text(a, row, &mut v).then_some(v)
                    })
                    .collect(),
            );
        }
    }
    out
}

#[tokio::test]
async fn explicit_creation_missing_invalid_and_no_clobber() {
    let (_dir, p) = fixture();
    assert!(connect(&p).await.is_err());
    assert!(!p.file_path.exists());
    create_database(p.kind, &p.file_path).unwrap();
    assert!(
        std::fs::read(&p.file_path)
            .unwrap()
            .starts_with(b"SQLite format 3\0")
    );
    let before = std::fs::read(&p.file_path).unwrap();
    assert!(create_database(p.kind, &p.file_path).is_err());
    assert_eq!(before, std::fs::read(&p.file_path).unwrap());
    let mut s = connect(&p).await.unwrap();
    assert_eq!(
        rows(&mut *s, "select 42").await[0][0].as_deref(),
        Some("42")
    );
    drop(s);
    std::fs::write(&p.file_path, "not a database").unwrap();
    assert!(connect(&p).await.is_err());
}

#[tokio::test]
async fn dynamic_values_nulls_blobs_empty_results_and_returning() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    s.execute("create table t(v)").await.unwrap();
    let returned = rows(&mut *s, "insert into t values (9223372036854775807), (1.25), ('hi ü'), (null), (x'00ff') returning v").await;
    assert_eq!(
        returned,
        vec![
            vec![Some("9223372036854775807".into())],
            vec![Some("1.25".into())],
            vec![Some("hi ü".into())],
            vec![None],
            vec![Some("\\x00ff".into())]
        ]
    );
    assert_eq!(rows(&mut *s, "select v from t").await, returned);
    assert!(rows(&mut *s, "select v from t where 0").await.is_empty());
    let e = s.execute("select * from missing").await.err().unwrap();
    assert_eq!(e.kind, ErrorKind::Query);
    assert_eq!(
        rows(&mut *s, "select count(*) from t").await[0][0].as_deref(),
        Some("5")
    );
}

#[tokio::test]
async fn pull_drop_cancel_and_session_reuse() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    let sql = "with recursive n(x) as (values(1) union all select x+1 from n where x<1000000000) select x from n";
    let Execution::Rows(mut stream) = s.execute(sql).await.unwrap() else {
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
    let Execution::Rows(mut stream) = s
        .execute(&sql.replace("select x from n", "select sum(x) from n"))
        .await
        .unwrap()
    else {
        panic!()
    };
    let cancel = s.cancel_handle();
    let task = tokio::spawn(async move { stream.next_batch(3, Duration::from_millis(10)).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancel.cancel().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
    assert_eq!(rows(&mut *s, "select 8").await[0][0].as_deref(), Some("8"));
    let Execution::Rows(stream) = s.execute(sql).await.unwrap() else {
        panic!()
    };
    drop(stream);
    assert_eq!(rows(&mut *s, "select 9").await[0][0].as_deref(), Some("9"));
}

#[tokio::test]
async fn transactions_savepoint_error_recovery_and_disconnect_rollback() {
    let (_dir, p) = fixture();
    create_database(p.kind, &p.file_path).unwrap();
    let mut s = connect(&p).await.unwrap();
    for sql in [
        "create table t(id integer primary key)",
        "begin",
        "insert into t values (1)",
        "savepoint nbq_sp",
    ] {
        s.execute(sql).await.unwrap();
    }
    assert!(s.execute("insert into t values (1)").await.is_err());
    s.execute("rollback to savepoint nbq_sp").await.unwrap();
    assert_eq!(
        rows(&mut *s, "select count(*) from t").await[0][0].as_deref(),
        Some("1")
    );
    s.execute("commit").await.unwrap();
    s.execute("begin").await.unwrap();
    s.execute("insert into t values (2)").await.unwrap();
    drop(s);
    let mut s = connect(&p).await.unwrap();
    assert_eq!(
        rows(&mut *s, "select count(*) from t").await[0][0].as_deref(),
        Some("1")
    );
}
