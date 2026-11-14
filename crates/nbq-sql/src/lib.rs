//! Pure SQL text utilities. No IO, no allocation-heavy parsing: everything here
//! runs on every keystroke, so it must stay fast and must never panic.

mod complete;
mod kind;
mod split;

pub use complete::{CompletionContext, TableRef, Want, completion_context};
pub use kind::{StatementKind, classify, words};
pub use split::{SplitOptions, Statement, split, statement_at};

/// SQL dialect. Only lexical differences matter here (quoting, comments,
/// escape rules), not grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    Postgres,
    Redshift,
    MySql,
    Sqlite,
    DuckDb,
}

impl Dialect {
    pub(crate) fn dollar_quotes(self) -> bool {
        matches!(self, Dialect::Postgres | Dialect::Redshift | Dialect::DuckDb)
    }
    pub(crate) fn hash_comments(self) -> bool {
        matches!(self, Dialect::MySql)
    }
    pub(crate) fn backtick_idents(self) -> bool {
        matches!(self, Dialect::MySql | Dialect::Sqlite)
    }
    pub(crate) fn backslash_escapes_in_strings(self) -> bool {
        matches!(self, Dialect::MySql)
    }
    pub(crate) fn nested_block_comments(self) -> bool {
        matches!(self, Dialect::Postgres | Dialect::DuckDb)
    }
}
