//! RRF properties and speed.

use std::time::{Duration, Instant};

use athrix_ranking::{Fused, RankedList, reciprocal_rank_fusion};
use proptest::prelude::*;

fn order(lists: &[Vec<u16>], weights: &[f64], k: f64) -> Vec<(u16, f64)> {
    let ranked: Vec<RankedList<'_, u16>> = lists
        .iter()
        .zip(weights)
        .map(|(l, w)| RankedList::weighted(l, *w))
        .collect();
    reciprocal_rank_fusion(&ranked, k)
        .iter()
        .map(|f: &Fused| (lists[f.list][f.index], f.score))
        .collect()
}

fn pos(out: &[(u16, f64)], key: u16) -> usize {
    out.iter()
        .position(|(k, _)| *k == key)
        .expect("key present")
}

fn score(out: &[(u16, f64)], key: u16) -> f64 {
    out[pos(out, key)].1
}

fn lists_strategy() -> impl Strategy<Value = (Vec<Vec<u16>>, Vec<f64>)> {
    prop::collection::vec(prop::collection::vec(0u16..60, 0..40), 1..5).prop_flat_map(|lists| {
        let n = lists.len();
        (Just(lists), prop::collection::vec(0.1f64..5.0, n))
    })
}

proptest! {
    #[test]
    fn output_is_the_sorted_union((lists, weights) in lists_strategy(), k in 0.0f64..100.0) {
        let out = order(&lists, &weights, k);
        let mut keys: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
        for w in out.windows(2) {
            prop_assert!(w[0].1 >= w[1].1);
        }
        keys.sort_unstable();
        let mut union: Vec<u16> = lists.iter().flatten().copied().collect();
        union.sort_unstable();
        union.dedup();
        prop_assert_eq!(keys, union);
    }

    #[test]
    fn deterministic((lists, weights) in lists_strategy()) {
        // Each call builds a fresh HashMap with a different random hasher seed.
        let a = order(&lists, &weights, 60.0);
        let b = order(&lists, &weights, 60.0);
        prop_assert_eq!(a, b);
    }

    #[test]
    fn improving_a_rank_never_hurts(
        (lists, weights) in lists_strategy(),
        pick in any::<prop::sample::Index>(),
        which in any::<prop::sample::Index>(),
    ) {
        let li = which.index(lists.len());
        let list = &lists[li];
        prop_assume!(list.len() >= 2);
        let p = pick.index(list.len() - 1) + 1;
        let x = list[p];
        // Only meaningful when x's first occurrence in this list is at p.
        prop_assume!(!list[..p].contains(&x));
        let before = order(&lists, &weights, 60.0);
        let mut better = lists.clone();
        better[li].swap(p - 1, p);
        let after = order(&better, &weights, 60.0);
        prop_assert!(score(&after, x) >= score(&before, x) - 1e-12);
        prop_assert!(pos(&after, x) <= pos(&before, x));
    }

    #[test]
    fn appearing_in_another_list_helps(
        (lists, weights) in lists_strategy(),
        x in 0u16..60,
        at in any::<prop::sample::Index>(),
    ) {
        prop_assume!(lists.iter().any(|l| l.contains(&x)));
        let before = order(&lists, &weights, 60.0);
        let mut more = lists.clone();
        let mut extra: Vec<u16> = (100..110).collect();
        extra.insert(at.index(extra.len() + 1), x);
        more.push(extra);
        let mut w = weights.clone();
        w.push(1.0);
        let after = order(&more, &w, 60.0);
        prop_assert!(score(&after, x) > score(&before, x));
    }

    #[test]
    fn scaling_all_weights_keeps_the_order((lists, weights) in lists_strategy(), c in 0.1f64..10.0) {
        let a: Vec<u16> = order(&lists, &weights, 60.0).into_iter().map(|(k, _)| k).collect();
        let scaled: Vec<f64> = weights.iter().map(|w| w * c).collect();
        let b: Vec<u16> = order(&lists, &scaled, 60.0).into_iter().map(|(k, _)| k).collect();
        // Floating point can reorder near-equal scores; compare where scores differ clearly.
        let sa = order(&lists, &weights, 60.0);
        for (i, (k, s)) in sa.iter().enumerate() {
            let clear = sa.iter().all(|(k2, s2)| k2 == k || (s - s2).abs() > 1e-9);
            if clear {
                prop_assert_eq!(b[i], a[i]);
            }
        }
    }
}

#[test]
fn equal_scores_keep_input_order() {
    // Interleaved, symmetric lists: every key has the same score; order is first-seen.
    let a: Vec<u16> = vec![1, 2, 3, 4];
    let b: Vec<u16> = vec![4, 3, 2, 1];
    let out = order(&[a, b], &[1.0, 1.0], 60.0);
    // 1 and 4 tie on score (1/61+1/64) and best rank: 1 was seen first.
    assert_eq!(
        out.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        [1, 4, 2, 3]
    );
}

fn paths(n: usize, offset: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            format!(
                r"C:\Users\user\Documents\مشاريع\2026\تقرير الميزانية {}.docx",
                i + offset
            )
        })
        .collect()
}

#[test]
fn fuses_two_thousand_paths_fast() {
    let a = paths(1000, 0);
    let b = paths(1000, 500); // half overlap
    let lists = [RankedList::new(&a[..]), RankedList::new(&b[..])];
    let mut best = Duration::MAX;
    for _ in 0..20 {
        let t = Instant::now();
        let out = reciprocal_rank_fusion(&lists, 60.0);
        best = best.min(t.elapsed());
        assert_eq!(out.len(), 1500);
    }
    eprintln!("RRF 2×1000 paths: {best:?}");
    let bound = if cfg!(debug_assertions) {
        Duration::from_millis(20)
    } else {
        Duration::from_millis(1)
    };
    assert!(best < bound, "{best:?}");
}
