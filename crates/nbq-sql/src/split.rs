//! Script splitting: find statement boundaries in an editor buffer.
//!
//! Statements end at a top-level `;` or (optionally) at a blank line that is
//! not inside parentheses. Strings, quoted identifiers, comments, dollar
//! quotes and `CREATE ... BEGIN ... END` bodies are respected.

use crate::Dialect;
use std::ops::Range;

#[derive(Debug, Clone, Copy)]
pub struct SplitOptions {
    pub dialect: Dialect,
    /// Treat a blank line (outside parentheses) as a statement delimiter,
    /// the way DBeaver's "smart" mode does.
    pub blank_line_delimits: bool,
}

impl SplitOptions {
    pub fn new(dialect: Dialect) -> Self {
        Self { dialect, blank_line_delimits: true }
    }
}

/// One statement in a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Byte range of the statement text: from its first token to the end of
    /// its last token. Excludes leading comments/whitespace and the `;`.
    pub range: Range<usize>,
    /// Byte offset just past the terminating `;`, if there was one.
    pub delimiter_end: Option<usize>,
}

impl Statement {
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.range.clone()]
    }
    /// End of the statement including its delimiter.
    pub fn outer_end(&self) -> usize {
        self.delimiter_end.unwrap_or(self.range.end)
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

struct Segment {
    first_code: Option<usize>,
    last_code_end: usize,
    paren_depth: i32,
    block_depth: i32,
    /// Words seen so far, uppercased, only kept for the first few words to
    /// decide whether this is a `CREATE TRIGGER/FUNCTION/PROCEDURE`.
    words_seen: u32,
    starts_with_create: bool,
    is_routine: bool,
}

impl Segment {
    fn new() -> Self {
        Segment {
            first_code: None,
            last_code_end: 0,
            paren_depth: 0,
            block_depth: 0,
            words_seen: 0,
            starts_with_create: false,
            is_routine: false,
        }
    }
    fn code(&mut self, start: usize, end: usize) {
        if self.first_code.is_none() {
            self.first_code = Some(start);
        }
        self.last_code_end = end;
    }
}

/// Split `sql` into statements.
pub fn split(sql: &str, opts: SplitOptions) -> Vec<Statement> {
    let b = sql.as_bytes();
    let n = b.len();
    let d = opts.dialect;
    let mut out = Vec::new();
    let mut seg = Segment::new();
    let mut line_has_content = false;
    let mut i = 0;

    let finish = |seg: &mut Segment, out: &mut Vec<Statement>, delim_end: Option<usize>| {
        if let Some(start) = seg.first_code {
            out.push(Statement { range: start..seg.last_code_end, delimiter_end: delim_end });
        }
        *seg = Segment::new();
    };

    while i < n {
        let c = b[i];
        match c {
            b'\n' => {
                if !line_has_content
                    && opts.blank_line_delimits
                    && seg.first_code.is_some()
                    && seg.paren_depth <= 0
                    && seg.block_depth <= 0
                    && seg.last_code_end < i
                {
                    finish(&mut seg, &mut out, None);
                }
                line_has_content = false;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            b';' => {
                line_has_content = true;
                if seg.block_depth > 0 {
                    i += 1;
                    continue;
                }
                finish(&mut seg, &mut out, Some(i + 1));
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                line_has_content = true;
                i = skip_line(b, i);
            }
            b'#' if d.hash_comments() => {
                line_has_content = true;
                i = skip_line(b, i);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                line_has_content = true;
                // A block comment may span lines; blank lines inside it are not delimiters.
                i = skip_block_comment(b, i, d.nested_block_comments());
            }
            b'\'' => {
                line_has_content = true;
                let escapes = d.backslash_escapes_in_strings() || is_e_string_prefix(b, i, d);
                let end = skip_quoted(b, i, b'\'', escapes);
                seg.code(i, end);
                i = end;
            }
            b'"' => {
                line_has_content = true;
                let end = skip_quoted(b, i, b'"', d.backslash_escapes_in_strings());
                seg.code(i, end);
                i = end;
            }
            b'`' if d.backtick_idents() => {
                line_has_content = true;
                let end = skip_quoted(b, i, b'`', false);
                seg.code(i, end);
                i = end;
            }
            b'$' if d.dollar_quotes() => {
                line_has_content = true;
                if let Some(tag_end) = dollar_tag(b, i) {
                    let tag = &b[i..tag_end];
                    let end = find_subslice(b, tag_end, tag).map(|p| p + tag.len()).unwrap_or(n);
                    seg.code(i, end);
                    i = end;
                } else {
                    seg.code(i, i + 1);
                    i += 1;
                }
            }
            b'(' => {
                line_has_content = true;
                seg.paren_depth += 1;
                seg.code(i, i + 1);
                i += 1;
            }
            b')' => {
                line_has_content = true;
                seg.paren_depth -= 1;
                seg.code(i, i + 1);
                i += 1;
            }
            c if is_ident_byte(c) => {
                line_has_content = true;
                let start = i;
                while i < n && is_ident_byte(b[i]) {
                    i += 1;
                }
                seg.code(start, i);
                on_word(&mut seg, b, start, i);
            }
            _ => {
                line_has_content = true;
                seg.code(i, i + 1);
                i += 1;
            }
        }
    }
    finish(&mut seg, &mut out, None);
    out
}

/// Track `CREATE ... TRIGGER/FUNCTION/PROCEDURE ... BEGIN ... END` so that
/// semicolons inside the body don't end the statement.
fn on_word(seg: &mut Segment, b: &[u8], start: usize, end: usize) {
    let w = &b[start..end];
    seg.words_seen += 1;
    if seg.words_seen == 1 {
        seg.starts_with_create = w.eq_ignore_ascii_case(b"CREATE");
        return;
    }
    if !seg.starts_with_create {
        return;
    }
    if seg.block_depth == 0 && !seg.is_routine {
        if w.eq_ignore_ascii_case(b"TRIGGER")
            || w.eq_ignore_ascii_case(b"FUNCTION")
            || w.eq_ignore_ascii_case(b"PROCEDURE")
        {
            seg.is_routine = true;
        }
        return;
    }
    if !seg.is_routine {
        return;
    }
    if w.eq_ignore_ascii_case(b"BEGIN") || (seg.block_depth > 0 && w.eq_ignore_ascii_case(b"CASE")) {
        // `CASE` only nests once we are inside a body.
        if w.eq_ignore_ascii_case(b"CASE") && preceded_by_word(b, start, b"END") {
            return; // `END CASE` already handled on END
        }
        seg.block_depth += 1;
    } else if w.eq_ignore_ascii_case(b"END") && seg.block_depth > 0 {
        match next_word(b, end) {
            Some(nw)
                if nw.eq_ignore_ascii_case(b"IF")
                    || nw.eq_ignore_ascii_case(b"LOOP")
                    || nw.eq_ignore_ascii_case(b"WHILE")
                    || nw.eq_ignore_ascii_case(b"REPEAT") => {}
            _ => seg.block_depth -= 1,
        }
    }
}

fn next_word(b: &[u8], mut i: usize) -> Option<&[u8]> {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let s = i;
    while i < b.len() && is_ident_byte(b[i]) {
        i += 1;
    }
    (i > s).then(|| &b[s..i])
}

fn preceded_by_word(b: &[u8], start: usize, word: &[u8]) -> bool {
    let mut i = start;
    while i > 0 && b[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let e = i;
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    b[i..e].eq_ignore_ascii_case(word)
}

fn skip_line(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map(|p| i + p).unwrap_or(b.len())
}

fn skip_block_comment(b: &[u8], mut i: usize, nested: bool) -> usize {
    let n = b.len();
    let mut depth = 0;
    while i < n {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            if depth == 0 || nested {
                depth += 1;
            }
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    n
}

/// Skip a quoted run starting at `i` (which holds `q`). A doubled quote is an
/// escaped quote. Returns the offset just past the closing quote, or the end
/// of input if unterminated.
fn skip_quoted(b: &[u8], i: usize, q: u8, backslash: bool) -> usize {
    let n = b.len();
    let mut j = i + 1;
    while j < n {
        let c = b[j];
        if backslash && c == b'\\' {
            j += 2;
            continue;
        }
        if c == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    n
}

/// Postgres `E'...'` strings honour backslash escapes.
fn is_e_string_prefix(b: &[u8], i: usize, d: Dialect) -> bool {
    matches!(d, Dialect::Postgres | Dialect::Redshift)
        && i >= 1
        && (b[i - 1] == b'E' || b[i - 1] == b'e')
        && (i < 2 || !is_ident_byte(b[i - 2]))
}

/// If a dollar-quote tag (`$$` or `$tag$`) starts at `i`, return the offset
/// just past it. `$1` (a parameter) is not a tag.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    if i > 0 && is_ident_byte(b[i - 1]) {
        return None; // e.g. `foo$bar` identifiers
    }
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn find_subslice(h: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > h.len() {
        return None;
    }
    h[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// The statement that "Run statement" should execute for a cursor at byte
/// offset `cursor`.
///
/// Inside a statement (including its `;`) → that statement. In the gap
/// between statements: the previous one if the cursor is on the same line as
/// its end, otherwise the next one if no blank line separates them,
/// otherwise the previous one if no blank line separates them.
pub fn statement_at(sql: &str, cursor: usize, opts: SplitOptions) -> Option<Statement> {
    let stmts = split(sql, opts);
    let cursor = cursor.min(sql.len());
    let mut prev: Option<&Statement> = None;
    for s in &stmts {
        if cursor >= s.range.start && cursor <= s.outer_end() {
            return Some(s.clone());
        }
        if s.range.start > cursor {
            if let Some(p) = prev {
                if !sql[p.outer_end()..cursor].contains('\n') {
                    return Some(p.clone());
                }
            }
            if !has_blank_line(&sql[cursor..s.range.start]) {
                return Some(s.clone());
            }
            return prev.filter(|p| !has_blank_line(&sql[p.outer_end()..cursor])).cloned();
        }
        prev = Some(s);
    }
    prev.filter(|p| !has_blank_line(&sql[p.outer_end()..cursor])).cloned()
}

/// True if `gap` contains a whitespace-only line between two newlines.
fn has_blank_line(gap: &str) -> bool {
    let parts: Vec<&str> = gap.split('\n').collect();
    parts.len() >= 3 && parts[1..parts.len() - 1].iter().any(|l| l.trim().is_empty())
}

#[cfg(test)]
mod tests;
