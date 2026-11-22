//! Grouping near-identical hashes: a multi-index (pigeonhole) lookup for "every hash within
//! `d` bits of this one", then greedy leader clustering.
//!
//! Why not a BK-tree: perceptual hashes of unrelated pictures sit about 32 bits apart, so a
//! BK-tree query with a radius of 8 must still visit almost every child (distances 24..=40)
//! and degrades to a linear scan. Instead each hash is cut into `m` chunks; two hashes at most
//! `d` bits apart agree within `d / m` bits on at least one chunk (pigeonhole). With `m` chosen
//! so that `d / m <= 1`, a query probes, per chunk, its exact value and its one-bit variants in
//! a hash table: a few hundred candidates instead of all of them.

use std::collections::HashMap;

use crate::ImageHash;

/// An index of hashes answering "which are within `max_distance` bits of this hash?".
#[derive(Debug, Clone)]
pub struct NeighborIndex {
    hashes: Vec<ImageHash>,
    max_distance: u32,
    /// `(shift, width)` of each chunk.
    chunks: Vec<(u32, u32)>,
    /// Bits a matching chunk may differ by: `max_distance / chunks.len()`.
    radius: u32,
    /// Per chunk: chunk value → positions in `hashes`.
    tables: Vec<HashMap<u64, Vec<u32>>>,
}

impl NeighborIndex {
    /// Indexes `hashes` (positions are kept: results refer to them) for queries of up to
    /// `max_distance` bits.
    pub fn new(hashes: Vec<ImageHash>, max_distance: u32) -> Self {
        // Enough chunks for a radius of at most 1 (up to 15 bits); 2 beyond that.
        let count = (max_distance / 2 + 1).clamp(1, 8);
        let radius = max_distance / count;
        let mut chunks = Vec::with_capacity(count as usize);
        let mut shift = 0;
        for i in 0..count {
            let width = 64 / count + u32::from(i < 64 % count);
            chunks.push((shift, width));
            shift += width;
        }
        let mut tables = vec![HashMap::new(); chunks.len()];
        for (pos, h) in hashes.iter().enumerate() {
            let pos = u32::try_from(pos).unwrap_or(u32::MAX);
            for (table, &(shift, width)) in tables.iter_mut().zip(&chunks) {
                table
                    .entry(chunk_of(*h, shift, width))
                    .or_insert_with(Vec::new)
                    .push(pos);
            }
        }
        NeighborIndex {
            hashes,
            max_distance,
            chunks,
            radius,
            tables,
        }
    }

    /// Number of indexed hashes.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// True when nothing is indexed.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Every indexed position whose hash is within `max_distance` (at most the index's) of
    /// `query`, with its distance, sorted by position.
    pub fn within(&self, query: ImageHash, max_distance: u32) -> Vec<(usize, u32)> {
        let max_distance = max_distance.min(self.max_distance);
        let mut candidates: Vec<u32> = Vec::new();
        let mut probe = Vec::new();
        for (table, &(shift, width)) in self.tables.iter().zip(&self.chunks) {
            probe.clear();
            variants(
                chunk_of(query, shift, width),
                width,
                self.radius,
                &mut probe,
            );
            for v in &probe {
                if let Some(list) = table.get(v) {
                    candidates.extend_from_slice(list);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .filter_map(|pos| {
                let pos = pos as usize;
                let d = self.hashes[pos].distance(query);
                (d <= max_distance).then_some((pos, d))
            })
            .collect()
    }
}

fn chunk_of(h: ImageHash, shift: u32, width: u32) -> u64 {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    (h.0 >> shift) & mask
}

/// Pushes `value` and every `width`-bit value differing from it in at most `radius` bits.
fn variants(value: u64, width: u32, radius: u32, out: &mut Vec<u64>) {
    fn flip(value: u64, from: u32, width: u32, left: u32, out: &mut Vec<u64>) {
        out.push(value);
        if left == 0 {
            return;
        }
        for bit in from..width {
            flip(value ^ (1 << bit), bit + 1, width, left - 1, out);
        }
    }
    flip(value, 0, width, radius, out);
}

/// A group of similar images: indexes into the slice given to [`group_similar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// `(index, distance to the group's first member)`, closest first; the first member (the
    /// "leader", distance 0) is the image with the most near neighbours.
    pub members: Vec<(usize, u32)>,
}

/// Groups of images whose hashes are within `max_distance` bits of their group's leader.
/// Only groups of two or more images are returned, the biggest first (ties: the one whose
/// leader comes first in `hashes`); `None` entries (images that could not be hashed) are left
/// out.
///
/// Leaders are taken greedily, the image with the most neighbours first, and each image joins
/// at most one group, so a long chain of slightly different pictures (a time-lapse) does not
/// fuse into one giant group: every member is close to its leader. Cost: one
/// [`NeighborIndex`] query per distinct hash.
pub fn group_similar(hashes: &[Option<ImageHash>], max_distance: u32) -> Vec<Group> {
    // Distinct hashes, in order of first appearance, with the images that have them.
    let mut slots: HashMap<ImageHash, usize> = HashMap::new();
    let mut distinct: Vec<ImageHash> = Vec::new();
    let mut images: Vec<Vec<usize>> = Vec::new();
    for (i, hash) in hashes.iter().enumerate() {
        let Some(hash) = *hash else { continue };
        match slots.get(&hash) {
            Some(&slot) => images[slot].push(i),
            None => {
                slots.insert(hash, distinct.len());
                distinct.push(hash);
                images.push(vec![i]);
            }
        }
    }
    let index = NeighborIndex::new(distinct.clone(), max_distance);
    // Neighbours of each distinct hash (itself included), as slots.
    let neighbours: Vec<Vec<(usize, u32)>> = distinct
        .iter()
        .map(|h| index.within(*h, max_distance))
        .collect();
    let weights: Vec<usize> = neighbours
        .iter()
        .map(|near| near.iter().map(|&(s, _)| images[s].len()).sum())
        .collect();
    let mut order: Vec<usize> = (0..distinct.len()).collect();
    order.sort_by(|&a, &b| weights[b].cmp(&weights[a]).then(a.cmp(&b)));

    let mut taken = vec![false; distinct.len()];
    let mut groups = Vec::new();
    for leader in order {
        if taken[leader] || weights[leader] < 2 {
            continue;
        }
        let mut members: Vec<(usize, u32)> = Vec::new();
        for &(slot, d) in &neighbours[leader] {
            if !taken[slot] {
                taken[slot] = true;
                members.extend(images[slot].iter().map(|&i| (i, d)));
            }
        }
        if members.len() >= 2 {
            members.sort_unstable_by_key(|&(i, d)| (d, i));
            groups.push(Group { members });
        }
    }
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then(a.members[0].0.cmp(&b.members[0].0))
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random sequence (xorshift), so the tests need no `rand`.
    fn randoms(seed: u64) -> impl Iterator<Item = u64> {
        let mut x = seed | 1;
        std::iter::repeat_with(move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        })
    }

    #[test]
    fn variants_cover_the_radius() {
        let mut out = Vec::new();
        variants(0b101, 4, 1, &mut out);
        out.sort();
        assert_eq!(out, vec![0b001, 0b100, 0b101, 0b111, 0b1101]);
        out.clear();
        variants(0, 6, 2, &mut out);
        assert_eq!(out.len(), 1 + 6 + 15);
    }

    #[test]
    fn index_finds_exactly_the_hashes_within_range() {
        for max in [0, 3, 8, 12, 20] {
            let mut hashes: Vec<ImageHash> = randoms(7).take(2000).map(ImageHash).collect();
            // Near copies at every distance up to `max + 2`.
            for d in 0..=max + 2 {
                let base = hashes[d as usize].0;
                let flipped = (0..d).fold(base, |h, b| h ^ (1 << ((b * 5) % 64)));
                hashes.push(ImageHash(flipped));
            }
            let index = NeighborIndex::new(hashes.clone(), max);
            for q in hashes.iter().step_by(37).chain(&hashes[2000..]) {
                let got = index.within(*q, max);
                let want: Vec<(usize, u32)> = hashes
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (i, h.distance(*q)))
                    .filter(|(_, d)| *d <= max)
                    .collect();
                assert_eq!(got, want, "max {max}");
            }
        }
    }

    #[test]
    fn near_hashes_group_and_strangers_do_not() {
        let a = ImageHash(0x0F0F_0F0F_0F0F_0F0F);
        let b = ImageHash(a.0 ^ 0b111); // 3 bits from a
        let c = ImageHash(a.0 ^ 0xFF00_0000_0000_0000); // 8 bits from a
        let far = ImageHash(!a.0);
        let hashes = [Some(far), Some(b), None, Some(a), Some(c), Some(a)];
        let groups = group_similar(&hashes, 8);
        assert_eq!(groups.len(), 1);
        let mut idx: Vec<usize> = groups[0].members.iter().map(|m| m.0).collect();
        idx.sort();
        assert_eq!(idx, vec![1, 3, 4, 5]);
        // The leader (a, the most connected) comes first at distance 0, twice.
        assert_eq!(&groups[0].members[..2], &[(3, 0), (5, 0)]);
        assert_eq!(group_similar(&hashes, 2)[0].members.len(), 2);
        assert!(group_similar(&[Some(a), Some(far)], 8).is_empty());
        assert!(group_similar(&[], 8).is_empty());
    }

    #[test]
    fn a_chain_does_not_fuse_into_one_group() {
        // Each hash is 6 bits from the previous: neighbours link, ends are 12+ bits apart.
        let mut chain = vec![0u64];
        for i in 0..4 {
            chain.push(chain[i] ^ (0b11_1111 << (i * 6)));
        }
        let hashes: Vec<Option<ImageHash>> =
            chain.into_iter().map(|h| Some(ImageHash(h))).collect();
        let groups = group_similar(&hashes, 8);
        assert!(groups.len() >= 2);
        for g in groups {
            let leader = hashes[g.members[0].0].unwrap();
            for &(i, d) in &g.members {
                assert_eq!(hashes[i].unwrap().distance(leader), d);
                assert!(d <= 8);
            }
        }
    }

    #[test]
    fn groups_fifty_thousand_hashes() {
        let mut hashes: Vec<Option<ImageHash>> = randoms(99)
            .take(50_000)
            .map(|h| Some(ImageHash(h)))
            .collect();
        // 500 planted near-copies.
        for i in 0..500 {
            let h = hashes[i * 7].unwrap();
            hashes.push(Some(ImageHash(h.0 ^ (1 << (i % 64)))));
        }
        let started = std::time::Instant::now();
        let groups = group_similar(&hashes, 8);
        eprintln!("50 500 hashes grouped in {:?}", started.elapsed());
        let grouped: usize = groups.iter().map(|g| g.members.len()).sum();
        assert_eq!((groups.len(), grouped), (500, 1000));
    }
}
