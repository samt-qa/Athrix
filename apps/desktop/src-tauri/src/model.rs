//! The JSON types exchanged with the UI, exactly as described in `docs/IPC-CONTRACT.md`.
//!
//! All field names are camelCase on the wire; offsets sent to the UI are UTF-16 code units so
//! JavaScript can use them with `String.prototype.slice` directly.

use serde::Serialize;

/// Lifecycle of the name index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexState {
    /// Nothing indexed yet and no indexing running.
    Idle,
    /// A walk is running in the background.
    Indexing,
    /// The last walk finished, or a snapshot was loaded; searches use that index.
    Ready,
    /// The last walk failed (see [`IndexStatus::error`]).
    Error,
}

/// Payload of `index_status` and of the `index://status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    /// Current state.
    pub state: IndexState,
    /// Entries in the index searches currently use (0 until the first walk finishes or a
    /// snapshot is loaded).
    pub entries: u64,
    /// The roots being indexed (or last indexed).
    pub roots: Vec<String>,
    /// Duration of the last completed walk.
    pub elapsed_ms: Option<u64>,
    /// English message when `state` is `error`.
    pub error: Option<String>,
}

impl Default for IndexStatus {
    fn default() -> Self {
        IndexStatus {
            state: IndexState::Idle,
            entries: 0,
            roots: Vec::new(),
            elapsed_ms: None,
            error: None,
        }
    }
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultItem {
    /// Result id, used by `open_entry` / `reveal_entry` / `entry_path`: the index generation
    /// and the entry, `generation << 32 | entry`, always below 2^53 (see
    /// [`crate::results::encode_id`]). Ids from an older index are rejected, never reused.
    pub id: u64,
    /// Original name as on disk.
    pub name: String,
    /// Full path of the parent folder (`""` for a root).
    pub parent: String,
    /// The entry is a folder.
    pub is_dir: bool,
    /// Size in bytes; `None` for folders.
    pub size: Option<u64>,
    /// Last modification, Unix seconds.
    pub modified: Option<i64>,
    /// Lowercase extension without the dot, `""` if none.
    pub ext: String,
    /// Matched ranges inside `name`, as UTF-16 `[start, end)` pairs, sorted and non-overlapping.
    pub highlights: Vec<(usize, usize)>,
}

/// A query syntax error, positioned in UTF-16 units of the query text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryError {
    /// English explanation.
    pub message: String,
    /// Stable `ParseErrorKind` variant name (e.g. `UnknownFilter`) for UI translation.
    pub kind: String,
    /// Start of the problem (UTF-16 units).
    pub start: usize,
    /// End of the problem (UTF-16 units, exclusive).
    pub end: usize,
    /// Suggested replacement, if any.
    pub suggestion: Option<String>,
}

/// Response of the `search` command.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// Echo of the request's `requestId`.
    pub request_id: u64,
    /// The hits, at most `limit`.
    pub items: Vec<ResultItem>,
    /// More hits exist than `limit`.
    pub truncated: bool,
    /// Wall time spent in the command.
    pub elapsed_ms: f64,
    /// Set when the query could not be parsed (`items` is then empty).
    pub error: Option<QueryError>,
    /// Set when a newer request cancelled this one (`items` is then empty).
    pub cancelled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_in_camel_case() {
        let status = IndexStatus {
            state: IndexState::Ready,
            entries: 3,
            roots: vec!["C:\\Users\\me".into()],
            elapsed_ms: Some(12),
            error: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "state": "ready", "entries": 3, "roots": ["C:\\Users\\me"],
                "elapsedMs": 12, "error": null
            })
        );
        let item = ResultItem {
            id: 1,
            name: "a.txt".into(),
            parent: "/x".into(),
            is_dir: false,
            size: Some(5),
            modified: None,
            ext: "txt".into(),
            highlights: vec![(0, 1)],
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["isDir"], false);
        assert_eq!(json["highlights"], serde_json::json!([[0, 1]]));
    }
}
