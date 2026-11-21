//! Parsing of structured filter values: sizes, dates and comparisons.
//!
//! These functions take the value text (quotes already removed) and return a message on
//! failure; the parser attaches the span.

use crate::ast::{CalendarDate, Comparison, DateSpec};
use crate::error::ParseErrorKind;
use crate::lexicon;

pub(crate) type ValueResult<T> = Result<T, (ParseErrorKind, String)>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// Splits a leading comparison operator (`>=`, `<=`, `>`, `<`, `=`) from `s`.
fn split_operator(s: &str) -> (Option<&'static str>, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = s.strip_prefix(op) {
            return (Some(op), rest);
        }
    }
    (None, s)
}

fn compare<T>(op: Option<&str>, v: T) -> Comparison<T> {
    match op {
        Some(">=") => Comparison::Ge(v),
        Some("<=") => Comparison::Le(v),
        Some(">") => Comparison::Gt(v),
        Some("<") => Comparison::Lt(v),
        _ => Comparison::Eq(v),
    }
}

/// Parses a generic comparison: `v`, `>v`, `>=v`, `<v`, `<=v`, `=v`, `a..b`, `a..`, `..b`.
/// `extra_range_sep` is an additional range separator (sizes accept Everything's `a-b`).
fn parse_comparison<T>(
    s: &str,
    extra_range_sep: Option<char>,
    leaf: impl Fn(&str) -> ValueResult<T>,
    invalid: ParseErrorKind,
    what: &str,
) -> ValueResult<Comparison<T>> {
    let s = s.trim();
    let (op, rest) = split_operator(s);
    if op.is_some() {
        if rest.trim().is_empty() {
            return Err((
                invalid,
                format!("expected a {what} after `{}`", op.unwrap_or("")),
            ));
        }
        return Ok(compare(op, leaf(rest.trim())?));
    }
    let range = s.split_once("..").or_else(|| {
        let sep = extra_range_sep?;
        // Skip the first character so a leading `-` is never taken as a separator.
        let (i, ch) = s.char_indices().skip(1).find(|&(_, ch)| ch == sep)?;
        Some((&s[..i], &s[i + ch.len_utf8()..]))
    });
    match range {
        Some((a, b)) => {
            let (a, b) = (a.trim(), b.trim());
            match (a.is_empty(), b.is_empty()) {
                (true, true) => Err((invalid, format!("a range needs at least one {what}"))),
                (false, true) => Ok(Comparison::Ge(leaf(a)?)),
                (true, false) => Ok(Comparison::Le(leaf(b)?)),
                (false, false) => Ok(Comparison::Range {
                    min: leaf(a)?,
                    max: leaf(b)?,
                }),
            }
        }
        None => Ok(Comparison::Eq(leaf(s)?)),
    }
}

/// Parses a `size:` value into a byte comparison.
///
/// Units are **1024-based** (Windows Explorer and Everything convention): `kb` = 1024 bytes,
/// `mb` = 1024², `gb` = 1024³, `tb` = 1024⁴. No unit means bytes. Decimals (`1.5gb`) and
/// Arabic digits (`١٠٠mb`) are accepted. Named sizes follow Everything: `empty`, `tiny`
/// (≤10 KB), `small` (10-100 KB), `medium` (100 KB-1 MB), `large` (1-16 MB),
/// `huge` (16-128 MB), `gigantic` (>128 MB).
pub(crate) fn parse_size(s: &str) -> ValueResult<Comparison<u64>> {
    let s = lexicon::ascii_digits(s).to_lowercase();
    if let Some(named) = named_size(s.trim()) {
        return Ok(named);
    }
    let cmp = parse_comparison(
        &s,
        Some('-'),
        size_literal,
        ParseErrorKind::InvalidSize,
        "size",
    )?;
    if let Comparison::Range { min, max } = cmp {
        if min > max {
            return Err((
                ParseErrorKind::InvertedRange,
                "the first size in a range must not be larger than the second".into(),
            ));
        }
    }
    Ok(cmp)
}

fn named_size(s: &str) -> Option<Comparison<u64>> {
    let range = |min, max| Comparison::Range { min, max };
    Some(match s {
        "empty" | "فارغ" => Comparison::Eq(0),
        "tiny" => range(0, 10 * KIB),
        "small" => range(10 * KIB + 1, 100 * KIB),
        "medium" => range(100 * KIB + 1, MIB),
        "large" => range(MIB + 1, 16 * MIB),
        "huge" => range(16 * MIB + 1, 128 * MIB),
        "gigantic" => Comparison::Gt(128 * MIB),
        _ => return None,
    })
}

fn unit_multiplier(unit: &str) -> Option<u64> {
    Some(match unit {
        "" | "b" | "byte" | "bytes" | "بايت" => 1,
        "k" | "kb" | "kib" | "كيلوبايت" | "كيلو" | "ك.ب" => KIB,
        "m" | "mb" | "mib" | "ميجابايت" | "ميغابايت" | "ميجا" | "ميغا" | "م.ب" => {
            MIB
        }
        "g" | "gb" | "gib" | "جيجابايت" | "غيغابايت" | "جيجا" | "غيغا" | "ج.ب" => {
            GIB
        }
        "t" | "tb" | "tib" | "تيرابايت" | "تيرا" => TIB,
        _ => return None,
    })
}

fn size_literal(s: &str) -> ValueResult<u64> {
    let bad = |msg: String| (ParseErrorKind::InvalidSize, msg);
    let num_end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map_or(s.len(), |(i, _)| i);
    let (num, unit) = s.split_at(num_end);
    let unit = unit.trim();
    let Some(mult) = unit_multiplier(unit) else {
        return Err(bad(format!(
            "unknown size unit `{unit}`; use b, kb, mb, gb or tb (1 kb = 1024 bytes)"
        )));
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if (int.is_empty() && frac.is_empty()) || frac.contains('.') {
        return Err(bad(format!(
            "`{s}` is not a size; try something like `100mb`"
        )));
    }
    let overflow = || bad(format!("`{s}` is too large"));
    let mut total: u128 = 0;
    for b in int.bytes() {
        total = total
            .checked_mul(10)
            .and_then(|t| t.checked_add(u128::from(b - b'0')))
            .ok_or_else(overflow)?;
        if total > u128::from(u64::MAX) {
            return Err(overflow());
        }
    }
    total = total.checked_mul(u128::from(mult)).ok_or_else(overflow)?;
    // Fractional part: only the first 12 digits matter for byte precision up to TiB.
    let frac = &frac[..frac.len().min(12)];
    if !frac.is_empty() {
        let mut f: u128 = 0;
        for b in frac.bytes() {
            f = f * 10 + u128::from(b - b'0');
        }
        let scale = 10u128.pow(frac.len() as u32);
        total = total
            .checked_add(f * u128::from(mult) / scale)
            .ok_or_else(overflow)?;
    }
    u64::try_from(total).map_err(|_| overflow())
}

/// Parses a `dm:`/`dc:`/`da:` value. Accepts named periods (English or Arabic), ISO-style
/// calendar dates (`2026`, `2026-03`, `2026-03-15`, `/` also accepted as separator), Arabic
/// digits, comparisons and `..` ranges. Nothing is resolved against the clock here.
pub(crate) fn parse_date(s: &str) -> ValueResult<Comparison<DateSpec>> {
    let s = lexicon::ascii_digits(s);
    let cmp = parse_comparison(&s, None, date_literal, ParseErrorKind::InvalidDate, "date")?;
    if let Comparison::Range {
        min: DateSpec::Calendar(a),
        max: DateSpec::Calendar(b),
    } = cmp
    {
        let start = |d: CalendarDate| (d.year, d.month.unwrap_or(1), d.day.unwrap_or(1));
        if start(a) > start(b) {
            return Err((
                ParseErrorKind::InvertedRange,
                "the first date in a range must not be after the second".into(),
            ));
        }
    }
    Ok(cmp)
}

fn date_literal(s: &str) -> ValueResult<DateSpec> {
    if let Some(named) = lexicon::named_date(s) {
        return Ok(DateSpec::Named(named));
    }
    let bad = || {
        (
            ParseErrorKind::InvalidDate,
            format!(
                "`{s}` is not a date; use today, yesterday, thisweek, lastmonth, \
                 a year (2026), a month (2026-03) or a day (2026-03-15)"
            ),
        )
    };
    let mut parts = s.split(['-', '/']);
    let mut next_num = |max_len: usize| -> ValueResult<Option<u16>> {
        match parts.next() {
            None => Ok(None),
            Some(p)
                if !p.is_empty() && p.len() <= max_len && p.bytes().all(|b| b.is_ascii_digit()) =>
            {
                p.parse::<u16>().map(Some).map_err(|_| bad())
            }
            Some(_) => Err(bad()),
        }
    };
    let year = next_num(4)?.ok_or_else(bad)?;
    let month = next_num(2)?;
    let day = next_num(2)?;
    if parts.next().is_some() || s.split(['-', '/']).next().map(str::len) != Some(4) {
        return Err(bad());
    }
    if year == 0 {
        return Err((
            ParseErrorKind::InvalidDate,
            "year must be between 1 and 9999".into(),
        ));
    }
    let month = match month {
        Some(m @ 1..=12) => Some(m as u8),
        Some(_) => {
            return Err((
                ParseErrorKind::InvalidDate,
                "month must be between 1 and 12".into(),
            ));
        }
        None => None,
    };
    let day = match (month, day) {
        (Some(m), Some(d)) => {
            if d == 0 || d > u16::from(days_in_month(year, m)) {
                return Err((
                    ParseErrorKind::InvalidDate,
                    format!("day {d} does not exist in {year}-{m:02}"),
                ));
            }
            Some(d as u8)
        }
        _ => None,
    };
    Ok(DateSpec::Calendar(CalendarDate { year, month, day }))
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NamedDate;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100mb"), Ok(Comparison::Eq(100 * MIB)));
        assert_eq!(parse_size(">1gb"), Ok(Comparison::Gt(GIB)));
        assert_eq!(parse_size(">=1.5KB"), Ok(Comparison::Ge(1536)));
        assert_eq!(parse_size("<10"), Ok(Comparison::Lt(10)));
        assert_eq!(
            parse_size("10mb..50mb"),
            Ok(Comparison::Range {
                min: 10 * MIB,
                max: 50 * MIB
            })
        );
        assert_eq!(
            parse_size("1mb-2mb"),
            Ok(Comparison::Range {
                min: MIB,
                max: 2 * MIB
            })
        );
        assert_eq!(parse_size("10mb.."), Ok(Comparison::Ge(10 * MIB)));
        assert_eq!(parse_size("..10mb"), Ok(Comparison::Le(10 * MIB)));
        assert_eq!(parse_size("١٠٠ميجا"), Ok(Comparison::Eq(100 * MIB)));
        assert_eq!(parse_size("empty"), Ok(Comparison::Eq(0)));
        assert!(parse_size("50mb..10mb").is_err());
        assert!(parse_size("10xb").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size(">").is_err());
        assert!(parse_size("..").is_err());
        assert!(parse_size("1.2.3mb").is_err());
        assert!(parse_size("99999999999999999999999tb").is_err());
        assert!(parse_size("16777216tb").is_err());
    }

    #[test]
    fn dates() {
        let cal = |year, month, day| DateSpec::Calendar(CalendarDate { year, month, day });
        assert_eq!(
            parse_date("today"),
            Ok(Comparison::Eq(DateSpec::Named(NamedDate::Today)))
        );
        assert_eq!(
            parse_date("2026"),
            Ok(Comparison::Eq(cal(2026, None, None)))
        );
        assert_eq!(
            parse_date("٢٠٢٦-٠٣"),
            Ok(Comparison::Eq(cal(2026, Some(3), None)))
        );
        assert_eq!(
            parse_date(">=2024-02-29"),
            Ok(Comparison::Ge(cal(2024, Some(2), Some(29))))
        );
        assert_eq!(
            parse_date("2026-01..2026-03"),
            Ok(Comparison::Range {
                min: cal(2026, Some(1), None),
                max: cal(2026, Some(3), None)
            })
        );
        assert!(parse_date("2025-02-29").is_err());
        assert!(parse_date("2026-13").is_err());
        assert!(parse_date("26-01").is_err());
        assert!(parse_date("2026-03..2026-01").is_err());
        assert!(parse_date("2026-01-01-01").is_err());
        assert!(parse_date("tomorrow").is_err());
        assert!(parse_date("0000").is_err());
    }
}
