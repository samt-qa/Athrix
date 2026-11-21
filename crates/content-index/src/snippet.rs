//! Snippets: a short fragment of the ORIGINAL (diacritized) text around the best cluster of
//! query matches, with highlight ranges.
//!
//! The stored text is re-tokenized with the same analyzer; tokens whose term is one of the
//! query's terms are matches, and their spans are already byte spans of the original text, so
//! highlights land on «الْمَكْتَبَاتِ» even though the query was «مكتبة».
//!
//! Cost: tokenizing is the expensive part (about 15 µs per KiB of Arabic), so the text is
//! scanned in growing windows (the first [`FIRST_WINDOW`] bytes, then 4× more each round,
//! always cut at a whitespace) and the scan stops at the first window holding a match, plus
//! [`snippet_bytes`](crate::IndexOptions::snippet_bytes) of context after it so a cluster
//! that straddles the cut is kept whole. A document whose first match is near its start costs
//! the same whatever its length; one without a match in the stored text is scanned once.

use athrix_text_ar::Analyzer;
use std::collections::BTreeSet;
use std::ops::Range;

/// Bytes of text tokenized first when looking for matches (see the module docs).
pub(crate) const FIRST_WINDOW: usize = 64 << 10;

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

/// Builds the snippet of `text` for the query `terms`, at most about `max_bytes` long, from
/// the first window of the text that holds a match (see the module docs).
/// Without a match in `text` (e.g. the match lies past the stored prefix), returns the start
/// of the text with no highlights.
pub(crate) fn make(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Analyzer,
    max_bytes: usize,
) -> Snippet {
    let spans = match_spans(text, terms, analyzer, max_bytes);

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

/// Matched spans, in order, one per source word (variants share the span), of the first
/// window of `text` that has any, plus `context` bytes after that window.
fn match_spans<'t>(
    text: &str,
    terms: &'t BTreeSet<String>,
    analyzer: Analyzer,
    context: usize,
) -> Vec<(Range<usize>, &'t str)> {
    let mut spans = Vec::new();
    if terms.is_empty() {
        return spans;
    }
    let mut start = 0;
    let mut window = FIRST_WINDOW;
    while start < text.len() {
        let end = word_end(text, start.saturating_add(window));
        collect(text, start..end, terms, analyzer, &mut spans);
        if !spans.is_empty() {
            let tail = word_end(text, end.saturating_add(context));
            collect(text, end..tail, terms, analyzer, &mut spans);
            break;
        }
        start = end;
        window = window.saturating_mul(4);
    }
    spans
}

/// Appends the matches of `text[range]` to `spans` (offsets in the whole text).
fn collect<'t>(
    text: &str,
    range: Range<usize>,
    terms: &'t BTreeSet<String>,
    analyzer: Analyzer,
    spans: &mut Vec<(Range<usize>, &'t str)>,
) {
    let base = range.start;
    for t in analyzer.tokens(&text[range]) {
        if let Some(term) = terms.get(&t.text) {
            let span = base + t.start..base + t.end;
            if spans.last().is_none_or(|(r, _)| r.start != span.start) {
                spans.push((span, term.as_str()));
            }
        }
    }
}

/// The first whitespace at or after `at` (on a char boundary), or the end of the text: a
/// window cut there never splits a word.
fn word_end(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let mut i = at;
    while !text.is_char_boundary(i) {
        i += 1;
    }
    text[i..]
        .find(char::is_whitespace)
        .map_or(text.len(), |j| i + j)
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
    // Skip the whole whitespace char: some take several bytes (U+00A0: 2, U+3000: 3).
    match text[s..lo].char_indices().find(|&(_, c)| c.is_whitespace()) {
        Some((i, c)) if s > 0 => s + i + c.len_utf8(),
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

    /// Regression (found by the perf audit, task 5.8): a multi-byte whitespace (here a
    /// no-break space, 2 bytes in UTF-8) just before the window start made `cut_start` return
    /// an offset inside it, and slicing the fragment panicked.
    #[test]
    fn multi_byte_whitespace_before_the_window_is_skipped_whole() {
        for ws in ['\u{a0}', '\u{2003}', '\u{3000}'] {
            for pad in 0..12 {
                let filler: String = std::iter::repeat_n(format!("yy{ws}"), 30).collect();
                let text = format!("{}{ws}{filler}match here", "x".repeat(pad));
                for max in [8, 12, 20, 40, 80] {
                    let s = make(&text, &terms(&["match"]), Analyzer::default(), max);
                    assert_eq!(&text[s.offset..s.offset + s.fragment.len()], s.fragment);
                    assert_eq!(&s.fragment[s.highlights[0].clone()], "match");
                }
            }
        }
    }

    #[test]
    fn a_match_past_the_first_window_is_found() {
        let filler = "كلام عادي بلا شيء ".repeat(FIRST_WINDOW / 20);
        assert!(filler.len() > FIRST_WINDOW);
        let text = format!("{filler}وهنا الْمَكْتَبَاتُ أخيرا");
        let s = make(&text, &terms(&["مكتب"]), Analyzer::default(), 100);
        let words: Vec<_> = s.original_ranges().map(|r| &text[r]).collect();
        assert_eq!(words, ["الْمَكْتَبَاتُ"]);
    }

    #[test]
    fn a_cluster_across_the_window_cut_is_kept_whole() {
        // The first window ends right after «عقد»; «الايجار» follows just past the cut.
        let pad = "س ".repeat((FIRST_WINDOW - 4) / 3);
        let text = format!("{pad}عقد الايجار {}", "ص ".repeat(1000));
        let s = make(&text, &terms(&["عقد", "ايجار"]), Analyzer::default(), 60);
        let words: Vec<_> = s.original_ranges().map(|r| &text[r]).collect();
        assert_eq!(words, ["عقد", "الايجار"]);
    }

    #[test]
    fn a_word_is_never_cut_at_a_window_edge() {
        // «reporting» straddles the window end, which falls right after «report»: cutting
        // there would give a false match.
        let pad = "a".repeat(FIRST_WINDOW - 7);
        let text = format!("{pad} reporting end");
        assert_eq!(&text[FIRST_WINDOW - 6..FIRST_WINDOW], "report");
        let s = make(&text, &terms(&["report"]), Analyzer::default(), 60);
        assert!(s.highlights.is_empty(), "{s:?}");
    }

    #[test]
    fn no_match_gives_the_start_of_the_text() {
        let s = make("نص بلا مطابقة", &terms(&["x"]), Analyzer::default(), 10);
        assert_eq!(s.offset, 0);
        assert!(s.highlights.is_empty());
        assert!(s.fragment.len() <= 10);
    }
}
