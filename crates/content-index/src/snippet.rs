//! Snippets: a short fragment of the ORIGINAL (diacritized) text around the best cluster of
//! query matches, with highlight ranges.
//!
//! The stored text is re-tokenized with the same analyzer; tokens whose term is one of the
//! query's terms are matches, and their spans are already byte spans of the original text, so
//! highlights land on «الْمَكْتَبَاتِ» even though the query was «مكتبة».

use athrix_text_ar::Analyzer;
use std::collections::BTreeSet;
use std::ops::Range;

/// A fragment of a document's text with the matched words marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The fragment, a substring of the original text.
    pub fragment: String,
    /// Byte offset of `fragment` in the original text.
    pub offset: usize,
    /// Matched words, as byte ranges of `fragment`, sorted and non-overlapping.
    pub highlights: Vec<Range<usize>>,
}

impl Snippet {
    /// The highlights as byte ranges of the whole original text.
    pub fn original_ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        self.highlights
            .iter()
            .map(|r| r.start + self.offset..r.end + self.offset)
    }
}

/// Builds the snippet of `text` for the query `terms`, at most about `max_bytes` long.
/// Without a match in `text` (e.g. the match lies past the stored prefix), returns the start
/// of the text with no highlights.
pub(crate) fn make(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Analyzer,
    max_bytes: usize,
) -> Snippet {
    // Matched spans, in order, one per source word (variants share the span).
    let mut spans: Vec<(Range<usize>, &str)> = Vec::new();
    for t in analyzer.tokens(text) {
        if let Some(term) = terms.get(&t.text) {
            if spans.last().is_none_or(|(r, _)| r.start != t.start) {
                spans.push((t.span(), term.as_str()));
            }
        }
    }

    // Best window: most distinct terms, then most matches, within max_bytes.
    let mut best: Option<(usize, usize, usize, usize)> = None; // (distinct, count, i, j)
    for i in 0..spans.len() {
        let mut distinct = BTreeSet::new();
        let mut j = i;
        while j < spans.len() && spans[j].0.end - spans[i].0.start <= max_bytes {
            distinct.insert(spans[j].1);
            j += 1;
        }
        let j = j.max(i + 1); // a single overlong word still counts
        let score = (distinct.len(), j - i);
        if best.is_none_or(|(d, c, _, _)| score > (d, c)) {
            best = Some((score.0, score.1, i, j));
        }
    }

    let (start, end, hits) = match best {
        Some((_, _, i, j)) => {
            let (lo, hi) = (spans[i].0.start, spans[j - 1].0.end);
            let (start, end) = widen(text, lo, hi, max_bytes);
            (start, end, &spans[i..j])
        }
        None => (0, cut_end(text, 0, max_bytes.min(text.len())), &[][..]),
    };
    Snippet {
        fragment: text[start..end].to_owned(),
        offset: start,
        highlights: hits
            .iter()
            .map(|(r, _)| r.start - start..r.end.min(end) - start)
            .collect(),
    }
}

/// Grows `lo..hi` with context up to `max_bytes`, a third before and the rest after, snapped
/// to word boundaries where possible.
fn widen(text: &str, lo: usize, hi: usize, max_bytes: usize) -> (usize, usize) {
    let room = max_bytes.saturating_sub(hi - lo);
    let before = room / 3;
    let start = cut_start(text, lo.saturating_sub(before), lo);
    let end = cut_end(text, hi, (hi + room - (lo - start)).min(text.len()));
    (start, end)
}

/// A start in `[min, lo]` on a char boundary, after a whitespace if one is near.
fn cut_start(text: &str, min: usize, lo: usize) -> usize {
    let mut s = min;
    while !text.is_char_boundary(s) {
        s += 1;
    }
    match text[s..lo].find(char::is_whitespace) {
        Some(i) if s > 0 => s + i + 1,
        _ => s,
    }
}

/// An end in `[hi, max]` on a char boundary, before a whitespace if one is near.
fn cut_end(text: &str, hi: usize, max: usize) -> usize {
    let mut e = max;
    while !text.is_char_boundary(e) {
        e -= 1;
    }
    if e >= text.len() {
        return text.len();
    }
    match text[hi..e].rfind(char::is_whitespace) {
        Some(i) => hi + i,
        None => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(q: &[&str]) -> BTreeSet<String> {
        q.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn highlights_the_diacritized_words() {
        let text = "فِي هَذَا الْعَامِ نَشَرَتِ الْمَكْتَبَاتُ تَقَارِيرَهَا السَّنَوِيَّةَ.";
        let s = make(text, &terms(&["مكتب", "تقرير"]), Analyzer::default(), 400);
        let words: Vec<_> = s
            .highlights
            .iter()
            .map(|r| &s.fragment[r.clone()])
            .collect();
        assert_eq!(words, ["الْمَكْتَبَاتُ", "تَقَارِيرَهَا"]);
        for r in s.original_ranges() {
            assert!(
                text[r].contains('\u{064E}'),
                "points into the vocalized original"
            );
        }
    }

    #[test]
    fn picks_the_densest_window_and_stays_on_char_boundaries() {
        let filler = "كلام عادي ".repeat(100);
        let text = format!("{filler}عقد هنا {filler}عقد الايجار في النهاية {filler}");
        let s = make(&text, &terms(&["عقد", "ايجار"]), Analyzer::default(), 120);
        assert!(s.fragment.len() <= 120);
        assert_eq!(s.highlights.len(), 2);
        assert_eq!(&s.fragment[s.highlights[1].clone()], "الايجار");
        assert_eq!(&text[s.offset..s.offset + s.fragment.len()], s.fragment);
    }

    #[test]
    fn no_match_gives_the_start_of_the_text() {
        let s = make("نص بلا مطابقة", &terms(&["x"]), Analyzer::default(), 10);
        assert_eq!(s.offset, 0);
        assert!(s.highlights.is_empty());
        assert!(s.fragment.len() <= 10);
    }
}
