//! Hand-written recursive-descent parser. See the crate docs for the grammar and why it is
//! hand-written.

use crate::ast::{Expr, ExprKind, FileKind, Filter, FilterName, FilterValue, Query, Span, Term};
use crate::error::{ParseError, ParseErrorKind as K};
use crate::{lexicon, values};

/// Maximum nesting of groups and negations. Keeps recursion bounded so hostile input such as
/// ten thousand `<` cannot overflow the stack.
pub const MAX_DEPTH: usize = 64;

type PResult<T> = Result<T, ParseError>;

/// Parses a query string into a [`Query`]. Never panics; any malformed input is reported as a
/// [`ParseError`] with the byte span of the problem.
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let mut p = Parser {
        src: input,
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    if p.peek().is_none() {
        return Ok(Query { root: None });
    }
    let root = p.parse_or()?;
    p.skip_ws();
    match p.peek() {
        None => Ok(Query { root: Some(root) }),
        Some('>') => Err(p.empty_operand_error()),
        Some(c) => Err(ParseError::new(
            K::UnmatchedClose,
            Span::new(p.pos, p.pos + c.len_utf8()),
            format!("unexpected `{c}`"),
        )),
    }
}

struct Parser<'a> {
    src: &'a str,
    /// Always on a char boundary of `src`.
    pos: usize,
    depth: usize,
}

/// Characters that end a plain word or a raw filter value.
fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '|' | '<' | '>' | '"')
}

fn combine(mut items: Vec<Expr>, make: fn(Vec<Expr>) -> ExprKind) -> Expr {
    if items.len() == 1 {
        if let Some(only) = items.pop() {
            return only;
        }
    }
    let span = match (items.first(), items.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::default(),
    };
    Expr {
        kind: make(items),
        span,
    }
}

fn value_example(f: FilterName) -> &'static str {
    match f {
        FilterName::Ext => "ext:pdf;docx",
        FilterName::Type => "type:image",
        FilterName::Size => "size:>100mb",
        FilterName::DateModified => "dm:today",
        FilterName::DateCreated => "dc:2026-01..2026-03",
        FilterName::DateAccessed => "da:lastmonth",
        FilterName::Path => "path:projects",
        FilterName::Parent => "parent:Downloads",
        FilterName::InFolder => "infolder:Documents",
        FilterName::Content => "content:\"rental contract\"",
        FilterName::Semantic => "sem:rental contract",
        FilterName::Regex => "regex:^IMG_\\d{4}",
        FilterName::Case => "case:Report",
        FilterName::WholeWord => "ww:report",
        FilterName::Fuzzy => "fuzzy:rprt",
        FilterName::Dupe => "dupe:",
        FilterName::Similar => "similar:photo.jpg",
        FilterName::Tag => "tag:work",
    }
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        self.src.get(self.pos..).unwrap_or("")
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    /// Advances over a run of non-delimiter characters.
    fn skip_word(&mut self) {
        while self.peek().is_some_and(|c| !is_delim(c)) {
            self.bump();
        }
    }

    fn here(&self) -> Span {
        Span::new(self.pos, self.pos + self.peek().map_or(0, char::len_utf8))
    }

    fn enter(&mut self, at: usize) -> PResult<()> {
        if self.depth >= MAX_DEPTH {
            return Err(ParseError::new(
                K::TooDeep,
                Span::new(at, at + 1),
                format!("the query is nested too deeply (more than {MAX_DEPTH} levels)"),
            ));
        }
        self.depth += 1;
        Ok(())
    }

    /// `or := and ('|' and)*`
    fn parse_or(&mut self) -> PResult<Expr> {
        let mut items = vec![self.parse_and()?];
        loop {
            self.skip_ws();
            if self.peek() != Some('|') {
                break;
            }
            let bar = self.here();
            self.bump();
            self.skip_ws();
            if matches!(self.peek(), None | Some('|' | '>')) {
                return Err(ParseError::new(
                    K::DanglingOperator,
                    bar,
                    "nothing after `|`; add a search term or remove the `|`",
                ));
            }
            items.push(self.parse_and()?);
        }
        Ok(combine(items, ExprKind::Or))
    }

    /// `and := unary+` (juxtaposition), stopping at `|`, `>` or the end.
    fn parse_and(&mut self) -> PResult<Expr> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None | Some('|' | '>') => break,
                Some(_) => items.push(self.parse_unary()?),
            }
        }
        if items.is_empty() {
            return Err(self.empty_operand_error());
        }
        Ok(combine(items, ExprKind::And))
    }

    fn empty_operand_error(&self) -> ParseError {
        match self.peek() {
            Some('|') => ParseError::new(
                K::DanglingOperator,
                self.here(),
                "nothing before `|`; add a search term or remove the `|`",
            ),
            Some('>') if self.depth == 0 => ParseError::new(
                K::UnmatchedClose,
                self.here(),
                "`>` closes a group that was never opened; add `<` or remove the `>`",
            ),
            Some('>') => ParseError::new(
                K::EmptyGroup,
                self.here(),
                "empty group; put something between `<` and `>`",
            ),
            _ => ParseError::new(K::UnclosedGroup, self.here(), "missing closing `>`"),
        }
    }

    /// `unary := ('!' | '-') unary | primary`. A lone `!`/`-` (followed by a space, `|`, `>`
    /// or the end) is a literal term, so names like `Artist - Song` still work.
    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let Some(op @ ('!' | '-')) = self.peek() else {
            return self.parse_primary();
        };
        self.bump();
        if self
            .peek()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '|' | '>'))
        {
            return Ok(Expr {
                kind: ExprKind::Term(Term {
                    text: op.to_string(),
                    quoted: false,
                }),
                span: Span::new(start, self.pos),
            });
        }
        self.enter(start)?;
        let inner = self.parse_unary();
        self.depth -= 1;
        let inner = inner?;
        Ok(Expr {
            span: Span::new(start, inner.span.end),
            kind: ExprKind::Not(Box::new(inner)),
        })
    }

    /// `primary := '<' or '>' | '"' phrase '"' | '~' words | filter | word`
    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.pos;
        match self.peek() {
            Some('<') => self.parse_group(),
            Some('"') => {
                let (text, span) = self.read_quoted()?;
                if text.trim().is_empty() {
                    return Err(ParseError::new(
                        K::EmptyPhrase,
                        span,
                        "empty phrase; type something between the quotes",
                    ));
                }
                Ok(Expr {
                    kind: ExprKind::Term(Term { text, quoted: true }),
                    span,
                })
            }
            Some('~') => {
                self.bump();
                let name_span = Span::new(start, self.pos);
                let (text, value_span) = self.read_semantic(name_span)?;
                Ok(Expr {
                    span: Span::new(start, value_span.end),
                    kind: ExprKind::Filter(Filter {
                        name: FilterName::Semantic,
                        name_span,
                        value: FilterValue::Text(text),
                        value_span,
                    }),
                })
            }
            _ => self.parse_word_or_filter(),
        }
    }

    fn parse_group(&mut self) -> PResult<Expr> {
        let start = self.pos;
        self.enter(start)?;
        self.bump(); // '<'
        let inner = self.parse_group_body(start);
        self.depth -= 1;
        let inner = inner?;
        Ok(Expr {
            span: Span::new(start, self.pos),
            kind: ExprKind::Group(Box::new(inner)),
        })
    }

    fn parse_group_body(&mut self, start: usize) -> PResult<Expr> {
        let unclosed = || {
            ParseError::new(
                K::UnclosedGroup,
                Span::new(start, start + 1),
                "this `<` is never closed; add `>`",
            )
        };
        self.skip_ws();
        match self.peek() {
            None => return Err(unclosed()),
            Some('>') => {
                return Err(ParseError::new(
                    K::EmptyGroup,
                    Span::new(start, self.pos + 1),
                    "empty group; put something between `<` and `>`",
                ));
            }
            _ => {}
        }
        let inner = self.parse_or()?;
        self.skip_ws();
        if self.peek() != Some('>') {
            return Err(unclosed());
        }
        self.bump();
        Ok(inner)
    }

    /// Reads `"..."` starting at the opening quote. Returns the inner text and the full span.
    fn read_quoted(&mut self) -> PResult<(String, Span)> {
        let start = self.pos;
        self.bump(); // '"'
        let rest = self.rest();
        match rest.find('"') {
            Some(i) => {
                let text = rest[..i].to_string();
                self.pos += i + 1;
                Ok((text, Span::new(start, self.pos)))
            }
            None => Err(ParseError::new(
                K::UnclosedQuote,
                Span::new(start, start + 1),
                "this quote is never closed; add `\"`",
            )),
        }
    }

    /// If the input at `pos` looks like `name:` (letters, digits or `_`, then a colon),
    /// returns the name and the byte index of the colon.
    fn filter_name_candidate(&self) -> Option<(&'a str, usize)> {
        let rest = self.rest();
        let end = rest
            .char_indices()
            .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
            .map(|(i, _)| i)?;
        (end > 0 && rest[end..].starts_with(':')).then(|| (&rest[..end], self.pos + end))
    }

    fn at_known_filter(&self) -> bool {
        self.filter_name_candidate()
            .is_some_and(|(name, _)| lexicon::filter_name(name).is_some())
    }

    fn parse_word_or_filter(&mut self) -> PResult<Expr> {
        let start = self.pos;
        if let Some((name, colon)) = self.filter_name_candidate() {
            let name_span = Span::new(start, colon + 1);
            if let Some(f) = lexicon::filter_name(name) {
                self.pos = colon + 1;
                return self.parse_filter(f, name_span);
            }
            let after = self.src.get(colon + 1..).unwrap_or("");
            // Drive letters (`C:\`), times (`10:30`) and URLs (`http://`) are plain words.
            let plain = name.chars().count() == 1
                || name.chars().any(char::is_numeric)
                || after.starts_with("//");
            if !plain {
                let mut err = ParseError::new(
                    K::UnknownFilter,
                    name_span,
                    format!("unknown filter `{name}:`"),
                );
                if let Some(s) = lexicon::suggest_filter(name) {
                    err.message = format!("unknown filter `{name}:`; did you mean `{s}:`?");
                    err = err.with_suggestion(s);
                } else {
                    err.message = format!(
                        "unknown filter `{name}:`; known filters are ext, type, size, dm, dc, \
                         da, path, parent, infolder, content, sem, regex, case, ww, fuzzy, \
                         dupe, similar and tag"
                    );
                }
                return Err(err);
            }
        }
        self.skip_word();
        if self.pos == start {
            // Defensive: never loop without progress.
            self.bump();
        }
        let span = Span::new(start, self.pos);
        Ok(Expr {
            kind: ExprKind::Term(Term {
                text: span.slice(self.src).to_string(),
                quoted: false,
            }),
            span,
        })
    }

    fn parse_filter(&mut self, f: FilterName, name_span: Span) -> PResult<Expr> {
        if f == FilterName::Semantic {
            let (text, value_span) = self.read_semantic(name_span)?;
            return Ok(Expr {
                span: name_span.to(value_span),
                kind: ExprKind::Filter(Filter {
                    name: f,
                    name_span,
                    value: FilterValue::Text(text),
                    value_span,
                }),
            });
        }
        let value_start = self.pos;
        let (raw, text_offset) = if self.peek() == Some('"') {
            let (t, span) = self.read_quoted()?;
            (t, span.start + 1)
        } else {
            let compares = matches!(
                f,
                FilterName::Size
                    | FilterName::DateModified
                    | FilterName::DateCreated
                    | FilterName::DateAccessed
            );
            if compares {
                // Leading comparison operators belong to the value, even `>`.
                while self.pos - value_start < 2 && matches!(self.peek(), Some('<' | '>' | '=')) {
                    self.bump();
                }
            }
            self.skip_word();
            (self.src[value_start..self.pos].to_string(), value_start)
        };
        let value_span = Span::new(value_start, self.pos);
        let filter = |value, value_span| Expr {
            span: name_span.to(value_span),
            kind: ExprKind::Filter(Filter {
                name: f,
                name_span,
                value,
                value_span,
            }),
        };

        if raw.trim().is_empty() {
            if matches!(
                f,
                FilterName::Case | FilterName::WholeWord | FilterName::Fuzzy | FilterName::Dupe
            ) {
                return Ok(filter(FilterValue::None, value_span));
            }
            return Err(ParseError::new(
                K::MissingValue,
                name_span.to(value_span),
                format!(
                    "`{}` needs a value, for example `{}`",
                    name_span.slice(self.src),
                    value_example(f)
                ),
            ));
        }

        let value_err = |(kind, message): (K, String)| ParseError::new(kind, value_span, message);
        let value = match f {
            FilterName::Ext => FilterValue::Extensions(
                split_list(&raw)
                    .map(|(_, item)| item.trim_start_matches(['*', '.']).to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect(),
            ),
            FilterName::Tag => {
                FilterValue::Tags(split_list(&raw).map(|(_, t)| t.to_string()).collect())
            }
            FilterName::Type => FilterValue::Kinds(parse_kinds(&raw, text_offset)?),
            FilterName::Size => FilterValue::Size(values::parse_size(&raw).map_err(value_err)?),
            FilterName::DateModified | FilterName::DateCreated | FilterName::DateAccessed => {
                FilterValue::Date(values::parse_date(&raw).map_err(value_err)?)
            }
            _ => FilterValue::Text(raw),
        };
        let empty_list = match &value {
            FilterValue::Extensions(v) | FilterValue::Tags(v) => v.is_empty(),
            FilterValue::Kinds(v) => v.is_empty(),
            _ => false,
        };
        if empty_list {
            return Err(ParseError::new(
                K::MissingValue,
                name_span.to(value_span),
                format!(
                    "`{}` needs at least one value, for example `{}`",
                    name_span.slice(self.src),
                    value_example(f)
                ),
            ));
        }
        Ok(filter(value, value_span))
    }

    /// Reads free text for `~` / `sem:`: words and quoted phrases up to the end, `|`, a
    /// bracket, a `!`/`-`/`~` prefixed word, or the next known filter. Returns the words
    /// joined by single spaces and the span from the first to the last word.
    fn read_semantic(&mut self, name_span: Span) -> PResult<(String, Span)> {
        let mut text = String::new();
        let mut first = None;
        let mut end = self.pos;
        loop {
            self.skip_ws();
            let piece_start = self.pos;
            match self.peek() {
                None | Some('|' | '<' | '>' | '!' | '-' | '~') => break,
                Some('"') => {
                    let (t, _) = self.read_quoted()?;
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&t);
                }
                Some(_) => {
                    if self.at_known_filter() {
                        break;
                    }
                    self.skip_word();
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&self.src[piece_start..self.pos]);
                }
            }
            first.get_or_insert(piece_start);
            end = self.pos;
        }
        self.pos = end;
        if text.trim().is_empty() {
            return Err(ParseError::new(
                K::MissingValue,
                name_span,
                format!(
                    "`{}` needs a description, for example `~rental contract`",
                    name_span.slice(self.src)
                ),
            ));
        }
        Ok((text, Span::new(first.unwrap_or(end), end)))
    }
}

/// Splits a `;`/`؛` separated list. Yields `(byte offset in raw, trimmed item)`, skipping
/// empty items (so a trailing `;` while typing is harmless).
fn split_list(raw: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    raw.split([';', '؛']).filter_map(move |item| {
        let item_offset = offset;
        offset += item.len() + 1; // `;` is 1 byte; `؛` is 2, corrected below.
        if raw
            .get(item_offset + item.len()..)
            .is_some_and(|r| r.starts_with('؛'))
        {
            offset += '؛'.len_utf8() - 1;
        }
        let trimmed = item.trim_start();
        let lead = item.len() - trimmed.len();
        let trimmed = trimmed.trim_end();
        (!trimmed.is_empty()).then_some((item_offset + lead, trimmed))
    })
}

fn parse_kinds(raw: &str, text_offset: usize) -> PResult<Vec<FileKind>> {
    let mut kinds = Vec::new();
    for (off, item) in split_list(raw) {
        let span = Span::new(text_offset + off, text_offset + off + item.len());
        let Some(k) = lexicon::kind(item) else {
            let mut err = ParseError::new(
                K::InvalidKind,
                span,
                format!(
                    "unknown type `{item}`; use doc, image, video, audio, code, archive or folder"
                ),
            );
            if let Some(s) = lexicon::suggest_kind(item) {
                err.message = format!("unknown type `{item}`; did you mean `{s}`?");
                err = err.with_suggestion(s);
            }
            return Err(err);
        };
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    Ok(kinds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_list_offsets() {
        let raw = "pdf; docx؛ txt;;";
        let items: Vec<_> = split_list(raw).collect();
        assert_eq!(items.len(), 3);
        for (off, item) in items {
            assert_eq!(&raw[off..off + item.len()], item);
        }
    }
}
