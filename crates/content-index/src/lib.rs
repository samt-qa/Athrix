//! Full-text content index (Tantivy) using the Arabic analyzer from `athrix-text-ar`.
//!
//! # Schema
//!
//! | Field | Type | Why |
//! |---|---|---|
//! | `key` | u64, indexed + stored + fast | [`FileKey`]: hash of path + size + mtime. Stable across rescans, unlike name-index ids, and changes when the file does |
//! | `path` | raw string, indexed + stored + fast | the path as given; the fast column lets [`ContentIndex::paths`] list every document without touching the doc store |
//! | `pkey` | raw string, indexed | [`path_key`] of the path: identity for update / delete and prefix ranges |
//! | `phash` | u64, fast | 64-bit hash of `pkey`: [`PathFilter::paths`] sets are matched against this column |
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
//! # Searching and snippets
//!
//! Searches return [`Hit`]s read from the fast columns only (path, key, size, mtime and the
//! score): fetching thousands of hits for ranking or an AND with the name index costs
//! milliseconds. A snippet needs the stored text (decompressed, then tokenized until the
//! first match), so it is built separately, with [`ContentIndex::snippets`], for the few hits
//! on screen.
//!
//! # Writing
//!
//! One writer per index directory (tantivy's lock), single indexing thread, bounded memory
//! ([`IndexOptions::memory_budget`]). All methods take `&self` and [`ContentIndex`] is
//! `Send + Sync`, so a background indexer can share it through an `Arc` with the search side.
//! Changes become visible to [`ContentIndex::search`] after [`ContentIndex::commit`].
//!
//! # Paths, case and separators
//!
//! Identity is the [`path_key`] of the path:
//!
//! - **Windows** (`cfg(windows)`): `\` and `/` are the same separator and case is ignored
//!   ([`str::to_lowercase`]), as on NTFS: `C:\Docs\A.txt` and `c:/docs/a.txt` are one
//!   document, and adding one replaces the other. Arabic has no case; Arabic paths only get
//!   the separator change.
//! - **Elsewhere**: byte for byte, case-sensitive, and `\` is an ordinary character, as on
//!   Linux and macOS file systems.
//!
//! The stored `path` (returned in [`Hit::path`] and by [`ContentIndex::paths`]) keeps the form
//! last given. [`FileKey`] hashes that exact form, so a case-only rename makes
//! [`ContentIndex::is_current`] false once and the file is re-indexed under its new spelling.
//!
//! Prefixes ([`ContentIndex::delete_prefix`], [`PathFilter::prefix`]) are folders, not string
//! prefixes: `C:/docs` covers `C:/docs` itself and everything under `C:/docs/`, never
//! `C:/docs2`. A prefix ending with the separator (`C:/`, `/`) covers what is under it. An
//! empty prefix covers nothing.
//!
//! # Keeping the index in step with the disk
//!
//! - [`ContentIndex::delete_prefix`]: a folder was deleted or renamed (re-add the new paths).
//! - [`ContentIndex::retain`]: at startup, drop every document whose file the name index no
//!   longer has (deleted while Athrix was not running). It streams over
//!   [`ContentIndex::paths`] and deletes in batches.
//!
//! # Scoped search, paging and totals
//!
//! [`ContentIndex::search_with`] takes [`SearchOptions`]: `limit` + `offset` for pages, an
//! optional [`PathFilter`] (a set of paths from the name side of an AND query, or folders /
//! drives) applied inside tantivy, and `count_total` for the number of matches in the same
//! pass. [`ContentIndex::count`] / [`ContentIndex::count_within`] only count.
//!
//! # Corruption and upgrades
//!
//! [`ContentIndex::open_or_create`] rebuilds an empty index when the existing one is damaged,
//! or was built with another schema or [`athrix_text_ar::ANALYZER_VERSION`], and reports it
//! through [`ContentIndex::rebuilt`]: the caller must then re-index every file. Errors met
//! later (a segment file damaged while running) come back as [`Error::Corrupt`].
//!
//! Schema version 2 added the `pkey` and `phash` fields and the `path` fast column: an index from version 1
//! is rebuilt on first open ([`RebuildReason::Outdated`]) and its files re-indexed.

mod analyzer;
mod error;
mod query;
mod scope;
mod snippet;

pub use analyzer::{ArabicTokenizer, TOKENIZER_NAME};
pub use error::{Error, Result};
pub use scope::{PathFilter, path_key};
pub use snippet::Snippet;

use athrix_text_ar::{ANALYZER_VERSION, Analyzer};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{
    DocId, Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy, TantivyDocument, Term,
};

/// Version of the schema below. Bump it when fields change; old indexes are then rebuilt.
pub const SCHEMA_VERSION: u32 = 2;

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

/// One search result: identity and score only. Its snippet is built separately, for the hits
/// actually shown, by [`ContentIndex::snippets`].
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
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    key: Field,
    path: Field,
    pkey: Field,
    phash: Field,
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
            path: b.add_text_field("path", STRING | STORED | FAST),
            pkey: b.add_text_field("pkey", STRING),
            phash: b.add_u64_field(scope::PATH_HASH_FIELD, FAST),
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
        let pkey = path_key(path);
        doc.add_text(f.pkey, &pkey);
        doc.add_u64(f.phash, scope::path_hash(&pkey));
        doc.add_u64(f.size, meta.size);
        doc.add_i64(f.mtime, meta.mtime);
        doc.add_text(f.body, text);
        if self.options.stored_text_limit > 0 {
            doc.add_text(f.preview, prefix(text, self.options.stored_text_limit));
        }
        let w = self.writer();
        w.delete_term(Term::from_field_text(f.pkey, &pkey));
        w.add_document(doc).map_err(|e| self.err(e))?;
        Ok(key)
    }

    /// Removes `path` (compared by [`path_key`]) from the index. Visible after
    /// [`commit`](Self::commit).
    pub fn delete(&self, path: &str) {
        self.writer()
            .delete_term(Term::from_field_text(self.fields.pkey, &path_key(path)));
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
            Term::from_field_text(self.fields.pkey, &path_key(path)),
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
    /// excludes (see the `query` module docs). Returns at most `limit` hits, best first,
    /// without snippets (see [`snippets`](Self::snippets)).
    pub fn search(&self, query_text: &str, limit: usize) -> Result<Vec<Hit>> {
        let opts = SearchOptions {
            limit,
            ..SearchOptions::default()
        };
        Ok(self.search_with(query_text, &opts)?.hits)
    }

    /// [`search`](Self::search) restricted to the files `filter` lets through: the best
    /// `limit` hits *within* the scope.
    pub fn search_within(
        &self,
        query_text: &str,
        limit: usize,
        filter: &PathFilter,
    ) -> Result<Vec<Hit>> {
        let opts = SearchOptions {
            limit,
            filter: Some(filter),
            ..SearchOptions::default()
        };
        Ok(self.search_with(query_text, &opts)?.hits)
    }

    /// Number of committed documents matching the query, without ranking or snippets.
    pub fn count(&self, query_text: &str) -> Result<u64> {
        self.count_impl(query_text, None)
    }

    /// Number of committed documents matching the query within `filter`.
    pub fn count_within(&self, query_text: &str, filter: &PathFilter) -> Result<u64> {
        self.count_impl(query_text, Some(filter))
    }

    fn count_impl(&self, query_text: &str, filter: Option<&PathFilter>) -> Result<u64> {
        let opts = SearchOptions {
            limit: 0,
            filter,
            count_total: true,
            ..SearchOptions::default()
        };
        Ok(self.search_with(query_text, &opts)?.total.unwrap_or(0))
    }

    /// The general search: a page (`offset`, `limit`) of the best hits, optionally scoped by
    /// a [`PathFilter`] and with the total number of matches. Pages are stable as long as no
    /// commit happens in between.
    pub fn search_with(&self, query_text: &str, opts: &SearchOptions<'_>) -> Result<SearchPage> {
        let f = self.fields;
        let empty = SearchPage {
            hits: Vec::new(),
            total: opts.count_total.then_some(0),
        };
        let analyzer = Analyzer::default();
        let built = query::build(query_text, f.body, analyzer);
        let Some(mut q) = built.query else {
            return Ok(empty);
        };
        if let Some(filter) = opts.filter {
            let Some(scope) = filter.query() else {
                return Ok(empty);
            };
            // Score 0: the scope must not change BM25 ranking.
            let scope: Box<dyn Query> = Box::new(ConstScoreQuery::new(scope, 0.0));
            q = Box::new(BooleanQuery::new(vec![
                (Occur::Must, q),
                (Occur::Must, scope),
            ]));
        }
        let searcher = self.reader.searcher();
        let (top, total) = match (opts.limit, opts.count_total) {
            (0, false) => return Ok(empty),
            (0, true) => {
                let n = searcher.search(&q, &Count).map_err(|e| self.err(e))?;
                (Vec::new(), Some(n as u64))
            }
            (limit, false) => {
                let top = TopDocs::with_limit(limit).and_offset(opts.offset);
                (searcher.search(&q, &top).map_err(|e| self.err(e))?, None)
            }
            (limit, true) => {
                let top = TopDocs::with_limit(limit).and_offset(opts.offset);
                let (top, n) = searcher
                    .search(&q, &(top, Count))
                    .map_err(|e| self.err(e))?;
                (top, Some(n as u64))
            }
        };
        // Identity from the fast columns: the doc store (lz4 blocks holding up to
        // `stored_text_limit` of text per document) is never read here.
        let mut columns: Vec<Option<HitColumns>> = Vec::new();
        columns.resize_with(searcher.segment_readers().len(), || None);
        let mut hits = Vec::with_capacity(top.len());
        let mut path = String::new();
        for (score, addr) in top {
            let seg = addr.segment_ord as usize;
            let cols = match &mut columns[seg] {
                Some(c) => c,
                slot => slot.insert(
                    HitColumns::open(searcher.segment_reader(addr.segment_ord))
                        .map_err(|e| self.err(e))?,
                ),
            };
            path.clear();
            if let Some(paths) = &cols.paths {
                if let Some(ord) = paths.ords().first(addr.doc_id) {
                    paths
                        .ord_to_str(ord, &mut path)
                        .map_err(|e| self.err(e.into()))?;
                }
            }
            let doc = addr.doc_id;
            hits.push(Hit {
                path: path.clone(),
                key: FileKey(cols.keys.first(doc).unwrap_or(0)),
                meta: FileMeta {
                    size: cols.sizes.first(doc).unwrap_or(0),
                    mtime: cols.mtimes.first(doc).unwrap_or(0),
                },
                score,
            });
        }
        Ok(SearchPage { hits, total })
    }

    /// Snippets of the committed documents at `paths` (compared by [`path_key`]) for
    /// `query_text` (the same syntax as [`search`](Self::search)), in the order of `paths`;
    /// `None` for a path the index does not hold.
    ///
    /// A snippet is the fragment of the stored text around the best cluster of matches, with
    /// highlights on the original (diacritized) words. Each one reads the document's stored
    /// text and tokenizes it from the start until a match is found, so build snippets only
    /// for the hits shown, not for every hit of a search. Without a match in the stored text
    /// (past the stored prefix, or a query with only excluded words) the snippet is the start
    /// of the text with no highlights. The documents are read on a few threads when there
    /// are many.
    pub fn snippets<S: AsRef<str> + Sync>(
        &self,
        query_text: &str,
        paths: &[S],
    ) -> Result<Vec<Option<Snippet>>> {
        let analyzer = Analyzer::default();
        let terms = query::build(query_text, self.fields.body, analyzer).terms;
        let searcher = self.reader.searcher();
        let one = |path: &S| self.snippet_of(&searcher, &terms, analyzer, path.as_ref());
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(paths.len().div_ceil(SNIPPETS_PER_THREAD))
            .min(MAX_SNIPPET_THREADS);
        if threads <= 1 {
            return paths.iter().map(one).collect();
        }
        let chunk = paths.len().div_ceil(threads);
        std::thread::scope(|s| {
            let workers: Vec<_> = paths
                .chunks(chunk)
                .map(|part| s.spawn(move || part.iter().map(one).collect::<Result<Vec<_>>>()))
                .collect();
            let mut out = Vec::with_capacity(paths.len());
            for w in workers {
                let part = w.join().unwrap_or_else(|p| std::panic::resume_unwind(p))?;
                out.extend(part);
            }
            Ok(out)
        })
    }

    fn snippet_of(
        &self,
        searcher: &tantivy::Searcher,
        terms: &std::collections::BTreeSet<String>,
        analyzer: Analyzer,
        path: &str,
    ) -> Result<Option<Snippet>> {
        let q = TermQuery::new(
            Term::from_field_text(self.fields.pkey, &path_key(path)),
            IndexRecordOption::Basic,
        );
        let top = searcher
            .search(&q, &TopDocs::with_limit(1))
            .map_err(|e| self.err(e))?;
        let Some(&(_, addr)) = top.first() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(addr).map_err(|e| self.err(e))?;
        let text = doc
            .get_first(self.fields.preview)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(Some(snippet::make(
            text,
            terms,
            analyzer,
            self.options.snippet_bytes,
        )))
    }

    /// Calls `f` with the path (as last given to [`add_or_update`](Self::add_or_update)) and
    /// meta of every committed document; returns how many there were.
    ///
    /// Reads only the fast columns (path dictionary, size, mtime), never the stored text:
    /// memory is 8 bytes per document of the largest segment, whatever the index size. Order:
    /// sorted by path within each segment, segments one after another (not globally sorted).
    /// Runs on a snapshot: commits made meanwhile are not seen.
    pub fn paths(&self, mut f: impl FnMut(&str, FileMeta)) -> Result<u64> {
        let searcher = self.reader.searcher();
        let mut count = 0u64;
        let mut pairs: Vec<(u32, DocId)> = Vec::new();
        for segment in searcher.segment_readers() {
            let ff = segment.fast_fields();
            let Some(paths) = ff.str("path").map_err(|e| self.err(e))? else {
                continue;
            };
            let sizes = ff.u64("size").map_err(|e| self.err(e))?;
            let mtimes = ff.i64("mtime").map_err(|e| self.err(e))?;
            pairs.clear();
            pairs.extend(segment.doc_ids_alive().filter_map(|doc| {
                // Ordinals are < number of docs in the segment, so they fit in u32.
                Some((u32::try_from(paths.ords().first(doc)?).ok()?, doc))
            }));
            pairs.sort_unstable();
            let mut next = 0;
            let complete = paths
                .dictionary()
                .sorted_ords_to_term_cb(pairs.iter().map(|&(ord, _)| u64::from(ord)), |bytes| {
                    let doc = pairs[next].1;
                    next += 1;
                    let path = std::str::from_utf8(bytes)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    let meta = FileMeta {
                        size: sizes.first(doc).unwrap_or(0),
                        mtime: mtimes.first(doc).unwrap_or(0),
                    };
                    f(path, meta);
                    Ok(())
                })
                .map_err(|e| self.err(e.into()))?;
            if !complete {
                return Err(Error::Corrupt {
                    path: self.dir.clone(),
                    message: "path dictionary is missing ordinals".to_owned(),
                });
            }
            count += next as u64;
        }
        Ok(count)
    }

    /// Deletes every document for which `keep(path, meta)` is false, e.g. files the name
    /// index no longer has. Streams over [`paths`](Self::paths) and queues the deletes in
    /// batches; returns how many documents were removed. Visible after
    /// [`commit`](Self::commit).
    pub fn retain(&self, mut keep: impl FnMut(&str, FileMeta) -> bool) -> Result<u64> {
        const BATCH: usize = 4096;
        let field = self.fields.path;
        let mut batch: Vec<Term> = Vec::new();
        let mut removed = 0u64;
        let flush = |batch: &mut Vec<Term>| {
            if batch.is_empty() {
                return;
            }
            let w = self.writer();
            for term in batch.drain(..) {
                w.delete_term(term);
            }
        };
        self.paths(|path, meta| {
            if !keep(path, meta) {
                // By the exact stored path: the document `keep` judged, nothing else.
                batch.push(Term::from_field_text(field, path));
                removed += 1;
                if batch.len() >= BATCH {
                    flush(&mut batch);
                }
            }
        })?;
        flush(&mut batch);
        Ok(removed)
    }

    /// Deletes every document in the folder `prefix` or below it (compared by [`path_key`];
    /// `C:/docs` covers `C:/docs/…` but not `C:/docs2`, see [`PathFilter`]). An empty prefix
    /// deletes nothing. Returns the number of committed documents removed; documents added
    /// under the prefix since the last commit are removed too. Visible after
    /// [`commit`](Self::commit).
    pub fn delete_prefix(&self, prefix: &str) -> Result<u64> {
        let Some(q) = scope::prefix_query(self.fields.pkey, &path_key(prefix)) else {
            return Ok(0);
        };
        let n = self
            .reader
            .searcher()
            .search(&q, &Count)
            .map_err(|e| self.err(e))?;
        self.writer().delete_query(q).map_err(|e| self.err(e))?;
        Ok(n as u64)
    }
}

/// [`ContentIndex::snippets`] uses one more thread per this many paths...
const SNIPPETS_PER_THREAD: usize = 4;
/// ...up to this many threads.
const MAX_SNIPPET_THREADS: usize = 8;

/// The fast columns a [`Hit`] is read from, opened once per segment and search.
struct HitColumns {
    paths: Option<tantivy::columnar::StrColumn>,
    keys: tantivy::columnar::Column<u64>,
    sizes: tantivy::columnar::Column<u64>,
    mtimes: tantivy::columnar::Column<i64>,
}

impl HitColumns {
    fn open(segment: &tantivy::SegmentReader) -> tantivy::Result<Self> {
        let ff = segment.fast_fields();
        Ok(HitColumns {
            paths: ff.str("path")?,
            keys: ff.u64("key")?,
            sizes: ff.u64("size")?,
            mtimes: ff.i64("mtime")?,
        })
    }
}

/// Options of [`ContentIndex::search_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions<'a> {
    /// Maximum number of hits (0: none, e.g. to only count).
    pub limit: usize,
    /// Hits to skip, for paging: page `n` is `offset = n * limit`.
    pub offset: usize,
    /// Only documents this filter lets through.
    pub filter: Option<&'a PathFilter>,
    /// Also count every match ([`SearchPage::total`]) in the same pass.
    pub count_total: bool,
}

/// Result of [`ContentIndex::search_with`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchPage {
    /// The requested page of hits, best first.
    pub hits: Vec<Hit>,
    /// Number of matching documents, when [`SearchOptions::count_total`] was set.
    pub total: Option<u64>,
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
