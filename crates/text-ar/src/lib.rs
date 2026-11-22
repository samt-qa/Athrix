//! Arabic text layer for Athrix, built on the [`nass`] normalization spec.
//!
//! nass handles normalization and offsets into the original text. This crate adds what file
//! search needs on top: name normalization for the index, keyboard-layout correction
//! ([`swap_layout`], Arabic (101) ⇄ US QWERTY), a light stemmer ([`light_stem`]) and the
//! content tokenizer ([`tokenize()`]) whose tokens carry byte spans in the original text. The
//! tokenizer does not depend on Tantivy; `athrix-content-index` adapts it.

pub mod layout;
pub mod stem;
pub mod tokenize;

pub use layout::{swap_layout, swap_layout_variants};
pub use nass::{Level, Match, Normalized, Options};
pub use stem::{is_arabic_word, light_stem, stem_without_conjunction};
pub use tokenize::{ANALYZER_VERSION, Analyzer, MAX_TOKEN_BYTES, Token, Tokens, tokenize};

/// Normalizes a file or folder name for the name index, writing into `out` (cleared first) so
/// one buffer can be reused for millions of names.
///
/// Uses the nass `search` level (diacritics, tatweel and invisible marks are removed, alef/yaa/kaf
/// variants and Arabic/Persian digits are unified, ى → ي, punctuation becomes spaces), then folds
/// the letters users commonly type either way:
///
/// | Folded | To | Example |
/// |---|---|---|
/// | ة (taa marbuta) | ه | «فاتورة» = «فاتوره» |
/// | ؤ (hamza on waw) | و | «مسؤول» = «مسوول» |
/// | ئ (hamza on yaa) | ي | «مسئول» = «مسيول», «جزئي» = «جزيي» |
///
/// These are the letter folds of nass `loose`, which content search uses. The one `loose` rule
/// names do NOT get is deleting the standalone hamza ء: it would shorten words such as «ماء» to
/// «ما», which as a substring of names matches almost everything.
///
/// Changing these rules changes every persisted name snapshot; the snapshot format detects it
/// (see `athrix-fs-index`), so keep its probe strings covering the rules.
pub fn normalize_name_into(name: &str, out: &mut String) {
    nass::normalize_into(name, &Options::default(), out);
    fold_name_letters(out);
}

/// Like [`normalize_name_into`], returning a new `String`.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    normalize_name_into(name, &mut out);
    out
}

/// Like [`normalize_name`], also mapping every byte of the result to its source span in `name`
/// (see [`Normalized::source_range`]), for turning matches on the normalized name into
/// highlight ranges of the original. Meant for the few rows on screen.
pub fn normalize_name_with_map(name: &str) -> Normalized {
    let mut n = nass::normalize_with_map(name, &Options::default());
    // Every fold replaces a 2-byte letter with a 2-byte letter, so the byte map stays exact.
    fold_name_letters(&mut n.text);
    n
}

/// Finds `query` in a displayed name and returns the matches as byte spans of the ORIGINAL
/// name, ready for highlighting: both sides are normalized like the index
/// ([`normalize_name_into`]) and every occurrence of the query is reported, without overlaps.
/// Meant for the few rows on screen, not for scanning the index.
pub fn find_in_name(name: &str, query: &str) -> Vec<Match> {
    let needle = normalize_name(query);
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = normalize_name_with_map(name);
    let mut out = Vec::new();
    let mut from = 0;
    let mut last_end = 0;
    while let Some(rel) = hay.text[from..].find(needle.as_str()) {
        let s = from + rel;
        let e = s + needle.len();
        if let Some(r) = hay.source_range(s..e) {
            if r.start >= last_end {
                last_end = r.end;
                out.push(Match {
                    start: r.start,
                    end: r.end,
                    text: name[r].to_string(),
                });
            }
        }
        from = e;
    }
    out
}

/// UTF-8 lead byte shared by ة (U+0629), ؤ (U+0624) and ئ (U+0626).
const LEAD_D8: u8 = 0xD8;

/// Applies the name letter folds of [`normalize_name_into`] in place: ة → ه, ؤ → و, ئ → ي.
///
/// Each fold swaps one 2-byte UTF-8 letter for another, so lengths and byte offsets are kept.
/// Names without these letters (most of them) are only scanned, never copied.
fn fold_name_letters(s: &mut String) {
    let b = s.as_bytes();
    let Some(first) = b
        .windows(2)
        .position(|w| w[0] == LEAD_D8 && matches!(w[1], 0xA4 | 0xA6 | 0xA9))
    else {
        return;
    };
    let mut bytes = std::mem::take(s).into_bytes();
    let mut i = first;
    while i + 1 < bytes.len() {
        if bytes[i] == LEAD_D8 {
            let to = match bytes[i + 1] {
                0xA9 => Some(0x87), // ة U+0629 → ه U+0647
                0xA4 => Some(0x88), // ؤ U+0624 → و U+0648
                0xA6 => Some(0x8A), // ئ U+0626 → ي U+064A
                _ => None,
            };
            if let Some(to) = to {
                bytes[i] = 0xD9;
                bytes[i + 1] = to;
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    // Only whole 2-byte sequences were swapped for other valid 2-byte sequences.
    *s = String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_diacritics_from_names() {
        assert_eq!(
            normalize_name("مُحَاضَرَةُ الفِقْهِ"),
            normalize_name("محاضرة الفقه")
        );
    }

    #[test]
    fn unifies_alef_variants_and_digits() {
        assert_eq!(normalize_name("أحمد"), normalize_name("احمد"));
        assert_eq!(normalize_name("إحصاء"), normalize_name("احصاء"));
        assert_eq!(normalize_name("فاتورة ١٢٣"), normalize_name("فاتورة 123"));
    }

    #[test]
    fn reuses_the_output_buffer() {
        let mut out = String::from("old content");
        normalize_name_into("تقرير", &mut out);
        assert_eq!(out, normalize_name("تقرير"));
    }

    #[test]
    fn finds_matches_in_the_original_vocalized_name() {
        let name = "مُحَاضَرَةُ الفِقْهِ - الدَّرْسُ الأَوَّل.docx";
        let m = find_in_name(name, "الفقه");
        assert_eq!(m.len(), 1);
        assert_eq!(&name[m[0].start..m[0].end], "الفِقْهِ");
    }

    #[test]
    fn map_points_back_into_the_original() {
        let name = "مُحَاضَرَة_Report.pdf";
        let n = normalize_name_with_map(name);
        assert_eq!(n.text, normalize_name(name));
        let at = n.text.find("report").unwrap();
        let r = n.source_range(at..at + "report".len()).unwrap();
        assert_eq!(&name[r], "Report");
    }

    #[test]
    fn mixed_arabic_and_latin_names() {
        let name = "تقرير_Report_2026.pdf";
        assert_eq!(find_in_name(name, "report").len(), 1);
        assert_eq!(find_in_name(name, "تقرير").len(), 1);
    }
    #[test]
    fn folds_taa_marbuta_and_hamza_seats() {
        assert_eq!(normalize_name("فاتوره"), normalize_name("فاتورة"));
        assert_eq!(normalize_name("مسؤول"), normalize_name("مسوول"));
        assert_eq!(normalize_name("مسئول"), normalize_name("مسيول"));
        assert_eq!(normalize_name("مستشفى"), normalize_name("مستشفي"));
        assert_eq!(normalize_name("فاتورة_الكهرباء.PDF"), "فاتوره الكهرباء pdf");
        // The standalone hamza is kept (unlike content search's loose level).
        assert_eq!(normalize_name("ماء"), "ماء");
        // Other letters with the same UTF-8 lead byte are untouched.
        assert_eq!(normalize_name("ءآأؤإئابة"), "ءااوايابه");
    }

    #[test]
    fn finds_folded_letters_both_ways() {
        let name = "فاتورة الكهرباء.pdf";
        let m = find_in_name(name, "فاتوره");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "فاتورة");
        let name = "فاتوره قديمة";
        assert_eq!(find_in_name(name, "فاتورة")[0].text, "فاتوره");
        let name = "تقرير المسؤول.docx";
        assert_eq!(find_in_name(name, "مسوول")[0].text, "مسؤول");
        assert_eq!(find_in_name("مسوول", "مسؤول")[0].text, "مسوول");
    }

    #[test]
    fn folded_map_points_back_into_the_original() {
        let name = "مُحَاضَرَةُ_المسؤولية.pdf";
        let n = normalize_name_with_map(name);
        assert_eq!(n.text, normalize_name(name));
        let at = n.text.find("مسووليه").unwrap();
        let r = n.source_range(at..at + "مسووليه".len()).unwrap();
        assert_eq!(&name[r], "مسؤولية");
    }

    #[test]
    fn find_reports_every_occurrence_without_overlap() {
        let m = find_in_name("aaaa", "aa");
        assert_eq!(
            m.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            [(0, 2), (2, 4)]
        );
        assert!(find_in_name("abc", "  ").is_empty());
        assert!(find_in_name("abc", "x").is_empty());
    }
}
