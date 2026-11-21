//! Reciprocal Rank Fusion.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;

/// The usual RRF constant (Cormack, Clarke & Büttcher 2009).
pub const DEFAULT_K: f64 = 60.0;

/// One ranked input list: keys best first, with a weight.
#[derive(Debug, Clone, Copy)]
pub struct RankedList<'a, K> {
    /// Multiplies this list's contribution. Negative or non-finite weights count as 0.
    pub weight: f64,
    /// The keys, best first. A key repeated in the same list counts once, at its best rank.
    pub keys: &'a [K],
}

impl<'a, K> RankedList<'a, K> {
    /// A list with weight 1.
    pub fn new(keys: &'a [K]) -> Self {
        RankedList { weight: 1.0, keys }
    }

    /// A list with the given weight.
    pub fn weighted(keys: &'a [K], weight: f64) -> Self {
        RankedList { weight, keys }
    }
}

/// One fused result. The key is `lists[list].keys[index]` (its first occurrence).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    /// The list where the key was first seen (lists are scanned in order).
    pub list: usize,
    /// Its position in that list (0-based).
    pub index: usize,
    /// The fused score `Σ wᵢ / (k + rankᵢ)`.
    pub score: f64,
    /// Its best (smallest) 1-based rank over all lists.
    pub best_rank: usize,
    /// Bit `i` is set when the key appears in list `i` (lists beyond 64 are not recorded).
    pub lists: u64,
}

impl Fused {
    /// True when the key appears in list `i` (`i < 64`).
    pub fn in_list(&self, i: usize) -> bool {
        i < 64 && self.lists & (1 << i) != 0
    }
}

/// Fuses ranked lists with Reciprocal Rank Fusion:
///
/// `score(d) = Σᵢ wᵢ / (k + rankᵢ(d))`, `rankᵢ` 1-based, summed over the lists that contain `d`.
///
/// Results are sorted by score (highest first); ties go to the better best rank, then to the
/// key seen first (earlier list, then earlier position). The order therefore depends only on
/// the input, never on hashing. `k` below 0 or non-finite falls back to [`DEFAULT_K`].
///
/// Cost: one hash-map insert per input key plus a sort of the distinct keys.
pub fn reciprocal_rank_fusion<K: Hash + Eq>(lists: &[RankedList<'_, K>], k: f64) -> Vec<Fused> {
    let k = if k.is_finite() && k >= 0.0 {
        k
    } else {
        DEFAULT_K
    };
    let total: usize = lists.iter().map(|l| l.keys.len()).sum();
    let mut slot: HashMap<&K, usize> = HashMap::with_capacity(total);
    let mut out: Vec<Fused> = Vec::with_capacity(total);
    // The last list each key was counted in, to ignore repeats inside one list.
    let mut last: Vec<usize> = Vec::with_capacity(total);
    for (li, list) in lists.iter().enumerate() {
        let w = if list.weight.is_finite() && list.weight > 0.0 {
            list.weight
        } else {
            0.0
        };
        let bit = if li < 64 { 1u64 << li } else { 0 };
        for (index, key) in list.keys.iter().enumerate() {
            let rank = index + 1;
            match slot.entry(key) {
                Entry::Vacant(v) => {
                    v.insert(out.len());
                    last.push(li);
                    out.push(Fused {
                        list: li,
                        index,
                        score: w / (k + rank as f64),
                        best_rank: rank,
                        lists: bit,
                    });
                }
                Entry::Occupied(o) => {
                    let i = *o.get();
                    if last[i] == li {
                        continue; // repeated within this list: keep the first (best) rank
                    }
                    last[i] = li;
                    let f = &mut out[i];
                    f.score += w / (k + rank as f64);
                    f.best_rank = f.best_rank.min(rank);
                    f.lists |= bit;
                }
            }
        }
    }
    sort_fused(&mut out);
    out
}

/// Sorts by score descending, then best rank, then first-seen order. Stable for equal keys.
pub(crate) fn sort_fused(out: &mut [Fused]) {
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.best_rank.cmp(&b.best_rank))
            .then(a.list.cmp(&b.list))
            .then(a.index.cmp(&b.index))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<'a>(lists: &[RankedList<'a, &'a str>], fused: &[Fused]) -> Vec<&'a str> {
        fused.iter().map(|f| lists[f.list].keys[f.index]).collect()
    }

    #[test]
    fn fuses_two_lists() {
        let a = ["x", "y", "z"];
        let b = ["z", "x", "w"];
        let lists = [RankedList::new(&a[..]), RankedList::new(&b[..])];
        let f = reciprocal_rank_fusion(&lists, 60.0);
        // x: 1/61 + 1/62, z: 1/63 + 1/61, y: 1/62, w: 1/63
        assert_eq!(keys(&lists, &f), ["x", "z", "y", "w"]);
        assert!((f[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        assert!(f[0].in_list(0) && f[0].in_list(1));
        assert!(f[2].in_list(0) && !f[2].in_list(1));
    }

    #[test]
    fn weights_and_duplicates() {
        let a = ["x", "x", "y"];
        let b = ["y"];
        let lists = [
            RankedList::weighted(&a[..], 1.0),
            RankedList::weighted(&b[..], 3.0),
        ];
        let f = reciprocal_rank_fusion(&lists, 0.0);
        // x: 1/1 (the duplicate is ignored); y: 1/3 + 3/1
        assert_eq!(keys(&lists, &f), ["y", "x"]);
        assert!((f[1].score - 1.0).abs() < 1e-12);
        let zero = [RankedList::weighted(&a[..], f64::NAN)];
        assert!(
            reciprocal_rank_fusion(&zero, -1.0)
                .iter()
                .all(|f| f.score == 0.0)
        );
    }

    #[test]
    fn ties_go_to_first_seen() {
        let a = ["p", "q"];
        let b = ["q", "p"];
        let lists = [RankedList::new(&a[..]), RankedList::new(&b[..])];
        let f = reciprocal_rank_fusion(&lists, 60.0);
        assert_eq!(keys(&lists, &f), ["p", "q"]);
        let lists = [RankedList::new(&b[..]), RankedList::new(&a[..])];
        let f = reciprocal_rank_fusion(&lists, 60.0);
        assert_eq!(keys(&lists, &f), ["q", "p"]);
        assert!(reciprocal_rank_fusion::<&str>(&[], 60.0).is_empty());
    }
}
