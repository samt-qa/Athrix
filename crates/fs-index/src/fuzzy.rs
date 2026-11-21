//! Fuzzy and acronym matching of one term against a normalized name (`fuzzy:`).
//!
//! A fuzzy term is normalized like every term, then tried against the normalized name in
//! tiers, best first. The first tier that matches decides the score:
//!
//! | tier | match | example (`term` → name) |
//! |---|---|---|
//! | 7 | the whole name, or the name without its extension | `report` → `Report.pdf` |
//! | 6 | prefix | `rep` → `report 2026.pdf` |
//! | 5 | substring at a word start | `2026` → `report_2026.pdf` |
//! | 4 | substring elsewhere | `port` → `report.pdf` |
//! | 3 | acronym: consecutive word initials | `mrs` → `Monthly Report Summary.pdf`, `mtr` → `MyTravelReport`, `تمس` → `تقرير المبيعات السنوي` |
//! | 2 | one typo (substitution, insertion, deletion or swap of neighbours) | `reprot` → `report.pdf` |
//! | 1 | subsequence (terms of 3+ characters) with few, short gaps | `rprt` → `report.pdf` |
//! | 0 | two typos (terms of 9+ characters) | `managmnet` → `management.pdf` |
//!
//! Words are separated by anything the normalization turns into a space (`_ - . ( )`, spaces…)
//! and, for acronyms, by camelCase and letter/digit changes in the original name
//! (`MyTravelReport`, `XMLParser`, `IMG2026`). An Arabic word starting with the article `ال`
//! offers both `ا` and the letter after the article as its initial (`المبيعات` → `ا` or `م`).
//!
//! Typos are allowed only for longer terms (none up to 4 characters, one up to 8, two from 9),
//! so short terms do not match half the disk. The subsequence tier needs a score of at least
//! three quarters of a gap-free match: gaps cost 3 plus 2 per extra skipped character, matches
//! at word starts and consecutive matches earn bonuses (fzf-style, simplified).
//!
//! The score of an entry is `tier << 24 | detail << 8 | 255 - min(name length, 255)`: higher
//! is better; within a tier, the subsequence/acronym detail and then shorter names win.
//!
//! # Why fuzzy (and acronyms) are opt-in
//!
//! Acronym and fuzzy matching need `fuzzy:word`, or the `fuzzy:` switch for the whole query;
//! plain words stay exact substrings. (`~` is not used: the query language reserves it for
//! semantic search, `~عقد الإيجار`.) Reasons:
//!
//! - Plain search must stay predictable and Everything-compatible: `ms` should not also return
//!   every `Monthly Summary`, and the result count shown while typing should mean "names
//!   containing this".
//! - Plain search stops at `limit` and costs ~1 ms per million names; a fuzzy query ranks, so
//!   it scans every name and costs several times more (see the `index` bench).
//! - The natural automatic fallback, "no plain results → retry fuzzy", belongs to the caller,
//!   which knows the result count: it can re-run the text with `fuzzy: ` in front (and also
//!   try [`athrix_text_ar::swap_layout`]) and show "did you mean…".
//!
//! Typo distance is computed with Hyyrö's bit-parallel optimal-string-alignment algorithm
//! (Myers' algorithm extended with transpositions), O(name length) per name for terms up to 64
//! characters. Longer terms only use the substring tiers.

use std::ops::Range;

use memchr::memmem;

/// Longest term (in characters) that gets acronym, typo and subsequence matching.
const MAX_FUZZY_CHARS: usize = 64;

const MATCH: i32 = 16;
const BOUNDARY: i32 = 8;
const FIRST_BOUNDARY: i32 = 8;
const CONSECUTIVE: i32 = 8;
const GAP_OPEN: i32 = 3;
const GAP_EXTEND: i32 = 2;
const NEG: i32 = i32::MIN / 4;

const TIER_EXACT: u32 = 7;
const TIER_PREFIX: u32 = 6;
const TIER_WORD_SUBSTRING: u32 = 5;
const TIER_SUBSTRING: u32 = 4;
const TIER_ACRONYM: u32 = 3;
const TIER_ONE_TYPO: u32 = 2;
const TIER_SUBSEQUENCE: u32 = 1;
const TIER_TWO_TYPOS: u32 = 0;

/// A compiled fuzzy term.
#[derive(Debug, Clone)]
pub(crate) struct FuzzyMatcher {
    /// The normalized term (spaces kept).
    needle: Box<str>,
    finder: memmem::Finder<'static>,
    /// The term's characters without spaces (acronyms, subsequences). Empty when the term is
    /// longer than [`MAX_FUZZY_CHARS`].
    compact: Box<[char]>,
    /// Typos allowed (0 disables the typo tiers).
    max_typos: u32,
    /// Bit-parallel pattern table for the whole term (spaces included).
    peq: Peq,
}

/// `PM[c]`: bit `i` is set when term character `i` is `c`.
#[derive(Debug, Clone)]
struct Peq {
    ascii: Box<[u64; 128]>,
    other: Box<[(char, u64)]>,
    len: usize,
}

impl Peq {
    fn new(chars: &[char]) -> Self {
        let mut ascii = Box::new([0u64; 128]);
        let mut other: Vec<(char, u64)> = Vec::new();
        for (i, &c) in chars.iter().enumerate().take(64) {
            if c.is_ascii() {
                ascii[c as usize] |= 1 << i;
            } else if let Some(e) = other.iter_mut().find(|(o, _)| *o == c) {
                e.1 |= 1 << i;
            } else {
                other.push((c, 1 << i));
            }
        }
        Peq {
            ascii,
            other: other.into(),
            len: chars.len(),
        }
    }

    #[inline]
    fn get(&self, c: char) -> u64 {
        if c.is_ascii() {
            self.ascii[c as usize]
        } else {
            self.other
                .iter()
                .find(|(o, _)| *o == c)
                .map_or(0, |(_, m)| *m)
        }
    }
}

/// Reusable per-thread buffers.
#[derive(Debug, Default)]
pub(crate) struct FuzzyBuf {
    hay: Vec<char>,
    starts: Vec<bool>,
    dp: Vec<i32>,
    initials: Vec<Initial>,
    norm: String,
}

/// A word initial: the character, an alternative (after `ال`) and where both are.
#[derive(Debug, Clone, Copy)]
struct Initial {
    c: char,
    at: usize,
    alt: Option<(char, usize)>,
}

/// Where highlight ranges point.
enum Coords {
    Normalized(Vec<Range<usize>>),
    Original(Vec<Range<usize>>),
}

fn pack(tier: u32, detail: u32, name_len: usize) -> u32 {
    (tier << 24) | (detail.min(0xFFFF) << 8) | (255 - name_len.min(255) as u32)
}

fn is_word_start(name: &[u8], i: usize) -> bool {
    i == 0 || name[i - 1] == b' '
}

impl FuzzyMatcher {
    /// Compiles an already normalized, non-empty term.
    pub(crate) fn new(normalized: &str) -> Self {
        let chars: Vec<char> = normalized.chars().collect();
        let fuzzy = chars.len() <= MAX_FUZZY_CHARS;
        let compact: Box<[char]> = if fuzzy {
            chars.iter().copied().filter(|&c| c != ' ').collect()
        } else {
            Box::default()
        };
        let max_typos = match compact.len() {
            0..=4 => 0,
            5..=8 => 1,
            _ => 2,
        };
        FuzzyMatcher {
            needle: normalized.into(),
            finder: memmem::Finder::new(normalized.as_bytes()).into_owned(),
            peq: Peq::new(if fuzzy { &chars } else { &[] }),
            compact,
            max_typos,
        }
    }

    /// The score of `name` (normalized) whose original is `original`, or `None` if it does
    /// not match.
    pub(crate) fn score(&self, name: &str, original: &str, buf: &mut FuzzyBuf) -> Option<u32> {
        self.matches(name, original, buf, false).map(|(s, _)| s)
    }

    /// Byte ranges of the ORIGINAL name that the term matched, sorted (possibly overlapping).
    pub(crate) fn highlights(&self, original: &str) -> Vec<Range<usize>> {
        let map = athrix_text_ar::normalize_name_with_map(original);
        let mut buf = FuzzyBuf::default();
        match self.matches(&map.text, original, &mut buf, true) {
            Some((_, Some(Coords::Original(r)))) => r,
            Some((_, Some(Coords::Normalized(r)))) => {
                r.into_iter().filter_map(|r| map.source_range(r)).collect()
            }
            _ => Vec::new(),
        }
    }

    fn matches(
        &self,
        name: &str,
        original: &str,
        buf: &mut FuzzyBuf,
        want_ranges: bool,
    ) -> Option<(u32, Option<Coords>)> {
        let bytes = name.as_bytes();
        let n = self.needle.len();
        // Substring tiers.
        if let Some(first) = self.finder.find(bytes) {
            let tier = if first == 0
                && (bytes.len() == n || (bytes[n] == b' ' && !bytes[n + 1..].contains(&b' ')))
            {
                TIER_EXACT
            } else if first == 0 {
                TIER_PREFIX
            } else if self
                .finder
                .find_iter(bytes)
                .any(|i| is_word_start(bytes, i))
            {
                TIER_WORD_SUBSTRING
            } else {
                TIER_SUBSTRING
            };
            let ranges = want_ranges.then(|| {
                Coords::Normalized(self.finder.find_iter(bytes).map(|i| i..i + n).collect())
            });
            return Some((pack(tier, 0, name.len()), ranges));
        }
        if self.compact.is_empty() {
            return None;
        }
        // Acronym initials and subsequences are both ordered subsequences of the normalized
        // name (camelCase initials are lower-cased letters of it too), so one greedy pass
        // rules both out for most names.
        let subsequence = is_subsequence(&self.compact, name);
        // Acronyms.
        if subsequence && self.compact.len() >= 2 {
            normalized_initials(name, &mut buf.initials);
            if let Some((detail, hits)) = self.acronym(&buf.initials, want_ranges) {
                let r = want_ranges.then(|| Coords::Normalized(char_ranges(name, &hits)));
                return Some((pack(TIER_ACRONYM, detail, name.len()), r));
            }
            if has_camel_boundary(original) {
                original_initials(original, &mut buf.initials, &mut buf.norm);
                if let Some((detail, hits)) = self.acronym(&buf.initials, want_ranges) {
                    let r = want_ranges.then(|| Coords::Original(char_ranges(original, &hits)));
                    return Some((pack(TIER_ACRONYM, detail, name.len()), r));
                }
            }
        }
        let typos = if self.max_typos > 0 && self.few_missing(name) {
            let d = osa_substring_distance(&self.peq, name);
            (d <= self.max_typos as usize).then_some(d)
        } else {
            None
        };
        let typo_hit = |tier| {
            let r = want_ranges.then(|| {
                let chars: Vec<char> = self.needle.chars().collect();
                let (_, span) = osa_substring_naive(&chars, name);
                Coords::Normalized(vec![span])
            });
            Some((pack(tier, 0, name.len()), r))
        };
        if typos == Some(1) {
            return typo_hit(TIER_ONE_TYPO);
        }
        if subsequence && self.compact.len() >= 3 {
            if let Some((s, hits)) = self.subsequence(name, buf, want_ranges) {
                let r = want_ranges.then(|| Coords::Normalized(char_ranges(name, &hits)));
                return Some((pack(TIER_SUBSEQUENCE, s, name.len()), r));
            }
        }
        if typos.is_some() {
            return typo_hit(TIER_TWO_TYPOS);
        }
        None
    }

    /// Cheap filter for the typo tiers: within `max_typos` edits, at most `max_typos` of the
    /// term's characters can be absent from the name.
    fn few_missing(&self, name: &str) -> bool {
        let hay = name.as_bytes();
        let mut enc = [0u8; 4];
        let mut missing = 0;
        for &c in &self.compact {
            let present = if c.is_ascii() {
                memchr::memchr(c as u8, hay).is_some()
            } else {
                memmem::find(hay, c.encode_utf8(&mut enc).as_bytes()).is_some()
            };
            if !present {
                missing += 1;
                if missing > self.max_typos {
                    return false;
                }
            }
        }
        true
    }

    /// The term as consecutive initials: `(detail, byte offsets of the used initials)`.
    fn acronym(&self, initials: &[Initial], want: bool) -> Option<(u32, Vec<usize>)> {
        let k = self.compact.len();
        if initials.len() < k {
            return None;
        }
        for start in 0..=initials.len() - k {
            let ok = self
                .compact
                .iter()
                .zip(&initials[start..])
                .all(|(&c, ini)| ini.c == c || ini.alt.is_some_and(|(a, _)| a == c));
            if ok {
                // Starting at the first word is better; covering every word but the
                // extension is better still.
                let detail =
                    u32::from(start == 0) * 2 + u32::from(start == 0 && initials.len() <= k + 1);
                let hits = if want {
                    self.compact
                        .iter()
                        .zip(&initials[start..])
                        .map(|(&c, ini)| match ini.alt {
                            Some((a, at)) if ini.c != c && a == c => at,
                            _ => ini.at,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                return Some((detail, hits));
            }
        }
        None
    }

    /// Best fzf-style subsequence alignment: `(score, byte offsets of matched characters)`,
    /// or `None` if its score is below the threshold.
    fn subsequence(&self, name: &str, buf: &mut FuzzyBuf, want: bool) -> Option<(u32, Vec<usize>)> {
        let FuzzyBuf {
            hay, starts, dp, ..
        } = buf;
        hay.clear();
        starts.clear();
        let mut prev = ' ';
        for c in name.chars() {
            hay.push(c);
            starts.push(prev == ' ' && c != ' ');
            prev = c;
        }
        let (m, h) = (self.compact.len(), hay.len());
        dp.clear();
        dp.resize(m * h, NEG);
        for (i, &nc) in self.compact.iter().enumerate() {
            let mut gap = NEG;
            for j in 0..h {
                if i > 0 && j >= 2 {
                    gap = (gap - GAP_EXTEND).max(dp[(i - 1) * h + j - 2] - GAP_OPEN);
                }
                if hay[j] != nc {
                    continue;
                }
                let bonus = if starts[j] { BOUNDARY } else { 0 };
                dp[i * h + j] = if i == 0 {
                    MATCH + bonus + if starts[j] { FIRST_BOUNDARY } else { 0 }
                } else {
                    let consec = if j >= 1 {
                        dp[(i - 1) * h + j - 1] + CONSECUTIVE
                    } else {
                        NEG
                    };
                    let best = consec.max(gap);
                    if best <= NEG / 2 {
                        NEG
                    } else {
                        best + MATCH + bonus
                    }
                };
            }
        }
        let last = &dp[(m - 1) * h..];
        let (mut j, &best) = last
            .iter()
            .enumerate()
            .max_by_key(|&(j, &v)| (v, std::cmp::Reverse(j)))?;
        let threshold = MATCH * m as i32 * 3 / 4;
        if best < threshold {
            return None;
        }
        let mut hits = Vec::new();
        if want {
            // Walk back through the matrix to the positions of the best alignment.
            let mut pos = vec![j];
            for i in (1..m).rev() {
                let bonus = if starts[j] { BOUNDARY } else { 0 };
                let want_prev = dp[i * h + j] - MATCH - bonus;
                let row = &dp[(i - 1) * h..i * h];
                j = if j >= 1 && row[j - 1] > NEG / 2 && row[j - 1] + CONSECUTIVE == want_prev {
                    j - 1
                } else {
                    (0..j.saturating_sub(1))
                        .rev()
                        .find(|&k| {
                            row[k] > NEG / 2
                                && row[k] - GAP_OPEN - GAP_EXTEND * (j - k - 2) as i32 == want_prev
                        })
                        .unwrap_or(0)
                };
                pos.push(j);
            }
            pos.reverse();
            // Char indices to byte offsets.
            let offsets: Vec<usize> = name.char_indices().map(|(b, _)| b).collect();
            hits = pos.into_iter().map(|p| offsets[p]).collect();
        }
        Some((best.max(0) as u32, hits))
    }
}

/// Byte ranges of the characters starting at `offsets` in `s`.
fn char_ranges(s: &str, offsets: &[usize]) -> Vec<Range<usize>> {
    offsets
        .iter()
        .filter_map(|&o| s[o..].chars().next().map(|c| o..o + c.len_utf8()))
        .collect()
}

/// True if `needle` appears in order in `hay` (greedy, O(hay), one `memchr`/`memmem` jump per
/// needle character; UTF-8 is self-synchronizing, so byte search finds whole characters).
fn is_subsequence(needle: &[char], hay: &str) -> bool {
    let mut rest = hay.as_bytes();
    let mut enc = [0u8; 4];
    for &c in needle {
        let found = if c.is_ascii() {
            memchr::memchr(c as u8, rest)
        } else {
            memmem::find(rest, c.encode_utf8(&mut enc).as_bytes())
        };
        match found {
            Some(i) => rest = &rest[i + c.len_utf8()..],
            None => return false,
        }
    }
    true
}

/// Initials of the words of a normalized name (words are separated by single spaces).
fn normalized_initials(name: &str, out: &mut Vec<Initial>) {
    out.clear();
    for (at, word) in words(name) {
        let mut cs = word.chars();
        let Some(c) = cs.next() else { continue };
        out.push(Initial {
            c,
            at,
            alt: article_alt(word, at),
        });
    }
}

/// Byte offset and text of each space-separated word.
fn words(name: &str) -> impl Iterator<Item = (usize, &str)> {
    name.split(' ')
        .scan(0usize, |pos, w| {
            let at = *pos;
            *pos += w.len() + 1;
            Some((at, w))
        })
        .filter(|(_, w)| !w.is_empty())
}

/// For a normalized word starting with the article `ال` and at least two more letters, the
/// letter after the article and its byte offset.
fn article_alt(word: &str, at: usize) -> Option<(char, usize)> {
    let rest = word.strip_prefix("ال")?;
    let mut cs = rest.chars();
    let c = cs.next()?;
    cs.next()?;
    Some((c, at + "ال".len()))
}

/// True if the original name has an ASCII camelCase boundary (`aB`, or `ABc`) or a
/// letter/digit change inside a word, which the normalized name cannot show.
fn has_camel_boundary(original: &str) -> bool {
    let b = original.as_bytes();
    (1..b.len()).any(|i| {
        let (p, c) = (b[i - 1], b[i]);
        (p.is_ascii_lowercase() && c.is_ascii_uppercase())
            || (p.is_ascii_uppercase()
                && c.is_ascii_uppercase()
                && b.get(i + 1).is_some_and(u8::is_ascii_lowercase))
            || (p.is_ascii_alphabetic() && c.is_ascii_digit())
            || (p.is_ascii_digit() && c.is_ascii_alphabetic())
    })
}

/// Initials of the words of an original name, splitting also at camelCase and letter/digit
/// changes. Separators are ASCII punctuation and whitespace; any other character belongs to
/// a word. Initials are normalized (ASCII lower-cased, others through the name normalization).
fn original_initials(original: &str, out: &mut Vec<Initial>, norm: &mut String) {
    out.clear();
    let chars: Vec<(usize, char)> = original.char_indices().collect();
    let is_sep = |c: char| c.is_whitespace() || (c.is_ascii() && !c.is_ascii_alphanumeric());
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if is_sep(c) {
            i += 1;
            continue;
        }
        // Find the end of this word.
        let mut j = i + 1;
        while j < chars.len() {
            let (p, c) = (chars[j - 1].1, chars[j].1);
            let next_lower = chars.get(j + 1).is_some_and(|n| n.1.is_ascii_lowercase());
            let boundary = is_sep(c)
                || (p.is_ascii_lowercase() && c.is_ascii_uppercase())
                || (p.is_ascii_uppercase() && c.is_ascii_uppercase() && next_lower)
                || (p.is_ascii_alphabetic() && c.is_ascii_digit())
                || (p.is_ascii_digit() && c.is_ascii_alphabetic());
            if boundary {
                break;
            }
            j += 1;
        }
        let end = chars.get(j).map_or(original.len(), |&(b, _)| b);
        let word = &original[at..end];
        if c.is_ascii() {
            out.push(Initial {
                c: c.to_ascii_lowercase(),
                at,
                alt: None,
            });
        } else {
            athrix_text_ar::normalize_name_into(word, norm);
            if let Some(first) = norm.chars().next() {
                // Offsets of the alternative point at the word start: good enough for a
                // highlight in the rare Arabic-and-camelCase name.
                let alt = article_alt(norm, 0).map(|(a, _)| (a, at));
                out.push(Initial { c: first, at, alt });
            }
        }
        i = j;
    }
}

/// Smallest optimal-string-alignment distance between the term in `peq` and any substring
/// of `hay` (Hyyrö's bit-parallel algorithm, search variant). The term must be 1..=64 chars.
fn osa_substring_distance(peq: &Peq, hay: &str) -> usize {
    let m = peq.len;
    if m == 0 || m > 64 {
        return usize::MAX;
    }
    let top = 1u64 << (m - 1);
    let mut vp: u64 = !0;
    let mut vn: u64 = 0;
    let mut d0: u64 = 0;
    let mut pm_prev: u64 = 0;
    let mut score = m;
    let mut best = m;
    for c in hay.chars() {
        let pm = peq.get(c);
        let tr = (((!d0) & pm) << 1) & pm_prev;
        d0 = (((pm & vp).wrapping_add(vp)) ^ vp) | pm | vn | tr;
        let hp = vn | !(d0 | vp);
        let hn = d0 & vp;
        if hp & top != 0 {
            score += 1;
        } else if hn & top != 0 {
            score -= 1;
        }
        let hp = hp << 1;
        let hn = hn << 1;
        vp = hn | !(d0 | hp);
        vn = d0 & hp;
        pm_prev = pm;
        best = best.min(score);
        if best == 0 {
            break;
        }
    }
    best
}

/// Plain dynamic-programming version of [`osa_substring_distance`], also returning the byte
/// range of the best substring (the first one with the smallest distance). For highlights and
/// tests.
fn osa_substring_naive(needle: &[char], hay: &str) -> (usize, Range<usize>) {
    let hay_chars: Vec<(usize, char)> = hay.char_indices().collect();
    let (m, h) = (needle.len(), hay_chars.len());
    // d[i][j]: distance of needle[..i] to the best substring ending at hay[..j]; s: its start.
    let w = h + 1;
    let mut d = vec![0usize; (m + 1) * w];
    let mut s = vec![0usize; (m + 1) * w];
    for (j, start) in s.iter_mut().enumerate().take(w) {
        *start = j;
    }
    for i in 1..=m {
        d[i * w] = i;
        s[i * w] = 0;
        for j in 1..=h {
            let cost = usize::from(needle[i - 1] != hay_chars[j - 1].1);
            let mut best = (d[(i - 1) * w + j - 1] + cost, s[(i - 1) * w + j - 1]);
            let up = (d[(i - 1) * w + j] + 1, s[(i - 1) * w + j]);
            if up.0 < best.0 {
                best = up;
            }
            let left = (d[i * w + j - 1] + 1, s[i * w + j - 1]);
            if left.0 < best.0 {
                best = left;
            }
            if i > 1
                && j > 1
                && needle[i - 1] == hay_chars[j - 2].1
                && needle[i - 2] == hay_chars[j - 1].1
            {
                let t = (d[(i - 2) * w + j - 2] + 1, s[(i - 2) * w + j - 2]);
                if t.0 < best.0 {
                    best = t;
                }
            }
            d[i * w + j] = best.0;
            s[i * w + j] = best.1;
        }
    }
    let (mut bj, mut bd) = (0, m);
    for j in 0..=h {
        if d[m * w + j] < bd {
            bd = d[m * w + j];
            bj = j;
        }
    }
    let byte = |k: usize| hay_chars.get(k).map_or(hay.len(), |&(b, _)| b);
    let start = s[m * w + bj];
    (bd, byte(start)..byte(bj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn norm(s: &str) -> String {
        athrix_text_ar::normalize_name(s)
    }

    fn score(term: &str, original: &str) -> Option<u32> {
        let m = FuzzyMatcher::new(&norm(term));
        m.score(&norm(original), original, &mut FuzzyBuf::default())
    }

    fn tier(term: &str, original: &str) -> Option<u32> {
        score(term, original).map(|s| s >> 24)
    }

    fn lit(original: &str, term: &str) -> Vec<String> {
        FuzzyMatcher::new(&norm(term))
            .highlights(original)
            .into_iter()
            .map(|r| original[r].to_owned())
            .collect()
    }

    #[test]
    fn tiers() {
        assert_eq!(tier("report", "Report.pdf"), Some(TIER_EXACT));
        assert_eq!(tier("report pdf", "Report.pdf"), Some(TIER_EXACT));
        assert_eq!(tier("rep", "report 2026.pdf"), Some(TIER_PREFIX));
        assert_eq!(tier("2026", "report_2026.pdf"), Some(TIER_WORD_SUBSTRING));
        assert_eq!(tier("port", "report.pdf"), Some(TIER_SUBSTRING));
        assert_eq!(
            tier("mrs", "Monthly Report Summary.pdf"),
            Some(TIER_ACRONYM)
        );
        assert_eq!(tier("mtr", "MyTravelReport.docx"), Some(TIER_ACRONYM));
        assert_eq!(tier("xp", "XMLParser.cs"), Some(TIER_ACRONYM));
        assert_eq!(tier("i2", "IMG2026.jpg"), Some(TIER_ACRONYM));
        assert_eq!(
            tier("تمس", "تقرير المبيعات السنوي.xlsx"),
            Some(TIER_ACRONYM)
        );
        assert_eq!(
            tier("تاا", "تقرير المبيعات السنوي.xlsx"),
            Some(TIER_ACRONYM)
        );
        assert_eq!(tier("reprot", "report.pdf"), Some(TIER_ONE_TYPO));
        assert_eq!(tier("raport", "final report.pdf"), Some(TIER_ONE_TYPO));
        assert_eq!(tier("rprt", "report.pdf"), Some(TIER_SUBSEQUENCE));
        assert_eq!(tier("محضر", "مُحَاضَرَة.docx"), Some(TIER_SUBSEQUENCE));
        assert_eq!(tier("managmnet", "management.pdf"), Some(TIER_TWO_TYPOS));
    }

    #[test]
    fn rejects() {
        // Too far apart for a subsequence.
        assert_eq!(
            tier("rprt", "r_aaaaaaaaaaaaaaaa_p_bbbbbbbbbbbbbbbbbb_rt"),
            None
        );
        assert_eq!(tier("zzq", "report.pdf"), None);
        // Short terms get no typos.
        assert_eq!(tier("rpeort", "xxxx"), None);
        assert_eq!(tier("data", "date.txt"), None);
        // Initials must be consecutive.
        assert_eq!(tier("ms", "Monthly Report Summary"), None);
    }

    #[test]
    fn ordering_within_and_across_tiers() {
        let s = |t, n| score(t, n).unwrap();
        assert!(s("report", "report.pdf") > s("report", "report 2026.pdf"));
        assert!(s("report", "report 2026.pdf") > s("report", "my report.pdf"));
        assert!(s("mrs", "Monthly Report Summary.pdf") > s("mrs", "x Monthly Report Summary.pdf"));
        // Shorter names win ties.
        assert!(s("port", "report.pdf") > s("port", "report_long_name.pdf"));
        // A tight subsequence beats a loose one.
        assert!(s("rprt", "report.pdf") > s("rprt", "rxpxrxt.pdf"));
    }

    #[test]
    fn highlights_point_into_the_original() {
        assert_eq!(lit("Report.PDF", "report"), ["Report"]);
        assert_eq!(lit("Monthly Report Summary.pdf", "mrs"), ["M", "R", "S"]);
        assert_eq!(lit("MyTravelReport.docx", "mtr"), ["M", "T", "R"]);
        assert_eq!(lit("report.pdf", "rprt"), ["r", "p", "r", "t"]);
        assert_eq!(lit("final report.pdf", "raport"), ["report"]);
        assert_eq!(lit("تقرير المبيعات السنوي.xlsx", "تمس"), ["ت", "م", "س"]);
        // Vocalized original: the highlight covers the letter with its harakat.
        assert_eq!(lit("مُحَاضَرَة.docx", "محضر"), ["مُ", "حَ", "ضَ", "رَ"]);
        assert!(lit("report.pdf", "zzq").is_empty());
    }

    proptest! {
        #[test]
        fn bit_parallel_osa_matches_the_dp(
            needle in "[abc]{1,10}",
            hay in "[abcd ]{0,30}",
        ) {
            let chars: Vec<char> = needle.chars().collect();
            let fast = osa_substring_distance(&Peq::new(&chars), &hay);
            let (slow, _) = osa_substring_naive(&chars, &hay);
            prop_assert_eq!(fast, slow);
        }

        #[test]
        fn bit_parallel_osa_matches_the_dp_arabic(
            needle in "[تقرير]{1,8}",
            hay in "[تقريرا ]{0,20}",
        ) {
            let chars: Vec<char> = needle.chars().collect();
            let fast = osa_substring_distance(&Peq::new(&chars), &hay);
            let (slow, _) = osa_substring_naive(&chars, &hay);
            prop_assert_eq!(fast, slow);
        }

        #[test]
        fn never_panics_and_highlights_are_char_ranges(
            term in "[a-zA-Z ت م س ا ل_.]{1,12}",
            name in "[a-zA-Z0-9 ت م س ا ل ُ _.-]{0,30}",
        ) {
            let n = norm(&term);
            prop_assume!(!n.is_empty());
            let m = FuzzyMatcher::new(&n);
            let s = m.score(&norm(&name), &name, &mut FuzzyBuf::default());
            let h = m.highlights(&name);
            prop_assert_eq!(s.is_some(), !h.is_empty());
            for r in h {
                prop_assert!(r.start < r.end && name.is_char_boundary(r.start) && name.is_char_boundary(r.end));
            }
        }
    }
}
