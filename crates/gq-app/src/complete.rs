//! Object autocomplete: tables after FROM/JOIN, columns via aliases.
//! The "what goes here" logic is `gq_sql::completion_context` (unit-tested);
//! this file only matches it against the connection's catalog.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::*;
use gq_engine::meta::{CatalogTable, ObjKind};
use gq_sql::{Dialect, SplitOptions, Want, completion_context};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};

/// Catalogs by connection id, shared by every tab's completer.
pub type Catalogs = Rc<RefCell<HashMap<String, Arc<Vec<CatalogTable>>>>>;

pub struct SqlCompleter {
    pub catalogs: Catalogs,
    /// The owning tab's connection id.
    pub conn: Rc<RefCell<Option<String>>>,
}

const MAX_ITEMS: usize = 200;

/// Quote an identifier if it isn't a plain lowercase name.
fn ident(s: &str) -> String {
    let plain = s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { s.to_string() } else { format!("\"{}\"", s.replace('"', "\"\"")) }
}

fn rank(name: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(1);
    }
    let (n, p) = (name.to_lowercase(), prefix.to_lowercase());
    if n.starts_with(&p) {
        Some(0)
    } else if n.contains(&p) {
        Some(1)
    } else {
        None
    }
}

/// Matching items plus the byte range they replace (the identifier being typed).
pub fn items_for(tables: &[CatalogTable], text: &str, offset: usize) -> (Vec<CompletionItem>, std::ops::Range<usize>) {
    let Some(ctx) = completion_context(text, offset, SplitOptions::new(Dialect::Postgres)) else {
        return (Vec::new(), offset..offset);
    };
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let mut scored: Vec<(u8, CompletionItem)> = Vec::new();
    match &ctx.want {
        Want::Tables { schema } => {
            let mut schemas: Vec<&str> = Vec::new();
            for t in tables {
                if schema.as_deref().is_some_and(|s| !eq(s, &t.schema)) {
                    continue;
                }
                // Unqualified: public tables by bare name, others as schema.table.
                let label = if schema.is_some() || t.schema == "public" {
                    ident(&t.name)
                } else {
                    format!("{}.{}", ident(&t.schema), ident(&t.name))
                };
                if let Some(r) = rank(&t.name, &ctx.prefix).or_else(|| rank(&label, &ctx.prefix)) {
                    scored.push((
                        r,
                        CompletionItem {
                            label,
                            kind: Some(match t.kind {
                                ObjKind::View | ObjKind::MatView => CompletionItemKind::INTERFACE,
                                _ => CompletionItemKind::STRUCT,
                            }),
                            detail: Some(format!("{} · {} columns", t.schema, t.columns.len())),
                            ..Default::default()
                        },
                    ));
                }
                if schema.is_none() && !schemas.contains(&t.schema.as_str()) {
                    schemas.push(&t.schema);
                }
            }
            for s in schemas {
                if let Some(r) = rank(s, &ctx.prefix) {
                    scored.push((
                        r,
                        CompletionItem {
                            label: ident(s),
                            kind: Some(CompletionItemKind::MODULE),
                            detail: Some("schema".into()),
                            ..Default::default()
                        },
                    ));
                }
            }
        }
        Want::Columns { tables: refs, qualified } => {
            for r in refs {
                let found = tables.iter().filter(|t| {
                    eq(&t.name, &r.name) && r.schema.as_deref().is_none_or(|s| eq(s, &t.schema))
                });
                for t in found {
                    for (c, ty) in &t.columns {
                        if let Some(score) = rank(c, &ctx.prefix) {
                            let source = r.alias.clone().unwrap_or_else(|| t.name.clone());
                            scored.push((
                                score,
                                CompletionItem {
                                    label: ident(c),
                                    kind: Some(CompletionItemKind::FIELD),
                                    detail: Some(if *qualified { ty.clone() } else { format!("{source} · {ty}") }),
                                    ..Default::default()
                                },
                            ));
                        }
                    }
                }
            }
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.to_lowercase().cmp(&b.1.label.to_lowercase())));
    scored.dedup_by(|a, b| a.1.label == b.1.label && a.1.detail == b.1.detail);
    (scored.into_iter().take(MAX_ITEMS).map(|(_, i)| i).collect(), ctx.replace)
}

impl CompletionProvider for SqlCompleter {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let tables = self.conn.borrow().as_ref().and_then(|c| self.catalogs.borrow().get(c).cloned());
        let Some(tables) = tables else { return Task::ready(Ok(CompletionResponse::Array(Vec::new()))) };
        let (mut items, replace) = items_for(&tables, &text.to_string(), offset);
        // Replace exactly the identifier being typed (not a qualifier like `o.`).
        let range = lsp_types::Range { start: text.offset_to_position(replace.start), end: text.offset_to_position(replace.end) };
        for it in &mut items {
            it.text_edit = Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit { range, new_text: it.label.clone() }));
        }
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Vec<CatalogTable> {
        let t = |schema: &str, name: &str, cols: &[&str]| CatalogTable {
            schema: schema.into(),
            name: name.into(),
            kind: ObjKind::Table,
            columns: cols.iter().map(|c| (c.to_string(), "text".to_string())).collect(),
        };
        vec![
            t("shop", "orders", &["id", "status", "total"]),
            t("shop", "customers", &["id", "email"]),
            t("public", "Order Log", &["msg"]),
        ]
    }

    fn labels(marked: &str) -> Vec<String> {
        let c = marked.find('|').unwrap();
        let s = marked.replacen('|', "", 1);
        items_for(&cat(), &s, c).0.into_iter().map(|i| i.label).collect()
    }

    #[::core::prelude::v1::test]
    fn tables_and_schemas() {
        assert_eq!(labels("select * from ord|"), ["\"Order Log\"", "shop.orders"]);
        assert_eq!(labels("select * from shop.c|"), ["customers"]);
        assert!(labels("select * from |").contains(&"shop".to_string()));
    }

    #[::core::prelude::v1::test]
    fn columns_by_alias() {
        assert_eq!(labels("select o.| from shop.orders o"), ["id", "status", "total"]);
        assert_eq!(labels("select o.st| from shop.orders o"), ["status"]);
        assert_eq!(labels("select em| from shop.customers c"), ["email"]);
    }
}
