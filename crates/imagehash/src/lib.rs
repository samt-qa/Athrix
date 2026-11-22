//! Perceptual image hashes for "find similar images" (`similar:` / `مشابه:`).
//!
//! - [`hash_file`] decodes an image (JPEG, PNG, WebP, GIF, BMP, TIFF; pure-Rust decoders only),
//!   shrinks it to 32×32 grey levels and keeps the signs of its lowest 8×8 DCT frequencies
//!   against their median: a 64-bit **pHash** ([`ImageHash`]). Big JPEGs use their embedded EXIF
//!   thumbnail when it has the picture's shape, so a 20 MB photo costs a 10 KB decode.
//! - Two hashes are compared by Hamming distance ([`ImageHash::distance`], one popcount):
//!   re-encoded, resized or lightly edited copies stay within a few bits, unrelated pictures
//!   land near 32 of 64.
//! - [`group_similar`] clusters many hashes (a pigeonhole multi-index, [`NeighborIndex`], so
//!   50 000 images take well under a second);[`HashCache`] keeps hashes in SQLite keyed by path, size and modification time;
//!   [`hash_all`] hashes what the cache lacks in parallel, cancellable and within a time budget.
//!
//! Why DCT pHash rather than dHash: dHash (row gradients of a 9×8 thumbnail) is cheaper to
//! compute, but the hash is computed on a 32×32 thumbnail either way and decoding dominates the
//! cost by far. pHash's low frequencies survive JPEG re-compression, gamma and contrast changes
//! and small crops better, so a single distance threshold separates copies from strangers more
//! cleanly.
//!
//! HEIC/HEIF and AVIF are not read: no decoder builds without a C toolchain.

mod batch;
mod cache;
mod decode;
mod exif;
mod group;
mod phash;

pub use batch::{Candidate, HashRun, hash_all, hash_threads};
pub use cache::{CACHE_FILE, CacheError, CachedHash, HashCache, PruneLimits, PruneReport};
pub use decode::{
    HashError, IMAGE_EXTENSIONS, MAX_FILE_BYTES, hash_bytes, hash_file, hash_image,
    is_image_extension,
};
pub use group::{Group, NeighborIndex, group_similar};

/// Number of bits in an [`ImageHash`].
pub const HASH_BITS: u32 = 64;

/// Largest distance at which two images join the same `similar:` group (of [`HASH_BITS`]).
/// Copies saved at another size or quality stay within 0–4; a crop or a filter reaches 6–10.
pub const GROUP_MAX_DISTANCE: u32 = 8;

/// Largest distance listed by "find similar images" for one reference image: a little looser
/// than [`GROUP_MAX_DISTANCE`], since the list is ranked closest first.
pub const REFERENCE_MAX_DISTANCE: u32 = 12;

/// A 64-bit perceptual hash of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageHash(pub u64);

impl ImageHash {
    /// Hamming distance: how many of the 64 bits differ (0 = same picture as far as the hash
    /// can tell).
    pub fn distance(self, other: ImageHash) -> u32 {
        (self.0 ^ other.0).count_ones()
    }
}

/// A distance turned into a similarity percentage for the UI: 0 bits apart is 100 %, 32 bits
/// (what unrelated images average) is 50 %.
pub fn similarity_percent(distance: u32) -> u8 {
    let d = distance.min(HASH_BITS);
    // (64 - d) * 100 / 64, rounded to nearest; at most 100, so it fits a u8.
    let pct = ((HASH_BITS - d) * 100 + HASH_BITS / 2) / HASH_BITS;
    u8::try_from(pct).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_differing_bits() {
        assert_eq!(ImageHash(0).distance(ImageHash(0)), 0);
        assert_eq!(ImageHash(0b1011).distance(ImageHash(0)), 3);
        assert_eq!(ImageHash(u64::MAX).distance(ImageHash(0)), 64);
    }

    #[test]
    fn similarity_is_a_percentage() {
        assert_eq!(similarity_percent(0), 100);
        assert_eq!(similarity_percent(32), 50);
        assert_eq!(similarity_percent(64), 0);
        assert_eq!(similarity_percent(99), 0);
        assert_eq!(similarity_percent(8), 88);
    }
}
