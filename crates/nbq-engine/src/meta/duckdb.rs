//! DuckDB introspection uses the current database's schemas and catalog functions.
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
    strings(s, &format!("SELECT table_name, table_type FROM information_schema.tables WHERE table_catalog = current_database() AND table_schema = {} ORDER BY table_name", quote_literal(schema))).await
}

async fn columns(
    s: &mut dyn Session,
    schema: &str,
    name: &str,
) -> Result<Vec<Vec<Option<String>>>, DriverError> {
    strings(s, &format!("SELECT c.column_name, c.data_type, c.is_nullable, EXISTS (SELECT 1 FROM duckdb_constraints() k WHERE k.database_name = c.table_catalog AND k.schema_name = c.table_schema AND k.table_name = c.table_name AND k.constraint_type = 'PRIMARY KEY' AND list_contains(k.constraint_column_names, c.column_name)) FROM information_schema.columns c WHERE c.table_catalog = current_database() AND c.table_schema = {} AND c.table_name = {} ORDER BY c.ordinal_position", quote_literal(schema), quote_literal(name))).await
}

pub(super) async fn list(s: &mut dyn Session, path: &ObjPath) -> Result<Vec<ObjItem>, DriverError> {
    Ok(match path {
        ObjPath::Root => strings(s, "SELECT schema_name FROM information_schema.schemata WHERE catalog_name = current_database() AND schema_name NOT IN ('information_schema', 'pg_catalog') ORDER BY schema_name").await?.iter().map(|r| {
            let name = col(r, 0);
            item(name.clone(), ObjKind::Schema, None, false, Some(ObjPath::Schema(name)))
        }).collect(),
        ObjPath::Schema(schema) => {
            let relations = relations(s, schema).await?;
            let mut out: Vec<_> = [(ObjKind::Table, "BASE TABLE"), (ObjKind::View, "VIEW")].iter().map(|(kind, ty)| {
                item(kind.plural().into(), *kind, Some(relations.iter().filter(|r| col(r, 1) == *ty || (*kind == ObjKind::Table && col(r, 1) == "LOCAL TEMPORARY")).count().to_string()), false, Some(ObjPath::Folder { schema: schema.clone(), kind: *kind }))
            }).collect();
            for (kind, source) in [(ObjKind::Sequence, "duckdb_sequences()"), (ObjKind::Function, "duckdb_functions()")] {
                let count = strings(s, &format!("SELECT count(*) FROM {source} WHERE database_name = current_database() AND schema_name = {}", quote_literal(schema))).await?;
                out.push(item(kind.plural().into(), kind, count.first().map(|r| col(r, 0)), false, Some(ObjPath::Folder { schema: schema.clone(), kind })));
            }
            out
        }
        ObjPath::Folder { schema, kind } => {
            match kind {
                ObjKind::Table | ObjKind::View => relations(s, schema).await?.iter().filter(|r| if *kind == ObjKind::View { col(r, 1) == "VIEW" } else { col(r, 1) != "VIEW" }).map(|r| {
                    let name = col(r, 0);
                    item(name.clone(), *kind, None, false, Some(ObjPath::Object { schema: schema.clone(), name, kind: *kind }))
                }).collect(),
                ObjKind::Sequence | ObjKind::Function => {
                    let (source, name, detail) = if *kind == ObjKind::Sequence { ("duckdb_sequences()", "sequence_name", "sql") } else { ("duckdb_functions()", "function_name", "coalesce(macro_definition, return_type)") };
                    strings(s, &format!("SELECT {name}, {detail} FROM {source} WHERE database_name = current_database() AND schema_name = {} ORDER BY 1", quote_literal(schema))).await?.iter().map(|r| item(col(r, 0), *kind, r[1].clone(), false, None)).collect()
                }
                _ => Vec::new(),
            }
        }
        ObjPath::Object { schema, name, kind } => {
            let mut out: Vec<_> = columns(s, schema, name).await?.iter().map(|r| {
                let ty = col(r, 1);
                item(col(r, 0), ObjKind::Column, Some(if col(r, 2) == "NO" { ty } else { format!("{ty}?") }), col(r, 3) == "true", None)
            }).collect();
            if *kind == ObjKind::Table {
                let indexes = strings(s, &format!("SELECT index_name, is_unique, sql FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = {} AND table_name = {} ORDER BY index_name", quote_literal(schema), quote_literal(name))).await?;
                out.extend(indexes.iter().map(|r| item(col(r, 0), ObjKind::Index, r[2].clone(), col(r, 1) == "true", None)));
            }
            out
        }
    })
}

pub(super) async fn catalog(s: &mut dyn Session) -> Result<Vec<CatalogTable>, DriverError> {
    let rows = strings(s, "SELECT t.table_schema, t.table_name, t.table_type, c.column_name, c.data_type FROM information_schema.tables t LEFT JOIN information_schema.columns c ON c.table_catalog = t.table_catalog AND c.table_schema = t.table_schema AND c.table_name = t.table_name WHERE t.table_catalog = current_database() AND t.table_schema NOT IN ('information_schema','pg_catalog') ORDER BY t.table_schema, t.table_name, c.ordinal_position").await?;
    let mut out: Vec<CatalogTable> = Vec::new();
    for r in rows {
        let (schema, name) = (col(&r, 0), col(&r, 1));
        if out
            .last()
            .is_none_or(|t| t.schema != schema || t.name != name)
        {
            out.push(CatalogTable {
                schema,
                name,
                kind: if col(&r, 2) == "VIEW" {
                    ObjKind::View
                } else {
                    ObjKind::Table
                },
                columns: Vec::new(),
            });
        }
        if let Some(column) = r[3].clone() {
            out.last_mut().unwrap().columns.push((column, col(&r, 4)));
        }
    }
    Ok(out)
}
