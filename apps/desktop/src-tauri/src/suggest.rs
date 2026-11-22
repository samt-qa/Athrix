//! "Did you mean…": when a valid search finds nothing, a cheap rewrite of the query that does
//! find something.
//!
//! Two candidates, tried in order, each only if the previous one found nothing:
//! 1. **layout**: the free-text words retyped on the other keyboard layout (Arabic 101 ⇄ US
//!    QWERTY, [`athrix_text_ar::swap_layout`]): `hgjrvdv ext:pdf` → `التقرير ext:pdf`. Filters,
//!    operators and quotes are kept as typed.
//! 2. **fuzzy**: the same query with the `fuzzy:` switch in front (every plain word fuzzy).
//!
//! Each candidate is one more search with the same scope, limit and cancel flag, without
//! building result rows.
//!
//! Not part of `search`: the UI asks `search_suggest` after an empty result (and a short
//! pause), so these probes (the fuzzy one scans and ranks the whole index) never delay the
//! empty answer of the keystroke.

use std::sync::atomic::{AtomicBool, Ordering};

use athrix_query::{Expr, ExprKind};

use crate::indexing::IndexView;
use crate::model::{SearchSuggestion, SuggestionKind};
use crate::query::{CompiledQuery, compile_query};
use crate::results::{SearchFailure, SearchScope, count_matches};

/// Text put in front of a query for the fuzzy suggestion (the valueless `fuzzy:` switch).
pub const FUZZY_PREFIX: &str = "fuzzy: ";

/// The first candidate that finds results for `text` (which compiled to `query` and found
/// nothing), or `None`. [`SearchFailure::Cancelled`] if `cancel` is set meanwhile.
pub fn suggest(
    view: &IndexView,
    text: &str,
    query: &CompiledQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<Option<SearchSuggestion>, SearchFailure> {
    let candidates = [
        (layout_swapped(text), SuggestionKind::Layout),
        (fuzzy_variant(text, query), SuggestionKind::Fuzzy),
    ];
    for (candidate, kind) in candidates {
        let Some(candidate) = candidate else {
            continue;
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        let Ok(compiled) = compile_query(&candidate) else {
            continue;
        };
        let count = count_matches(view, &compiled.query, limit, scope, cancel)?;
        if count > 0 {
            return Ok(Some(SearchSuggestion {
                query: candidate,
                kind,
                count,
            }));
        }
    }
    Ok(None)
}

/// `text` with every term (plain word or quoted phrase, negated ones included) retyped on the
/// other keyboard layout; filters and syntax untouched. `None` if the text does not parse or
/// no term changes.
pub fn layout_swapped(text: &str) -> Option<String> {
    let ast = athrix_query::parse(text).ok()?;
    let mut spans = Vec::new();
    if let Some(root) = &ast.root {
        term_ranges(text, root, &mut spans);
    }
    spans.sort_unstable_by_key(|r| r.start);
    let mut out = String::with_capacity(text.len() * 2);
    let mut at = 0;
    let mut changed = false;
    for range in spans {
        if range.start < at {
            continue;
        }
        let word = &text[range.clone()];
        out.push_str(&text[at..range.start]);
        match athrix_text_ar::swap_layout(word) {
            Some(swapped) => {
                out.push_str(&swapped);
                changed = true;
            }
            None => out.push_str(word),
        }
        at = range.end;
    }
    out.push_str(&text[at..]);
    changed.then_some(out)
}

/// Byte ranges of `text` holding the text of each term (inside the quotes for a phrase).
fn term_ranges(text: &str, expr: &Expr, out: &mut Vec<std::ops::Range<usize>>) {
    match &expr.kind {
        ExprKind::Term(term) => {
            let span = expr.span.slice(text);
            // The term's text sits in its span as typed, unless a phrase had escapes.
            if term.text.is_empty() {
                return;
            }
            if let Some(offset) = span.find(term.text.as_str()) {
                let start = expr.span.start + offset;
                out.push(start..start + term.text.len());
            }
        }
        ExprKind::And(children) | ExprKind::Or(children) => {
            for child in children {
                term_ranges(text, child, out);
            }
        }
        ExprKind::Group(inner) | ExprKind::Not(inner) => term_ranges(text, inner, out),
        ExprKind::Filter(_) => {}
    }
}

/// `text` with the `fuzzy:` switch in front, or `None` if that cannot change anything: the
/// query is already fuzzy, or it has no positive plain word.
pub fn fuzzy_variant(text: &str, query: &CompiledQuery) -> Option<String> {
    if query.query.is_ranked() || query.highlight_terms.is_empty() {
        return None;
    }
    Some(format!("{FUZZY_PREFIX}{}", text.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_fs_index::{EntryMeta, IndexBuilder};

    fn view() -> IndexView {
        let mut b = IndexBuilder::new();
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let root = b.push(None, "/data", dir);
        b.push(Some(root), "التقرير السنوي.pdf", EntryMeta::default());
        b.push(Some(root), "report.docx", EntryMeta::default());
        b.push(
            Some(root),
            "Monthly Report Summary.xlsx",
            EntryMeta::default(),
        );
        IndexView::from_index(1, b.finish())
    }

    fn run(view: &IndexView, text: &str) -> Option<SearchSuggestion> {
        let q = compile_query(text).unwrap();
        suggest(view, text, &q, 100, None, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn swaps_only_the_words() {
        assert_eq!(layout_swapped("hgjrvdv").as_deref(), Some("التقرير"));
        assert_eq!(
            layout_swapped("hgjrvdv ext:pdf").as_deref(),
            Some("التقرير ext:pdf")
        );
        assert_eq!(
            layout_swapped("\"hgjrvdv\" -hgjrvdv").as_deref(),
            Some("\"التقرير\" -التقرير")
        );
        assert_eq!(layout_swapped("قثحخقف").as_deref(), Some("report"));
        // Nothing to swap: digits only, filters only, or a parse error.
        assert_eq!(layout_swapped("123"), None);
        assert_eq!(layout_swapped("ext:pdf"), None);
        assert_eq!(layout_swapped("szie:1 hgjrvdv"), None);
    }

    #[test]
    fn suggests_the_other_layout_first() {
        let ix = view();
        let s = run(&ix, "hgjrvdv ext:pdf").expect("a suggestion");
        assert_eq!(s.kind, SuggestionKind::Layout);
        assert_eq!(s.query, "التقرير ext:pdf");
        assert_eq!(s.count, 1);
        // Arabic typed for an English word.
        let s = run(&ix, "قثحخقف").expect("a suggestion");
        assert_eq!(
            (s.kind, s.query.as_str(), s.count),
            (SuggestionKind::Layout, "report", 2)
        );
    }

    #[test]
    fn falls_back_to_fuzzy() {
        let ix = view();
        let s = run(&ix, "rprt").expect("a suggestion");
        assert_eq!(s.kind, SuggestionKind::Fuzzy);
        assert_eq!(s.query, "fuzzy: rprt");
        assert!(s.count >= 1);
        // The swapped layout finds nothing (`rprt` → Arabic), so fuzzy still wins.
        assert!(layout_swapped("rprt").is_some());
    }

    #[test]
    fn no_suggestion_when_nothing_helps_or_nothing_to_change() {
        let ix = view();
        assert_eq!(run(&ix, "zzzzqqqq"), None);
        // Already fuzzy, or no plain word: no fuzzy candidate.
        let q = compile_query("fuzzy:zzzzqqqq").unwrap();
        assert_eq!(fuzzy_variant("fuzzy:zzzzqqqq", &q), None);
        let q = compile_query("ext:zip").unwrap();
        assert_eq!(fuzzy_variant("ext:zip", &q), None);
        assert_eq!(run(&ix, "ext:zip"), None);
    }

    #[test]
    fn a_set_cancel_flag_cancels() {
        let ix = view();
        let q = compile_query("rprt").unwrap();
        assert_eq!(
            suggest(&ix, "rprt", &q, 10, None, &AtomicBool::new(true)).unwrap_err(),
            SearchFailure::Cancelled
        );
    }
}
