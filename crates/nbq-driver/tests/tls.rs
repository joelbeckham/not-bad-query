//! Real certificate-chain, hostname, and cancellation checks against PostgreSQL.
#![cfg(windows)]
use std::process::Command;
use std::time::Duration;
use nbq_driver::{ConnectParams, DbKind, ErrorKind, Execution, SslMode, connect};

struct Fixture(String);
impl Drop for Fixture {
    fn drop(&mut self) { let _ = Command::new("docker").args(["stop", &self.0]).output(); }
}

#[tokio::test]
async fn verified_tls_checks_chain_hostname_and_cancel_connection() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join(format!("target/tls-{}", std::process::id()));
    let script = root.join("dev/start-postgres-tls.ps1");
    let output = tokio::task::spawn_blocking(move || Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script)
        .arg("-Fixture").arg(dir).output().unwrap()).await.unwrap();
    assert!(output.status.success(), "TLS fixture: {}", String::from_utf8_lossy(&output.stderr));
    let output = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 3, "{output}");
    let _fixture = Fixture(lines[0].trim().into());
    let mut params = ConnectParams {
        kind: DbKind::Postgres, host: "localhost".into(), port: lines[1].trim().parse().unwrap(),
        database: "postgres".into(), user: "postgres".into(), password: Some("postgres".into()),
        ssl: SslMode::VerifyFull, ssl_root_cert: String::new(), connect_timeout: Duration::from_secs(5),
        application_name: "nbq-tls-check".into(),
    };
    assert!(connect(&params).await.is_err(), "untrusted private CA must fail");
    params.ssl_root_cert = std::fs::read_to_string(lines[2].trim()).unwrap();
    let mut session = connect(&params).await.expect("custom CA validates");
    let Execution::Rows(mut stream) = session.execute("select 1").await.unwrap() else { panic!("rows") };
    assert!(stream.next_batch(1, Duration::from_secs(1)).await.unwrap().is_some());
    while stream.next_batch(1, Duration::from_secs(1)).await.unwrap().is_some() {}
    drop(stream);
    params.host = "127.0.0.1".into();
    assert!(connect(&params).await.is_err(), "hostname mismatch must fail");
    let cancel = session.cancel_handle();
    let running = tokio::spawn(async move {
        let error = match session.execute("select pg_sleep(30)").await {
            Err(error) => error,
            Ok(Execution::Rows(mut stream)) => stream.next_batch(1, Duration::from_secs(1)).await.unwrap_err(),
            _ => panic!("expected rows"),
        };
        (session, error)
    });
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancel.cancel().await.expect("cancel connection validates the same CA and hostname");
    let (mut session, error) = tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    let Execution::Rows(mut stream) = session.execute("select 1").await.unwrap() else { panic!("rows") };
    assert!(stream.next_batch(1, Duration::from_secs(1)).await.unwrap().is_some());
}
