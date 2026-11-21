//! Parse errors.

use std::fmt;

use crate::ast::Span;

/// What kind of problem the parser found. Stable codes the UI can map to localized text
/// (the English `message` is a fallback and for logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// `"` without a closing `"`.
    UnclosedQuote,
    /// `""` with nothing inside.
    EmptyPhrase,
    /// `<` without a closing `>`.
    UnclosedGroup,
    /// `>` without an opening `<`.
    UnmatchedClose,
    /// `< >` with nothing inside.
    EmptyGroup,
    /// `|`, `!` or `-` with nothing to apply to.
    DanglingOperator,
    /// `name:` that is not a known filter.
    UnknownFilter,
    /// A filter that needs a value but has none.
    MissingValue,
    /// A size that cannot be read.
    InvalidSize,
    /// A date that cannot be read.
    InvalidDate,
    /// A `type:` value that is not a known kind.
    InvalidKind,
    /// A range whose lower bound is above its upper bound.
    InvertedRange,
    /// Groups/negations nested deeper than the parser allows.
    TooDeep,
}

/// A query parse error: where it is, what went wrong, and (sometimes) a suggested fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The problem category.
    pub kind: ParseErrorKind,
    /// The offending bytes of the query.
    pub span: Span,
    /// A friendly English explanation.
    pub message: String,
    /// A replacement the UI may offer (e.g. `size` for `szie`).
    pub suggestion: Option<String>,
}

impl ParseError {
    pub(crate) fn new(kind: ParseErrorKind, span: Span, message: impl Into<String>) -> Self {
        ParseError {
            kind,
            span,
            message: message.into(),
            suggestion: None,
        }
    }

    pub(crate) fn with_suggestion(mut self, s: impl Into<String>) -> Self {
        self.suggestion = Some(s.into());
        self
    }

    /// Byte offset where the problem starts.
    pub fn position(&self) -> usize {
        self.span.start
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid query at position {}: {}",
            self.span.start, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for athrix_core::Error {
    fn from(e: ParseError) -> Self {
        athrix_core::Error::InvalidQuery {
            position: e.span.start,
            message: e.message,
        }
    }
}
