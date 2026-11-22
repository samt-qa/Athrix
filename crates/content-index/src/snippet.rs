//! Snippets: a short fragment of the ORIGINAL (diacritized) text around the best cluster of
//! query matches, with highlight ranges.
//!
//! The stored text is re-tokenized with the same analyzer; tokens whose term is one of the
//! query's terms are matches, and their spans are already byte spans of the original text, so
//! highlights land on «الْمَكْتَبَاتِ» even though the query was «مكتبة».
//!
//! Cost: tokenizing is the expensive part (about 15 µs per KiB of Arabic), so the text is
//! scanned in growing windows (the first [`FIRST_WINDOW`] bytes, then 4× more each round,
//! always cut at a whitespace) until a window holds a match, plus
//! [`snippet_bytes`](crate::IndexOptions::snippet_bytes) of context after it so a cluster
//! that straddles the cut is kept whole. Then, if the best cluster found so far lacks some
//! query term, [`MORE_WINDOW`] more bytes (and their context) are tokenized, so a fuller
//! cluster just past the first match (an intro naming one word, the paragraph with all of
//! them further on) wins; the best cluster of everything scanned is used.
//!
//! A document whose first match is near its start costs at most about `FIRST_WINDOW +
//! MORE_WINDOW` bytes of tokenizing, whatever its length; one without a match in the stored
//! text is scanned once. A cluster further on is not looked for: that would mean the whole
//! stored text (up to 512 KiB, about 8 ms of Arabic) for every row shown.

use athrix_text_ar::Analyzer;
use std::collections::BTreeSet;
use std::ops::Range;

/// Bytes of text tokenized first when looking for matches (see the module docs).
pub(crate) const FIRST_WINDOW: usize = 64 << 10;

/// Bytes tokenized past the first window holding a match when its best cluster lacks some
/// query term (see the module docs).
pub(crate) const MORE_WINDOW: usize = 64 << 10;

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

/// Builds the snippet of `text` for the query `terms`, at most about `max_bytes` long, around
/// the best cluster of matches in the scanned part of the text (see the module docs).
/// Without a match in `text` (e.g. the match lies past the stored prefix), returns the start
/// of the text with no highlights.
pub(crate) fn make(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Analyzer,
    max_bytes: usize,
) -> Snippet {
    let spans = match_spans(text, terms, analyzer, max_bytes);
    let (start, end, hits) = match best_cluster(&spans, max_bytes) {
        Some(Cluster { i, j, .. }) => {
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

/// The best run `spans[i..j]` of matches within `max_bytes`: most distinct terms, then most
/// matches, the first one on ties. A single overlong word still counts (as a run of one
/// with no distinct term).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cluster {
    distinct: usize,
    i: usize,
    j: usize,
}

/// The [`Cluster`] of `spans` (sorted, non-overlapping), in one sliding-window pass.
fn best_cluster(spans: &[(Range<usize>, &str)], max_bytes: usize) -> Option<Cluster> {
    // How many spans of the current window hold each term (a query has few terms).
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut distinct = 0;
    let mut best: Option<(usize, usize, Cluster)> = None; // (distinct, count, cluster)
    let mut j = 0;
    for i in 0..spans.len() {
        j = j.max(i);
        while j < spans.len() && spans[j].0.end - spans[i].0.start <= max_bytes {
            match counts.iter_mut().find(|(t, _)| *t == spans[j].1) {
                Some((_, n)) => {
                    *n += 1;
                    if *n == 1 {
                        distinct += 1;
                    }
                }
                None => {
                    counts.push((spans[j].1, 1));
                    distinct += 1;
                }
            }
            j += 1;
        }
        let (d, c) = if j > i { (distinct, j - i) } else { (0, 1) };
        if best.is_none_or(|(bd, bc, _)| (d, c) > (bd, bc)) {
            let cluster = Cluster {
                distinct: d,
                i,
                j: j.max(i + 1),
            };
            best = Some((d, c, cluster));
        }
        // Slide: spans[i] leaves the window (if it was in it).
        if j > i {
            if let Some((_, n)) = counts.iter_mut().find(|(t, _)| *t == spans[i].1) {
                *n -= 1;
                if *n == 0 {
                    distinct -= 1;
                }
            }
        }
    }
    best.map(|(_, _, c)| c)
}

/// Matched spans, in order, one per source word (variants share the span), of the scanned
/// part of `text` (see the module docs); `context` is the snippet length.
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
    while start < text.len() && spans.is_empty() {
        let end = word_end(text, start.saturating_add(window));
        collect(text, start..end, terms, analyzer, &mut spans);
        start = end;
        window = window.saturating_mul(4);
    }
    if spans.is_empty() {
        return spans;
    }
    // Context past the cut, so a cluster across it is whole before it is judged.
    let tail = word_end(text, start.saturating_add(context));
    collect(text, start..tail, terms, analyzer, &mut spans);
    let complete = best_cluster(&spans, context).is_some_and(|c| c.distinct >= terms.len());
    if !complete && tail < text.len() {
        let end = word_end(text, tail.saturating_add(MORE_WINDOW));
        collect(text, tail..end, terms, analyzer, &mut spans);
        let tail = word_end(text, end.saturating_add(context));
        collect(text, end..tail, terms, analyzer, &mut spans);
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
mod prop_tests;

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

    /// Regression (docs/05-REMAINING-WORK.md §5): in a document longer than the first window,
    /// a cluster holding every query word just past the first window that holds a match
    /// beats the lone match in that window.
    #[test]
    fn a_fuller_cluster_after_the_first_window_wins() {
        let pad = "كلام عادي ".repeat(FIRST_WINDOW / 18 + 10);
        let text = format!(
            "مقدمة عن العقد {pad}{}العقد وشروط الايجار هنا {}",
            "س ".repeat(1000),
            "ص ".repeat(1000)
        );
        assert!(text.find("الايجار").unwrap() > FIRST_WINDOW);
        let s = make(&text, &terms(&["عقد", "ايجار"]), Analyzer::default(), 80);
        let words: Vec<_> = s.original_ranges().map(|r| &text[r]).collect();
        assert_eq!(words, ["العقد", "الايجار"]);
    }

    /// The extra scan is bounded: a cluster further than [`MORE_WINDOW`] past the first
    /// window with a match is not looked for, and the first match is used.
    #[test]
    fn the_extra_scan_is_bounded() {
        let pad = "كلام عادي ".repeat((FIRST_WINDOW + MORE_WINDOW) / 18 + 100);
        let text = format!("مقدمة عن العقد {pad}{pad}العقد وشروط الايجار هنا");
        let s = make(&text, &terms(&["عقد", "ايجار"]), Analyzer::default(), 80);
        let words: Vec<_> = s.original_ranges().map(|r| &text[r]).collect();
        assert_eq!(words, ["العقد"]);
        assert!(s.offset < 100);
    }

    /// When the first window's cluster already holds every term, nothing more is scanned and
    /// that cluster is kept.
    #[test]
    fn a_complete_first_cluster_is_kept() {
        let pad = "كلام عادي ".repeat(FIRST_WINDOW / 18 + 10);
        let text = format!("العقد والايجار {pad} العقد الايجار العقد الايجار");
        let s = make(&text, &terms(&["عقد", "ايجار"]), Analyzer::default(), 80);
        assert_eq!(s.offset, 0);
        assert_eq!(s.highlights.len(), 2);
    }

    /// The sliding-window cluster search picks what the plain one (every start, every
    /// window) picks.
    #[test]
    fn best_cluster_equals_the_quadratic_search() {
        fn quadratic(spans: &[(Range<usize>, &str)], max: usize) -> Option<(usize, usize)> {
            let mut best: Option<(usize, usize, usize, usize)> = None;
            for i in 0..spans.len() {
                let mut distinct = BTreeSet::new();
                let mut j = i;
                while j < spans.len() && spans[j].0.end - spans[i].0.start <= max {
                    distinct.insert(spans[j].1);
                    j += 1;
                }
                let j = j.max(i + 1);
                let score = (distinct.len(), j - i);
                if best.is_none_or(|(d, c, _, _)| score > (d, c)) {
                    best = Some((score.0, score.1, i, j));
                }
            }
            best.map(|(_, _, i, j)| (i, j))
        }
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n) as usize
        };
        let names = ["a", "b", "c", "d"];
        for _ in 0..2000 {
            let mut spans = Vec::new();
            let mut at = 0;
            for _ in 0..next(30) {
                at += next(25);
                let len = 1 + next(40);
                spans.push((at..at + len, names[next(4)]));
                at += len;
            }
            let max = next(120);
            let fast = best_cluster(&spans, max).map(|c| (c.i, c.j));
            assert_eq!(fast, quadratic(&spans, max), "{spans:?} max {max}");
        }
    }

    #[test]
    fn no_match_gives_the_start_of_the_text() {
        let s = make("نص بلا مطابقة", &terms(&["x"]), Analyzer::default(), 10);
        assert_eq!(s.offset, 0);
        assert!(s.highlights.is_empty());
        assert!(s.fragment.len() <= 10);
    }
}
