//! Object tree and catalog introspection against the dev Postgres.

use std::time::Duration;

use gq_engine::meta::{ObjKind, ObjPath};
use gq_engine::{Engine, EngineOptions, Event};

async fn setup(schema: &str) -> gq_driver::ConnectParams {
    let p = tokio::task::spawn_blocking(gq_testkit::pg).await.unwrap();
    let mut s = gq_driver::connect(&p).await.unwrap();
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

async fn objects(engine: &Engine, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, p: &gq_driver::ConnectParams, path: ObjPath) -> Vec<gq_engine::meta::ObjItem> {
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
