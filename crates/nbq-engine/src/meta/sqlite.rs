//! SQLite introspection, separate from the PostgreSQL catalog queries.
use super::*;

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

async fn relations(
    s: &mut dyn Session,
    schema: &str,
) -> Result<Vec<Vec<Option<String>>>, DriverError> {
    strings(s, &format!("SELECT name, type FROM {}.sqlite_schema WHERE type IN ('table','view') AND substr(name,1,7) <> 'sqlite_' ORDER BY name", quote_ident(schema))).await
}

async fn columns(
    s: &mut dyn Session,
    schema: &str,
    name: &str,
) -> Result<Vec<Vec<Option<String>>>, DriverError> {
    strings(s, &format!("SELECT name, type, \"notnull\", pk FROM pragma_table_xinfo({}, {}) WHERE hidden <> 1 ORDER BY cid", quote_literal(name), quote_literal(schema))).await
}

pub(super) async fn list(s: &mut dyn Session, path: &ObjPath) -> Result<Vec<ObjItem>, DriverError> {
    Ok(match path {
        ObjPath::Root => strings(s, "SELECT name FROM pragma_database_list ORDER BY seq")
            .await?
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
            .collect(),
        ObjPath::Schema(schema) => {
            let relations = relations(s, schema).await?;
            [(ObjKind::Table, "table"), (ObjKind::View, "view")]
                .iter()
                .map(|(kind, ty)| {
                    item(
                        kind.plural().into(),
                        *kind,
                        Some(
                            relations
                                .iter()
                                .filter(|r| col(r, 1) == *ty)
                                .count()
                                .to_string(),
                        ),
                        false,
                        Some(ObjPath::Folder {
                            schema: schema.clone(),
                            kind: *kind,
                        }),
                    )
                })
                .collect()
        }
        ObjPath::Folder { schema, kind } => {
            let ty = match kind {
                ObjKind::Table => "table",
                ObjKind::View => "view",
                _ => return Ok(Vec::new()),
            };
            relations(s, schema)
                .await?
                .iter()
                .filter(|r| col(r, 1) == ty)
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
                .collect()
        }
        ObjPath::Object { schema, name, .. } => {
            let mut out: Vec<_> = columns(s, schema, name)
                .await?
                .iter()
                .map(|r| {
                    let ty = col(r, 1);
                    let key = col(r, 3) != "0";
                    item(
                        col(r, 0),
                        ObjKind::Column,
                        Some(if col(r, 2) == "1" || key {
                            ty
                        } else {
                            format!("{ty}?")
                        }),
                        key,
                        None,
                    )
                })
                .collect();
            let indexes = strings(
                s,
                &format!(
                    "SELECT name, \"unique\" FROM pragma_index_list({}, {}) ORDER BY name",
                    quote_literal(name),
                    quote_literal(schema)
                ),
            )
            .await?;
            out.extend(
                indexes
                    .iter()
                    .map(|r| item(col(r, 0), ObjKind::Index, None, col(r, 1) == "1", None)),
            );
            let triggers = strings(s, &format!("SELECT name, sql FROM {}.sqlite_schema WHERE type = 'trigger' AND tbl_name = {} ORDER BY name", quote_ident(schema), quote_literal(name))).await?;
            out.extend(
                triggers
                    .iter()
                    .map(|r| item(col(r, 0), ObjKind::Trigger, r[1].clone(), false, None)),
            );
            out
        }
    })
}

pub(super) async fn catalog(s: &mut dyn Session) -> Result<Vec<CatalogTable>, DriverError> {
    let schemas = strings(s, "SELECT name FROM pragma_database_list ORDER BY seq").await?;
    let mut out = Vec::new();
    for row in schemas {
        let schema = col(&row, 0);
        for row in relations(s, &schema).await? {
            let name = col(&row, 0);
            let kind = if col(&row, 1) == "view" {
                ObjKind::View
            } else {
                ObjKind::Table
            };
            let columns = columns(s, &schema, &name)
                .await?
                .iter()
                .map(|r| (col(r, 0), col(r, 1)))
                .collect();
            out.push(CatalogTable {
                schema: schema.clone(),
                name,
                kind,
                columns,
            });
        }
    }
    Ok(out)
}
