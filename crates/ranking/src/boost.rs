//! Ranking boosts applied on top of the fused score.

use crate::frecency::decay;

/// Multiplicative boosts. The final score of a result is
///
/// `rrf × (1 + exact_name·[name is exact] + recency·0.5^(age / recency_half_life)
///            + frecency·f / (f + frecency_saturation))`
///
/// where `f` is the file's frecency (see [`crate::FrecencyStore`]). Each term is bounded
/// (`[0, weight]`), so a boost can lift a result past neighbours with a similar fused score
/// but cannot make a weak match outrank a much stronger one, and a result with a fused score
/// of 0 stays at 0. All weights 0 ([`Boosts::none`]) keeps the pure RRF order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Boosts {
    /// Added when the file name (with or without its extension), normalized, equals the
    /// query's plain words joined by spaces. Default 1.0 (doubles the score).
    pub exact_name: f64,
    /// Weight of the recency term. Default 0.1.
    pub recency: f64,
    /// Age at which the recency term halves, in seconds. Default 30 days.
    pub recency_half_life_secs: f64,
    /// Weight of the frecency term. Default 0.5.
    pub frecency: f64,
    /// Frecency at which the frecency term reaches half its weight. Default 3.0 (about three
    /// recent opens).
    pub frecency_saturation: f64,
}

impl Default for Boosts {
    fn default() -> Self {
        Boosts {
            exact_name: 1.0,
            recency: 0.1,
            recency_half_life_secs: 30.0 * 86_400.0,
            frecency: 0.5,
            frecency_saturation: 3.0,
        }
    }
}

/// What the boosts look at for one result.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoostInput {
    /// The file name equals the query words.
    pub exact_name: bool,
    /// Modification time, Unix seconds.
    pub mtime: Option<i64>,
    /// The current time, Unix seconds.
    pub now: i64,
    /// The file's frecency at `now`.
    pub frecency: f64,
}

fn weight(w: f64) -> f64 {
    if w.is_finite() && w > 0.0 { w } else { 0.0 }
}

impl Boosts {
    /// No boosts: the fused order is kept.
    pub const fn none() -> Self {
        Boosts {
            exact_name: 0.0,
            recency: 0.0,
            recency_half_life_secs: 30.0 * 86_400.0,
            frecency: 0.0,
            frecency_saturation: 3.0,
        }
    }

    /// The multiplier `≥ 1` for one result (see the type docs).
    pub fn factor(&self, input: &BoostInput) -> f64 {
        let mut f = 1.0;
        if input.exact_name {
            f += weight(self.exact_name);
        }
        if let Some(mtime) = input.mtime {
            f += weight(self.recency) * decay(input.now - mtime, self.recency_half_life_secs);
        }
        let fr = input.frecency;
        if fr.is_finite() && fr > 0.0 {
            let sat = if self.frecency_saturation.is_finite() && self.frecency_saturation > 0.0 {
                self.frecency_saturation
            } else {
                1.0
            };
            f += weight(self.frecency) * fr / (fr + sat);
        }
        f
    }
}

/// Tests whether a file name is exactly the query's words (Arabic-normalized, so hamza,
/// harakat, tatweel and case are ignored, and `_`/`-`/`.` count as spaces like in the name
/// index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactName {
    target: Vec<String>,
}

impl ExactName {
    /// From the query's plain words (see `SearchPlan::name_words`). `None` when there are none.
    pub fn new<S: AsRef<str>>(words: &[S]) -> Option<Self> {
        let joined = words
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(" ");
        let norm = athrix_text_ar::normalize_name(&joined);
        let target: Vec<String> = split_words(&norm).map(str::to_string).collect();
        (!target.is_empty()).then_some(ExactName { target })
    }

    /// True when the file name of `path` (last component), with or without its last
    /// extension, normalizes to the query words.
    pub fn matches(&self, path: &str) -> bool {
        let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let mut buf = String::with_capacity(name.len());
        athrix_text_ar::normalize_name_into(name, &mut buf);
        let words: Vec<&str> = split_words(&buf).collect();
        if words.iter().eq(self.target.iter()) {
            return true;
        }
        match name.rfind('.') {
            // A plain extension is exactly the last normalized word: drop it.
            Some(i) if i > 0 && name[i + 1..].chars().all(char::is_alphanumeric) => {
                !name[i + 1..].is_empty()
                    && words.len() == self.target.len() + 1
                    && words[..self.target.len()].iter().eq(self.target.iter())
            }
            Some(i) if i > 0 => {
                buf.clear();
                athrix_text_ar::normalize_name_into(&name[..i], &mut buf);
                split_words(&buf).eq(self.target.iter().map(String::as_str))
            }
            _ => false,
        }
    }
}

fn split_words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_whitespace() || matches!(c, '_' | '-' | '.'))
        .filter(|w| !w.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_name() {
        let e = ExactName::new(&["تقرير", "الميزانية"]).unwrap();
        assert!(e.matches(r"C:\Docs\تقرير الميزانية.pdf"));
        assert!(e.matches("/home/u/تقرير_الميزانيّة.docx"));
        assert!(e.matches("تقرير الميزانية"));
        assert!(!e.matches(r"C:\Docs\تقرير الميزانية 2026.pdf"));
        let e = ExactName::new(&["Report"]).unwrap();
        assert!(e.matches(r"D:\x\report.PDF"));
        assert!(e.matches(r"D:\x\REPORT"));
        assert!(!e.matches(r"D:\report\other.txt"));
        let e = ExactName::new(&["أحمد"]).unwrap();
        assert!(e.matches("احمد.txt"));
        assert_eq!(ExactName::new::<&str>(&[]), None);
        assert_eq!(ExactName::new(&["  "]), None);
    }

    #[test]
    fn factor_terms_are_bounded() {
        let b = Boosts::default();
        let base = BoostInput {
            now: 1_000_000_000,
            ..Default::default()
        };
        assert_eq!(b.factor(&base), 1.0);
        assert_eq!(
            b.factor(&BoostInput {
                exact_name: true,
                ..base
            }),
            2.0
        );
        let fresh = b.factor(&BoostInput {
            mtime: Some(base.now),
            ..base
        });
        assert!((fresh - 1.1).abs() < 1e-12);
        let month = b.factor(&BoostInput {
            mtime: Some(base.now - 30 * 86_400),
            ..base
        });
        assert!((month - 1.05).abs() < 1e-12);
        let hot = b.factor(&BoostInput {
            frecency: 1e9,
            ..base
        });
        assert!(hot < 1.5 && hot > 1.49);
        assert_eq!(
            Boosts::none().factor(&BoostInput {
                exact_name: true,
                mtime: Some(base.now),
                frecency: 5.0,
                ..base
            }),
            1.0
        );
    }
}
