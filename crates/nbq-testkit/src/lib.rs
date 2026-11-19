//! Shared fixtures for integration tests.
//!
//! [`pg()`] makes sure the dev Postgres from `dev/docker-compose.yml` is up
//! (idempotent; fast when already running) and returns params for it.
//! Requires Docker: if it isn't running, tests fail loudly instead of being
//! skipped.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use nbq_driver::{ConnectParams, DbKind, SslMode};

pub const PG_PORT: u16 = 55432;
pub const MYSQL_PORT: u16 = 33306;

fn compose_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/docker-compose.yml")
}

fn ensure_service(service: &str) {
    let docker_ok = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(docker_ok, "Docker is not running. Start Docker Desktop and re-run the tests.");
    let out = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose_file())
        .args(["up", "-d", "--wait", service])
        .output()
        .expect("run docker compose");
    assert!(
        out.status.success(),
        "docker compose up {service} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Connection params for the dev Postgres 17 (port 55432).
pub fn pg() -> ConnectParams {
    static UP: OnceLock<()> = OnceLock::new();
    UP.get_or_init(|| ensure_service("postgres"));
    ConnectParams {
        kind: DbKind::Postgres,
        file_path: Default::default(),
        host: "127.0.0.1".into(),
        port: PG_PORT,
        database: "postgres".into(),
        user: "postgres".into(),
        password: Some("postgres".into()),
        ssl: SslMode::Disable,
        ssl_root_cert: String::new(),
        vpn_required: false,
        connect_timeout: Duration::from_secs(10),
        application_name: "nbq-test".into(),
    }
}

/// Connection params for dev MySQL 8.4 using a regular database user.
pub fn mysql() -> ConnectParams {
    static UP: OnceLock<()> = OnceLock::new();
    UP.get_or_init(|| ensure_service("mysql"));
    ConnectParams {
        kind: DbKind::MySql,
        file_path: Default::default(),
        host: "127.0.0.1".into(),
        port: MYSQL_PORT,
        database: "nbq".into(),
        user: "nbq".into(),
        password: Some("nbq".into()),
        ssl: SslMode::Disable,
        ssl_root_cert: String::new(),
        vpn_required: false,
        connect_timeout: Duration::from_secs(10),
        application_name: "nbq-test".into(),
    }
}
