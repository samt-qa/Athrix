//! Light Arabic stemmer for the content index, in the spirit of Larkey et al.'s *light10*.
//!
//! It strips frequent prefixes (the article and its prepositional/conjunctive forms) and
//! suffixes (pronouns, sound plurals, duals, ta marbuta, nisba), and folds one very common
//! broken-plural pattern (تفاعيل → تفعيل). It does NOT extract roots: «مكتبة» and «كاتب» stay
//! different, which keeps precision high for search.
//!
//! The input is one word, preferably normalized with nass at [`Level::Loose`](crate::Level)
//! (ة → ه, ى → ي, hamza seats folded), which is what the content tokenizer feeds it. Text at
//! the `Search` level also works: ة and ية are handled as well.
//!
//! Every rule is length-guarded: a prefix or suffix is removed only when enough letters remain,
//! and words of 3 letters or fewer are never changed. The exact rules:
//!
//! | Step | Removed | Only when |
//! |---|---|---|
//! | prefix | وال فال بال كال لل | ≥ 3 letters remain |
//! | prefix | ال | ≥ 2 letters remain |
//! | pronoun suffix (once) | هما كما هم هن كم ها نا ه ي | ≥ 3 letters remain |
//! | …or else the alef of tanween fath (تقريرًا → تقريرا) | ا | ≥ 4 letters remain |
//! | ending suffix (up to twice) | ات ون ين يه ية ه ة ي | ≥ 3 letters remain |
//! | ending suffix | ان | ≥ 4 letters remain |
//! | broken plural | تفاعيل → تفعيل (6 letters: ت ? ا ? ي ?) | always |
//!
//! A bare conjunction (و / ف) is ambiguous: «وزارة» is not «و + زارة». [`light_stem`] keeps it;
//! [`stem_without_conjunction`] gives the stem with it removed, and the tokenizer indexes both
//! at the same position, so a search for «سيارات» finds «وسيارات» without breaking «وزارة».
//!
//! «الله» and its prefixed forms (والله، بالله، لله…) are kept as «الله» instead of becoming «له».

use std::borrow::Cow;

/// Prefixes of the article with a preposition or conjunction, removed when ≥ 3 letters remain.
const ARTICLE_PREFIXES: [&str; 5] = ["وال", "فال", "بال", "كال", "لل"];

/// Pronoun suffixes, longest first: at most one is removed. `(suffix, min letters left)`.
/// The last entry is the alef that carries tanween fath («تقريرًا» normalizes to «تقريرا»);
/// it comes after نا / ها / هما / كما so those pronouns win.
const PRONOUN_SUFFIXES: [(&str, usize); 10] = [
    ("هما", 3),
    ("كما", 3),
    ("هم", 3),
    ("هن", 3),
    ("كم", 3),
    ("ها", 3),
    ("نا", 3),
    ("ه", 3),
    ("ي", 3),
    ("ا", 4),
];

/// Plural, dual, feminine and nisba endings, longest first. `(suffix, min letters left)`.
const ENDING_SUFFIXES: [(&str, usize); 9] = [
    ("ات", 3),
    ("ون", 3),
    ("ين", 3),
    ("ان", 4),
    ("يه", 3),
    ("ية", 3),
    ("ه", 3),
    ("ة", 3),
    ("ي", 3),
];

/// How many times the ending rules may apply (e.g. «عربيات» → «عربي» → «عرب»).
const ENDING_PASSES: usize = 2;

/// Forms of the divine name that must not lose their article.
const ALLAH_FORMS: [&str; 9] = [
    "الله",
    "لله",
    "والله",
    "بالله",
    "فالله",
    "تالله",
    "ولله",
    "فلله",
    "كالله",
];

/// Letter count of an Arabic word (every Arabic letter is 2 bytes in UTF-8).
fn letters(s: &str) -> usize {
    s.chars().count()
}

/// True if `word` is made only of Arabic letters (U+0621..U+064A and the extended letters
/// U+0671..U+06D3, e.g. Persian پ چ ژ گ). Words with digits, Latin letters or marks are not
/// stemmed.
pub fn is_arabic_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| matches!(c, '\u{0621}'..='\u{064A}' | '\u{0671}'..='\u{06D3}'))
}

/// Returns the light stem of one word. Non-Arabic words and words of 3 letters or fewer are
/// returned unchanged. See the [module docs](self) for the rules.
///
/// ```
/// use athrix_text_ar::light_stem;
/// assert_eq!(light_stem("والمكتبات"), "مكتب");
/// assert_eq!(light_stem("بالتقارير"), "تقرير");
/// assert_eq!(light_stem("المعلمون"), "معلم");
/// assert_eq!(light_stem("كتب"), "كتب");
/// ```
pub fn light_stem(word: &str) -> Cow<'_, str> {
    if ALLAH_FORMS.contains(&word) {
        return Cow::Borrowed("الله");
    }
    if !is_arabic_word(word) || letters(word) <= 3 {
        return Cow::Borrowed(word);
    }
    let w = strip_suffixes(strip_article(word));
    match fold_tafaeel(w) {
        Some(folded) => Cow::Owned(folded),
        None => Cow::Borrowed(w),
    }
}

/// The stem of `word` without a leading conjunction و / ف, or `None` when the word does not
/// start with one, or too little would remain (fewer than 4 letters), or the conjunction is
/// part of an article prefix that [`light_stem`] already removes (وال / فال).
///
/// ```
/// use athrix_text_ar::stem_without_conjunction;
/// assert_eq!(stem_without_conjunction("وسيارات").as_deref(), Some("سيار"));
/// assert_eq!(stem_without_conjunction("وزير"), None); // too short to be sure
/// ```
pub fn stem_without_conjunction(word: &str) -> Option<Cow<'_, str>> {
    if !is_arabic_word(word) {
        return None;
    }
    let rest = word.strip_prefix('و').or_else(|| word.strip_prefix('ف'))?;
    if letters(rest) < 4 {
        return None;
    }
    if let Some(after_article) = rest.strip_prefix("ال") {
        if letters(after_article) >= 3 {
            return None; // وال / فال: light_stem handles it
        }
    }
    Some(light_stem(rest))
}

fn strip_article(word: &str) -> &str {
    for p in ARTICLE_PREFIXES {
        if let Some(rest) = word.strip_prefix(p) {
            if letters(rest) >= 3 {
                return rest;
            }
        }
    }
    if let Some(rest) = word.strip_prefix("ال") {
        if letters(rest) >= 2 {
            return rest;
        }
    }
    word
}

fn strip_one<'a>(word: &'a str, rules: &[(&str, usize)]) -> Option<&'a str> {
    rules.iter().find_map(|&(suffix, min_left)| {
        word.strip_suffix(suffix)
            .filter(|rest| letters(rest) >= min_left)
    })
}

fn strip_suffixes(word: &str) -> &str {
    let mut w = strip_one(word, &PRONOUN_SUFFIXES).unwrap_or(word);
    for _ in 0..ENDING_PASSES {
        match strip_one(w, &ENDING_SUFFIXES) {
            Some(rest) => w = rest,
            None => break,
        }
    }
    w
}

/// تفاعيل → تفعيل: «تقارير» → «تقرير», «تعاليم» → «تعليم», «تفاصيل» → «تفصيل».
fn fold_tafaeel(w: &str) -> Option<String> {
    let mut it = w.chars();
    let c: [char; 6] = [
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
    ];
    if it.next().is_some() || c[0] != 'ت' || c[2] != 'ا' || c[4] != 'ي' {
        return None;
    }
    Some([c[0], c[1], c[3], c[4], c[5]].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Level, Options};

    /// Stems a word the way the tokenizer does: loose normalization first.
    fn s(word: &str) -> String {
        let n = nass::normalize(word, &Options::level(Level::Loose));
        light_stem(&n).into_owned()
    }

    #[test]
    fn article_with_prepositions_and_plurals() {
        assert_eq!(s("والمكتبات"), "مكتب");
        assert_eq!(s("بالتقارير"), "تقرير");
        assert_eq!(s("المعلمون"), "معلم");
        assert_eq!(s("للمعلمين"), "معلم");
        assert_eq!(s("كالمعلمات"), "معلم");
        assert_eq!(s("فالطالبان"), "طالب");
        assert_eq!(s("المكتبة"), "مكتب");
        assert_eq!(s("مكتبة"), "مكتب");
    }

    #[test]
    fn diacritized_and_variant_spellings_share_a_stem() {
        assert_eq!(s("المَكْتَبَاتُ"), s("المكتبات"));
        assert_eq!(s("مدرسة"), s("مدرسه"));
        assert_eq!(s("مستشفى"), s("مستشفي"));
        assert_eq!(s("المؤتمرات"), s("موتمر"));
    }

    #[test]
    fn pronoun_then_plural_suffixes() {
        assert_eq!(s("مكتباتها"), "مكتب");
        assert_eq!(s("سياراتهم"), "سيار");
        assert_eq!(s("سياراتي"), "سيار");
        assert_eq!(s("كتابنا"), "كتاب");
        assert_eq!(s("كتابه"), "كتاب");
        assert_eq!(s("العربية"), "عرب");
        assert_eq!(s("عربيات"), "عرب");
        assert_eq!(s("عربي"), "عرب");
    }

    #[test]
    fn tanween_fath_alef() {
        assert_eq!(s("تقريرًا"), "تقرير");
        assert_eq!(s("كتابًا"), "كتاب");
        assert_eq!(s("اقتصادا"), "اقتصاد");
        assert_eq!(s("شكرًا"), "شكرا"); // only 3 letters would remain
        assert_eq!(s("كتابنا"), "كتاب"); // the pronoun wins
        assert_eq!(s("كتبها"), "كتب");
    }

    #[test]
    fn tafaeel_broken_plural() {
        assert_eq!(s("التقارير"), "تقرير");
        assert_eq!(s("تقاريرها"), "تقرير");
        assert_eq!(s("تقرير"), "تقرير");
        assert_eq!(s("التعاليم"), s("تعليمات"));
        assert_eq!(s("تفاصيل"), "تفصيل");
    }

    #[test]
    fn short_words_are_untouched() {
        for w in [
            "كتب", "فهم", "وجه", "بيت", "يد", "من", "الم", "الف", "مهم", "وقت",
        ] {
            assert_eq!(
                s(w),
                nass::normalize(w, &Options::level(Level::Loose)),
                "{w}"
            );
        }
    }

    #[test]
    fn over_stemming_guards() {
        // Prefix-like starts that are part of the word.
        assert_eq!(s("بالغ"), "بالغ");
        assert_eq!(s("والد"), "والد");
        assert_eq!(s("وزارة"), "وزار");
        assert_eq!(s("الوزارة"), "وزار");
        // Suffix-like ends guarded by length.
        assert_eq!(s("الفقه"), "فقه");
        assert_eq!(s("انسان"), "انسان"); // ان needs 4 letters left
        assert_eq!(s("لبنان"), "لبنان");
        assert_eq!(s("بيانات"), "بيان");
        assert_eq!(s("متهم"), "متهم");
        assert_eq!(s("حاكم"), "حاكم");
    }

    #[test]
    fn allah_keeps_its_article() {
        for w in ["الله", "والله", "بالله", "لله", "اللَّهِ"] {
            assert_eq!(s(w), "الله", "{w}");
        }
    }

    #[test]
    fn non_arabic_words_are_untouched() {
        assert_eq!(light_stem("reports"), "reports");
        assert_eq!(light_stem("2026"), "2026");
        assert_eq!(light_stem("تقرير2026"), "تقرير2026");
    }

    #[test]
    fn conjunction_variant() {
        assert_eq!(stem_without_conjunction("وسيارات").as_deref(), Some("سيار"));
        assert_eq!(
            stem_without_conjunction("وبالتقارير").as_deref(),
            Some("تقرير")
        );
        // Known limit: the ت of a ta marbuta before a pronoun is not restored.
        assert_eq!(
            stem_without_conjunction("فمكتبته").as_deref(),
            Some("مكتبت")
        );
        // Handled by light_stem itself.
        assert_eq!(stem_without_conjunction("والمكتبات"), None);
        // Too short to strip safely.
        assert_eq!(stem_without_conjunction("وزير"), None);
        assert_eq!(stem_without_conjunction("فكره"), None);
        assert_eq!(stem_without_conjunction("سيارات"), None);
        assert_eq!(stem_without_conjunction("word"), None);
    }
}
