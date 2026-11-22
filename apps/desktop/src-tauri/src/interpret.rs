//! `interpret_query`: reads plain Arabic or English in the search field as filters (task 6.8)
//! with `athrix_query::interpret_with`, for the one-line suggestion under the field.

use athrix_query::{CalendarDate, InterpretOptions, interpret_with};
use serde::Serialize;

use crate::query::utf16_offset;

/// A suggested query (see `athrix_query::Interpretation`), with ranges in UTF-16 units.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Interpretation {
    /// The suggested query text.
    pub query: String,
    /// The pieces of the input, in order.
    pub parts: Vec<InterpretPart>,
    /// A rough 0-1 score (a hint, not a probability).
    pub confidence: f32,
}

/// One piece of the input and what it became.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterpretPart {
    /// Start of the piece in the input, in UTF-16 units.
    pub start: usize,
    /// End of the piece (exclusive), in UTF-16 units.
    pub end: usize,
    /// `kind`, `date`, `size`, `content`, `location`, `term` or `ignored`.
    pub role: &'static str,
    /// What the piece contributes (`ext:pdf`, a kept word), empty for `ignored`.
    pub output: String,
}

/// Interprets `text`; `null` when there is nothing to suggest (plain names, or the text
/// already uses filters). `today` is the user's local date as `YYYY-MM-DD` (needed for «قبل
/// 3 أيام», "last 7 days"; without it those phrases stay words). Cheap: no index access.
#[tauri::command]
pub fn interpret_query(text: String, today: Option<String>) -> Option<Interpretation> {
    let options = InterpretOptions {
        today: today.as_deref().and_then(parse_day),
        // The name index has no creation / access times: keep every date `dm:`.
        ..Default::default()
    };
    let found = interpret_with(&text, &options)?;
    Some(Interpretation {
        query: found.query,
        parts: found
            .parts
            .into_iter()
            .map(|p| InterpretPart {
                start: utf16_offset(&text, p.span.start),
                end: utf16_offset(&text, p.span.end),
                role: p.role.as_str(),
                output: p.output,
            })
            .collect(),
        confidence: found.confidence,
    })
}

/// `YYYY-MM-DD` → a full calendar date (range-checked by `interpret_with`).
fn parse_day(s: &str) -> Option<CalendarDate> {
    let mut it = s.trim().splitn(3, '-');
    let year = it.next()?.parse().ok()?;
    let month = it.next()?.parse().ok()?;
    let day = it.next()?.parse().ok()?;
    Some(CalendarDate {
        year,
        month: Some(month),
        day: Some(day),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interprets_with_utf16_ranges() {
        let text = "😀 ملفات PDF من الأسبوع الماضي";
        let i = interpret_query(text.into(), Some("2026-11-22".into())).expect("suggestion");
        assert_eq!(i.query, "😀 ext:pdf dm:lastweek");
        let pdf = i
            .parts
            .iter()
            .find(|p| p.output == "ext:pdf")
            .expect("pdf part");
        let utf16: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(String::from_utf16_lossy(&utf16[pdf.start..pdf.end]), "PDF");
    }

    #[test]
    fn plain_names_and_bad_dates() {
        assert!(interpret_query("تقرير المبيعات".into(), None).is_none());
        assert!(parse_day("2026-13").is_none());
        let relative = interpret_query("ملفات قبل 3 أيام".into(), Some("2026-11-22".into()));
        assert_eq!(relative.map(|i| i.query).as_deref(), Some("dm:2026-11-19"));
    }

    #[test]
    fn date_kinds_and_size_words() {
        let q = |s: &str| interpret_query(s.into(), Some("2026-11-22".into())).map(|i| i.query);
        assert_eq!(q("ملفات أُنشئت أمس").as_deref(), Some("dm:yesterday"));
        assert_eq!(q("files opened last week").as_deref(), Some("dm:lastweek"));
        assert_eq!(q("الملفات الضخمة").as_deref(), Some("size:>1gb"));
    }
}
