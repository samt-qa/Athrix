//! Arabic text layer for Athrix, built on the [`nass`] normalization spec.
//!
//! nass handles normalization and offsets into the original text. This crate adds what file
//! search needs on top: name normalization for the index, and later light stemming,
//! keyboard-layout correction and the Tantivy tokenizer (see `docs/03-PROGRESS-TRACKER.md`, M3).

pub use nass::{Level, Match, Normalized, Options};

/// Normalizes a file or folder name for the name index, writing into `out` (cleared first) so
/// one buffer can be reused for millions of names.
///
/// Uses the nass `search` level: diacritics, tatweel and invisible marks are removed, alef/yaa/kaf
/// variants and Arabic/Persian digits are unified, and punctuation becomes spaces.
pub fn normalize_name_into(name: &str, out: &mut String) {
    nass::normalize_into(name, &Options::default(), out);
}

/// Like [`normalize_name_into`], returning a new `String`.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    normalize_name_into(name, &mut out);
    out
}

/// Finds `query` in a displayed name and returns the matches as byte spans of the ORIGINAL
/// name, ready for highlighting. Meant for the few rows on screen, not for scanning the index.
pub fn find_in_name(name: &str, query: &str) -> Vec<Match> {
    nass::find_all(name, query, &nass::SearchOptions::default())
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
    fn mixed_arabic_and_latin_names() {
        let name = "تقرير_Report_2026.pdf";
        assert_eq!(find_in_name(name, "report").len(), 1);
        assert_eq!(find_in_name(name, "تقرير").len(), 1);
    }
}
