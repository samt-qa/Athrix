//! Content query text → tantivy query.
//!
//! Syntax (the `content:` part of the Athrix query language hands its text here):
//! - words are ANDed: `عقد ايجار`;
//! - `"…"` is a phrase: `"عقد الإيجار"` (an unclosed quote runs to the end);
//! - a leading `-` excludes a word or phrase: `عقد -بيع`.
//!
//! Every word goes through the same analyzer as the documents, so diacritics, letter variants
//! and the light stemmer apply to the query too. A word that the analyzer splits into several
//! tokens («تقرير_2026», «3.14») is searched as a phrase. A single word matches any of its
//! forms at that position (the stem, and the stem without a leading و / ف).

use athrix_text_ar::Analyzer;
use std::collections::BTreeSet;
use tantivy::Term;
use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};

/// One clause of a parsed query: a word or a quoted phrase, possibly negated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clause<'a> {
    pub text: &'a str,
    pub negated: bool,
}

/// Splits the raw query into clauses.
pub(crate) fn parse(query: &str) -> Vec<Clause<'_>> {
    let mut out = Vec::new();
    let mut rest = query;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return out;
        }
        let negated = rest.starts_with('-');
        if negated {
            rest = &rest[1..];
        }
        if let Some(body) = rest.strip_prefix('"') {
            let end = body.find('"').unwrap_or(body.len());
            out.push(Clause {
                text: &body[..end],
                negated,
            });
            rest = body.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(Clause {
                text: &rest[..end],
                negated,
            });
            rest = &rest[end..];
        }
    }
}

/// The analyzed form of a query: the tantivy query plus every term it can match (for
/// snippet highlighting). `None` query when nothing searchable remains.
pub(crate) struct Built {
    pub query: Option<Box<dyn Query>>,
    pub terms: BTreeSet<String>,
}

/// Builds the tantivy query over `field` (indexed with positions).
pub(crate) fn build(query: &str, field: Field, analyzer: Analyzer) -> Built {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut terms = BTreeSet::new();
    let mut has_positive = false;
    for clause in parse(query) {
        // positions[i] = (position, [primary, variants…])
        let mut positions: Vec<(usize, Vec<String>)> = Vec::new();
        for t in analyzer.tokens(clause.text) {
            match positions.last_mut() {
                Some((p, forms)) if *p == t.position => forms.push(t.text),
                _ => positions.push((t.position, vec![t.text])),
            }
        }
        let Some(&(first_pos, _)) = positions.first() else {
            continue;
        };
        let q: Box<dyn Query> = if positions.len() == 1 {
            let forms = &positions[0].1;
            let mut subs: Vec<(Occur, Box<dyn Query>)> = forms
                .iter()
                .map(|f| {
                    let tq = TermQuery::new(
                        Term::from_field_text(field, f),
                        IndexRecordOption::WithFreqs,
                    );
                    (Occur::Should, Box::new(tq) as Box<dyn Query>)
                })
                .collect();
            if subs.len() == 1 {
                subs.pop().expect("one form").1
            } else {
                Box::new(BooleanQuery::new(subs))
            }
        } else {
            // A phrase matches the primary form at each position: documents index every form
            // at the same position, so the primary of the query word is always there.
            let phrase = positions
                .iter()
                .map(|(p, forms)| (p - first_pos, Term::from_field_text(field, &forms[0])))
                .collect();
            Box::new(PhraseQuery::new_with_offset(phrase))
        };
        if clause.negated {
            clauses.push((Occur::MustNot, q));
        } else {
            has_positive = true;
            terms.extend(positions.into_iter().flat_map(|(_, forms)| forms));
            clauses.push((Occur::Must, q));
        }
    }
    let query = has_positive.then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>);
    Built { query, terms }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(q: &str) -> Vec<(&str, bool)> {
        parse(q).into_iter().map(|c| (c.text, c.negated)).collect()
    }

    #[test]
    fn parses_words_phrases_and_negation() {
        assert_eq!(
            texts(r#"عقد "عقد الإيجار" -بيع -"نسخة قديمة"  x"#),
            [
                ("عقد", false),
                ("عقد الإيجار", false),
                ("بيع", true),
                ("نسخة قديمة", true),
                ("x", false),
            ]
        );
        assert_eq!(texts(r#""unclosed phrase"#), [("unclosed phrase", false)]);
        assert!(texts("   ").is_empty());
    }
}
