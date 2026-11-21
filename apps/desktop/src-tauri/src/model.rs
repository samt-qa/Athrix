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
    /// Result id, used by `open_entry` / `reveal_entry` / `entry_path`: the generation of the
    /// index view it came from and the entry, `generation << 32 | entry`, always below 2^53
    /// (see [`crate::results::encode_id`]). It resolves only with that view, kept for the
    /// last [`crate::indexing::RECENT_VIEWS`] publishes of the current base; older ids are
    /// rejected, never resolved to another file.
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
    /// Which part of the query found this result: always [`Matched::Name`] for name-only
    /// searches.
    pub matched: Matched,
    /// A fragment of the file's text with the matched words, when the content matched. Only
    /// the first [`crate::content::SNIPPET_ROWS`] rows of a response carry it; the UI asks
    /// for the others with `content_snippets` ([`RowSnippet`]).
    pub snippet: Option<ContentSnippet>,
}

/// One answer of `content_snippets`: the snippet of the result row `id`, or `None` when the
/// row is stale, not from the last content search, or its file is not in the content index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowSnippet {
    /// The result id, as given.
    pub id: u64,
    /// Its snippet.
    pub snippet: Option<ContentSnippet>,
}

/// Which part of a query found a result (`ResultItem.matched`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Matched {
    /// The name (and metadata) only.
    #[default]
    Name,
    /// The content only.
    Content,
    /// Both the name part and the content part.
    Both,
}

impl From<athrix_ranking::Matched> for Matched {
    fn from(m: athrix_ranking::Matched) -> Self {
        match m {
            athrix_ranking::Matched::Name => Matched::Name,
            athrix_ranking::Matched::Content => Matched::Content,
            athrix_ranking::Matched::Both => Matched::Both,
        }
    }
}

/// A short piece of a document's original text around the matched words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSnippet {
    /// The fragment (original text, diacritics kept).
    pub text: String,
    /// Matched words inside `text`, as UTF-16 `[start, end)` pairs, sorted and non-overlapping.
    pub highlights: Vec<(usize, usize)>,
}

/// What background content indexing is doing (`ContentStatus.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentState {
    /// Content indexing is turned off in the settings.
    Disabled,
    /// Nothing to do (or not started yet: the name index is not ready).
    Idle,
    /// Extracting and indexing files.
    Running,
    /// Work is waiting but paused (see `pausedBy`).
    Paused,
    /// Shut down (the app is exiting).
    Stopped,
    /// The content index cannot be used (see `error`).
    Error,
}

/// Why content indexing is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseReason {
    /// `content_pause` was called.
    User,
    /// The computer runs on battery.
    Battery,
    /// Other programs keep the CPU busy.
    Busy,
}

/// Payload of `content_status` and of the `content://status` event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentStatus {
    /// Current state.
    pub state: ContentState,
    /// Why it is paused (only when `state` is `paused`).
    pub paused_by: Vec<PauseReason>,
    /// Files finished (indexed, up to date, deleted or failed) since the app started.
    pub processed: u64,
    /// `processed` plus the files still queued or in progress.
    pub total: u64,
    /// Documents in the content index.
    pub indexed: u64,
    /// Files that failed since the app started.
    pub failed: u64,
    /// Documents indexed per second (0 when not running).
    pub docs_per_second: f64,
    /// English message when `state` is `error`.
    pub error: Option<String>,
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
    /// "Did you mean…": set only when a valid query found nothing and a rewrite of it does
    /// find something (see [`crate::suggest`]).
    pub suggestion: Option<SearchSuggestion>,
}

/// How a [`SearchSuggestion`] rewrote the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionKind {
    /// The words retyped on the other keyboard layout (Arabic ⇄ English).
    Layout,
    /// The `fuzzy:` switch put in front of the query.
    Fuzzy,
}

/// A query to offer instead of one that found nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSuggestion {
    /// The full replacement query text.
    pub query: String,
    /// What was changed.
    pub kind: SuggestionKind,
    /// How many results it finds (at least 1, at most the request's `limit`).
    pub count: usize,
}

/// Response of the `preview_entry` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResponse {
    /// Echo of the request's `requestId`.
    pub request_id: u64,
    /// Set when a newer preview request cancelled this one (`preview` is then `None`).
    pub cancelled: bool,
    /// The preview; `None` only when cancelled.
    pub preview: Option<Preview>,
}

/// A preview of one entry: its header and a body that depends on what the entry is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// Name, path and metadata for the pane's header.
    pub entry: PreviewEntry,
    /// What to show.
    pub body: PreviewBody,
}

/// Header of a preview (from the index; the path is built on the Rust side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewEntry {
    /// Original name as on disk.
    pub name: String,
    /// Full path.
    pub path: String,
    /// The entry is a folder.
    pub is_dir: bool,
    /// Size in bytes; `None` for folders.
    pub size: Option<u64>,
    /// Last modification, Unix seconds.
    pub modified: Option<i64>,
    /// Lowercase extension without the dot, `""` if none.
    pub ext: String,
}

/// The body of a [`Preview`], tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PreviewBody {
    /// Plain text (prose, Markdown, logs, CSV...).
    Text {
        /// The first `maxChars` characters, line endings normalized to `\n`.
        text: String,
        /// More text exists than was returned.
        truncated: bool,
        /// Detected encoding (WHATWG name, such as `"UTF-8"` or `"windows-1256"`).
        encoding: Option<String>,
    },
    /// Source code or structured config, with a language hint for coloring.
    Code {
        /// As for [`PreviewBody::Text`].
        text: String,
        /// As for [`PreviewBody::Text`].
        truncated: bool,
        /// As for [`PreviewBody::Text`].
        encoding: Option<String>,
        /// Language hint from the extension (`"rust"`, `"python"`, `"json"`...).
        language: String,
    },
    /// Text extracted from a document (DOCX, XLSX, PPTX, ODF, EPUB, HTML, EML).
    Document {
        /// Paragraphs separated by `\n`.
        text: String,
        /// More text exists than was returned.
        truncated: bool,
        /// Format name (`"docx"`, `"epub"`, `"html"`...).
        format: String,
        /// Document title, if stored.
        title: Option<String>,
        /// Author, if stored.
        author: Option<String>,
    },
    /// An image, served by the `athrix-preview` URI scheme (never as bytes over IPC).
    Image {
        /// URL for `<img src>`, valid for this preview only.
        src: String,
        /// MIME type it is served as.
        mime: String,
    },
    /// A folder: its children from the index.
    Folder {
        /// Number of children (files and folders, not recursive).
        child_count: usize,
        /// The first children, folders first, then by name.
        children: Vec<PreviewChild>,
    },
    /// Nothing to show inside the app; the UI offers "Open".
    Unsupported {
        /// Why: `pdf`, `format`, `tooLarge`, `encrypted`, `corrupt`, `missing`, `unreadable`.
        reason: UnsupportedReason,
        /// English detail.
        message: String,
    },
}

/// One child of a folder preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewChild {
    /// Name as on disk.
    pub name: String,
    /// The child is a folder.
    pub is_dir: bool,
}

/// Why a file has no in-app preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnsupportedReason {
    /// PDF: waits for the isolated PDF worker.
    Pdf,
    /// Unknown or binary format.
    Format,
    /// Over the preview size limits.
    TooLarge,
    /// Password-protected or DRM-encrypted.
    Encrypted,
    /// Malformed document.
    Corrupt,
    /// The file is gone from the disk.
    Missing,
    /// Reading failed (permissions, locked file...).
    Unreadable,
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
            matched: Matched::Both,
            snippet: Some(ContentSnippet {
                text: "ميزانية".into(),
                highlights: vec![(0, 7)],
            }),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["isDir"], false);
        assert_eq!(json["highlights"], serde_json::json!([[0, 1]]));
        assert_eq!(json["matched"], "both");
        assert_eq!(
            json["snippet"],
            serde_json::json!({ "text": "ميزانية", "highlights": [[0, 7]] })
        );

        let content = ContentStatus {
            state: ContentState::Paused,
            paused_by: vec![PauseReason::User, PauseReason::Battery],
            processed: 1,
            total: 2,
            indexed: 3,
            failed: 0,
            docs_per_second: 0.5,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            serde_json::json!({
                "state": "paused", "pausedBy": ["user", "battery"], "processed": 1, "total": 2,
                "indexed": 3, "failed": 0, "docsPerSecond": 0.5, "error": null
            })
        );

        let suggestion = SearchSuggestion {
            query: "التقرير ext:pdf".into(),
            kind: SuggestionKind::Layout,
            count: 2,
        };
        assert_eq!(
            serde_json::to_value(&suggestion).unwrap(),
            serde_json::json!({ "query": "التقرير ext:pdf", "kind": "layout", "count": 2 })
        );
        assert_eq!(
            serde_json::to_value(SuggestionKind::Fuzzy).unwrap(),
            serde_json::json!("fuzzy")
        );
    }
}
