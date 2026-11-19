//! Object tree and catalog introspection against the dev Postgres.

use std::time::Duration;

use nbq_engine::meta::{ObjKind, ObjPath};
use nbq_engine::{ConnState, Engine, EngineOptions, Event};

async fn setup(schema: &str) -> nbq_driver::ConnectParams {
    let p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    let mut s = nbq_driver::connect(&p).await.unwrap();
    for sql in [
        "drop schema if exists t_meta cascade",
        "create schema t_meta",
        "create table t_meta.\"Weird \"\"Name\" (id int primary key, label text not null, note varchar(20))",
        "create index weird_label on t_meta.\"Weird \"\"Name\" (label)",
        "create view t_meta.v as select id from t_meta.\"Weird \"\"Name\"",
        "create function t_meta.f(a int, b text) returns int language sql as 'select a'",
        "create function t_meta.trg() returns trigger language plpgsql as $$ begin return new; end $$",
        "create trigger tr before insert on t_meta.\"Weird \"\"Name\" for each row execute function t_meta.trg()",
        "create sequence t_meta.seq",
    ] {
        let sql = sql.replace("t_meta", schema);
        s.execute(&sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    p
}

async fn objects(engine: &Engine, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, p: &nbq_driver::ConnectParams, path: ObjPath) -> Vec<nbq_engine::meta::ObjItem> {
    engine.load_objects("c1", p, path.clone());
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            Event::Objects { conn, path: got, items } if conn == "c1" && got == path => return items.unwrap(),
            _ => {}
        }
    }
}

#[tokio::test]
async fn object_tree_levels() {
    let p = setup("t_meta").await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().to_path_buf()));

    let schemas = objects(&engine, &mut rx, &p, ObjPath::Root).await;
    assert!(schemas.iter().any(|s| s.name == "t_meta" && s.kind == ObjKind::Schema));
    assert!(!schemas.iter().any(|s| s.name.starts_with("pg_") || s.name == "information_schema"));

    let cats = objects(&engine, &mut rx, &p, ObjPath::Schema("t_meta".into())).await;
    let summary: Vec<_> = cats.iter().map(|c| (c.name.as_str(), c.detail.as_deref())).collect();
    assert_eq!(
        summary,
        [("Tables", Some("1")), ("Views", Some("1")), ("Functions", Some("2")), ("Sequences", Some("1"))]
    );

    let tables = objects(&engine, &mut rx, &p, cats[0].path.clone().unwrap()).await;
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "Weird \"Name");

    let fns = objects(&engine, &mut rx, &p, ObjPath::Folder { schema: "t_meta".into(), kind: ObjKind::Function }).await;
    let names: Vec<_> = fns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["f(a integer, b text)", "trg()"]);

    let children = objects(&engine, &mut rx, &p, tables[0].path.clone().unwrap()).await;
    let got: Vec<_> = children.iter().map(|c| (c.kind, c.name.as_str(), c.detail.as_deref(), c.key)).collect();
    assert_eq!(got[0], (ObjKind::Column, "id", Some("integer"), true));
    assert_eq!(got[1], (ObjKind::Column, "label", Some("text"), false));
    assert_eq!(got[2], (ObjKind::Column, "note", Some("character varying(20)?"), false));
    assert!(got.iter().any(|c| c.0 == ObjKind::Index && c.1 == "weird_label"));
    assert!(got.iter().any(|c| c.0 == ObjKind::Index && c.3), "primary key index is unique");
    assert!(got.iter().any(|c| c.0 == ObjKind::Trigger && c.1 == "tr"));
}

#[tokio::test]
async fn catalog_lists_relations_with_columns() {
    let p = setup("t_meta_cat").await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().to_path_buf()));
    engine.load_catalog("c1", &p);
    let tables = loop {
        if let Event::Catalog { tables, .. } = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            break tables.unwrap();
        }
    };
    let t = tables.iter().find(|t| t.schema == "t_meta_cat" && t.name == "Weird \"Name").unwrap();
    let cols: Vec<_> = t.columns.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(cols, ["id", "label", "note"]);
    let v = tables.iter().find(|t| t.schema == "t_meta_cat" && t.name == "v").unwrap();
    assert_eq!(v.kind, ObjKind::View);
}

#[tokio::test]
async fn mysql_object_browser_and_catalog_handle_quoted_names() {
    let p = tokio::task::spawn_blocking(nbq_testkit::mysql).await.unwrap();
    let mut session = nbq_driver::connect(&p).await.unwrap();
    for sql in [
        "DROP VIEW IF EXISTS mysql_meta_view",
        "DROP TABLE IF EXISTS `mysql 'meta`",
        "CREATE TABLE `mysql 'meta` (id BIGINT UNSIGNED PRIMARY KEY, label VARCHAR(32), UNIQUE INDEX by_label (label))",
        "CREATE VIEW mysql_meta_view AS SELECT id FROM `mysql 'meta`",
    ] {
        session.execute(sql).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().into()));
    let schemas = objects(&engine, &mut rx, &p, ObjPath::Root).await;
    assert!(schemas.iter().any(|schema| schema.name == "nbq"));
    assert!(!schemas.iter().any(|schema| schema.name == "information_schema"));
    let categories = objects(&engine, &mut rx, &p, ObjPath::Schema("nbq".into())).await;
    let tables = objects(&engine, &mut rx, &p, categories[0].path.clone().unwrap()).await;
    let table = tables.iter().find(|table| table.name == "mysql 'meta").unwrap();
    let children = objects(&engine, &mut rx, &p, table.path.clone().unwrap()).await;
    assert!(children.iter().any(|column| column.kind == ObjKind::Column && column.name == "id" && column.key));
    assert!(children.iter().any(|index| index.kind == ObjKind::Index && index.name == "by_label" && index.key));

    engine.load_catalog("c1", &p);
    let catalog = loop {
        if let Event::Catalog { tables, .. } = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            break tables.unwrap();
        }
    };
    assert!(catalog.iter().any(|table| table.name == "mysql 'meta" && table.columns.contains(&("label".into(), "varchar(32)".into()))));
    assert!(catalog.iter().any(|table| table.name == "mysql_meta_view" && table.kind == ObjKind::View));
    session.execute("DROP VIEW mysql_meta_view").await.unwrap();
    session.execute("DROP TABLE `mysql 'meta`").await.unwrap();
}

async fn metadata_state(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> ConnState {
    loop {
        if let Event::MetadataConn { conn, state } = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            assert_eq!(conn, "c1");
            return state;
        }
    }
}

#[tokio::test]
async fn explicit_connect_reports_authentication_failure_and_recovers() {
    let p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    let mut invalid = p.clone();
    invalid.password = Some("nbq-intentionally-incorrect-password".into());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let dir = tempfile::tempdir_in(root).unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().into()));
    engine.connect_profile("c1", &invalid);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    let ConnState::Failed(error) = metadata_state(&mut rx).await else { panic!("expected failed authentication") };
    assert_eq!(error.kind, nbq_driver::ErrorKind::Connection);
    assert_eq!(error.code.as_deref(), Some("28P01"));
    assert!(error.message.contains("password authentication failed"), "{error}");

    engine.connect_profile("c1", &p);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Disconnected);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connected);
    assert!(!objects(&engine, &mut rx, &p, ObjPath::Root).await.is_empty());
}

#[tokio::test]
async fn explicit_connect_disconnect_and_reconnect_releases_backend() {
    use nbq_driver::{Execution, display::cell_text};
    let mut p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    p.application_name = "nbq-meta-disconnect-test".into();
    let mut observer = nbq_driver::connect(&p).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().into()));
    engine.connect_profile("c1", &p);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connected);
    // Browsing reuses the same backend.
    objects(&engine, &mut rx, &p, ObjPath::Root).await;
    let count_sql = "select count(*) from pg_stat_activity where application_name = 'nbq-meta-disconnect-test (metadata)'";
    let count = async |observer: &mut Box<dyn nbq_driver::Session>| {
        let Execution::Rows(mut rows) = observer.execute(count_sql).await.unwrap() else { panic!("expected rows") };
        let batch = rows.next_batch(10, Duration::from_secs(1)).await.unwrap().unwrap();
        let mut text = String::new();
        cell_text(batch.column(0), 0, &mut text);
        text
    };
    assert_eq!(count(&mut observer).await, "1");
    engine.forget_connection("c1");
    assert_eq!(metadata_state(&mut rx).await, ConnState::Disconnected);
    tokio::time::timeout(Duration::from_secs(5), async {
        while count(&mut observer).await != "0" { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.expect("metadata backend remained connected");
    engine.connect_profile("c1", &p);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connected);
}

#[tokio::test]
async fn disconnect_cancels_pending_metadata_and_ignores_retired_results() {
    let mut p = tokio::task::spawn_blocking(nbq_testkit::pg).await.unwrap();
    let real = p.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    p.host = "127.0.0.1".into();
    p.port = listener.local_addr().unwrap().port();
    p.ssl = nbq_driver::SslMode::Disable;
    p.connect_timeout = Duration::from_secs(30);
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut rx) = Engine::new(EngineOptions::new(dir.path().into()));
    engine.load_objects("c1", &p, ObjPath::Root);
    let (socket, _) = listener.accept().await.unwrap(); // Hold an unresponsive server open.
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    engine.load_catalog("c1", &p); // Also cancel queued work behind the connect.
    engine.forget_connection("c1");
    assert_eq!(metadata_state(&mut rx).await, ConnState::Disconnected);
    drop(socket);
    assert!(tokio::time::timeout(Duration::from_millis(150), rx.recv()).await.is_err(), "retired request published a stale result");
    engine.load_objects("c1", &real, ObjPath::Root);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connecting);
    assert_eq!(metadata_state(&mut rx).await, ConnState::Connected);
    loop {
        if let Event::Objects { items, .. } = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            assert!(items.is_ok());
            break;
        }
    }
}
