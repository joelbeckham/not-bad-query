//! Object browser and autocomplete use permission-filtered MySQL metadata.
use std::fmt::Write;

use super::{CatalogTable, DriverError, ObjItem, ObjKind, ObjPath, Session, col, strings};

const USER_SCHEMAS: &str = "NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')";
const KINDS: [(ObjKind, &str); 2] = [(ObjKind::Table, "BASE TABLE"), (ObjKind::View, "VIEW")];

fn item(
    name: String,
    kind: ObjKind,
    detail: Option<String>,
    key: bool,
    path: Option<ObjPath>,
) -> ObjItem {
    ObjItem {
        name,
        kind,
        detail,
        key,
        has_children: path.is_some(),
        path,
    }
}

// MySQL string escaping depends on SQL mode. A hex literal works with and
// without NO_BACKSLASH_ESCAPES, even for names containing quotes or backslashes.
fn name_literal(name: &str) -> String {
    let mut out = String::from("CONVERT(X'");
    for byte in name.bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out.push_str("' USING utf8mb4)");
    out
}

pub(super) async fn list(s: &mut dyn Session, path: &ObjPath) -> Result<Vec<ObjItem>, DriverError> {
    match path {
        ObjPath::Root => {
            let rows = strings(s, &format!(
                "SELECT schema_name FROM information_schema.schemata WHERE schema_name {USER_SCHEMAS} ORDER BY schema_name"
            )).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let name = col(r, 0);
                    item(
                        name.clone(),
                        ObjKind::Schema,
                        None,
                        false,
                        Some(ObjPath::Schema(name)),
                    )
                })
                .collect())
        }
        ObjPath::Schema(schema) => {
            let schema_name = name_literal(schema);
            let counts = strings(
                s,
                &format!(
                    "SELECT table_type, COUNT(*) FROM information_schema.tables \
                 WHERE table_schema = {schema_name} GROUP BY table_type"
                ),
            )
            .await?;
            Ok(KINDS
                .into_iter()
                .map(|(kind, ty)| {
                    let count = counts
                        .iter()
                        .find(|r| col(r, 0) == ty)
                        .map_or_else(|| "0".into(), |r| col(r, 1));
                    item(
                        kind.plural().into(),
                        kind,
                        Some(count),
                        false,
                        Some(ObjPath::Folder {
                            schema: schema.clone(),
                            kind,
                        }),
                    )
                })
                .collect())
        }
        ObjPath::Folder { schema, kind } => {
            let Some((_, ty)) = KINDS.iter().find(|(candidate, _)| candidate == kind) else {
                return Ok(Vec::new());
            };
            let schema_name = name_literal(schema);
            let rows = strings(s, &format!(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = {schema_name} \
                 AND table_type = '{ty}' ORDER BY table_name"
            )).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let name = col(r, 0);
                    item(
                        name.clone(),
                        *kind,
                        None,
                        false,
                        Some(ObjPath::Object {
                            schema: schema.clone(),
                            name,
                            kind: *kind,
                        }),
                    )
                })
                .collect())
        }
        ObjPath::Object { schema, name, kind } => {
            let (schema, name) = (name_literal(schema), name_literal(name));
            let filter = format!("table_schema = {schema} AND table_name = {name}");
            let rows = strings(s, &format!(
                "SELECT column_name, column_type, is_nullable, column_key FROM information_schema.columns \
                 WHERE {filter} ORDER BY ordinal_position"
            )).await?;
            let mut out: Vec<_> = rows
                .iter()
                .map(|r| {
                    let ty = col(r, 1);
                    let detail = if col(r, 2) == "YES" {
                        format!("{ty}?")
                    } else {
                        ty
                    };
                    item(
                        col(r, 0),
                        ObjKind::Column,
                        Some(detail),
                        col(r, 3) == "PRI",
                        None,
                    )
                })
                .collect();
            if *kind == ObjKind::Table {
                let indexes = strings(s, &format!(
                    "SELECT DISTINCT index_name, non_unique, index_type FROM information_schema.statistics \
                     WHERE {filter} ORDER BY index_name"
                )).await?;
                out.extend(indexes.iter().map(|r| {
                    item(
                        col(r, 0),
                        ObjKind::Index,
                        r[2].clone(),
                        col(r, 1) == "0",
                        None,
                    )
                }));
                let triggers = strings(s, &format!(
                    "SELECT trigger_name, CONCAT(action_timing, ' ', event_manipulation) FROM information_schema.triggers \
                     WHERE event_object_schema = {schema} AND event_object_table = {name} ORDER BY trigger_name"
                )).await?;
                out.extend(
                    triggers
                        .iter()
                        .map(|r| item(col(r, 0), ObjKind::Trigger, r[1].clone(), false, None)),
                );
            }
            Ok(out)
        }
    }
}

pub(super) async fn catalog(s: &mut dyn Session) -> Result<Vec<CatalogTable>, DriverError> {
    let rows = strings(
        s,
        &format!(
            "SELECT t.table_schema, t.table_name, t.table_type, c.column_name, c.column_type \
         FROM information_schema.tables t LEFT JOIN information_schema.columns c \
         ON c.table_schema = t.table_schema AND c.table_name = t.table_name \
         WHERE t.table_schema {USER_SCHEMAS} AND t.table_type IN ('BASE TABLE', 'VIEW') \
         ORDER BY t.table_schema, t.table_name, c.ordinal_position"
        ),
    )
    .await?;
    let mut out: Vec<CatalogTable> = Vec::new();
    for r in rows {
        let (schema, name) = (col(&r, 0), col(&r, 1));
        if out
            .last()
            .is_none_or(|table| table.schema != schema || table.name != name)
        {
            let kind = if col(&r, 2) == "VIEW" {
                ObjKind::View
            } else {
                ObjKind::Table
            };
            out.push(CatalogTable {
                schema,
                name,
                kind,
                columns: Vec::new(),
            });
        }
        if let Some(column) = r[3].clone() {
            out.last_mut().unwrap().columns.push((column, col(&r, 4)));
        }
    }
    Ok(out)
}
