use std::time::Duration;

use nbq_driver::{ErrorKind, Execution, Session, SslMode, connect, display::cell_text};

async fn session() -> Box<dyn Session> {
    let params = tokio::task::spawn_blocking(nbq_testkit::mysql)
        .await
        .unwrap();
    connect(&params).await.unwrap()
}

async fn rows(session: &mut dyn Session, sql: &str) -> Box<dyn nbq_driver::RowStream> {
    let Execution::Rows(rows) = session.execute(sql).await.unwrap() else {
        panic!("expected rows")
    };
    rows
}

fn cell(batch: &arrow_array::RecordBatch, column: usize) -> Option<String> {
    let mut text = String::new();
    cell_text(batch.column(column), 0, &mut text).then_some(text)
}

#[tokio::test]
async fn streams_bounded_batches_and_preserves_mysql_values() {
    let mut session = session().await;
    let mut rows = rows(&mut *session,
        "SELECT CAST(18446744073709551615 AS UNSIGNED), CAST(12345678901234567890.0010 AS DECIMAL(24,4)), \
         UNHEX('00FF'), _utf8mb4'日本語', NULL, CAST('2024-02-29 03:25:45' AS DATETIME)"
    ).await;
    let row = rows
        .next_batch(1, Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.schema()
            .field(1)
            .metadata()
            .get(nbq_driver::meta::DB_TYPE)
            .map(String::as_str),
        Some("decimal"),
    );
    assert_eq!(cell(&row, 0).as_deref(), Some("18446744073709551615"));
    assert_eq!(cell(&row, 1).as_deref(), Some("12345678901234567890.0010"));
    assert_eq!(cell(&row, 2).as_deref(), Some(r"\x00ff"));
    assert_eq!(cell(&row, 3).as_deref(), Some("日本語"));
    assert_eq!(cell(&row, 4), None);
    assert_eq!(cell(&row, 5).as_deref(), Some("2024-02-29 03:25:45"));
    assert!(
        rows.next_batch(1, Duration::from_secs(1))
            .await
            .unwrap()
            .is_none()
    );
    drop(rows);

    let mut count = 0;
    let mut rows = self::rows(
        &mut *session,
        "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3",
    )
    .await;
    while let Some(batch) = rows.next_batch(1, Duration::from_secs(1)).await.unwrap() {
        assert_eq!(batch.num_rows(), 1);
        count += 1;
    }
    assert_eq!(count, 3);
}

#[tokio::test]
async fn statement_errors_keep_pending_writes_and_ddl_commits() {
    let mut session = session().await;
    session
        .execute("CREATE TEMPORARY TABLE mysql_driver_test (id int PRIMARY KEY)")
        .await
        .unwrap();
    session.execute("BEGIN").await.unwrap();
    assert_eq!(session.transaction_active(), Some(true));
    let result = session
        .execute("INSERT INTO mysql_driver_test VALUES (1), (2)")
        .await
        .unwrap();
    assert!(matches!(
        result,
        Execution::Done {
            rows_affected: Some(2),
            ..
        }
    ));
    let duplicate = session
        .execute("INSERT INTO mysql_driver_test VALUES (1)")
        .await
        .err()
        .unwrap();
    assert_eq!(duplicate.code.as_deref(), Some("1062"));
    assert_eq!(session.transaction_active(), Some(true));
    session.execute("ROLLBACK").await.unwrap();
    assert_eq!(session.transaction_active(), Some(false));
    let mut result = rows(&mut *session, "SELECT COUNT(*) FROM mysql_driver_test").await;
    assert_eq!(
        cell(
            &result
                .next_batch(1, Duration::from_secs(1))
                .await
                .unwrap()
                .unwrap(),
            0
        )
        .as_deref(),
        Some("0")
    );
    assert!(
        result
            .next_batch(1, Duration::from_secs(1))
            .await
            .unwrap()
            .is_none()
    );
    drop(result);
    session.execute("BEGIN").await.unwrap();
    session
        .execute("CREATE TABLE IF NOT EXISTS mysql_driver_ddl (id int)")
        .await
        .unwrap();
    assert_eq!(session.transaction_active(), Some(false));
    session
        .execute("DROP TABLE mysql_driver_ddl")
        .await
        .unwrap();
}

#[tokio::test]
async fn cancels_on_server_and_reuses_the_transaction() {
    let mut session = session().await;
    session
        .execute("CREATE TEMPORARY TABLE mysql_cancel_test (id INT)")
        .await
        .unwrap();
    session
        .execute("INSERT INTO mysql_cancel_test VALUES (1)")
        .await
        .unwrap();
    session.execute("BEGIN").await.unwrap();
    let cancel = session.cancel_handle();
    let cancel_task = async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    };
    let query = async {
        match session
            .execute("SELECT id FROM mysql_cancel_test WHERE SLEEP(30) = 0")
            .await
        {
            Err(error) => error,
            Ok(Execution::Rows(mut rows)) => rows
                .next_batch(1, Duration::from_secs(1))
                .await
                .unwrap_err(),
            Ok(_) => panic!("expected a cancelled statement"),
        }
    };
    let (error, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(query, cancel_task)
    })
    .await
    .unwrap();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(session.transaction_active(), Some(true));
    session.execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn require_encrypts_and_verify_full_rejects_untrusted_certificate() {
    let mut params = tokio::task::spawn_blocking(nbq_testkit::mysql)
        .await
        .unwrap();
    params.ssl = SslMode::Require;
    let mut session = connect(&params).await.unwrap();
    let mut result = rows(&mut *session, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    let batch = result
        .next_batch(1, Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert!(cell(&batch, 1).is_some_and(|cipher| !cipher.is_empty()));
    params.ssl = SslMode::VerifyFull;
    assert!(connect(&params).await.is_err());
}
