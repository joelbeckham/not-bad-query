//! Autocomplete context: what kind of object belongs at the cursor.
//!
//! Heuristic and tolerant of incomplete SQL (the user is typing). It never
//! parses SQL properly; it looks at the tokens around the cursor and the
//! FROM/JOIN clauses of the current statement.

use std::ops::Range;

use crate::{Dialect, SplitOptions, statement_at};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// Tables/views, optionally within a schema.
    Tables { schema: Option<String> },
    /// Columns of these tables (all tables in scope when unqualified).
    Columns { tables: Vec<TableRef>, qualified: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    pub want: Want,
    /// What the user has typed of the name so far (unquoted).
    pub prefix: String,
    /// Byte range in the buffer that a chosen completion replaces.
    pub replace: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident { text: String, quoted: bool },
    Punct(char),
    Other,
}

#[derive(Debug, Clone)]
struct T {
    tok: Tok,
    start: usize,
    end: usize,
}

fn lex(sql: &str, d: Dialect) -> Vec<T> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    while i < n {
        let c = b[i];
        let s = i;
        match c {
            c if c.is_ascii_whitespace() => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
            }
            b'#' if d.hash_comments() => {
                i = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = sql[i + 2..].find("*/").map(|p| i + 2 + p + 2).unwrap_or(n);
            }
            b'\'' => {
                i += 1;
                while i < n {
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push(T { tok: Tok::Other, start: s, end: i });
            }
            b'"' | b'`' => {
                let q = c;
                i += 1;
                let mut text = String::new();
                let mut closed = false;
                while i < n {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            text.push(q as char);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        closed = true;
                        break;
                    }
                    let ch = sql[i..].chars().next().unwrap();
                    text.push(ch);
                    i += ch.len_utf8();
                }
                let _ = closed;
                out.push(T { tok: Tok::Ident { text, quoted: true }, start: s, end: i });
            }
            c if ident(c) && !c.is_ascii_digit() => {
                while i < n && ident(b[i]) {
                    i += 1;
                }
                out.push(T { tok: Tok::Ident { text: sql[s..i].to_string(), quoted: false }, start: s, end: i });
            }
            b'.' | b',' | b'(' | b')' | b';' => {
                i += 1;
                out.push(T { tok: Tok::Punct(c as char), start: s, end: i });
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap();
                i += ch.len_utf8();
                out.push(T { tok: Tok::Other, start: s, end: i });
            }
        }
    }
    out
}

const STOP: &[&str] = &[
    "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "ON", "USING", "GROUP", "ORDER", "LIMIT",
    "HAVING", "UNION", "SET", "VALUES", "SELECT", "NATURAL", "LATERAL", "WINDOW", "OFFSET", "FETCH", "FOR",
    "RETURNING", "EXCEPT", "INTERSECT", "AS", "WITH", "FROM", "AND", "OR", "NOT", "DEFAULT", "QUALIFY",
];

fn kw(t: &T, k: &str) -> bool {
    matches!(&t.tok, Tok::Ident { text, quoted: false } if text.eq_ignore_ascii_case(k))
}

fn is_stop(t: &T) -> bool {
    STOP.iter().any(|k| kw(t, k))
}

fn name(t: &T) -> Option<String> {
    match &t.tok {
        Tok::Ident { text, quoted: true } => Some(text.clone()),
        Tok::Ident { text, quoted: false } if !is_stop(t) => Some(text.clone()),
        _ => None,
    }
}

/// Keywords after which a table name is expected.
fn introduces_table(t: &T) -> bool {
    ["FROM", "JOIN", "UPDATE", "INTO", "TABLE"].iter().any(|k| kw(t, k))
}

/// Table references in FROM/JOIN/UPDATE/INTO clauses of the statement.
fn table_refs(toks: &[T]) -> Vec<TableRef> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if !introduces_table(&toks[i]) {
            i += 1;
            continue;
        }
        let in_from = kw(&toks[i], "FROM");
        i += 1;
        loop {
            // [schema .] name
            let Some(first) = toks.get(i).and_then(name) else { break };
            i += 1;
            let (schema, tname) = if matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Punct('.'))) {
                match toks.get(i + 1).and_then(name) {
                    Some(n) => {
                        i += 2;
                        (Some(first), n)
                    }
                    None => {
                        i += 1;
                        break;
                    }
                }
            } else {
                (None, first)
            };
            // [AS] alias
            if toks.get(i).is_some_and(|t| kw(t, "AS")) {
                i += 1;
            }
            let alias = toks.get(i).and_then(name).inspect(|_| i += 1);
            out.push(TableRef { schema, name: tname, alias });
            if in_from && matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Punct(','))) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// Work out what to complete at byte offset `cursor` of `buffer`.
pub fn completion_context(buffer: &str, cursor: usize, opts: SplitOptions) -> Option<CompletionContext> {
    let cursor = cursor.min(buffer.len());
    if !buffer.is_char_boundary(cursor) {
        return None;
    }
    // Restrict to the current statement. If the cursor is in whitespace past
    // the last statement, use the text from its start.
    let stmt = statement_at(buffer, cursor, opts);
    let base = stmt.as_ref().map(|s| s.range.start.min(cursor)).unwrap_or_else(|| {
        buffer[..cursor].rfind(';').map(|p| p + 1).unwrap_or(0)
    });
    let end = stmt.as_ref().map(|s| s.range.end.max(cursor)).unwrap_or(cursor);
    let text = &buffer[base..end];
    let rel = cursor - base;
    let toks = lex(text, opts.dialect);

    // Inside a string or comment: nothing to complete.
    let in_token = toks.iter().find(|t| t.start < rel && rel < t.end);
    if let Some(t) = in_token {
        if !matches!(t.tok, Tok::Ident { .. }) {
            return None;
        }
    }
    if text[..rel].ends_with("--") || in_comment(text, rel) {
        return None;
    }

    // The partial word ending at the cursor.
    let partial = toks.iter().position(|t| matches!(t.tok, Tok::Ident { .. }) && t.start < rel && rel <= t.end);
    let (prefix, replace, before) = match partial {
        Some(p) => {
            let t = &toks[p];
            let typed = &text[t.start..rel];
            let prefix = match &t.tok {
                Tok::Ident { quoted: true, .. } => typed.trim_start_matches(['"', '`']).to_string(),
                _ => typed.to_string(),
            };
            (prefix, base + t.start..base + t.end, p)
        }
        None => (String::new(), cursor..cursor, toks.iter().take_while(|t| t.end <= rel).count()),
    };
    let prev = |k: usize| before.checked_sub(k).and_then(|i| toks.get(i));

    let refs = table_refs(&toks);
    // `qualifier.` right before the partial word.
    if matches!(prev(1).map(|t| &t.tok), Some(Tok::Punct('.'))) {
        let q = prev(2).and_then(name)?;
        // A qualifier preceded by another `x.` is schema.table.column.
        let schema_q = matches!(prev(3).map(|t| &t.tok), Some(Tok::Punct('.')))
            .then(|| prev(4).and_then(name))
            .flatten();
        let after_from = prev(3).is_some_and(introduces_table)
            || (matches!(prev(3).map(|t| &t.tok), Some(Tok::Punct(','))) && in_from_list(&toks, before - 2));
        if after_from {
            return Some(CompletionContext { want: Want::Tables { schema: Some(q) }, prefix, replace });
        }
        let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        let matched: Vec<TableRef> = refs
            .iter()
            .filter(|r| match &r.alias {
                Some(a) => eq(a, &q),
                None => eq(&r.name, &q) && schema_q.as_deref().is_none_or(|s| r.schema.as_deref().is_some_and(|rs| eq(rs, s))),
            })
            .cloned()
            .collect();
        if matched.is_empty() {
            // Unknown qualifier: either a schema (tables) or an unlisted
            // table (columns). Offer tables in that schema.
            return Some(CompletionContext { want: Want::Tables { schema: Some(q) }, prefix, replace });
        }
        return Some(CompletionContext { want: Want::Columns { tables: matched, qualified: true }, prefix, replace });
    }

    match prev(1) {
        Some(t) if introduces_table(t) => {
            Some(CompletionContext { want: Want::Tables { schema: None }, prefix, replace })
        }
        Some(T { tok: Tok::Punct(','), .. }) if in_from_list(&toks, before - 1) => {
            Some(CompletionContext { want: Want::Tables { schema: None }, prefix, replace })
        }
        // After a table name / alias in a FROM list: keyword position.
        Some(t @ T { tok: Tok::Ident { .. }, .. }) if !is_stop(t) => None,
        None => None,
        _ => Some(CompletionContext { want: Want::Columns { tables: refs, qualified: false }, prefix, replace }),
    }
}

/// Is token index `i` (a comma) inside a FROM list?
fn in_from_list(toks: &[T], i: usize) -> bool {
    let mut depth = 0;
    for t in toks[..i].iter().rev() {
        match &t.tok {
            Tok::Punct(')') => depth += 1,
            Tok::Punct('(') => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ if depth > 0 => {}
            _ if kw(t, "FROM") => return true,
            _ if is_stop(t) && !kw(t, "AS") => return false,
            _ => {}
        }
    }
    false
}

fn in_comment(text: &str, rel: usize) -> bool {
    let line_start = text[..rel].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line = &text[line_start..rel];
    // `--` outside quotes on this line.
    let mut in_q = false;
    let lb = line.as_bytes();
    for (i, &c) in lb.iter().enumerate() {
        if c == b'\'' {
            in_q = !in_q;
        }
        if !in_q && c == b'-' && lb.get(i + 1) == Some(&b'-') {
            return true;
        }
    }
    let opens = text[..rel].matches("/*").count();
    let closes = text[..rel].matches("*/").count();
    opens > closes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(marked: &str) -> Option<CompletionContext> {
        let c = marked.find('|').unwrap();
        let s = marked.replacen('|', "", 1);
        completion_context(&s, c, SplitOptions::new(Dialect::Postgres))
    }

    fn want(marked: &str) -> Option<Want> {
        ctx(marked).map(|c| c.want)
    }

    fn tr(schema: Option<&str>, name: &str, alias: Option<&str>) -> TableRef {
        TableRef { schema: schema.map(Into::into), name: name.into(), alias: alias.map(Into::into) }
    }

    #[test]
    fn tables_after_from_and_join() {
        assert_eq!(want("select * from |"), Some(Want::Tables { schema: None }));
        assert_eq!(want("select * from ord|"), Some(Want::Tables { schema: None }));
        assert_eq!(want("select * from a join |"), Some(Want::Tables { schema: None }));
        assert_eq!(want("select * from a, |"), Some(Want::Tables { schema: None }));
        assert_eq!(want("update |"), Some(Want::Tables { schema: None }));
        assert_eq!(want("insert into |"), Some(Want::Tables { schema: None }));
        assert_eq!(want("select * from shop.|"), Some(Want::Tables { schema: Some("shop".into()) }));
        assert_eq!(want("select * from shop.or|"), Some(Want::Tables { schema: Some("shop".into()) }));
        assert_eq!(want("select * from a join shop.|"), Some(Want::Tables { schema: Some("shop".into()) }));
    }

    #[test]
    fn prefix_and_replace_range() {
        let c = ctx("select * from ord|ers").unwrap();
        assert_eq!(c.prefix, "ord");
        assert_eq!(c.replace, 14..20);
        let c = ctx("select * from |").unwrap();
        assert_eq!((c.prefix.as_str(), c.replace), ("", 14..14));
        let c = ctx("select * from \"My Ta|").unwrap();
        assert_eq!(c.prefix, "My Ta");
    }

    #[test]
    fn columns_through_aliases() {
        let w = want("select o.| from shop.orders o join customers as c on c.id = o.customer_id");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(Some("shop"), "orders", Some("o"))], qualified: true }));
        let w = want("select * from shop.orders o join customers as c on c.| ");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(None, "customers", Some("c"))], qualified: true }));
        let w = want("select orders.st| from orders");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(None, "orders", None)], qualified: true }));
    }

    #[test]
    fn unqualified_columns_use_all_tables_in_scope() {
        let w = want("select id, | from a x, b where 1=1");
        assert_eq!(
            w,
            Some(Want::Columns { tables: vec![tr(None, "a", Some("x")), tr(None, "b", None)], qualified: false })
        );
        let w = want("select * from a where na|");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(None, "a", None)], qualified: false }));
        let w = want("update t set |");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(None, "t", None)], qualified: false }));
    }

    #[test]
    fn unknown_qualifier_means_schema() {
        assert_eq!(want("select * from a where public.|"), Some(Want::Tables { schema: Some("public".into()) }));
    }

    #[test]
    fn no_completion_in_strings_comments_or_after_alias() {
        assert_eq!(want("select 'from |'"), None);
        assert_eq!(want("select 1 -- from |"), None);
        assert_eq!(want("select /* from | */ 1"), None);
        assert_eq!(want("select * from orders o |"), None);
        assert_eq!(want("|"), None);
    }

    #[test]
    fn only_current_statement() {
        let w = want("select * from a;\nselect x.| from b x");
        assert_eq!(w, Some(Want::Columns { tables: vec![tr(None, "b", Some("x"))], qualified: true }));
    }

    #[test]
    fn subquery_from_is_still_found() {
        let w = want("select * from (select | from inner_t) s");
        assert!(matches!(w, Some(Want::Columns { .. })));
    }

    #[test]
    fn never_panics_on_odd_input() {
        for s in ["\"", "select \"", "x.", ".", "from .", "from a.b.", "select ü.|", "from \"a\"\"b\".|"] {
            for c in 0..=s.len() {
                if s.is_char_boundary(c) {
                    let _ = completion_context(s, c, SplitOptions::new(Dialect::Postgres));
                }
            }
        }
    }
}

#[cfg(test)]
mod prop {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn completion_never_panics(parts in proptest::collection::vec(
            prop_oneof![
                Just("select "), Just("from "), Just("join "), Just("a"), Just("b c"), Just("."), Just(","),
                Just("\""), Just("'"), Just("("), Just(")"), Just("-- x\n"), Just("/*"), Just("*/"), Just(";"),
                Just("ü"), Just("as "), Just("where "), Just("\n\n"),
            ], 0..25)) {
            let s: String = parts.concat();
            for c in 0..=s.len() {
                if s.is_char_boundary(c) {
                    if let Some(ctx) = completion_context(&s, c, SplitOptions::new(Dialect::Postgres)) {
                        prop_assert!(ctx.replace.start <= ctx.replace.end && ctx.replace.end <= s.len());
                        prop_assert!(s.is_char_boundary(ctx.replace.start) && s.is_char_boundary(ctx.replace.end));
                    }
                }
            }
        }
    }
}
