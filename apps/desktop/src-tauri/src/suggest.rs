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
//! building result rows. It only counts matches (up to `limit`), so it runs unordered
//! ([`athrix_fs_index::Query::unordered`]): the probe stops as soon as `limit` matches exist
//! anywhere in the index, instead of ranking the whole index (fuzzy) or scanning it in order.
//!
//! Not part of `search`: the UI asks `search_suggest` after an empty result (and a short
//! pause), so these probes never delay the empty answer of the keystroke.
//!
//! # Keeping the probes cheap
//!
//! A suggestion is optional: it may be missing, but one that is offered always finds results
//! (its `count` is a real, completed count). Within that rule:
//!
//! - **Gram filter.** Most layout candidates are gibberish in the other script (`budget` →
//!   `لاعيلثف`) and match nothing, yet proving it costs a scan of every name. A small Bloom
//!   filter of the character 1- to 4-grams of every normalized name ([`GramFilter`], 4 MiB,
//!   built once per base index on a background thread) proves most of them empty at once:
//!   every match of a plain word (or phrase, or wildcard piece) contains its normalized text,
//!   so a word with a gram that no base name has can only match entries added since the base,
//!   which are few and checked directly. A Bloom filter has no false "absent", so a skipped
//!   candidate never had a match; until the filter is built the candidate is simply searched.
//! - **No empty rewrites.** A word whose swapped form normalizes to nothing (`ززز` → `...`)
//!   would drop a condition and match everything: no layout candidate then.
//! - **Time budget.** All probes together stop after [`PROBE_BUDGET`]; past it there is no
//!   suggestion (never a partial count).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use athrix_fs_index::FileIndex;
use athrix_query::{Expr, ExprKind, FilterName, FilterValue, Query as AstQuery, Term};

use crate::indexing::IndexView;
use crate::model::{SearchSuggestion, SuggestionKind};
use crate::query::{CompiledQuery, compile_ast};
use crate::results::{SearchFailure, SearchScope, count_matches};

/// Text put in front of a query for the fuzzy suggestion (the valueless `fuzzy:` switch).
pub const FUZZY_PREFIX: &str = "fuzzy: ";

/// Most time all probes of one suggestion may take. The UI asks after a pause and the answer
/// is optional, so past this there is simply no suggestion. Typical probes take 1–20 ms on a
/// 7M-entry index; this only cuts pathological ones (slow wildcard scans, a busy machine).
pub const PROBE_BUDGET: Duration = Duration::from_millis(40);

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
    suggest_within(view, text, query, limit, scope, cancel, PROBE_BUDGET)
}

/// [`suggest`] with an explicit time budget.
pub fn suggest_within(
    view: &IndexView,
    text: &str,
    query: &CompiledQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
    budget: Duration,
) -> Result<Option<SearchSuggestion>, SearchFailure> {
    if cancel.load(Ordering::Relaxed) {
        return Err(SearchFailure::Cancelled);
    }
    let candidates = [
        (layout_swapped(text), SuggestionKind::Layout),
        (fuzzy_variant(text, query), SuggestionKind::Fuzzy),
    ];
    if candidates.iter().all(|(c, _)| c.is_none()) {
        return Ok(None);
    }
    let grams = gram_filter(view);
    with_deadline(cancel, budget, |stop| {
        for (candidate, kind) in candidates {
            let Some(candidate) = candidate else {
                continue;
            };
            if stop.load(Ordering::Relaxed) {
                return Err(SearchFailure::Cancelled);
            }
            let Ok(ast) = athrix_query::parse(&candidate) else {
                continue;
            };
            let impossible = kind == SuggestionKind::Layout
                && grams.as_ref().is_some_and(|g| !may_match(view, g, &ast));
            if impossible {
                continue;
            }
            let Ok(compiled) = compile_ast(&candidate, &ast) else {
                continue;
            };
            let count = count_matches(view, &compiled.query.unordered(), limit, scope, stop)?;
            if count > 0 {
                return Ok(Some(SearchSuggestion {
                    query: candidate,
                    kind,
                    count,
                }));
            }
        }
        Ok(None)
    })
}

/// Runs `probe` with a stop flag that is set when `cancel` is set or `budget` has passed. A
/// probe stopped by the budget (not by `cancel`) gives `Ok(None)`.
fn with_deadline<T>(
    cancel: &AtomicBool,
    budget: Duration,
    probe: impl FnOnce(&AtomicBool) -> Result<Option<T>, SearchFailure>,
) -> Result<Option<T>, SearchFailure> {
    let deadline = Instant::now() + budget;
    let stop = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let outcome = std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            // Polls `cancel` every millisecond: a newer search stops the probe about as fast
            // as the scans themselves notice a flag.
            while !done.load(Ordering::Acquire) {
                let now = Instant::now();
                if cancel.load(Ordering::Relaxed) || now >= deadline {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::park_timeout((deadline - now).min(Duration::from_millis(1)));
            }
        });
        let outcome = probe(&stop);
        done.store(true, Ordering::Release);
        watcher.thread().unpark();
        outcome
    });
    match outcome {
        Err(SearchFailure::Cancelled) if !cancel.load(Ordering::Relaxed) => Ok(None),
        other => other,
    }
}

/// `text` with every term (plain word or quoted phrase, negated ones included) retyped on the
/// other keyboard layout; filters and syntax untouched. `None` if the text does not parse, no
/// term changes, or a term would normalize to nothing (`ززز` → `...`: the rewrite would drop
/// the word and match everything).
pub fn layout_swapped(text: &str) -> Option<String> {
    swap_terms(text, true)
}

/// [`layout_swapped`]; `reject_empty: false` keeps rewrites that normalize to nothing (the
/// behavior before the gram filter, for timing comparisons).
fn swap_terms(text: &str, reject_empty: bool) -> Option<String> {
    let ast = athrix_query::parse(text).ok()?;
    let mut spans = Vec::new();
    if let Some(root) = &ast.root {
        term_ranges(text, root, &mut spans);
    }
    spans.sort_unstable_by_key(|r| r.start);
    let mut out = String::with_capacity(text.len() * 2);
    let mut at = 0;
    let mut changed = false;
    let mut norm = String::new();
    for range in spans {
        if range.start < at {
            continue;
        }
        let word = &text[range.clone()];
        out.push_str(&text[at..range.start]);
        match athrix_text_ar::swap_layout(word) {
            Some(swapped) => {
                athrix_text_ar::normalize_name_into(&swapped, &mut norm);
                if reject_empty && norm.trim_matches(['*', '?', '^', '$']).trim().is_empty() {
                    return None;
                }
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

// ---------------------------------------------------------------------------------------------
// Gram filter
// ---------------------------------------------------------------------------------------------

/// Bits of the gram filter: 2^25 bits, 4 MiB. The ~1.9M distinct 1- to 4-grams of a real
/// 7.35M-entry index set ~16% of them, so an absent gram passes for present about 0.4% of
/// the time.
const GRAM_BITS_LOG2: u32 = 25;
/// Bits per gram.
const GRAM_HASHES: usize = 3;
/// Longest gram.
const MAX_GRAM: usize = 4;

/// A Bloom filter of the character n-grams (n = 1..=`MAX_GRAM`) of every normalized name of
/// one base index. [`GramFilter::may_contain`] is never false for a string that is a substring
/// of one of those names.
#[derive(Debug)]
pub struct GramFilter {
    words: Box<[u64]>,
}

impl GramFilter {
    /// The filter of `names` (normalized names), built on `threads` threads.
    fn build<'a>(len: usize, name: impl Fn(usize) -> &'a str + Sync, threads: usize) -> GramFilter {
        let words: Vec<AtomicU64> = (0..(1usize << GRAM_BITS_LOG2) / 64)
            .map(|_| AtomicU64::new(0))
            .collect();
        let threads = threads.clamp(1, 64);
        let per = len.div_ceil(threads).max(1);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (words, name) = (&words, &name);
                s.spawn(move || {
                    let mut window = ['\0'; MAX_GRAM];
                    for i in (t * per)..((t + 1) * per).min(len) {
                        for_each_gram(name(i), &mut window, |h| {
                            for bit in gram_bits(h) {
                                let (w, m) = (bit / 64, 1u64 << (bit % 64));
                                // Most grams repeat: a plain load avoids contended writes.
                                if words[w].load(Ordering::Relaxed) & m == 0 {
                                    words[w].fetch_or(m, Ordering::Relaxed);
                                }
                            }
                        });
                    }
                });
            }
        });
        GramFilter {
            words: words.into_iter().map(AtomicU64::into_inner).collect(),
        }
    }

    /// The filter of every name of `index`.
    pub fn of_index(index: &FileIndex, threads: usize) -> GramFilter {
        let n = index.len().min(u32::MAX as usize);
        Self::build(n, |i| index.normalized_name(i as u32), threads)
    }

    /// False only if no name of the index contains `s` (a normalized string).
    pub fn may_contain(&self, s: &str) -> bool {
        let mut window = ['\0'; MAX_GRAM];
        let mut all = true;
        for_each_gram(s, &mut window, |h| {
            all &= gram_bits(h).iter().all(|&bit| {
                let (w, m) = (bit / 64, 1u64 << (bit % 64));
                self.words[w] & m != 0
            });
        });
        all
    }
}

/// Calls `f` with the hash of every n-gram (n = 1..=`MAX_GRAM`) of `s`.
#[inline]
fn for_each_gram(s: &str, window: &mut [char; MAX_GRAM], mut f: impl FnMut(u64)) {
    let mut seen = 0usize;
    for c in s.chars() {
        window.rotate_left(1);
        window[MAX_GRAM - 1] = c;
        seen += 1;
        // Grams ending at `c`, longest last; the hash of each extends the previous one.
        let mut h = 0x9E37_79B9_7F4A_7C15u64;
        for &g in window[MAX_GRAM - seen.min(MAX_GRAM)..].iter().rev() {
            h = (h ^ u64::from(u32::from(g))).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
            h ^= h >> 32;
            f(h);
        }
    }
}

/// The [`GRAM_HASHES`] bits of a gram hash (double hashing over a finalized hash).
#[inline]
fn gram_bits(h: u64) -> [usize; GRAM_HASHES] {
    // MurmurHash3's 64-bit finalizer.
    let mut h = h;
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 33;
    let mask = (1u64 << GRAM_BITS_LOG2) - 1;
    let (a, b) = (h, (h >> 32) | 1);
    std::array::from_fn(|i| (a.wrapping_add(b.wrapping_mul(i as u64)) & mask) as usize)
}

/// The filter of the last base index seen, or the base it is being built for.
#[derive(Default)]
struct GramCache {
    /// A finished filter and the base it belongs to.
    ready: Option<(Weak<FileIndex>, Arc<GramFilter>)>,
    /// The base a background build is running for.
    building: Option<Weak<FileIndex>>,
}

static GRAMS: Mutex<GramCache> = Mutex::new(GramCache {
    ready: None,
    building: None,
});

/// The gram filter of `view`'s base, or `None` while it is not built yet (the first call for
/// a base starts a background build on half the cores).
fn gram_filter(view: &IndexView) -> Option<Arc<GramFilter>> {
    let base = view.snapshot().base();
    let mut cache = GRAMS.lock().unwrap_or_else(|e| e.into_inner());
    let is_base = |w: &Weak<FileIndex>| std::ptr::eq(w.as_ptr(), Arc::as_ptr(base));
    if let Some((b, filter)) = &cache.ready {
        if is_base(b) {
            return Some(Arc::clone(filter));
        }
    }
    if cache.building.as_ref().is_some_and(is_base) {
        return None;
    }
    cache.building = Some(Arc::downgrade(base));
    let base = Arc::clone(base);
    let spawned = std::thread::Builder::new()
        .name("suggest-grams".into())
        .spawn(move || {
            let threads = std::thread::available_parallelism().map_or(2, |n| n.get() / 2);
            let filter = Arc::new(GramFilter::of_index(&base, threads));
            let mut cache = GRAMS.lock().unwrap_or_else(|e| e.into_inner());
            let weak = Arc::downgrade(&base);
            if cache.building.as_ref().is_some_and(|b| b.ptr_eq(&weak)) {
                cache.building = None;
            }
            cache.ready = Some((weak, filter));
        });
    if spawned.is_err() {
        cache.building = None;
    }
    None
}

/// False only if `ast` (a name query) cannot match any live entry of `view`: some word it
/// requires holds a gram no base name has, and no entry added since the base contains it.
fn may_match(view: &IndexView, grams: &GramFilter, ast: &AstQuery) -> bool {
    let Some(root) = &ast.root else {
        return true;
    };
    // The `fuzzy:` switch makes plain words fuzzy: no literal is then required.
    if has_fuzzy_switch(root) {
        return true;
    }
    let mut literals = Vec::new();
    required_literals(root, &mut literals);
    literals
        .iter()
        .all(|lit| grams.may_contain(lit) || delta_contains(view, lit))
}

/// True if an entry added or updated since the base has a normalized name containing `lit`.
fn delta_contains(view: &IndexView, lit: &str) -> bool {
    let start = view.snapshot().base().len();
    (start..view.id_bound()).any(|id| {
        let id = id as u32;
        view.contains(id) && view.normalized_name(id).contains(lit)
    })
}

fn has_fuzzy_switch(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Filter(f) => f.name == FilterName::Fuzzy && f.value == FilterValue::None,
        ExprKind::Term(_) => false,
        ExprKind::Group(x) | ExprKind::Not(x) => has_fuzzy_switch(x),
        ExprKind::And(xs) | ExprKind::Or(xs) => xs.iter().any(has_fuzzy_switch),
    }
}

/// Normalized strings that the normalized name of every match of `expr` contains: the text of
/// each term that must hold (AND branches; not under OR or NOT), compiled like the name index
/// compiles terms (`athrix_fs_index` `compile_term` / `compile_phrase` / `compile_glob`).
fn required_literals(expr: &Expr, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Term(t) => term_literals(t, out),
        ExprKind::And(xs) => {
            for x in xs {
                required_literals(x, out);
            }
        }
        ExprKind::Group(x) => required_literals(x, out),
        ExprKind::Or(_) | ExprKind::Not(_) | ExprKind::Filter(_) => {}
    }
}

fn term_literals(t: &Term, out: &mut Vec<String>) {
    let mut push = |s: &str| {
        let s = s.trim();
        if !s.is_empty() {
            out.push(s.to_owned());
        }
    };
    if t.quoted {
        push(&athrix_text_ar::normalize_name(&t.text));
        return;
    }
    let word = t.text.strip_prefix('^').unwrap_or(&t.text);
    let word = word.strip_suffix('$').unwrap_or(word);
    if !t.text.contains(['*', '?']) {
        push(&athrix_text_ar::normalize_name(word));
        return;
    }
    // A wildcard pattern: each literal piece is matched as is (a substring of it, trimmed,
    // is required too).
    let mut buf = String::new();
    for piece in word.split(['*', '?']).filter(|p| !p.is_empty()) {
        // As the index's `normalize_inner`: keep edge separators (as spaces) using sentinels.
        athrix_text_ar::normalize_name_into(&format!("q{piece}q"), &mut buf);
        match buf.strip_prefix('q').and_then(|s| s.strip_suffix('q')) {
            Some(inner) => push(inner),
            None => push(&athrix_text_ar::normalize_name(piece)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_fs_index::{EntryMeta, IndexBuilder};

    use crate::query::compile_query;

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
    fn no_layout_rewrite_that_normalizes_to_nothing() {
        // `ززز` is `...` on the US layout: it would drop the word and match everything.
        assert_eq!(layout_swapped("ززز"), None);
        assert_eq!(layout_swapped("hgjrvdv ززز"), None);
        assert_eq!(swap_terms("ززز", false).as_deref(), Some("..."));
        let ix = view();
        assert_eq!(run(&ix, "ززز"), None);
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

    #[test]
    fn the_gram_filter_never_misses_a_substring() {
        let names = [
            "التقرير السنوي.pdf",
            "report.docx",
            "Monthly Report Summary.xlsx",
            "a",
            "",
            "ﻻ ئ ؤ ة 𝒳 é",
        ];
        let mut b = IndexBuilder::new();
        for n in names {
            b.push(None, n, EntryMeta::default());
        }
        let ix = b.finish();
        let grams = GramFilter::of_index(&ix, 3);
        for id in 0..ix.len() as u32 {
            let name: Vec<char> = ix.normalized_name(id).chars().collect();
            for i in 0..name.len() {
                for j in i + 1..=name.len() {
                    let sub: String = name[i..j].iter().collect();
                    assert!(grams.may_contain(&sub), "{sub:?}");
                }
            }
        }
        assert!(grams.may_contain(""));
        // Strings with grams no name has (flagged: 4 MiB of bits for a few dozen grams).
        for absent in ["لاعيلثف", "qzxwv", "zz", "رشوشف", "thj vi", "w"] {
            assert!(!grams.may_contain(absent), "{absent:?}");
        }
    }

    /// `may_match` false ⇒ the query really finds nothing, on queries of every shape.
    #[test]
    fn may_match_is_false_only_without_matches() {
        let ix = view();
        let grams = GramFilter::of_index(ix.snapshot().base(), 2);
        let c = AtomicBool::new(false);
        let cases = [
            ("التقرير", true),
            ("تقرير السنو", true),
            ("\"تقرير السنوي\"", true),
            ("^report", true),
            ("summary$", true),
            ("*.pdf", true),
            ("*سنوي*", true),
            ("mon?hly*", true),
            ("report ext:docx", true),
            ("report ext:pdf", true),
            ("لاعيلثف", false),
            ("report لاعيلثف", false),
            ("\"report  zz\"", false),
            ("^qzx", false),
            ("*.qzx", false),
            ("رشوشف ext:pdf", false),
            ("report (لاعيلثف)", false),
            // Not required: under OR or NOT, or fuzzy.
            ("report | لاعيلثف", true),
            ("report -لاعيلثف", true),
            ("fuzzy: لاعيلثف", true),
            ("fuzzy:qzxwv", true),
        ];
        for (text, expected) in cases {
            let ast = athrix_query::parse(text).unwrap();
            let may = may_match(&ix, &grams, &ast);
            assert_eq!(may, expected, "{text}");
            if !may {
                let q = compile_query(text).unwrap();
                assert_eq!(
                    count_matches(&ix, &q.query, 10, None, &c).unwrap(),
                    0,
                    "{text}"
                );
            }
        }
        // Every word of every name, plain, quoted or between stars: never skipped.
        for id in 0..ix.id_bound() as u32 {
            let name = ix.name(id).to_owned();
            for word in name.split([' ', '.', '/']).filter(|w| !w.is_empty()) {
                for text in [word.to_owned(), format!("\"{word}\""), format!("*{word}*")] {
                    let ast = athrix_query::parse(&text).unwrap();
                    assert!(may_match(&ix, &grams, &ast), "{text}");
                }
            }
        }
    }

    #[test]
    fn the_budget_gives_no_suggestion_and_cancel_still_cancels() {
        let spin = |stop: &AtomicBool| -> Result<Option<()>, SearchFailure> {
            while !stop.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
            Err(SearchFailure::Cancelled)
        };
        let t = Instant::now();
        let quiet = AtomicBool::new(false);
        assert_eq!(
            with_deadline(&quiet, Duration::from_millis(5), spin),
            Ok(None)
        );
        assert!(t.elapsed() >= Duration::from_millis(5));
        let cancelled = AtomicBool::new(false);
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(5));
                cancelled.store(true, Ordering::Relaxed);
            });
            with_deadline(&cancelled, Duration::from_secs(60), spin)
        });
        assert_eq!(outcome, Err(SearchFailure::Cancelled));
        // A probe that finishes in time keeps its answer.
        assert_eq!(
            with_deadline(&quiet, Duration::from_secs(60), |_| Ok(Some(7))),
            Ok(Some(7))
        );
    }

    /// The suggestions are the same before and after the gram filter is built.
    #[test]
    fn suggestions_do_not_depend_on_the_filter_being_ready() {
        let ix = view();
        let texts = ["hgjrvdv ext:pdf", "قثحخقف", "rprt", "zzzzqqqq", "ext:zip"];
        let before: Vec<_> = texts.iter().map(|t| run(&ix, t)).collect();
        for _ in 0..1000 {
            if gram_filter(&ix).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(gram_filter(&ix).is_some());
        let after: Vec<_> = texts.iter().map(|t| run(&ix, t)).collect();
        assert_eq!(before, after);
    }

    /// Timing of [`suggest`] on a real name snapshot (not run by default), against the probes
    /// as they were before the gram filter and the budget. Every keystroke of a typing set
    /// that finds nothing gets both timed, interleaved; prints p50 / p95 of all probes:
    ///
    /// `ATHRIX_SUGGEST_SNAPSHOT=…\names.snap cargo test --release -p athrix-desktop --lib
    ///  suggest::tests::probe_timings -- --ignored --nocapture`
    ///
    /// The snapshot (format v1) is only read; names are re-normalized with today's rules.
    /// `ATHRIX_SUGGEST_ROUNDS` (default 9) and `ATHRIX_SUGGEST_BUDGET_MS` (default
    /// [`PROBE_BUDGET`]) adjust the run.
    #[test]
    #[ignore = "needs a real snapshot (ATHRIX_SUGGEST_SNAPSHOT)"]
    fn probe_timings() {
        const TYPING: &[&str] = &[
            "report",
            "تقرير",
            "invoice",
            "فاتورة",
            "*.pdf",
            "ext:docx size:>1mb",
            "budget 2024",
            "type:image vacation",
            "fuzzy:rprt",
            "زززز",
            "qzxwv",
            "managmnet plan",
            "فاتوره الكهرباء",
        ];
        let Ok(path) = std::env::var("ATHRIX_SUGGEST_SNAPSHOT") else {
            eprintln!("ATHRIX_SUGGEST_SNAPSHOT not set");
            return;
        };
        let env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
        let rounds = env("ATHRIX_SUGGEST_ROUNDS").map_or(9, |r| r as usize);
        let budget = env("ATHRIX_SUGGEST_BUDGET_MS").map_or(PROBE_BUDGET, Duration::from_millis);
        let t = Instant::now();
        let ix = IndexView::from_index(1, load_v1(&path));
        eprintln!("{} entries loaded in {:?}", ix.len(), t.elapsed());
        let limit = 200;
        let c = AtomicBool::new(false);
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get() / 2);
        let t = Instant::now();
        let built = GramFilter::of_index(ix.snapshot().base(), threads);
        let set: u32 = built.words.iter().map(|w| w.count_ones()).sum();
        eprintln!(
            "gram filter built in {:?} on {threads} threads, {:.1}% of bits set",
            t.elapsed(),
            f64::from(set) * 100.0 / (1u64 << GRAM_BITS_LOG2) as f64
        );
        drop(built);
        // Start the app's background build and wait for it.
        while gram_filter(&ix).is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        // The probes before the gram filter and the budget.
        let baseline = |text: &str, q: &CompiledQuery| -> Option<SearchSuggestion> {
            let candidates = [
                (swap_terms(text, false), SuggestionKind::Layout),
                (fuzzy_variant(text, q), SuggestionKind::Fuzzy),
            ];
            for (candidate, kind) in candidates {
                let Some(candidate) = candidate else { continue };
                let Ok(compiled) = compile_query(&candidate) else {
                    continue;
                };
                let count =
                    count_matches(&ix, &compiled.query.unordered(), limit, None, &c).unwrap();
                if count > 0 {
                    return Some(SearchSuggestion {
                        query: candidate,
                        kind,
                        count,
                    });
                }
            }
            None
        };
        let show = |s: &Option<SearchSuggestion>| {
            s.as_ref().map_or_else(
                || "-".to_owned(),
                |s| format!("{:?} {:?} ({})", s.kind, s.query, s.count),
            )
        };
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let (mut all_old, mut all_new) = (Vec::new(), Vec::new());
        for word in TYPING {
            for (i, ch) in word.char_indices() {
                let key = &word[..i + ch.len_utf8()];
                if key.ends_with(' ') {
                    continue;
                }
                let Ok(q) = compile_query(key) else { continue };
                if count_matches(&ix, &q.query, limit + 1, None, &c).unwrap() > 0 {
                    continue;
                }
                let (mut old, mut new) = (Vec::new(), Vec::new());
                let (mut got_old, mut got_new) = (None, None);
                for _ in 0..rounds {
                    let t = Instant::now();
                    got_old = baseline(key, &q);
                    old.push(ms(t));
                    let t = Instant::now();
                    got_new = suggest_within(&ix, key, &q, limit, None, &c, budget).unwrap();
                    new.push(ms(t));
                }
                old.sort_by(f64::total_cmp);
                new.sort_by(f64::total_cmp);
                // The same answer, or none (budget), or the next candidate when the old
                // layout rewrite normalized to nothing (`ززز` → `...`): never another one.
                let empty_rewrite = layout_swapped(key).is_none()
                    && got_old
                        .as_ref()
                        .is_some_and(|s| s.kind == SuggestionKind::Layout);
                if got_new.is_some() && !empty_rewrite {
                    assert_eq!(got_new, got_old, "{key}");
                }
                let was = if got_new == got_old {
                    String::new()
                } else {
                    format!("  (before: {})", show(&got_old))
                };
                println!(
                    "{key:<22} before p50 {:>6.2} max {:>6.2} | after p50 {:>6.2} max {:>6.2}  {}{was}",
                    old[old.len() / 2],
                    old[old.len() - 1],
                    new[new.len() / 2],
                    new[new.len() - 1],
                    show(&got_new),
                );
                all_old.extend(old);
                all_new.extend(new);
            }
        }
        let pct = |v: &mut Vec<f64>, p: f64| {
            v.sort_by(f64::total_cmp);
            v[((p / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1]
        };
        for (label, v) in [("before", &mut all_old), ("after ", &mut all_new)] {
            println!(
                "suggest {label}: p50 {:.2} ms  p95 {:.2} ms  max {:.2} ms  (n={})",
                pct(v, 50.0),
                pct(v, 95.0),
                pct(v, 100.0),
                v.len()
            );
        }
    }

    /// Rebuilds a v1 name snapshot from its original names (read only).
    fn load_v1(path: &str) -> FileIndex {
        let b = std::fs::read(path).unwrap();
        assert_eq!(&b[..8], b"ATHRXIDX");
        let u32le = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64le = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let n = u64le(16) as usize;
        let parents = 40;
        let flags = parents + 4 * n;
        let sizes = flags + n;
        let mtimes = sizes + 8 * n;
        let ends = mtimes + 8 * n;
        let names = ends + 4 * n;
        let mut ib = IndexBuilder::with_capacity(n, u64le(24) as usize / n.max(1) + 1);
        let mut from = 0;
        for i in 0..n {
            let to = u32le(ends + 4 * i) as usize;
            let name = std::str::from_utf8(&b[names + from..names + to]).unwrap();
            from = to;
            let p = u32le(parents + 4 * i);
            let f = b[flags + i];
            let m = u64le(mtimes + 8 * i) as i64;
            let meta = EntryMeta {
                is_dir: f & 1 != 0,
                is_symlink: f & 2 != 0,
                size: u64le(sizes + 8 * i),
                modified: (m != i64::MIN).then_some(m),
                ..Default::default()
            };
            ib.push((p != u32::MAX).then_some(p), name, meta);
        }
        ib.finish()
    }
}
