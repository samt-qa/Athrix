//! Tokenizer for full-text search: nass normalization + light stemming, with every token's
//! byte span in the ORIGINAL text, so snippets and highlights land on the diacritized words.
//!
//! # How offsets stay exact without a whole-document offset map
//!
//! The original text is first cut into word segments (see [`is_word_char`]): runs of letters,
//! digits, combining marks (harakat), tatweel and joiners. Diacritics never cross a word
//! boundary, so normalizing each segment on its own gives the same result as normalizing the
//! whole text, and each byte is normalized exactly once (ADR-009).
//!
//! - Usually a segment normalizes to one word, and its token span is the whole segment: the
//!   diacritized original, e.g. «الْمَكْتَبَاتُ».
//! - When a segment normalizes to several words (a ligature such as «ﷺ» expands to four words,
//!   or a character nass treats as punctuation), that one segment is normalized again with
//!   [`nass::normalize_with_map`] and each word gets its exact source span. This is rare, and
//!   the map is only as large as the segment, never the document.
//!
//! # Terms
//!
//! - Normalization is nass [`Level::Loose`]: diacritics, tatweel and invisible marks removed,
//!   alef/yaa/kaf variants unified, ة → ه, ى → ي, hamza seats folded (ؤ → و, ئ → ي), Arabic and
//!   Persian digits → ASCII, Latin lowercased. Content search favours recall; BM25 ranks.
//! - Arabic words are light-stemmed ([`light_stem`]). A word starting with the conjunction
//!   و / ف also yields its stem without it ([`stem_without_conjunction`]) as a second token
//!   at the same position, marked [`Token::is_variant`].
//! - Latin words are lowercased but not stemmed: an English stemmer would add a dependency and
//!   mangle identifiers and file-like words («news» → «new», «class» → «class»), while users
//!   searching English file contents usually type the form they remember. Non-ASCII uppercase
//!   (É) is lowercased here because nass lowercases ASCII only. Accents are kept (é ≠ e).
//! - Words longer than [`MAX_TOKEN_BYTES`] after normalization (hashes, base64) are dropped.
//!
//! The rules are versioned by [`ANALYZER_VERSION`]: an index built with another version must
//! be rebuilt.

use crate::stem::{light_stem, stem_without_conjunction};
use crate::{Level, Options};
use std::ops::Range;

/// Version of the tokenizer and stemmer rules. Bump it whenever a change would give a
/// different term for the same text, so persisted indexes know they must be rebuilt.
pub const ANALYZER_VERSION: u32 = 1;

/// Normalized words longer than this (in bytes) are not indexed.
pub const MAX_TOKEN_BYTES: usize = 80;

/// One token: the term to index and where it came from in the original text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Token {
    /// The term: normalized and, for Arabic words, stemmed.
    pub text: String,
    /// Byte offset of the source word in the original text.
    pub start: usize,
    /// Byte offset (exclusive) of the end of the source word in the original text.
    pub end: usize,
    /// Word position (0, 1, 2…). A variant shares the position of the token before it.
    pub position: usize,
    /// True for an extra form at the same position (the stem without a leading و / ف).
    pub is_variant: bool,
}

impl Token {
    /// The source span in the original text.
    pub fn span(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// Analyzer settings. The default is what the content index uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Analyzer {
    /// Apply the light stemmer to Arabic words (default `true`).
    pub stem: bool,
    /// Also emit the stem without a leading و / ف (default `true`). Only used with `stem`.
    pub conjunction_variants: bool,
}

impl Default for Analyzer {
    fn default() -> Self {
        Analyzer {
            stem: true,
            conjunction_variants: true,
        }
    }
}

impl Analyzer {
    /// Tokenizes `text`.
    pub fn tokens<'a>(&self, text: &'a str) -> Tokens<'a> {
        Tokens {
            analyzer: *self,
            text,
            cursor: 0,
            position: 0,
            started: false,
            pending: Vec::new(),
            norm: String::new(),
        }
    }
}

/// Tokenizes `text` with the default [`Analyzer`].
///
/// ```
/// use athrix_text_ar::tokenize;
/// let text = "قرأتُ التَّقَارِيرَ";
/// let t: Vec<_> = tokenize(text).collect();
/// assert_eq!(t[1].text, "تقرير");
/// assert_eq!(&text[t[1].span()], "التَّقَارِيرَ");
/// ```
pub fn tokenize(text: &str) -> Tokens<'_> {
    Analyzer::default().tokens(text)
}

/// Streaming tokenizer over one text; see [`Analyzer::tokens`].
///
/// Use [`advance`](Self::advance) to reuse one [`Token`] buffer (no allocation per token
/// once warmed up), or iterate to get owned tokens.
#[derive(Debug)]
pub struct Tokens<'a> {
    analyzer: Analyzer,
    text: &'a str,
    cursor: usize,
    /// Position of the next primary token.
    position: usize,
    /// Whether a primary token was already emitted (positions start at 0).
    started: bool,
    /// Tokens of the current segment not yet returned, in reverse order.
    pending: Vec<Token>,
    norm: String,
}

impl Tokens<'_> {
    /// Writes the next token into `out` and returns `true`, or returns `false` at the end.
    pub fn advance(&mut self, out: &mut Token) -> bool {
        loop {
            if let Some(t) = self.pending.pop() {
                out.text.clear();
                out.text.push_str(&t.text);
                out.start = t.start;
                out.end = t.end;
                out.position = t.position;
                out.is_variant = t.is_variant;
                return true;
            }
            let Some(seg) = self.next_segment() else {
                return false;
            };
            self.norm.clear();
            nass::normalize_into(&self.text[seg.clone()], &LOOSE, &mut self.norm);
            if self.norm.is_empty() {
                continue;
            }
            if !self.norm.contains(' ') {
                let word = std::mem::take(&mut self.norm);
                self.push_word(&word, seg);
                self.norm = word;
            } else {
                let n = nass::normalize_with_map(&self.text[seg.clone()], &LOOSE);
                let mut at = 0;
                for word in n.text.split(' ') {
                    let range = at..at + word.len();
                    at = range.end + 1;
                    if let Some(src) = n.source_range(range) {
                        self.push_word(word, seg.start + src.start..seg.start + src.end);
                    }
                }
            }
            self.pending.reverse();
        }
    }

    /// Next run of word characters in the original text.
    fn next_segment(&mut self) -> Option<Range<usize>> {
        let rest = &self.text[self.cursor..];
        let (skip, _) = rest.char_indices().find(|&(_, c)| is_word_char(c))?;
        let start = self.cursor + skip;
        let len = self.text[start..]
            .char_indices()
            .find(|&(_, c)| !is_word_char(c))
            .map_or(self.text.len() - start, |(i, _)| i);
        self.cursor = start + len;
        Some(start..start + len)
    }

    /// Queues the term(s) for one normalized word (in segment order; reversed afterwards).
    fn push_word(&mut self, word: &str, src: Range<usize>) {
        if word.is_empty() || word.len() > MAX_TOKEN_BYTES {
            return;
        }
        if self.started {
            self.position += 1;
        }
        self.started = true;
        let position = self.position;
        let token = |text: String, is_variant| Token {
            text,
            start: src.start,
            end: src.end,
            position,
            is_variant,
        };
        if !self.analyzer.stem {
            self.pending.push(token(lower(word), false));
            return;
        }
        let primary = light_stem(word);
        let variant = if self.analyzer.conjunction_variants {
            stem_without_conjunction(word).filter(|v| *v != primary)
        } else {
            None
        };
        let primary = lower(&primary);
        let variant = variant.map(|v| v.into_owned());
        self.pending.push(token(primary, false));
        if let Some(v) = variant {
            self.pending.push(token(v, true));
        }
    }
}

impl Iterator for Tokens<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        let mut t = Token::default();
        self.advance(&mut t).then_some(t)
    }
}

const LOOSE: Options = Options {
    level: Level::Loose,
    honorifics: nass::Honorifics::Keep,
};

/// Lowercases non-ASCII letters that nass leaves alone (it lowercases ASCII only).
fn lower(word: &str) -> String {
    if word.is_ascii() || !word.chars().any(char::is_uppercase) {
        word.to_owned()
    } else {
        word.to_lowercase()
    }
}

/// Whether `c` belongs to a word segment of the ORIGINAL text.
///
/// Word characters: ASCII letters and digits, any Unicode alphanumeric, combining marks
/// (Arabic harakat, Quranic marks, Latin/Hebrew combining marks), the Arabic blocks except
/// Arabic punctuation (، ؛ ؟ ٪ ٫ ٬ ٭ ۔), and the zero-width joiners and direction marks
/// (nass removes them, but they appear inside words). Everything else separates words,
/// including `_`, `.`, `-` and `'`: «تقرير_2026.pdf» gives «تقرير», «2026», «pdf».
///
/// This is a character-class splitter, not full UAX #29 word segmentation: UAX #29 keeps
/// «a_b», «3.14» and «can't» together, which suits prose but not file contents with paths and
/// identifiers, and it would add a dependency. Phrase search still finds «3.14» because the
/// query goes through the same splitter. Scripts without spaces (CJK, Thai) come out as one
/// token per run, which is a known limitation.
pub fn is_word_char(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    match c {
        '\u{060C}' | '\u{061B}' | '\u{061F}' | '\u{066A}'..='\u{066D}' | '\u{06D4}' => false,
        '\u{FD3E}' | '\u{FD3F}' => false, // ornate parentheses
        '\u{0600}'..='\u{06FF}'
        | '\u{0750}'..='\u{077F}'
        | '\u{08A0}'..='\u{08FF}'
        | '\u{FB50}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFE}' => true,
        '\u{200C}'..='\u{200F}' => true,
        '\u{0300}'..='\u{036F}'
        | '\u{0483}'..='\u{0489}'
        | '\u{0591}'..='\u{05C7}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE20}'..='\u{FE2F}' => true,
        _ => c.is_alphanumeric(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        tokenize(text).map(|t| t.text).collect()
    }

    #[test]
    fn offsets_point_at_the_diacritized_original() {
        let text = "قَرَأْتُ الْمَكْتَبَاتِ وَالتَّقَارِيرَ";
        let toks: Vec<_> = tokenize(text).collect();
        let got: Vec<_> = toks
            .iter()
            .map(|t| (t.text.as_str(), &text[t.span()], t.position))
            .collect();
        assert_eq!(
            got,
            [
                ("قرات", "قَرَأْتُ", 0),
                ("مكتب", "الْمَكْتَبَاتِ", 1),
                ("تقرير", "وَالتَّقَارِيرَ", 2),
            ]
        );
    }

    #[test]
    fn mixed_scripts_digits_and_punctuation() {
        assert_eq!(
            terms("تقرير_Report-2026.pdf، صفحة ١٢"),
            ["تقرير", "report", "2026", "pdf", "صفح", "12"]
        );
        assert_eq!(terms("ÉTÉ Straße"), ["été", "straße"]);
    }

    #[test]
    fn tatweel_hamza_and_alef_variants() {
        assert_eq!(terms("الـــعربية"), terms("العربيه"));
        assert_eq!(terms("مؤتمر رئيس مستشفى"), terms("موتمر رييس مستشفي"));
        assert_eq!(terms("إسلام آمن"), terms("اسلام امن"));
    }

    #[test]
    fn conjunction_variant_shares_the_position() {
        let text = "سيارات وسيارات";
        let toks: Vec<_> = tokenize(text).collect();
        let got: Vec<_> = toks
            .iter()
            .map(|t| (t.text.as_str(), t.position, t.is_variant, &text[t.span()]))
            .collect();
        assert_eq!(
            got,
            [
                ("سيار", 0, false, "سيارات"),
                ("وسيار", 1, false, "وسيارات"),
                ("سيار", 1, true, "وسيارات"),
            ]
        );
    }

    #[test]
    fn ligature_expansion_gets_one_position_per_word() {
        let text = "قال ﷺ: نعم";
        let toks: Vec<_> = tokenize(text).collect();
        let words: Vec<_> = toks.iter().map(|t| (t.position, &text[t.span()])).collect();
        assert_eq!(words.len(), 6, "{toks:?}");
        assert!(words[1..5].iter().all(|&(_, w)| w == "ﷺ"));
        assert_eq!(words[5], (5, "نعم"));
    }

    #[test]
    fn long_tokens_are_dropped_without_a_position() {
        let long = "a".repeat(MAX_TOKEN_BYTES + 1);
        let toks: Vec<_> = tokenize(&format!("one {long} two")).collect();
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[1].position, 1);
    }

    #[test]
    fn empty_and_separator_only_text() {
        assert!(terms("").is_empty());
        assert!(terms(" ،.- \n\t؟ ").is_empty());
        assert!(terms("\u{064E}\u{0640}").is_empty()); // a lone fatha and tatweel
    }

    #[test]
    fn unstemmed_analyzer() {
        let a = Analyzer {
            stem: false,
            ..Analyzer::default()
        };
        let t: Vec<_> = a.tokens("والمكتبات").map(|t| t.text).collect();
        assert_eq!(t, ["والمكتبات"]);
    }

    #[test]
    fn advance_reuses_the_buffer() {
        let mut it = tokenize("one اثنان three");
        let mut tok = Token::default();
        let mut n = 0;
        while it.advance(&mut tok) {
            n += 1;
        }
        assert_eq!(n, 3);
    }
}
