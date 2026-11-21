//! Full-text content index (Tantivy) using the Arabic analyzer from `athrix-text-ar`.
//!
//! # Schema
//!
//! | Field | Type | Why |
//! |---|---|---|
//! | `key` | u64, indexed + stored + fast | [`FileKey`]: hash of path + size + mtime. Stable across rescans, unlike name-index ids, and changes when the file does |
//! | `path` | raw string, indexed + stored | delete / update by path, and the join with the name index |
//! | `size`, `mtime` | u64 / i64, stored + fast | freshness checks and future sorting/filtering |
//! | `body` | text, `athrix_ar` analyzer, freqs + positions, **not stored** | BM25 and phrase queries over the whole text |
//! | `preview` | stored only (lz4 doc store), the first [`IndexOptions::stored_text_limit`] bytes | snippets on the original diacritized text |
//!
//! Storage decision: snippets need the text, and re-extracting a PDF or DOCX for each of the
//! 20 hits on screen costs tens to hundreds of milliseconds each. So the text is stored, but
//! only a prefix (512 KiB by default, compressed with lz4 in pure Rust; zstd would need a C
//! build). Most documents fit entirely. For a longer one, a match past the prefix still
//! matches and ranks, and its snippet falls back to the start of the document; the preview
//! pane (task 4.6) can re-extract that one file on demand. Set the limit to 0 to store
//! nothing and always re-extract.
//!
//! # Writing
//!
//! One writer per index directory (tantivy's lock), single indexing thread, bounded memory
//! ([`IndexOptions::memory_budget`]). All methods take `&self` and [`ContentIndex`] is
//! `Send + Sync`, so a background indexer can share it through an `Arc` with the search side.
//! Changes become visible to [`ContentIndex::search`] after [`ContentIndex::commit`].
//!
//! # Corruption and upgrades
//!
//! [`ContentIndex::open_or_create`] rebuilds an empty index when the existing one is damaged,
//! or was built with another schema or [`athrix_text_ar::ANALYZER_VERSION`], and reports it
//! through [`ContentIndex::rebuilt`]: the caller must then re-index every file. Errors met
//! later (a segment file damaged while running) come back as [`Error::Corrupt`].

mod analyzer;
mod error;
mod query;
mod snippet;

pub use analyzer::{ArabicTokenizer, TOKENIZER_NAME};
pub use error::{Error, Result};
pub use snippet::Snippet;

use athrix_text_ar::{ANALYZER_VERSION, Analyzer};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{
    Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy, TantivyDocument, Term,
};

/// Version of the schema below. Bump it when fields change; old indexes are then rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

/// File written next to the tantivy files: marks the directory as ours and records versions.
const MARKER_FILE: &str = "athrix-content-index.version";

/// Size and modification time of a file, as the scanner reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FileMeta {
    /// Size in bytes.
    pub size: u64,
    /// Modification time in any fixed unit (e.g. NTFS FILETIME ticks or Unix nanoseconds);
    /// only compared for equality, so it must just be consistent.
    pub mtime: i64,
}

/// Stable identity of one version of a file: FNV-1a 64 of path + size + mtime.
///
/// Name-index ids change between scans, so the content index is keyed by this instead. The
/// same file with the same size and mtime always gets the same key; any change gives a new
/// key, so [`ContentIndex::is_current`] tells the indexer whether to re-extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey(pub u64);

impl FileKey {
    /// The key of `path` with `meta`. Paths are compared byte for byte: pass them in one
    /// canonical form (the name index's).
    pub fn of(path: &str, meta: FileMeta) -> FileKey {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(PRIME);
            }
        };
        eat(path.as_bytes());
        eat(&[0xff]); // not valid UTF-8: separates path from the numbers
        eat(&meta.size.to_le_bytes());
        eat(&meta.mtime.to_le_bytes());
        FileKey(h)
    }
}

/// Options of [`ContentIndex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOptions {
    /// Writer memory budget in bytes (default 64 MB; tantivy needs at least 15 MB). The
    /// writer flushes a segment when it is reached.
    pub memory_budget: usize,
    /// How many bytes of each document's text are stored for snippets (default 512 KiB).
    pub stored_text_limit: usize,
    /// Approximate snippet length in bytes (default 300: about 150 Arabic letters).
    pub snippet_bytes: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            memory_budget: 64 << 20,
            stored_text_limit: 512 << 10,
            snippet_bytes: 300,
        }
    }
}

/// Why [`ContentIndex::open_or_create`] started from an empty index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildReason {
    /// The index could not be opened; the message says why.
    Corrupt(String),
    /// It was built with another schema or analyzer version.
    Outdated {
        /// The versions found, as recorded in the marker file.
        found: String,
    },
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The file path, as given to [`ContentIndex::add_or_update`].
    pub path: String,
    /// The indexed version of the file.
    pub key: FileKey,
    /// Its size and mtime when indexed.
    pub meta: FileMeta,
    /// BM25 score.
    pub score: f32,
    /// A fragment of the original text with the matches marked.
    pub snippet: Snippet,
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    key: Field,
    path: Field,
    size: Field,
    mtime: Field,
    body: Field,
    preview: Field,
}

impl Fields {
    fn schema() -> (Schema, Fields) {
        let mut b = Schema::builder();
        let body_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let fields = Fields {
            key: b.add_u64_field("key", INDEXED | STORED | FAST),
            path: b.add_text_field("path", STRING | STORED),
            size: b.add_u64_field("size", STORED | FAST),
            mtime: b.add_i64_field("mtime", STORED | FAST),
            body: b.add_text_field("body", body_opts),
            preview: b.add_text_field("preview", STORED),
        };
        (b.build(), fields)
    }
}

/// The full-text content index. See the [crate docs](crate).
pub struct ContentIndex {
    dir: PathBuf,
    options: IndexOptions,
    fields: Fields,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    rebuilt: Option<RebuildReason>,
}

impl std::fmt::Debug for ContentIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentIndex")
            .field("dir", &self.dir)
            .field("options", &self.options)
            .field("rebuilt", &self.rebuilt)
            .finish_non_exhaustive()
    }
}

fn marker_contents() -> String {
    format!("schema={SCHEMA_VERSION} analyzer={ANALYZER_VERSION}\n")
}

impl ContentIndex {
    /// Opens the index in `dir` with default options, creating it if needed.
    /// See [`open_or_create_with`](Self::open_or_create_with).
    pub fn open_or_create(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_or_create_with(dir, IndexOptions::default())
    }

    /// Opens the index in `dir`, creating the directory and index if needed.
    ///
    /// A damaged or outdated index is wiped and recreated empty; [`rebuilt`](Self::rebuilt)
    /// then says why, and the caller must re-index. A directory holding other files is never
    /// wiped ([`Error::NotAnIndex`]). A writer lock held elsewhere gives [`Error::Locked`].
    pub fn open_or_create_with(dir: impl AsRef<Path>, options: IndexOptions) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let marker = dir.join(MARKER_FILE);
        let has_meta = dir.join("meta.json").exists();
        let found = std::fs::read_to_string(&marker).ok();

        if !has_meta && found.is_none() {
            let mut entries = std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))?;
            if entries.next().is_some() {
                return Err(Error::NotAnIndex(dir.to_owned()));
            }
            return Self::create(dir, options, None);
        }
        let reason = match found {
            Some(v) if v == marker_contents() && has_meta => match Self::open(dir, options) {
                Ok(index) => return Ok(index),
                Err(e @ Error::Corrupt { .. }) => RebuildReason::Corrupt(e.to_string()),
                Err(e) => return Err(e),
            },
            Some(v) if v != marker_contents() => RebuildReason::Outdated {
                found: v.trim().to_owned(),
            },
            _ => RebuildReason::Corrupt("missing meta.json or version marker".to_owned()),
        };
        Self::recreate_with_reason(dir, options, Some(reason))
    }

    /// Deletes the index in `dir` (which must be an Athrix content index or empty) and
    /// creates an empty one. Use it after an [`Error::Corrupt`] at run time; drop the old
    /// [`ContentIndex`] first, it holds the writer lock.
    pub fn recreate(dir: impl AsRef<Path>, options: IndexOptions) -> Result<Self> {
        let dir = dir.as_ref();
        let is_ours = dir.join(MARKER_FILE).exists() || dir.join("meta.json").exists();
        let is_empty = std::fs::read_dir(dir).map_or(true, |mut d| d.next().is_none());
        if !is_ours && !is_empty {
            return Err(Error::NotAnIndex(dir.to_owned()));
        }
        Self::recreate_with_reason(dir, options, None)
    }

    fn recreate_with_reason(
        dir: &Path,
        options: IndexOptions,
        reason: Option<RebuildReason>,
    ) -> Result<Self> {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        Self::create(dir, options, reason)
    }

    fn create(dir: &Path, options: IndexOptions, rebuilt: Option<RebuildReason>) -> Result<Self> {
        let (schema, fields) = Fields::schema();
        let index = Index::builder()
            .schema(schema)
            .settings(IndexSettings::default()) // lz4 doc store
            .create_in_dir(dir)
            .map_err(|e| Error::from_tantivy(dir, e))?;
        std::fs::write(dir.join(MARKER_FILE), marker_contents()).map_err(|e| Error::io(dir, e))?;
        Self::finish(dir, index, fields, options, rebuilt)
    }

    fn open(dir: &Path, options: IndexOptions) -> Result<Self> {
        let index = Index::open_in_dir(dir).map_err(|e| Error::from_tantivy(dir, e))?;
        let (expected, fields) = Fields::schema();
        if index.schema() != expected {
            return Err(Error::Corrupt {
                path: dir.to_owned(),
                message: "schema differs from this version".to_owned(),
            });
        }
        Self::finish(dir, index, fields, options, None)
    }

    fn finish(
        dir: &Path,
        index: Index,
        fields: Fields,
        options: IndexOptions,
        rebuilt: Option<RebuildReason>,
    ) -> Result<Self> {
        index
            .tokenizers()
            .register(TOKENIZER_NAME, ArabicTokenizer::default());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| Error::from_tantivy(dir, e))?;
        let budget = options.memory_budget.clamp(15_000_000, 2_000_000_000);
        let writer = index
            .writer_with_num_threads(1, budget)
            .map_err(|e| Error::from_tantivy(dir, e))?;
        Ok(ContentIndex {
            dir: dir.to_owned(),
            options,
            fields,
            reader,
            writer: Mutex::new(writer),
            rebuilt,
        })
    }

    /// `Some(reason)` if opening found a damaged or outdated index and started empty: every
    /// file must be indexed again.
    pub fn rebuilt(&self) -> Option<&RebuildReason> {
        self.rebuilt.as_ref()
    }

    /// The index directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn err(&self, e: tantivy::TantivyError) -> Error {
        Error::from_tantivy(&self.dir, e)
    }

    fn writer(&self) -> std::sync::MutexGuard<'_, IndexWriter> {
        // A panic while holding the lock leaves the writer usable: tantivy's own state is
        // consistent between calls, and uncommitted work is simply lost on rollback.
        self.writer.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Indexes the text of `path`, replacing any earlier version of it. Visible after
    /// [`commit`](Self::commit).
    pub fn add_or_update(&self, path: &str, meta: FileMeta, text: &str) -> Result<FileKey> {
        let key = FileKey::of(path, meta);
        let f = self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_u64(f.key, key.0);
        doc.add_text(f.path, path);
        doc.add_u64(f.size, meta.size);
        doc.add_i64(f.mtime, meta.mtime);
        doc.add_text(f.body, text);
        if self.options.stored_text_limit > 0 {
            doc.add_text(f.preview, prefix(text, self.options.stored_text_limit));
        }
        let w = self.writer();
        w.delete_term(Term::from_field_text(f.path, path));
        w.add_document(doc).map_err(|e| self.err(e))?;
        Ok(key)
    }

    /// Removes `path` from the index. Visible after [`commit`](Self::commit).
    pub fn delete(&self, path: &str) {
        self.writer()
            .delete_term(Term::from_field_text(self.fields.path, path));
    }

    /// Persists pending changes and makes them visible to searches.
    pub fn commit(&self) -> Result<()> {
        self.writer().commit().map_err(|e| self.err(e))?;
        self.reader.reload().map_err(|e| self.err(e))
    }

    /// Discards changes since the last commit.
    pub fn rollback(&self) -> Result<()> {
        self.writer().rollback().map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Number of committed documents.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Whether the committed index already holds this exact version (path + size + mtime) of
    /// the file, so the indexer can skip extracting it.
    pub fn is_current(&self, path: &str, meta: FileMeta) -> Result<bool> {
        let searcher = self.reader.searcher();
        let q = TermQuery::new(
            Term::from_field_text(self.fields.path, path),
            IndexRecordOption::Basic,
        );
        let top = searcher
            .search(&q, &TopDocs::with_limit(1))
            .map_err(|e| self.err(e))?;
        let Some(&(_, addr)) = top.first() else {
            return Ok(false);
        };
        let doc: TantivyDocument = searcher.doc(addr).map_err(|e| self.err(e))?;
        let stored = doc.get_first(self.fields.key).and_then(|v| v.as_u64());
        Ok(stored == Some(FileKey::of(path, meta).0))
    }

    /// Searches the committed documents. Words are ANDed, `"…"` is a phrase, `-word`
    /// excludes (see the `query` module docs). Returns at most `limit` hits, best first, each
    /// with a snippet of the original text.
    pub fn search(&self, query_text: &str, limit: usize) -> Result<Vec<Hit>> {
        let f = self.fields;
        let analyzer = Analyzer::default();
        let built = query::build(query_text, f.body, analyzer);
        let Some(q) = built.query else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let top = searcher
            .search(&q, &TopDocs::with_limit(limit))
            .map_err(|e| self.err(e))?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).map_err(|e| self.err(e))?;
            let text = |field| doc.get_first(field).and_then(|v| v.as_str()).unwrap_or("");
            let u = |field| doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0);
            let preview = text(f.preview);
            hits.push(Hit {
                path: text(f.path).to_owned(),
                key: FileKey(u(f.key)),
                meta: FileMeta {
                    size: u(f.size),
                    mtime: doc.get_first(f.mtime).and_then(|v| v.as_i64()).unwrap_or(0),
                },
                score,
                snippet: snippet::make(preview, &built.terms, analyzer, self.options.snippet_bytes),
            });
        }
        Ok(hits)
    }
}

/// The longest prefix of `text` of at most `limit` bytes, on a char boundary.
fn prefix(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
