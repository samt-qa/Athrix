//! Turning the search-bar text into something the name index can run, plus the UTF-16
//! bookkeeping the UI needs (error spans and highlight ranges).

use athrix_fs_index::{Query, QueryError as IndexQueryError};
use athrix_query::{Expr, ExprKind, ParseError};

use crate::model::QueryError;

/// A query ready to run: the executable name query plus the words to highlight in results.
#[derive(Debug)]
pub struct CompiledQuery {
    /// What [`athrix_fs_index::FileIndex::search`] runs.
    pub query: Query,
    /// Literal pieces of the positive (non-negated) terms, used for highlighting.
    pub highlight_terms: Vec<String>,
}

impl CompiledQuery {
    /// Highlight ranges for a result named `name` (the ORIGINAL name), in UTF-16 units, sorted
    /// and non-overlapping: the plain terms ([`Self::highlight_terms`]) plus what the fuzzy
    /// terms matched ([`Query::fuzzy_highlights`]: a subsequence's letters, an acronym's
    /// initials...).
    pub fn highlights(&self, name: &str) -> Vec<(usize, usize)> {
        highlights_with(
            name,
            &self.highlight_terms,
            self.query.fuzzy_highlights(name),
        )
    }
}

// ADAPTER: the only place that turns query text into an `athrix_fs_index::Query`. The text is
// parsed once with `athrix_query::parse`; the AST gives both the executable query
// (`Query::compile`) and the words to highlight. If the `fs-index` query API changes again,
// only this function (and `map_index_error`) should need to follow.
/// Parses and compiles `text` for searching, mapping any error to a [`QueryError`] in UTF-16
/// units.
pub fn compile_query(text: &str) -> Result<CompiledQuery, QueryError> {
    let ast = athrix_query::parse(text).map_err(|e| map_parse_error(text, &e))?;
    let query = Query::compile(&ast).map_err(|e| map_index_error(text, &e))?;
    let mut highlight_terms = Vec::new();
    if let Some(root) = &ast.root {
        collect_terms(root, &mut highlight_terms);
    }
    Ok(CompiledQuery {
        query,
        highlight_terms,
    })
}

/// Maps a compile error of the name index. A valid query that uses something the name index
/// cannot answer yet (`content:`, `sem:`...) gets the kind `Unsupported`.
pub fn map_index_error(text: &str, e: &IndexQueryError) -> QueryError {
    match e {
        IndexQueryError::Parse(p) => map_parse_error(text, p),
        other => QueryError {
            message: other.message().to_owned(),
            kind: "Unsupported".to_owned(),
            start: utf16_offset(text, other.span().start),
            end: utf16_offset(text, other.span().end),
            suggestion: None,
        },
    }
}

/// Converts a parser error (byte span) into the UI's [`QueryError`] (UTF-16 span).
pub fn map_parse_error(text: &str, e: &ParseError) -> QueryError {
    QueryError {
        message: e.message.clone(),
        kind: format!("{:?}", e.kind),
        start: utf16_offset(text, e.span.start),
        end: utf16_offset(text, e.span.end),
        suggestion: e.suggestion.clone(),
    }
}

/// UTF-16 offset of byte `byte` in `text`, clamped to the text and rounded down to a char boundary.
pub fn utf16_offset(text: &str, byte: usize) -> usize {
    let mut b = byte.min(text.len());
    while !text.is_char_boundary(b) {
        b -= 1;
    }
    nass::byte_to_utf16_offset(text, b)
}

/// Collects the literal pieces of every term that is not under a NOT. Filters are skipped
/// (they do not match the name text). Wildcards split a term into pieces and anchors are dropped.
fn collect_terms(expr: &Expr, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Term(term) => {
            if term.quoted {
                push_term(&term.text, out);
            } else {
                let text = term.text.strip_prefix('^').unwrap_or(&term.text);
                let text = text.strip_suffix('$').unwrap_or(text);
                for piece in text.split(['*', '?']) {
                    push_term(piece, out);
                }
            }
        }
        ExprKind::And(children) | ExprKind::Or(children) => {
            for child in children {
                collect_terms(child, out);
            }
        }
        ExprKind::Group(inner) => collect_terms(inner, out),
        ExprKind::Not(_) | ExprKind::Filter(_) => {}
    }
}

fn push_term(piece: &str, out: &mut Vec<String>) {
    // A piece that normalizes to nothing (".", "-") would match nowhere useful.
    if !athrix_text_ar::normalize_name(piece).is_empty() && !out.iter().any(|t| t == piece) {
        out.push(piece.to_owned());
    }
}

/// Highlight ranges of `terms` inside the ORIGINAL `name`, in UTF-16 units, sorted and merged.
pub fn highlights(name: &str, terms: &[String]) -> Vec<(usize, usize)> {
    highlights_with(name, terms, Vec::new())
}

/// [`highlights`] plus extra byte ranges of `name` (fuzzy matches), all merged together.
fn highlights_with(
    name: &str,
    terms: &[String],
    extra: Vec<std::ops::Range<usize>>,
) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = terms
        .iter()
        .flat_map(|t| athrix_text_ar::find_in_name(name, t))
        .map(|m| (m.start, m.end))
        .chain(extra.into_iter().map(|r| (r.start, r.end)))
        .filter(|&(s, e)| s < e && e <= name.len())
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
        .into_iter()
        .map(|(s, e)| (utf16_offset(name, s), utf16_offset(name, e)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The substring of `s` covered by a UTF-16 range, as JavaScript's `slice` would return it.
    fn js_slice(s: &str, (start, end): (usize, usize)) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        String::from_utf16(&units[start..end]).unwrap()
    }

    #[test]
    fn highlights_vocalized_arabic_in_utf16_units() {
        let name = "مُحَاضَرَةُ الفِقْهِ.pdf";
        let q = compile_query("الفقه").unwrap();
        let h = highlights(name, &q.highlight_terms);
        assert_eq!(h.len(), 1);
        assert_eq!(js_slice(name, h[0]), "الفِقْهِ");
        // UTF-16 offsets differ from byte offsets for Arabic text.
        assert_eq!(h[0].0, "مُحَاضَرَةُ ".encode_utf16().count());
    }

    #[test]
    fn highlights_after_a_surrogate_pair_and_merges_overlaps() {
        let name = "😀 Report report.txt";
        let q = compile_query("report rep").unwrap();
        let h = highlights(name, &q.highlight_terms);
        assert_eq!(h, vec![(3, 9), (10, 16)]);
        assert_eq!(js_slice(name, h[0]), "Report");
    }

    #[test]
    fn negated_terms_and_filters_are_not_highlighted() {
        let q = compile_query("تقرير -مسودة ext:pdf ^فا*ورة$").unwrap();
        assert_eq!(q.highlight_terms, vec!["تقرير", "فا", "ورة"]);
    }

    #[test]
    fn parse_errors_map_to_utf16_spans() {
        let text = "تقرير szie:10";
        let err = compile_query(text).unwrap_err();
        assert_eq!(err.kind, "UnknownFilter");
        assert_eq!(err.start, "تقرير ".encode_utf16().count());
        assert!(err.end > err.start && err.end <= text.encode_utf16().count());
        assert_eq!(err.suggestion.as_deref(), Some("size"));
        assert!(!err.message.is_empty());
    }

    #[test]
    fn unsupported_filters_map_to_an_unsupported_error() {
        let text = "تقرير content:عقد";
        let err = compile_query(text).unwrap_err();
        assert_eq!(err.kind, "Unsupported");
        assert_eq!(err.start, "تقرير ".encode_utf16().count());
        assert_eq!(err.end, text.encode_utf16().count());
        assert!(!err.message.is_empty());
    }

    #[test]
    fn utf16_offset_clamps_out_of_range_and_mid_char_bytes() {
        let s = "ab";
        assert_eq!(utf16_offset(s, 99), 2);
        let ar = "عر";
        assert_eq!(utf16_offset(ar, 1), 0);
        assert_eq!(utf16_offset(ar, 2), 1);
    }
}
