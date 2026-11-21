use std::ops::Range;

use nbq_sql::Dialect;

const PREFIX: &str = "-- ";

/// Toggle the comment prefixes on a block of complete lines. The selection's
/// byte offsets are relative to the block, including in the returned text.
pub fn toggle(text: &str, selection: Range<usize>, dialect: Dialect) -> (String, Range<usize>) {
    let lines = || {
        text.split_inclusive('\n')
            .chain(text.is_empty().then_some(""))
    };
    let has_nonblank_line = lines().any(|line| !line.trim_ascii().is_empty());
    let uncomment = has_nonblank_line
        && lines()
            .filter(|line| !line.trim_ascii().is_empty())
            .all(|line| prefix_len(line.trim_start_matches([' ', '\t']), dialect).is_some());

    let mut replacement = String::with_capacity(text.len());
    let mut adjusted = selection.clone();
    let mut line_start = 0;
    for line in lines() {
        if !has_nonblank_line || !line.trim_ascii().is_empty() {
            let content = line.trim_start_matches([' ', '\t']);
            let indent = line.len() - content.len();
            let (removed, inserted) = if uncomment {
                (prefix_len(content, dialect).unwrap_or(0), "")
            } else {
                (0, PREFIX)
            };
            let at = line_start + indent;
            for (new, old) in [
                (&mut adjusted.start, selection.start),
                (&mut adjusted.end, selection.end),
            ] {
                if old >= at {
                    *new = *new - (old - at).min(removed) + inserted.len();
                }
            }
            replacement.push_str(&line[..indent]);
            replacement.push_str(inserted);
            replacement.push_str(&content[removed..]);
        } else {
            replacement.push_str(line);
        }
        line_start += line.len();
    }
    (replacement, adjusted)
}

fn prefix_len(content: &str, dialect: Dialect) -> Option<usize> {
    let rest = content.strip_prefix("--")?;
    // MySQL only recognizes two dashes followed by whitespace or a control character.
    if dialect == Dialect::MySql
        && rest
            .as_bytes()
            .first()
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_control())
    {
        return None;
    }
    Some(2 + usize::from(rest.starts_with(' ')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_a_mixed_selection_twice_and_preserves_the_selection() {
        let original = "\tselect 'é';\r\n\r\n  -- already commented\r\n";
        let selection = 1..original.len();
        let (commented, adjusted) = toggle(original, selection.clone(), Dialect::Postgres);
        assert_eq!(
            commented,
            "\t-- select 'é';\r\n\r\n  -- -- already commented\r\n"
        );
        assert_eq!(adjusted, 4..commented.len());
        assert_eq!(
            toggle(&commented, adjusted, Dialect::Postgres),
            (original.into(), selection)
        );
    }

    #[test]
    fn toggles_an_empty_line_and_clamps_a_caret_inside_the_prefix() {
        assert_eq!(toggle("", 0..0, Dialect::Sqlite), ("-- ".into(), 3..3));
        assert_eq!(
            toggle("  -- \n", 3..3, Dialect::Sqlite),
            ("  \n".into(), 2..2)
        );
    }

    #[test]
    fn respects_mysql_line_comment_syntax() {
        assert_eq!(
            toggle("--price", 0..0, Dialect::MySql),
            ("-- --price".into(), 3..3)
        );
        assert_eq!(
            toggle("--price", 0..0, Dialect::Postgres),
            ("price".into(), 0..0)
        );
    }
}
