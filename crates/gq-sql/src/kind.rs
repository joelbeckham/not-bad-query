//! Classify a single statement by its keywords.

use crate::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// Read-only and returns rows: safe to pause mid-stream and to abort by
    /// cancelling.
    Query,
    Begin,
    Commit,
    Rollback,
    /// Anything that may change data or schema.
    Write,
}

/// Words outside strings, quoted identifiers and comments, in order.
pub fn words(sql: &str, d: Dialect) -> impl Iterator<Item = &str> {
    let b = sql.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        let n = b.len();
        while i < n {
            let c = b[i];
            let skip_to = match c {
                b'-' if b.get(i + 1) == Some(&b'-') => line_end(b, i),
                b'#' if d.hash_comments() => line_end(b, i),
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    find(b, i + 2, b"*/").map(|p| p + 2).unwrap_or(n)
                }
                b'\'' | b'"' | b'`' => {
                    let mut j = i + 1;
                    while j < n {
                        if d.backslash_escapes_in_strings() && b[j] == b'\\' {
                            j += 2;
                            continue;
                        }
                        if b[j] == c {
                            if b.get(j + 1) == Some(&c) {
                                j += 2;
                                continue;
                            }
                            break;
                        }
                        j += 1;
                    }
                    (j + 1).min(n)
                }
                b'$' if d.dollar_quotes() => {
                    let mut j = i + 1;
                    while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    if b.get(j) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                        let tag = &b[i..=j];
                        find(b, j + 1, tag).map(|p| p + tag.len()).unwrap_or(n)
                    } else {
                        i + 1
                    }
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let s = i;
                    while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                        i += 1;
                    }
                    return Some(&sql[s..i]);
                }
                _ => i + 1,
            };
            i = skip_to.max(i + 1);
        }
        None
    })
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map(|p| i + p).unwrap_or(b.len())
}

fn find(h: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > h.len() {
        return None;
    }
    h[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

pub fn classify(sql: &str, d: Dialect) -> StatementKind {
    let mut w = words(sql, d);
    let Some(first) = w.next() else { return StatementKind::Write };
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let second = w.next();
    if eq(first, "BEGIN") || (eq(first, "START") && second.is_some_and(|s| eq(s, "TRANSACTION"))) {
        return StatementKind::Begin;
    }
    if eq(first, "COMMIT") || eq(first, "END") {
        return StatementKind::Commit;
    }
    if eq(first, "ROLLBACK") || eq(first, "ABORT") {
        // `ROLLBACK TO [SAVEPOINT] x` keeps the transaction open.
        return if second.is_some_and(|s| eq(s, "TO")) { StatementKind::Write } else { StatementKind::Rollback };
    }
    let rest = || words(sql, d).skip(1);
    let writes = |w: &str| {
        ["INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "COPY", "TRUNCATE", "CREATE", "DROP", "ALTER"]
            .iter()
            .any(|k| eq(w, k))
    };
    match first.to_ascii_uppercase().as_str() {
        "SELECT" | "WITH" | "VALUES" | "TABLE" => {
            // Data-modifying CTEs and SELECT ... INTO write.
            if rest().any(writes) { StatementKind::Write } else { StatementKind::Query }
        }
        "SHOW" | "DESCRIBE" | "DESC" | "PRAGMA" => StatementKind::Query,
        "EXPLAIN" => {
            if rest().any(|w| eq(w, "ANALYZE") || eq(w, "ANALYSE")) && rest().any(writes) {
                StatementKind::Write
            } else {
                StatementKind::Query
            }
        }
        _ => StatementKind::Write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementKind::*;

    fn k(s: &str) -> StatementKind {
        classify(s, Dialect::Postgres)
    }

    #[test]
    fn kinds() {
        assert_eq!(k("select 1"), Query);
        assert_eq!(k("  -- c\n/* x */ SELECT * from t"), Query);
        assert_eq!(k("with a as (select 1) select * from a"), Query);
        assert_eq!(k("with a as (delete from t returning *) select * from a"), Write);
        assert_eq!(k("select * into t2 from t"), Write);
        assert_eq!(k("select 'insert into x' from t"), Query);
        assert_eq!(k("select \"update\" from t"), Query);
        assert_eq!(k("select $$delete$$"), Query);
        assert_eq!(k("values (1)"), Query);
        assert_eq!(k("table t"), Query);
        assert_eq!(k("show search_path"), Query);
        assert_eq!(k("explain select 1"), Query);
        assert_eq!(k("explain analyze delete from t"), Write);
        assert_eq!(k("update t set a = 1 returning *"), Write);
        assert_eq!(k("insert into t values (1)"), Write);
        assert_eq!(k("create table t (a int)"), Write);
        assert_eq!(k("begin"), Begin);
        assert_eq!(k("BEGIN TRANSACTION"), Begin);
        assert_eq!(k("start transaction"), Begin);
        assert_eq!(k("commit"), Commit);
        assert_eq!(k("end"), Commit);
        assert_eq!(k("rollback"), Rollback);
        assert_eq!(k("abort"), Rollback);
        assert_eq!(k("rollback to savepoint a"), Write);
        assert_eq!(k(""), Write);
        assert_eq!(k("select updated_at, inserted from t"), Query);
    }

    #[test]
    fn words_skip_noise() {
        let w: Vec<_> = words("a 'b c' \"d\" -- e\n f /* g */ $$h$$ $1 i", Dialect::Postgres).collect();
        assert_eq!(w, ["a", "f", "i"]);
    }
}
