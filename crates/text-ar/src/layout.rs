//! Keyboard-layout correction: text typed with the wrong layout active.
//!
//! Someone who means to type `التقرير` with the US layout active gets `hgjrvdv`; someone who
//! means `asdf` with the Arabic layout active gets `شسيب`. [`swap_layout`] maps such text to
//! what the same keys produce on the other layout.
//!
//! # The table
//!
//! [`KEYS`] is the Windows **Arabic (101)** layout (`KBDA1.DLL`, the default Arabic layout on
//! Windows) against **US QWERTY**, as listed by kbdlayout.info for KBDA1 and the printed
//! keycaps, both states (unshifted and Shift) of every key whose Arabic output differs from
//! the US one. Keys that produce the same character on both layouts (digits, `-`, `=`, `\`,
//! Shift+digits except 9/0) are left out, so they pass through unchanged.
//!
//! Notable keys:
//!
//! - `b` types two code points, `لا` (lam + alef); Shift+`b` `لآ`, Shift+`g` `لأ`, Shift+`t` `لإ`.
//! - Shift+`9`/`0` type `)`/`(` (mirrored for right-to-left text), unshifted `[`/`]` type `ج`/`د`
//!   and Shift+`d`/`f` type `]`/`[`.
//! - Shift on many letter keys types harakat (`َ ً ُ ٌ ِ ٍ ْ ّ`).
//!
//! # Ambiguity
//!
//! Arabic→English is ambiguous for lam + alef: `لا` comes from the `b` key, but also from `g`
//! then `h` (`ل` + `ا`), which is common in English (`light`, `night`, `high`). Windows emits
//! the same two code points either way. [`swap_layout`] picks the dedicated key (`b`);
//! [`swap_layout_variants`] lists the alternatives too.

/// One key: the US character and the Arabic text the same key types on Arabic (101).
/// Shifted keys are listed with their shifted US character (`'Q'`, `'{'`...).
pub const KEYS: &[(char, &str)] = &[
    // Unshifted.
    ('`', "ذ"),
    ('q', "ض"),
    ('w', "ص"),
    ('e', "ث"),
    ('r', "ق"),
    ('t', "ف"),
    ('y', "غ"),
    ('u', "ع"),
    ('i', "ه"),
    ('o', "خ"),
    ('p', "ح"),
    ('[', "ج"),
    (']', "د"),
    ('a', "ش"),
    ('s', "س"),
    ('d', "ي"),
    ('f', "ب"),
    ('g', "ل"),
    ('h', "ا"),
    ('j', "ت"),
    ('k', "ن"),
    ('l', "م"),
    (';', "ك"),
    ('\'', "ط"),
    ('z', "ئ"),
    ('x', "ء"),
    ('c', "ؤ"),
    ('v', "ر"),
    ('b', "لا"),
    ('n', "ى"),
    ('m', "ة"),
    (',', "و"),
    ('.', "ز"),
    ('/', "ظ"),
    // Shift.
    ('~', "\u{651}"), // shadda
    ('(', ")"),
    (')', "("),
    ('Q', "\u{64E}"), // fatha
    ('W', "\u{64B}"), // fathatan
    ('E', "\u{64F}"), // damma
    ('R', "\u{64C}"), // dammatan
    ('T', "لإ"),
    ('Y', "إ"),
    ('U', "\u{2018}"),
    ('I', "÷"),
    ('O', "×"),
    ('P', "؛"),
    ('{', "<"),
    ('}', ">"),
    ('A', "\u{650}"), // kasra
    ('S', "\u{64D}"), // kasratan
    ('D', "]"),
    ('F', "["),
    ('G', "لأ"),
    ('H', "أ"),
    ('J', "ـ"),
    ('K', "،"),
    ('L', "/"),
    ('Z', "~"),
    ('X', "\u{652}"), // sukun
    ('C', "}"),
    ('V', "{"),
    ('B', "لآ"),
    ('N', "آ"),
    ('M', "\u{2019}"),
    ('<', ","),
    ('>', "."),
    ('?', "؟"),
];

/// Lam-alef forms typed by one key, and the two-key spelling they can also come from.
const LAM_ALEF: &[(&str, char, &str)] = &[
    ("لا", 'b', "gh"),
    ("لأ", 'G', "gH"),
    ("لإ", 'T', "gY"),
    ("لآ", 'B', "gN"),
];

/// Presentation-form ligatures, treated like the two code points they stand for.
const LIGATURES: &[(char, &str)] = &[
    ('\u{FEFB}', "لا"),
    ('\u{FEFC}', "لا"),
    ('\u{FEF7}', "لأ"),
    ('\u{FEF8}', "لأ"),
    ('\u{FEF9}', "لإ"),
    ('\u{FEFA}', "لإ"),
    ('\u{FEF5}', "لآ"),
    ('\u{FEF6}', "لآ"),
];

/// Which way a text should be swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Latin letters typed while Arabic was meant.
    ToArabic,
    /// Arabic letters typed while English was meant.
    ToEnglish,
}

fn is_arabic_letter(c: char) -> bool {
    matches!(c, '\u{621}'..='\u{64A}' | '\u{671}'..='\u{6D3}' | '\u{FB50}'..='\u{FEFC}')
}

/// The direction for `text`, or `None` if it has no letters or mixes Latin and Arabic letters
/// (mixed text is most likely intentional, such as `تقرير report`).
fn direction(text: &str) -> Option<Direction> {
    let latin = text.chars().any(|c| c.is_ascii_alphabetic());
    let arabic = text.chars().any(is_arabic_letter);
    match (latin, arabic) {
        (true, false) => Some(Direction::ToArabic),
        (false, true) => Some(Direction::ToEnglish),
        _ => None,
    }
}

fn arabic_for(key: char) -> Option<&'static str> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, a)| *a)
}

fn key_for(arabic: char) -> Option<char> {
    KEYS.iter()
        .find(|(_, a)| {
            let mut cs = a.chars();
            cs.next() == Some(arabic) && cs.next().is_none()
        })
        .map(|(k, _)| *k)
}

fn to_arabic(text: &str) -> String {
    // Text without any lower-case letter was most likely typed with Caps Lock on, which does
    // not shift the Arabic layout: read it as unshifted keys.
    let caps_lock = !text.chars().any(|c| c.is_ascii_lowercase());
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        let key = if caps_lock { c.to_ascii_lowercase() } else { c };
        match arabic_for(key) {
            Some(a) => out.push_str(a),
            None => out.push(c),
        }
    }
    out
}

/// Expands presentation-form ligatures into their code points.
fn expand_ligatures(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match LIGATURES.iter().find(|(l, _)| *l == c) {
            Some((_, s)) => out.push_str(s),
            None => out.push(c),
        }
    }
    out
}

/// Arabic→English spellings of `text`: the first uses the dedicated key for every lam-alef,
/// the others the two-key spelling for some of them. At most `max` results.
fn to_english(text: &str, max: usize) -> Vec<String> {
    let text = expand_ligatures(text);
    let mut out = vec![String::with_capacity(text.len())];
    let mut rest = text.as_str();
    while let Some(c) = rest.chars().next() {
        if let Some((form, key, pair)) = LAM_ALEF.iter().find(|(f, _, _)| rest.starts_with(f)) {
            // Each existing spelling forks into the key and the pair.
            let n = out.len();
            for i in 0..n {
                if out.len() < max {
                    let mut alt = out[i].clone();
                    alt.push_str(pair);
                    out.push(alt);
                }
                out[i].push(*key);
            }
            rest = &rest[form.len()..];
            continue;
        }
        let mapped = key_for(c).unwrap_or(c);
        for s in &mut out {
            s.push(mapped);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Maps text typed on the wrong keyboard layout to what the same keys type on the other one,
/// between US QWERTY and Arabic (101) (see [`KEYS`]).
///
/// Latin text becomes Arabic (`hgjrvdv` → `التقرير`) and Arabic text becomes Latin (`شسيب` →
/// `asdf`). Characters that are the same on both layouts (spaces, digits) pass through.
/// Latin text with no lower-case letter is read as typed with Caps Lock (unshifted keys).
///
/// Returns `None` when there is nothing to swap: no letters, Latin and Arabic letters mixed,
/// or a result equal to the input. The function does not judge whether the result is a real
/// word; a caller suggests it ("did you mean…") only when it finds results, for example.
///
/// For `لا` the `b` key is assumed; see [`swap_layout_variants`] for `gh`.
pub fn swap_layout(text: &str) -> Option<String> {
    let out = match direction(text)? {
        Direction::ToArabic => to_arabic(text),
        Direction::ToEnglish => to_english(text, 1).pop()?,
    };
    (out != text).then_some(out)
}

/// Like [`swap_layout`], but for Arabic→English also returns the spellings where a lam-alef
/// came from two keys (`لا` = `g` + `h`) instead of the dedicated key. The first variant is
/// [`swap_layout`]'s result. At most `max` variants (an empty list when there is nothing to
/// swap or `max` is 0).
pub fn swap_layout_variants(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut out = match direction(text) {
        None => return Vec::new(),
        Some(Direction::ToArabic) => vec![to_arabic(text)],
        Some(Direction::ToEnglish) => to_english(text, max),
    };
    out.retain(|s| s != text);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_keys_to_arabic() {
        assert_eq!(swap_layout("hgjrvdv").as_deref(), Some("التقرير"));
        assert_eq!(swap_layout("lgt").as_deref(), Some("ملف"));
        assert_eq!(swap_layout("thj,vm").as_deref(), Some("فاتورة"));
        assert_eq!(swap_layout("lphqvm").as_deref(), Some("محاضرة"));
        assert_eq!(swap_layout("hgl;jf").as_deref(), Some("المكتب"));
        assert_eq!(swap_layout("ur]").as_deref(), Some("عقد"));
        assert_eq!(swap_layout("frhg").as_deref(), Some("بقال"));
        // `b` is lam-alef.
        assert_eq!(swap_layout("sbl").as_deref(), Some("سلام"));
        // Spaces and digits pass through.
        assert_eq!(swap_layout("hgjrvdv 2026").as_deref(), Some("التقرير 2026"));
        assert_eq!(
            swap_layout("jrvdv hgld.hkdm").as_deref(),
            Some("تقرير الميزانية")
        );
    }

    #[test]
    fn shifted_keys() {
        // Shift+h is hamza on alef.
        assert_eq!(swap_layout("Hpl]").as_deref(), Some("أحمد"));
        assert_eq!(swap_layout("Yfvhidl").as_deref(), Some("إبراهيم"));
        assert_eq!(swap_layout("Nl").as_deref(), Some("آم"));
        assert_eq!(swap_layout("Gwhs?").as_deref(), Some("لأصاس؟"));
        assert_eq!(swap_layout("Bhm").as_deref(), Some("لآاة"));
        // Shift+q types a fatha.
        assert_eq!(swap_layout("lQ").as_deref(), Some("م\u{64E}"));
    }

    #[test]
    fn caps_lock_reads_unshifted_keys() {
        assert_eq!(swap_layout("HGJRVDV").as_deref(), Some("التقرير"));
        assert_eq!(swap_layout("LGT 2").as_deref(), Some("ملف 2"));
    }

    #[test]
    fn arabic_keys_to_english() {
        assert_eq!(swap_layout("شسيب").as_deref(), Some("asdf"));
        assert_eq!(swap_layout("قثحخقف").as_deref(), Some("report"));
        assert_eq!(swap_layout("هىرخهؤث").as_deref(), Some("invoice"));
        assert_eq!(swap_layout("ذذ").as_deref(), Some("``"));
        assert_eq!(swap_layout("ئءؤ").as_deref(), Some("zxc"));
        assert_eq!(swap_layout("وزظ").as_deref(), Some(",./"));
        assert_eq!(swap_layout("جدكط").as_deref(), Some("[];'"));
        // The user's example: these keys spell "ayrere", which is not a word; swap_layout
        // maps keys and leaves judging the result to the caller.
        assert_eq!(swap_layout("شغقثقث").as_deref(), Some("ayrere"));
        // Hamza forms and harakat come from Shift.
        assert_eq!(swap_layout("أ").as_deref(), Some("H"));
        assert_eq!(swap_layout("مَ").as_deref(), Some("lQ"));
        assert_eq!(swap_layout("م؟،؛").as_deref(), Some("l?KP"));
    }

    #[test]
    fn lam_alef_prefers_the_b_key_and_lists_gh() {
        assert_eq!(swap_layout("لمخلاشم").as_deref(), Some("global"));
        assert_eq!(swap_layout("مهلاف").as_deref(), Some("libt"));
        assert_eq!(
            swap_layout_variants("مهلاف", 4),
            ["libt".to_string(), "light".to_string()]
        );
        assert_eq!(
            swap_layout_variants("لالا", 8),
            ["bb", "ghb", "bgh", "ghgh"].map(String::from)
        );
        assert_eq!(swap_layout_variants("لالا", 2).len(), 2);
        assert_eq!(swap_layout_variants("لأ", 4), ["G", "gH"].map(String::from));
        // The presentation-form ligature counts as lam + alef.
        assert_eq!(swap_layout("\u{FEFB}").as_deref(), Some("b"));
    }

    #[test]
    fn nothing_to_swap() {
        assert_eq!(swap_layout(""), None);
        assert_eq!(swap_layout("   "), None);
        assert_eq!(swap_layout("2026"), None);
        assert_eq!(swap_layout("تقرير report"), None); // mixed scripts
        assert!(swap_layout_variants("تقرير report", 4).is_empty());
        assert!(swap_layout_variants("abc", 0).is_empty());
    }

    #[test]
    fn round_trips_for_lowercase_keys() {
        // Every unshifted letter key survives English → Arabic → English (reversed so that
        // `g` is not followed by `h`, which would read back as the lam-alef key `b`).
        let keys: String = KEYS
            .iter()
            .map(|(k, _)| *k)
            .filter(char::is_ascii_lowercase)
            .rev()
            .collect();
        assert_eq!(keys.len(), 26);
        let arabic = swap_layout(&keys).unwrap();
        assert_eq!(swap_layout(&arabic).as_deref(), Some(keys.as_str()));
    }

    #[test]
    fn table_has_no_duplicate_keys_or_single_char_outputs() {
        for (i, (k, a)) in KEYS.iter().enumerate() {
            assert!(
                KEYS[i + 1..].iter().all(|(k2, _)| k2 != k),
                "key {k:?} listed twice"
            );
            let single = a.chars().count() == 1;
            if single {
                let c = a.chars().next().unwrap();
                assert!(
                    KEYS[i + 1..].iter().all(|(_, a2)| *a2 != *a),
                    "output {c:?} listed twice"
                );
            }
        }
    }
}
