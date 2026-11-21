//! Result fusion (Reciprocal Rank Fusion), ranking boosts, frecency, and the executor that
//! runs a [`athrix_query::SearchPlan`] on the name and content indexes.
//!
//! # Reciprocal Rank Fusion
//!
//! Several ranked lists (name hits, content hits, later semantic hits) are merged by rank,
//! not by score, because their scores are not comparable (fuzzy points vs BM25 vs cosine):
//!
//! ```text
//! rrf(d) = Σ over lists i containing d of  wᵢ / (k + rankᵢ(d))
//! ```
//!
//! with 1-based ranks, per-list weights `wᵢ` (default 1) and `k = 60` (Cormack et al. 2009):
//! a larger `k` flattens the difference between top and lower ranks. A document found by two
//! lists beats one found by a single list at a similar rank. Ties are broken by the best
//! rank, then by first appearance (earlier list, earlier position), so the order is a pure
//! function of the input. See [`reciprocal_rank_fusion`].
//!
//! # Boosts
//!
//! After fusion each result's score is multiplied by
//!
//! ```text
//! 1 + exact_name·[name = query words] + recency·0.5^(age / 30 d) + frecency·f / (f + 3)
//! ```
//!
//! (defaults `exact_name = 1`, `recency = 0.1`, `frecency = 0.5`; see [`Boosts`]). `f` is the
//! file's frecency: every open adds 1 and the sum halves every 14 days ([`FrecencyStore`],
//! [`MemoryFrecencyStore`], [`FileFrecencyStore`]). Every term is bounded, so boosts reorder
//! results of similar relevance without letting a weak match jump far ahead.
//!
//! # Executor
//!
//! [`execute`] runs a plan through caller-supplied searches ([`Sources`] / [`Closures`]),
//! intersects `name AND content`, subtracts excluded content, fuses, boosts, and reports which
//! part matched each result ([`Matched`]) with the content snippet. See the [`exec`] module
//! for the strategy and its bounds.
//!
//! Everything here is pure: no clock (`now` is passed in), no I/O except
//! [`FileFrecencyStore`].

use std::borrow::Cow;

pub mod boost;
pub mod exec;
pub mod frecency;
pub mod fusion;

pub use boost::{BoostInput, Boosts, ExactName};
pub use exec::{
    Closures, ContentHit, ExecOptions, Matched, NameHit, NameResults, ResultHit, SearchResults,
    Sources, Truncation, execute,
};
pub use frecency::{
    FileFrecencyStore, FrecencyConfig, FrecencyEntry, FrecencyStore, MemoryFrecencyStore,
};
pub use fusion::{DEFAULT_K, Fused, RankedList, reciprocal_rank_fusion};

/// The identity of a file path for joining results of different indexes: unchanged when
/// `case_insensitive` is false; otherwise lower-cased with `/` turned into `\` (Windows
/// file systems are case-insensitive and accept both separators).
pub fn path_key(path: &str, case_insensitive: bool) -> Cow<'_, str> {
    if !case_insensitive || !path.chars().any(|c| c == '/' || c.is_uppercase()) {
        return Cow::Borrowed(path);
    }
    let lower = path.to_lowercase();
    Cow::Owned(if lower.contains('/') {
        lower.replace('/', "\\")
    } else {
        lower
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_keys() {
        assert_eq!(path_key(r"C:\Docs/A.txt", true), r"c:\docs\a.txt");
        assert_eq!(path_key(r"C:\Docs/A.txt", false), r"C:\Docs/A.txt");
        assert!(matches!(path_key(r"c:\docs\a.txt", true), Cow::Borrowed(_)));
        assert_eq!(path_key(r"C:\تقرير\Ä.pdf", true), r"c:\تقرير\ä.pdf");
    }
}
