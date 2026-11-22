//! Property tests of the Arabic text layer: normalization, offset maps, the tokenizer, the light
//! stemmer and keyboard-layout swapping, on arbitrary Unicode and on Arabic-heavy "soup".
//!
//! Default: 256 cases per property (fast in `cargo test`). Raise with `PROPTEST_CASES=10000`.

use athrix_text_ar::{
    Analyzer, Level, MAX_TOKEN_BYTES, Options, find_in_name, is_arabic_word, light_stem,
    normalize_name, normalize_name_into, normalize_name_with_map, stem_without_conjunction,
    swap_layout, swap_layout_variants, tokenize,
};
use proptest::prelude::*;

/// Characters that stress the Arabic layer: letters and their variants, harakat and Quranic
/// marks, tatweel, joiners and direction marks, Arabic punctuation, Arabic-Indic and Persian
/// digits, presentation forms and ligatures (ﷺ ﷲ لا), multi-byte whitespace, Latin with
/// accents and case expansions (İ ß ﬁ), combining Latin marks, CJK and emoji.
const TRICKY: &[char] = &[
    'ا', 'أ', 'إ', 'آ', 'ٱ', 'ء', 'ؤ', 'ئ', 'ى', 'ي', 'ة', 'ه', 'ل', 'م', 'ب', 'ت', 'و', 'ف', 'ك',
    'ک', 'ی', 'پ', 'چ', 'ژ', 'گ', '\u{064B}', '\u{064C}', '\u{064D}', '\u{064E}', '\u{064F}',
    '\u{0650}', '\u{0651}', '\u{0652}', '\u{0670}', '\u{06D6}', '\u{06E1}', '\u{0640}', '\u{200C}',
    '\u{200D}', '\u{200E}', '\u{200F}', '\u{061C}', '\u{202B}', '\u{202C}', '\u{2066}', '\u{FEFF}',
    '،', '؛', '؟', '٪', '٫', '٬', '۔', '«', '»', '٠', '١', '٩', '۰', '۴', '۹', '\u{FDFA}',
    '\u{FDF2}', '\u{FEFB}', '\u{FEF7}', '\u{FE8D}', '\u{FB50}', '\u{FD3E}', ' ', '\t', '\n',
    '\u{A0}', '\u{2003}', '\u{3000}', '_', '-', '.', '\'', '"', 'a', 'Z', 'é', 'É', 'İ', 'ß',
    '\u{FB01}', '\u{0301}', '\u{0308}', '中', '😀', '0', '9',
];

/// One character: mostly from [`TRICKY`], sometimes any Unicode scalar value.
fn tricky_char() -> impl Strategy<Value = char> {
    prop_oneof![
        4 => prop::sample::select(TRICKY),
        1 => any::<char>(),
    ]
}

fn tricky_string(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(tricky_char(), 0..max).prop_map(|v| v.into_iter().collect())
}

/// Arabic letters (U+0621..U+064A without the tatweel U+0640).
fn arabic_letter() -> impl Strategy<Value = char> {
    prop_oneof![(0x0621u32..=0x063A), (0x0641u32..=0x064A)]
        .prop_map(|u| char::from_u32(u).expect("Arabic letter"))
}

fn arabic_word(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(arabic_letter(), 1..max).prop_map(|v| v.into_iter().collect())
}

/// Presentation forms of harakat on a tatweel (U+FE70..U+FE7F, «ﹱ») and the medial shadda
/// ligatures (U+FCF2..U+FCF4, «ﳲ»). nass 0.4 expands them to a tatweel + marks AFTER it has
/// removed tatweel, so `normalize("ﹱ")` is «ـ» and normalizing that again gives «»:
/// normalization is not idempotent for them. Upstream nass issue, harmless here (such names
/// are very rare and only keep a stray tatweel); the idempotence properties skip them.
fn has_tatweel_form(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{FE70}'..='\u{FE7F}' | '\u{FCF2}'..='\u{FCF4}'))
}

/// The tokenizer's normalization options.
const LOOSE: Options = Options {
    level: Level::Loose,
    honorifics: nass::Honorifics::Keep,
};

fn assert_range_in(text: &str, r: &std::ops::Range<usize>) -> Result<(), TestCaseError> {
    prop_assert!(r.start <= r.end, "inverted range {r:?}");
    prop_assert!(
        r.end <= text.len(),
        "range {r:?} past the end ({})",
        text.len()
    );
    prop_assert!(
        text.is_char_boundary(r.start),
        "start of {r:?} not on a char boundary"
    );
    prop_assert!(
        text.is_char_boundary(r.end),
        "end of {r:?} not on a char boundary"
    );
    Ok(())
}

proptest! {
    // ------------------------------------------------------------ name normalization

    #[test]
    fn normalize_name_is_idempotent(s in tricky_string(60)) {
        prop_assume!(!has_tatweel_form(&s));
        let once = normalize_name(&s);
        prop_assert_eq!(normalize_name(&once), once.clone(), "input {:?}", s);
    }

    #[test]
    fn normalize_name_on_arbitrary_unicode(s in "\\PC{0,80}") {
        prop_assume!(!has_tatweel_form(&s));
        let once = normalize_name(&s);
        prop_assert_eq!(normalize_name(&once), once);
    }

    #[test]
    fn normalize_name_into_ignores_old_buffer_content(s in tricky_string(40), junk in "\\PC{0,20}") {
        let mut out = junk;
        normalize_name_into(&s, &mut out);
        prop_assert_eq!(out, normalize_name(&s));
    }

    /// Harakat, tatweel and the always-invisible marks are gone. (ZWNJ / ZWJ are only removed
    /// next to Arabic: nass keeps them elsewhere on purpose, SPEC C2, for Indic scripts.)
    #[test]
    fn normalized_text_has_no_marks_tatweel_or_invisibles(s in tricky_string(60)) {
        prop_assume!(!has_tatweel_form(&s));
        let n = normalize_name(&s);
        for c in n.chars() {
            prop_assert!(
                !matches!(c, '\u{064B}'..='\u{0652}' | '\u{0640}' | '\u{00AD}' | '\u{061C}'
                    | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}' | '\u{FEFF}'),
                "{:?} kept {:?}", s, c
            );
        }
        prop_assert!(!n.starts_with(' ') && !n.ends_with(' ') && !n.contains("  "), "{n:?}");
    }

    #[test]
    fn offset_map_points_into_the_original(s in tricky_string(60)) {
        let n = normalize_name_with_map(&s);
        prop_assert_eq!(&n.text, &normalize_name(&s));
        prop_assert_eq!(n.start.len(), n.text.len());
        prop_assert_eq!(n.end.len(), n.text.len());
        let mut last_start = 0;
        for (i, c) in n.text.char_indices() {
            let r = n.source_range(i..i + c.len_utf8()).expect("non-empty range");
            assert_range_in(&s, &r)?;
            prop_assert!(r.start < r.end, "char {c:?} maps to an empty source {r:?}");
            prop_assert!(r.start >= last_start, "source starts go backwards at {i}");
            last_start = r.start;
        }
        if !n.text.is_empty() {
            let all = n.source_range(0..n.text.len()).expect("whole text");
            assert_range_in(&s, &all)?;
        }
        prop_assert!(n.source_range(0..0).is_none());
        prop_assert!(n.source_range(0..n.text.len() + 1).is_none());
    }

    /// Names never keep the letters that name search folds (ة ؤ ئ), and swapping any of them
    /// for its folded letter in the original gives the same key.
    #[test]
    fn taa_marbuta_and_hamza_seats_fold(s in tricky_string(60)) {
        prop_assume!(!has_tatweel_form(&s));
        let n = normalize_name(&s);
        prop_assert!(!n.contains(['ة', 'ؤ', 'ئ']), "{:?} -> {:?}", s, n);
        let swapped: String = s.chars().map(|c| match c {
            'ة' => 'ه',
            'ؤ' => 'و',
            'ئ' => 'ي',
            c => c,
        }).collect();
        prop_assert_eq!(normalize_name(&swapped), n);
    }

    /// A query with a folded letter highlights the original spelling, and vice versa.
    #[test]
    fn folded_spelling_is_highlighted(
        before in arabic_word(5),
        stem in arabic_word(5),
        pick in prop::sample::select(&[('ة', 'ه'), ('ؤ', 'و'), ('ئ', 'ي')][..]),
        swap in any::<bool>(),
    ) {
        let (orig, alt) = if swap { (pick.1, pick.0) } else { (pick.0, pick.1) };
        let word = format!("{stem}{orig}");
        let name = format!("{before} {word}.pdf");
        let q = format!("{stem}{alt}");
        let ms = find_in_name(&name, &q);
        let start = before.len() + 1;
        prop_assert!(
            ms.iter().any(|m| m.start <= start && m.end >= start + word.len()),
            "{:?} in {:?}: {:?}", q, name, ms
        );
    }

    /// Matches returned for highlighting are on char boundaries, in order, non-overlapping,
    /// and normalize to the query.
    #[test]
    fn find_in_name_matches_are_valid(name in tricky_string(50), q in tricky_string(6)) {
        let nq = normalize_name(&q);
        let mut last_end = 0;
        for m in find_in_name(&name, &q) {
            assert_range_in(&name, &(m.start..m.end))?;
            prop_assert!(m.start >= last_end, "overlapping matches");
            last_end = m.end;
            prop_assert_eq!(&m.text, &name[m.start..m.end]);
            prop_assert!(normalize_name(&m.text).contains(&nq),
                "{:?} does not normalize to contain {:?}", m.text, nq);
        }
    }

    /// A word of a diacritized name is found by its bare spelling, and the highlight covers
    /// the whole diacritized word in the original.
    #[test]
    fn bare_query_highlights_the_whole_diacritized_word(
        before in arabic_word(6),
        letters in prop::collection::vec(
            (arabic_letter(), prop::option::of(prop::sample::select(
                &['\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}', '\u{0652}', '\u{064B}', '\u{0640}'][..]
            ))),
            2..8,
        ),
        after in arabic_word(6),
        sep in prop::sample::select(&[" ", "_", "-", " - ", ".", "\u{A0}"][..]),
    ) {
        let word: String = letters.iter().flat_map(|(l, m)| std::iter::once(*l).chain(*m)).collect();
        let bare: String = letters.iter().map(|(l, _)| *l).collect();
        let name = format!("{before}{sep}{word}{sep}{after}.pdf");
        let word_start = before.len() + sep.len();
        let last_letter_end = word_start
            + word.char_indices().rfind(|(_, c)| letters.iter().any(|(l, _)| l == c))
                .map(|(i, c)| i + c.len_utf8()).unwrap();
        let ms = find_in_name(&name, &bare);
        prop_assert!(
            ms.iter().any(|m| m.start <= word_start && m.end >= last_letter_end),
            "{:?} in {:?}: {:?}", bare, name, ms
        );
    }

    // ------------------------------------------------------------ tokenizer

    #[test]
    fn tokens_have_valid_ordered_spans(s in tricky_string(80)) {
        let mut last_start = 0;
        let mut last_pos: Option<usize> = None;
        for t in tokenize(&s) {
            assert_range_in(&s, &t.span())?;
            prop_assert!(t.start < t.end, "empty span {t:?}");
            prop_assert!(t.start >= last_start, "spans go backwards: {t:?}");
            last_start = t.start;
            prop_assert!(!t.text.is_empty());
            prop_assert!(!t.text.contains(char::is_whitespace), "{t:?}");
            if t.is_variant {
                prop_assert_eq!(Some(t.position), last_pos, "variant must share the position");
            } else {
                let expected = last_pos.map_or(0, |p| p + 1);
                prop_assert_eq!(t.position, expected, "positions must count up by one");
            }
            last_pos = Some(t.position);
        }
    }

    #[test]
    fn tokenizer_on_arbitrary_unicode(s in "\\PC{0,120}") {
        for t in tokenize(&s) {
            assert_range_in(&s, &t.span())?;
        }
    }

    /// The streaming `advance` API yields exactly what the iterator does, for every analyzer.
    #[test]
    fn advance_matches_iterator(s in tricky_string(60), stem: bool, conj: bool) {
        let a = Analyzer { stem, conjunction_variants: conj };
        let all: Vec<_> = a.tokens(&s).collect();
        let mut it = a.tokens(&s);
        let mut tok = athrix_text_ar::Token { text: "garbage".into(), ..Default::default() };
        let mut streamed = Vec::new();
        while it.advance(&mut tok) {
            streamed.push(tok.clone());
        }
        prop_assert_eq!(all, streamed);
    }

    /// Tokenizing the source slice of a token alone gives that token's term again: offsets
    /// really delimit what produced the term.
    #[test]
    fn a_token_span_retokenizes_to_its_term(s in tricky_string(60)) {
        for t in tokenize(&s) {
            let again: Vec<String> = tokenize(&s[t.span()]).map(|t| t.text).collect();
            prop_assert!(again.contains(&t.text), "{:?} from {:?} gives {:?}", t.text, &s[t.span()], again);
        }
    }

    /// Normalizing first (Loose, what the tokenizer does per word) does not change the terms:
    /// the tokenizer's per-segment normalization equals whole-text normalization.
    #[test]
    fn tokenize_normalized_text_gives_the_same_terms(s in tricky_string(60)) {
        prop_assume!(!has_tatweel_form(&s));
        let normalized = nass::normalize(&s, &LOOSE);
        let a: Vec<String> = tokenize(&s).map(|t| t.text).collect();
        let b: Vec<String> = tokenize(&normalized).map(|t| t.text).collect();
        prop_assert_eq!(a, b, "normalized: {:?}", normalized);
    }

    /// Diacritics, tatweel and letter variants never change the terms.
    #[test]
    fn harakat_and_tatweel_do_not_change_terms(
        words in prop::collection::vec(arabic_word(8), 1..6),
        marks in prop::collection::vec(prop::sample::select(
            &['\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}', '\u{0652}', '\u{064B}', '\u{064C}', '\u{064D}', '\u{0640}', '\u{0670}'][..]
        ), 0..40),
    ) {
        let plain = words.join(" ");
        // Sprinkle marks after letters (never before the first letter of a word).
        let mut marks = marks.into_iter();
        let mut vocalized = String::new();
        for c in plain.chars() {
            vocalized.push(c);
            if c != ' ' {
                if let Some(m) = marks.next() { vocalized.push(m); }
            }
        }
        let a: Vec<String> = tokenize(&plain).map(|t| t.text).collect();
        let b: Vec<String> = tokenize(&vocalized).map(|t| t.text).collect();
        prop_assert_eq!(a, b, "{:?} vs {:?}", plain, vocalized);
    }

    #[test]
    fn very_long_words_are_dropped_not_truncated(n in (MAX_TOKEN_BYTES / 2 + 1)..200usize) {
        let long = "ب".repeat(n);
        let text = format!("قبل {long} بعد");
        let t: Vec<_> = tokenize(&text).collect();
        prop_assert_eq!(t.len(), 2);
        prop_assert_eq!(t[1].position, 1);
    }

    // ------------------------------------------------------------ stemmer

    #[test]
    fn light_stem_shrinks_and_stays_arabic(w in arabic_word(14)) {
        let s = light_stem(&w);
        prop_assert!(!s.is_empty());
        // The divine name keeps its article on purpose: «لله» → «الله» (see the stem module).
        let allah = s == "الله";
        prop_assert!(allah || s.chars().count() <= w.chars().count(), "{w:?} -> {s:?}");
        prop_assert!(is_arabic_word(&s), "{w:?} -> {s:?}");
        if w.chars().count() <= 3 && !allah {
            prop_assert_eq!(s.as_ref(), w.as_str());
        }
        if let Some(v) = stem_without_conjunction(&w) {
            prop_assert!(!v.is_empty() && is_arabic_word(&v));
            prop_assert!(v.chars().count() < w.chars().count());
        }
    }

    #[test]
    fn light_stem_never_panics_and_leaves_non_arabic_alone(w in tricky_string(20)) {
        let s = light_stem(&w);
        if !is_arabic_word(&w) && !w.is_empty() {
            prop_assert_eq!(s.as_ref(), w.as_str());
        }
        let _ = stem_without_conjunction(&w);
    }

    // ------------------------------------------------------------ keyboard layout

    #[test]
    fn swap_layout_is_the_first_variant(s in tricky_string(30), max in 1usize..6) {
        let one = swap_layout(&s);
        let vars = swap_layout_variants(&s, max);
        prop_assert!(vars.len() <= max);
        prop_assert_eq!(one.as_ref(), vars.first());
        prop_assert!(vars.iter().all(|v| *v != s));
        prop_assert!(swap_layout_variants(&s, 0).is_empty());
    }

    #[test]
    fn swap_layout_on_arbitrary_unicode(s in "\\PC{0,60}") {
        if let Some(out) = swap_layout(&s) {
            prop_assert_ne!(out, s);
        }
    }

    /// Latin text maps to text without any Latin letter left (every letter key has an Arabic
    /// output, shifted or not).
    #[test]
    fn latin_text_becomes_letter_free(s in "[a-zA-Z0-9 ]{1,30}") {
        prop_assume!(s.chars().any(|c| c.is_ascii_alphabetic()));
        let out = swap_layout(&s).expect("Latin letters always swap");
        prop_assert!(!out.chars().any(|c| c.is_ascii_alphabetic()), "{s:?} -> {out:?}");
    }

    /// English typed on the Arabic layout and swapped back round-trips, for every unshifted
    /// key, except `g` followed by `h`: Windows types the same `لا` as the dedicated `b` key.
    #[test]
    fn unshifted_keys_round_trip(s in "[a-z][a-z`\\[\\];',./0-9 ]{0,29}") {
        prop_assume!(!s.contains("gh"));
        let arabic = swap_layout(&s).expect("has letters");
        prop_assert_eq!(swap_layout(&arabic), Some(s.clone()), "via {:?}", arabic);
    }

    /// Arabic typed on the US layout and swapped back round-trips, for the letters of the
    /// unshifted keys.
    #[test]
    fn arabic_letters_round_trip(
        s in prop::collection::vec(
            prop::sample::select(&[
                'ض', 'ص', 'ث', 'ق', 'ف', 'غ', 'ع', 'ه', 'خ', 'ح', 'ج', 'د', 'ش', 'س', 'ي', 'ب', 'ل',
                'ا', 'ت', 'ن', 'م', 'ك', 'ط', 'ئ', 'ء', 'ؤ', 'ر', 'ى', 'ة', 'و', 'ز', 'ظ', 'ذ', ' ',
                '1', '7',
            ][..]),
            1..30,
        ).prop_map(|v| v.into_iter().collect::<String>())
    ) {
        let english = swap_layout(&s);
        // Only punctuation keys (ج د ك ط و ز ظ ذ) give no Latin letter to swap back from.
        let latin_letter = english.as_deref().is_some_and(|e| e.chars().any(|c| c.is_ascii_lowercase()));
        prop_assume!(latin_letter);
        let english = english.unwrap();
        prop_assert_eq!(swap_layout(&english), Some(s.clone()), "via {:?}", english);
    }
}
