//! Word lists: filter names, `type:` kinds and named dates, in English and Arabic, plus the
//! normalization used to match them tolerantly.

use crate::ast::{FileKind, FilterName, NamedDate};

/// Filter names and aliases. Matching is case-insensitive and Arabic-normalized
/// (see [`normalize_word`]), so `إمتداد:`, `امتداد:` and `أمتداد:` are all `ext:`.
pub(crate) const FILTER_NAMES: &[(&str, FilterName)] = &[
    ("ext", FilterName::Ext),
    ("extension", FilterName::Ext),
    ("type", FilterName::Type),
    ("size", FilterName::Size),
    ("dm", FilterName::DateModified),
    ("datemodified", FilterName::DateModified),
    ("dc", FilterName::DateCreated),
    ("datecreated", FilterName::DateCreated),
    ("da", FilterName::DateAccessed),
    ("dateaccessed", FilterName::DateAccessed),
    ("path", FilterName::Path),
    ("parent", FilterName::Parent),
    ("infolder", FilterName::InFolder),
    ("content", FilterName::Content),
    ("sem", FilterName::Semantic),
    ("semantic", FilterName::Semantic),
    ("regex", FilterName::Regex),
    ("case", FilterName::Case),
    ("ww", FilterName::WholeWord),
    ("wholeword", FilterName::WholeWord),
    ("fuzzy", FilterName::Fuzzy),
    ("dupe", FilterName::Dupe),
    ("similar", FilterName::Similar),
    ("tag", FilterName::Tag),
    // Arabic synonyms (task 3.6).
    ("امتداد", FilterName::Ext),
    ("نوع", FilterName::Type),
    ("حجم", FilterName::Size),
    ("تاريخ", FilterName::DateModified),
    ("تاريخ_التعديل", FilterName::DateModified),
    ("تاريخ_الإنشاء", FilterName::DateCreated),
    ("تاريخ_الوصول", FilterName::DateAccessed),
    ("مسار", FilterName::Path),
    ("مجلد", FilterName::InFolder),
    ("محتوى", FilterName::Content),
    ("دلالي", FilterName::Semantic),
    ("مكرر", FilterName::Dupe),
    ("مشابه", FilterName::Similar),
    ("وسم", FilterName::Tag),
];

const KINDS: &[(&str, FileKind)] = &[
    ("doc", FileKind::Document),
    ("docs", FileKind::Document),
    ("document", FileKind::Document),
    ("documents", FileKind::Document),
    ("image", FileKind::Image),
    ("images", FileKind::Image),
    ("img", FileKind::Image),
    ("picture", FileKind::Image),
    ("pictures", FileKind::Image),
    ("photo", FileKind::Image),
    ("photos", FileKind::Image),
    ("video", FileKind::Video),
    ("videos", FileKind::Video),
    ("audio", FileKind::Audio),
    ("music", FileKind::Audio),
    ("code", FileKind::Code),
    ("source", FileKind::Code),
    ("archive", FileKind::Archive),
    ("archives", FileKind::Archive),
    ("folder", FileKind::Folder),
    ("folders", FileKind::Folder),
    ("dir", FileKind::Folder),
    ("مستند", FileKind::Document),
    ("مستندات", FileKind::Document),
    ("وثيقة", FileKind::Document),
    ("وثائق", FileKind::Document),
    ("صورة", FileKind::Image),
    ("صور", FileKind::Image),
    ("فيديو", FileKind::Video),
    ("فيديوهات", FileKind::Video),
    ("صوت", FileKind::Audio),
    ("صوتيات", FileKind::Audio),
    ("كود", FileKind::Code),
    ("برمجة", FileKind::Code),
    ("أرشيف", FileKind::Archive),
    ("أرشيفات", FileKind::Archive),
    ("مضغوط", FileKind::Archive),
    ("مجلد", FileKind::Folder),
    ("مجلدات", FileKind::Folder),
];

/// Underscores are removed before matching, so `this_week` = `thisweek` and
/// `هذا_الأسبوع` = `هذاالأسبوع`.
const NAMED_DATES: &[(&str, NamedDate)] = &[
    ("today", NamedDate::Today),
    ("yesterday", NamedDate::Yesterday),
    ("thisweek", NamedDate::ThisWeek),
    ("lastweek", NamedDate::LastWeek),
    ("thismonth", NamedDate::ThisMonth),
    ("lastmonth", NamedDate::LastMonth),
    ("thisyear", NamedDate::ThisYear),
    ("lastyear", NamedDate::LastYear),
    ("اليوم", NamedDate::Today),
    ("أمس", NamedDate::Yesterday),
    ("البارحة", NamedDate::Yesterday),
    ("هذا_الأسبوع", NamedDate::ThisWeek),
    ("الأسبوع_الماضي", NamedDate::LastWeek),
    ("هذا_الشهر", NamedDate::ThisMonth),
    ("الشهر_الماضي", NamedDate::LastMonth),
    ("هذا_العام", NamedDate::ThisYear),
    ("هذه_السنة", NamedDate::ThisYear),
    ("العام_الماضي", NamedDate::LastYear),
    ("السنة_الماضية", NamedDate::LastYear),
];

/// Lower-cases and folds Arabic spelling variants: hamza forms of alef (أ إ آ ٱ → ا),
/// alef maqsura (ى → ي), and drops tatweel and short-vowel marks (harakat).
pub(crate) fn normalize_word(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'أ' | 'إ' | 'آ' | 'ٱ' => out.push('ا'),
            'ى' => out.push('ي'),
            'ـ' | '\u{064B}'..='\u{0652}' | '\u{0670}' => {}
            c if c.is_ascii() => out.push(c.to_ascii_lowercase()),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

fn lookup<T: Copy>(table: &[(&str, T)], word: &str, strip_underscores: bool) -> Option<T> {
    let mut key = normalize_word(word);
    if strip_underscores {
        key.retain(|c| c != '_');
    }
    table.iter().find_map(|(name, v)| {
        let mut n = normalize_word(name);
        if strip_underscores {
            n.retain(|c| c != '_');
        }
        (n == key).then_some(*v)
    })
}

pub(crate) fn filter_name(word: &str) -> Option<FilterName> {
    lookup(FILTER_NAMES, word, false)
}

pub(crate) fn kind(word: &str) -> Option<FileKind> {
    lookup(KINDS, word, false)
}

pub(crate) fn named_date(word: &str) -> Option<NamedDate> {
    lookup(NAMED_DATES, word, true)
}

/// The closest filter name to `word`, if it is close enough to be a plausible typo.
pub(crate) fn suggest_filter(word: &str) -> Option<&'static str> {
    suggest(FILTER_NAMES, word)
}

/// The closest `type:` value to `word`, if close enough.
pub(crate) fn suggest_kind(word: &str) -> Option<&'static str> {
    suggest(KINDS, word)
}

fn suggest<T>(table: &[(&'static str, T)], word: &str) -> Option<&'static str> {
    let key: Vec<char> = normalize_word(word).chars().collect();
    if key.is_empty() || key.len() > 64 {
        return None;
    }
    let threshold = (key.len() / 3).max(2);
    table
        .iter()
        .map(|(name, _)| {
            let n: Vec<char> = normalize_word(name).chars().collect();
            (edit_distance(&key, &n), *name)
        })
        .filter(|(d, _)| *d <= threshold)
        .min_by_key(|(d, _)| *d)
        .map(|(_, name)| name)
}

/// Levenshtein distance over characters.
pub(crate) fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Converts Arabic-Indic (٠-٩) and Extended Arabic-Indic (۰-۹) digits to ASCII, and the
/// Arabic decimal separator `٫` to `.`.
pub(crate) fn ascii_digits(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{0660}'..='\u{0669}' => char::from(b'0' + (c as u32 - 0x0660) as u8),
            '\u{06F0}'..='\u{06F9}' => char::from(b'0' + (c as u32 - 0x06F0) as u8),
            '٫' => '.',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        edit_distance(&a, &b)
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(d("", ""), 0);
        assert_eq!(d("size", "size"), 0);
        assert_eq!(d("szie", "size"), 2);
        assert_eq!(d("ex", "ext"), 1);
        assert_eq!(d("", "abc"), 3);
    }

    #[test]
    fn hamza_variants_match() {
        assert_eq!(filter_name("إمتداد"), Some(FilterName::Ext));
        assert_eq!(filter_name("أمتداد"), Some(FilterName::Ext));
        assert_eq!(filter_name("محتوي"), Some(FilterName::Content));
        assert_eq!(filter_name("EXT"), Some(FilterName::Ext));
        assert_eq!(kind("ارشيف"), Some(FileKind::Archive));
        assert_eq!(named_date("امس"), Some(NamedDate::Yesterday));
        assert_eq!(named_date("هذا_الاسبوع"), Some(NamedDate::ThisWeek));
        assert_eq!(named_date("this_week"), Some(NamedDate::ThisWeek));
    }

    #[test]
    fn digits_are_converted() {
        assert_eq!(ascii_digits("١٠٠mb"), "100mb");
        assert_eq!(ascii_digits("۲۰۲۶"), "2026");
        assert_eq!(ascii_digits("١٫٥"), "1.5");
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggest_filter("szie"), Some("size"));
        assert_eq!(suggest_filter("contnet"), Some("content"));
        assert_eq!(suggest_filter("حجن"), Some("حجم"));
        assert_eq!(suggest_filter("zzzzzzzzzz"), None);
        assert_eq!(suggest_kind("imgae"), Some("image"));
    }
}
