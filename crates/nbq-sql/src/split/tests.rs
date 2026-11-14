use super::*;
use proptest::prelude::*;

fn texts(sql: &str, d: Dialect) -> Vec<String> {
    split(sql, SplitOptions::new(d)).iter().map(|s| s.text(sql).to_string()).collect()
}

fn pg(sql: &str) -> Vec<String> {
    texts(sql, Dialect::Postgres)
}

/// `|` marks the cursor. Returns the text of the statement at the cursor.
fn at(marked: &str) -> Option<String> {
    at_d(marked, Dialect::Postgres)
}

fn at_d(marked: &str, d: Dialect) -> Option<String> {
    let cursor = marked.find('|').expect("cursor marker");
    let sql = marked.replacen('|', "", 1);
    statement_at(&sql, cursor, SplitOptions::new(d)).map(|s| s.text(&sql).to_string())
}

#[test]
fn semicolons() {
    assert_eq!(pg("select 1; select 2;"), ["select 1", "select 2"]);
    assert_eq!(pg("select 1;select 2"), ["select 1", "select 2"]);
    assert_eq!(pg(";;; select 1 ;;"), ["select 1"]);
    assert_eq!(pg(""), Vec::<String>::new());
    assert_eq!(pg("   \n\n  "), Vec::<String>::new());
}

#[test]
fn blank_lines_delimit() {
    assert_eq!(pg("select 1\n\nselect 2"), ["select 1", "select 2"]);
    assert_eq!(pg("select 1\n  \t\nselect 2"), ["select 1", "select 2"]);
    assert_eq!(pg("select 1\r\n\r\nselect 2"), ["select 1", "select 2"]);
    assert_eq!(pg("select 1\nfrom t\nwhere x"), ["select 1\nfrom t\nwhere x"]);
}

#[test]
fn blank_lines_inside_parens_do_not_delimit() {
    let sql = "with a as (\n  select 1\n\n), b as (select 2)\nselect * from a";
    assert_eq!(pg(sql), [sql]);
}

#[test]
fn blank_line_option_off() {
    let opts = SplitOptions { dialect: Dialect::Postgres, blank_line_delimits: false };
    let s = split("select 1\n\nfrom t;", opts);
    assert_eq!(s.len(), 1);
}

#[test]
fn strings_and_identifiers() {
    assert_eq!(pg("select ';'; select 2"), ["select ';'", "select 2"]);
    assert_eq!(pg("select 'it''s;'; select 2"), ["select 'it''s;'", "select 2"]);
    assert_eq!(pg("select \"a;b\" from t; x"), ["select \"a;b\" from t", "x"]);
    assert_eq!(pg("select 'a\n\nb'; x"), ["select 'a\n\nb'", "x"]);
    // Postgres standard strings: backslash is literal, so this string ends at the 2nd quote.
    assert_eq!(pg(r"select 'a\'; select 2"), [r"select 'a\'", "select 2"]);
    // E-strings honour backslash escapes.
    assert_eq!(pg(r"select E'a\';b'; select 2"), [r"select E'a\';b'", "select 2"]);
}

#[test]
fn mysql_lexing() {
    let d = Dialect::MySql;
    assert_eq!(texts(r"select 'a\';b'; select 2", d), [r"select 'a\';b'", "select 2"]);
    assert_eq!(texts("select `a;b` from t; x", d), ["select `a;b` from t", "x"]);
    assert_eq!(texts("select 1 # c;omment\n; x", d), ["select 1", "x"]);
}

#[test]
fn comments() {
    assert_eq!(pg("-- lead;\nselect 1 -- trail;\n; select 2"), ["select 1", "select 2"]);
    assert_eq!(pg("/* a; */ select /* b; */ 1;"), ["select /* b; */ 1"]);
    // Postgres block comments nest.
    assert_eq!(pg("/* a /* b */ ; */ select 1;"), ["select 1"]);
    // Comment-only segments are not statements.
    assert_eq!(pg("select 1;\n-- just a comment\n"), ["select 1"]);
    // Blank lines inside a block comment don't delimit.
    assert_eq!(pg("select 1 /*\n\n*/ + 2"), ["select 1 /*\n\n*/ + 2"]);
}

#[test]
fn dollar_quotes() {
    let f = "create function f() returns int as $$\nbegin\n  return 1;\n\nend;\n$$ language plpgsql";
    assert_eq!(pg(&format!("{f};\nselect f();")), [f, "select f()"]);
    let g = "do $body$ begin perform 1; end $body$";
    assert_eq!(pg(&format!("{g}; select 2")), [g, "select 2"]);
    // $1 is a parameter, not a dollar quote.
    assert_eq!(pg("select $1; select $2"), ["select $1", "select $2"]);
}

#[test]
fn trigger_bodies() {
    let t = "create trigger tr after insert on t begin\n  update x set a = 1;\n  select case when 1 then 2 end;\nend";
    assert_eq!(texts(&format!("{t};\nselect 1;"), Dialect::Sqlite), [t, "select 1"]);
    let p = "create procedure p() begin\n  if 1 then select 1; end if;\n  select 2;\nend";
    assert_eq!(texts(&format!("{p}; select 3"), Dialect::MySql), [p, "select 3"]);
    // A plain transaction BEGIN is not a block.
    assert_eq!(pg("begin; select 1; commit;"), ["begin", "select 1", "commit"]);
}

#[test]
fn unterminated_does_not_panic() {
    assert_eq!(pg("select 'abc"), ["select 'abc"]);
    assert_eq!(pg("select $$abc"), ["select $$abc"]);
    assert_eq!(pg("select /* abc"), ["select"]);
    assert_eq!(pg("select \"abc"), ["select \"abc"]);
}

#[test]
fn utf8() {
    assert_eq!(pg("select 'héllo;'; select 'ü'"), ["select 'héllo;'", "select 'ü'"]);
    assert_eq!(pg("select 日本 from t"), ["select 日本 from t"]);
}

#[test]
fn cursor_inside() {
    assert_eq!(at("select 1; sel|ect 2; select 3").as_deref(), Some("select 2"));
    assert_eq!(at("|select 1; select 2").as_deref(), Some("select 1"));
    assert_eq!(at("select 1|; select 2").as_deref(), Some("select 1"));
}

#[test]
fn cursor_after_semicolon_same_line() {
    assert_eq!(at("select 1;|").as_deref(), Some("select 1"));
    assert_eq!(at("select 1;   |\nselect 2").as_deref(), Some("select 1"));
    assert_eq!(at("select 1  |\n\nselect 2").as_deref(), Some("select 1"));
}

#[test]
fn cursor_in_gap() {
    // Next line down, no blank line → the next statement.
    assert_eq!(at("select 1;\n|\nselect 2").as_deref(), Some("select 2"));
    // Below the last statement with no blank line → previous.
    assert_eq!(at("select 1;\n|").as_deref(), Some("select 1"));
    // Separated by blank lines on both sides → nothing.
    assert_eq!(at("select 1;\n\n|\n\nselect 2"), None);
    assert_eq!(at("|"), None);
    assert_eq!(at("select 1;\n\n\n|"), None);
}

#[test]
fn cursor_multiline_statement() {
    let sql = "select a,\n  b\nfrom t\nwhere |x = 1;\n\nselect 2";
    assert_eq!(at(sql).as_deref(), Some("select a,\n  b\nfrom t\nwhere x = 1"));
}

#[test]
fn cursor_in_leading_comment() {
    assert_eq!(at("-- run| me\nselect 1;").as_deref(), Some("select 1"));
}

proptest! {
    #[test]
    fn never_panics_and_ranges_are_valid(s in "\\PC{0,200}", d in 0..5u8, cur in 0..250usize) {
        let d = [Dialect::Postgres, Dialect::Redshift, Dialect::MySql, Dialect::Sqlite, Dialect::DuckDb][d as usize];
        let opts = SplitOptions::new(d);
        let stmts = split(&s, opts);
        let mut last = 0;
        for st in &stmts {
            prop_assert!(st.range.start >= last);
            prop_assert!(st.range.start < st.range.end);
            prop_assert!(st.range.end <= s.len());
            prop_assert!(s.is_char_boundary(st.range.start) && s.is_char_boundary(st.range.end));
            prop_assert!(!st.text(&s).trim().is_empty());
            last = st.outer_end();
        }
        let cur = cur.min(s.len());
        if s.is_char_boundary(cur) {
            let _ = statement_at(&s, cur, opts);
        }
    }

    #[test]
    fn sql_like_inputs(parts in proptest::collection::vec(
        prop_oneof![
            Just("select 1"), Just(";"), Just("\n"), Just("\n\n"), Just("'a;b'"), Just("$$x;\n\ny$$"),
            Just("-- c;\n"), Just("/* c; */"), Just("("), Just(")"), Just("\"q\""), Just(" begin "), Just(" end "),
            Just("create trigger "), Just("E'\\''"), Just("$1"),
        ], 0..30)) {
        let s: String = parts.concat();
        for d in [Dialect::Postgres, Dialect::MySql, Dialect::Sqlite] {
            let stmts = split(&s, SplitOptions::new(d));
            for st in &stmts {
                prop_assert!(st.range.end <= s.len());
            }
            for c in 0..=s.len() {
                if s.is_char_boundary(c) {
                    if let Some(st) = statement_at(&s, c, SplitOptions::new(d)) {
                        prop_assert!(stmts.contains(&st));
                    }
                }
            }
        }
    }
}
