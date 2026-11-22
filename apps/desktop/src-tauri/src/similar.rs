//! Similar images (perceptual hashes, [`athrix_imagehash`]): the `similar:` / `مشابه:` filter
//! and the `find_similar` command ("find similar images" on one result).
//!
//! - `similar:` without a value groups near-identical pictures among the images the rest of
//!   the query finds (scope included): groups are adjacent in the results, biggest first, each
//!   row carrying `similarGroup` and `similarity` (to the group's first image).
//! - `similar:photo.jpg` (a file name) ranks the images the rest of the query finds by
//!   likeness to that picture, like `find_similar` does for a result id.
//!
//! Only images are candidates (jpg/jpeg/png/webp/gif/bmp/tiff, see
//! [`athrix_imagehash::IMAGE_EXTENSIONS`]; HEIC is skipped: no pure-Rust decoder). Hashes are
//! cached in `<app_data>/hashes/phash.sqlite3` (path key + size + modification time), computed
//! in parallel on the blocking pool, cancelled by a newer search, and bounded by
//! [`TIME_BUDGET`]: past it the answer lists what was compared so far with
//! `similarIncomplete: true`, and searching again continues where the cache left off.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use athrix_fs_index::EntryId;
use athrix_imagehash::{
    Candidate, GROUP_MAX_DISTANCE, HashCache, HashRun, IMAGE_EXTENSIONS, ImageHash,
    REFERENCE_MAX_DISTANCE, group_similar, hash_all, hash_threads, is_image_extension,
    similarity_percent,
};
use athrix_query::{Expr, ExprKind, FilterName, FilterValue, PlanOptions, Query as AstQuery};
use tauri::{AppHandle, Manager, State};

use crate::commands::{MAX_LIMIT, resolve_entry};
use crate::indexing::IndexView;
use crate::model::{QueryError, ResultItem, SearchResponse, SimilarSummary};
use crate::query::{CompiledQuery, compile_ast, compile_query, utf16_offset};
use crate::results::{SearchFailure, SearchScope, build_item, build_row, search_ids};
use crate::roots::path_key;
use crate::state::AppState;

/// Folder under the app data dir holding the hash caches.
pub const CACHE_DIR: &str = "hashes";

/// Time budget of one similar-image search; past it the answer is `similarIncomplete`.
pub const TIME_BUDGET: Duration = Duration::from_secs(8);

/// Rows `find_similar` returns when the UI gives no `limit`.
pub const DEFAULT_LIMIT: usize = 1000;

/// Where the cache lives (set at start-up) and the cache once opened.
static CACHE_PATH: OnceLock<PathBuf> = OnceLock::new();
static CACHE: Mutex<Option<Arc<HashCache>>> = Mutex::new(None);

/// Remembers `<app_data>/hashes/phash.sqlite3` as the cache file; it is opened on the first
/// similar-image search, never at start-up. Without an app data folder hashes are not cached.
pub fn configure(app: &AppHandle) {
    match app.path().app_data_dir() {
        Ok(dir) => {
            let _ = CACHE_PATH.set(dir.join(CACHE_DIR).join(athrix_imagehash::CACHE_FILE));
        }
        Err(e) => tracing::warn!(error = %e, "no app data folder; image hashes not cached"),
    }
}

/// The hash cache, opened on first use (`None` if unconfigured or it cannot be opened).
fn cache() -> Option<Arc<HashCache>> {
    let path = CACHE_PATH.get()?;
    let mut slot = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        match HashCache::open(path) {
            Ok(c) => *slot = Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "cannot open the image hash cache");
                return None;
            }
        }
    }
    slot.clone()
}

/// Results of a similar-image search.
#[derive(Debug)]
pub struct SimilarHits {
    /// Rows, at most `limit`.
    pub items: Vec<ResultItem>,
    /// More rows exist than `limit`.
    pub truncated: bool,
    /// Totals for the status line.
    pub summary: SimilarSummary,
    /// The time budget ran out before every image was compared.
    pub incomplete: bool,
}

impl SimilarHits {
    /// Copies the hits into a `search` / `find_similar` response.
    pub fn fill(self, response: &mut SearchResponse) {
        response.items = self.items;
        response.truncated = self.truncated;
        response.similar = Some(self.summary);
        response.similar_incomplete = self.incomplete;
    }
}

/// Why a similar-image search gave no hits.
#[derive(Debug)]
pub enum SimilarFailure {
    /// The query is invalid or unsupported, or the reference image cannot be used.
    Invalid(QueryError),
    /// Cancelled or failed like a plain search.
    Search(SearchFailure),
}

/// A query using `similar:`, split by [`split_similar`].
#[derive(Debug, Clone)]
pub struct SimilarSplit {
    /// The query without the filter (`root: None` = every image).
    pub rest: AstQuery,
    /// `similar:name`: the reference image's name and the filter (for error spans).
    pub reference: Option<(String, Expr)>,
}

/// If `ast` uses `similar:`, the query without it; `Ok(None)` if it does not. The filter must
/// be at the top level (alone or ANDed with the rest, not under `|`, `!`/`-` or `< >`), once,
/// and not together with `dupe:`; anything else is an error on the filter, of kind
/// `SimilarNotTopLevel`, `SimilarRepeated` or `SimilarWithDupe`.
pub fn split_similar(text: &str, ast: &AstQuery) -> Result<Option<SimilarSplit>, QueryError> {
    let Some(root) = &ast.root else {
        return Ok(None);
    };
    let top: Vec<&Expr> = match &root.kind {
        ExprKind::And(xs) => xs.iter().collect(),
        _ => vec![root],
    };
    let (filters, rest): (Vec<&Expr>, Vec<&Expr>) = top
        .into_iter()
        .partition(|e| filter_name(e) == Some(FilterName::Similar));
    if let Some(bad) = rest.iter().find_map(|e| nested(e, FilterName::Similar)) {
        return Err(error(
            text,
            bad,
            "SimilarNotTopLevel",
            "`similar:` applies to the whole query: use it outside `|`, `!` and `< >`",
        ));
    }
    let Some(&first) = filters.first() else {
        return Ok(None);
    };
    if let Some(&again) = filters.get(1) {
        return Err(error(
            text,
            again,
            "SimilarRepeated",
            "use `similar:` only once in a query",
        ));
    }
    if rest
        .iter()
        .any(|e| filter_name(e) == Some(FilterName::Dupe))
    {
        return Err(error(
            text,
            first,
            "SimilarWithDupe",
            "`similar:` and `dupe:` cannot be combined: use one of them",
        ));
    }
    let reference = match &first.kind {
        ExprKind::Filter(f) => match &f.value {
            FilterValue::Text(name) if !name.trim().is_empty() => {
                Some((name.trim().to_owned(), first.clone()))
            }
            _ => None,
        },
        _ => None,
    };
    let root = match rest.len() {
        0 => None,
        1 => Some(rest[0].clone()),
        _ => Some(Expr {
            span: root.span,
            kind: ExprKind::And(rest.into_iter().cloned().collect()),
        }),
    };
    Ok(Some(SimilarSplit {
        rest: AstQuery { root },
        reference,
    }))
}

fn filter_name(e: &Expr) -> Option<FilterName> {
    match &e.kind {
        ExprKind::Filter(f) => Some(f.name),
        _ => None,
    }
}

/// A `name` filter somewhere inside `e`.
fn nested(e: &Expr, name: FilterName) -> Option<&Expr> {
    match &e.kind {
        ExprKind::Filter(f) if f.name == name => Some(e),
        ExprKind::Term(_) | ExprKind::Filter(_) => None,
        ExprKind::Not(x) | ExprKind::Group(x) => nested(x, name),
        ExprKind::And(xs) | ExprKind::Or(xs) => xs.iter().find_map(|x| nested(x, name)),
    }
}

fn error(text: &str, e: &Expr, kind: &str, message: &str) -> QueryError {
    QueryError {
        message: message.to_owned(),
        kind: kind.to_owned(),
        start: utf16_offset(text, e.span.start),
        end: utf16_offset(text, e.span.end),
        suggestion: None,
    }
}

/// The query matching every image by extension.
fn image_query() -> CompiledQuery {
    compile_query(&format!("ext:{}", IMAGE_EXTENSIONS.join(";")))
        .expect("the image extension query compiles")
}

/// Runs a `similar:` search: the rest of the query picks candidate images under `scope`, then
/// either groups them ([`group_similar`]) or, for `similar:name`, ranks them against that image.
pub fn similar_search(
    view: &IndexView,
    text: &str,
    split: &SimilarSplit,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<SimilarHits, SimilarFailure> {
    // Content filters need the content index, which knows nothing of pictures.
    if let Ok(plan) = athrix_query::plan(&split.rest, &PlanOptions::default()) {
        if plan.uses_content() {
            return Err(SimilarFailure::Invalid(QueryError {
                message: "`similar:` cannot be combined with `content:` yet".to_owned(),
                kind: "SimilarWithContent".to_owned(),
                start: 0,
                end: text.encode_utf16().count(),
                suggestion: None,
            }));
        }
    }
    let query = match &split.rest.root {
        Some(_) => compile_ast(text, &split.rest).map_err(SimilarFailure::Invalid)?,
        None => image_query(),
    };
    let ids = image_ids(view, &query, scope, cancel)?;
    match &split.reference {
        None => group_search(view, &query, ids, limit, cancel),
        Some((name, filter)) => {
            let Some(reference) = find_by_name(view, name, cancel)? else {
                return Err(SimilarFailure::Invalid(error(
                    text,
                    filter,
                    "SimilarReferenceNotFound",
                    "no indexed image has this name",
                )));
            };
            rank_against(view, reference, ids, limit, Some(&query), cancel)
        }
    }
}

/// Every live image file (by extension, not empty) `query` finds under `scope`, in index order.
fn image_ids(
    view: &IndexView,
    query: &CompiledQuery,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SimilarFailure> {
    let all = view.id_bound().saturating_add(1);
    let ids = search_ids(view, &query.query, all, scope, cancel).map_err(SimilarFailure::Search)?;
    Ok(ids.into_iter().filter(|&id| is_image(view, id)).collect())
}

fn is_image(view: &IndexView, id: EntryId) -> bool {
    let meta = view.meta(id);
    !meta.is_dir
        && meta.size > 0
        && std::path::Path::new(view.name(id))
            .extension()
            .is_some_and(|e| is_image_extension(&e.to_string_lossy()))
}

/// The first indexed image named `name` (any case), or else whose name contains it.
fn find_by_name(
    view: &IndexView,
    name: &str,
    cancel: &AtomicBool,
) -> Result<Option<EntryId>, SimilarFailure> {
    let images = image_ids(view, &image_query(), None, cancel)?;
    let wanted = name.to_lowercase();
    let exact = images
        .iter()
        .copied()
        .find(|&id| view.name(id).to_lowercase() == wanted);
    Ok(exact.or_else(|| {
        images
            .iter()
            .copied()
            .find(|&id| view.name(id).to_lowercase().contains(&wanted))
    }))
}

/// The hash candidates of `ids` (path key, path, size, modification time).
fn candidates(
    view: &IndexView,
    ids: &[EntryId],
    cancel: &AtomicBool,
) -> Result<Vec<Candidate>, SimilarFailure> {
    let mut out = Vec::with_capacity(ids.len());
    for (n, &id) in ids.iter().enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SimilarFailure::Search(SearchFailure::Cancelled));
        }
        let meta = view.meta(id);
        let path = view.path(id);
        out.push(Candidate {
            key: path_key(&path),
            path,
            size: meta.size,
            modified: meta.modified.unwrap_or(0),
        });
    }
    Ok(out)
}

/// Hashes `ids` (cache first) within [`TIME_BUDGET`].
fn hash_ids(
    view: &IndexView,
    ids: &[EntryId],
    cancel: &AtomicBool,
) -> Result<HashRun, SimilarFailure> {
    let list = candidates(view, ids, cancel)?;
    let cache = cache();
    let deadline = Instant::now() + TIME_BUDGET;
    hash_all(cache.as_deref(), &list, cancel, deadline, hash_threads())
        .ok_or(SimilarFailure::Search(SearchFailure::Cancelled))
}

fn summary(run: &HashRun, groups: usize, found: usize) -> SimilarSummary {
    SimilarSummary {
        images: run.hashes.len(),
        checked: run.checked,
        skipped: run.checked - run.hashes.iter().filter(|h| h.is_some()).count(),
        groups,
        found,
    }
}

/// `similar:` without a reference: groups of near-identical pictures among `ids`.
fn group_search(
    view: &IndexView,
    query: &CompiledQuery,
    ids: Vec<EntryId>,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<SimilarHits, SimilarFailure> {
    let run = hash_ids(view, &ids, cancel)?;
    let groups = group_similar(&run.hashes, GROUP_MAX_DISTANCE);
    let found = groups.iter().map(|g| g.members.len()).sum();
    let generation = view.generation();
    let mut items = Vec::new();
    let mut truncated = false;
    'groups: for (n, group) in groups.iter().enumerate() {
        let group_id = u32::try_from(n + 1).unwrap_or(u32::MAX);
        for &(m, distance) in &group.members {
            if items.len() >= limit {
                truncated = true;
                break 'groups;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(SimilarFailure::Search(SearchFailure::Cancelled));
            }
            let mut item = build_item(view, generation, ids[m], query);
            item.similar_group = Some(group_id);
            item.similarity = Some(similarity_percent(distance));
            items.push(item);
        }
    }
    Ok(SimilarHits {
        items,
        truncated,
        summary: summary(&run, groups.len(), found),
        incomplete: run.incomplete,
    })
}

/// Ranks `ids` by likeness to the image `reference` (itself left out): the reference's folder
/// is hashed first, so an incomplete answer still covers its neighbours. Rows within
/// [`REFERENCE_MAX_DISTANCE`], closest first, with `similarity`.
fn rank_against(
    view: &IndexView,
    reference: EntryId,
    mut ids: Vec<EntryId>,
    limit: usize,
    query: Option<&CompiledQuery>,
    cancel: &AtomicBool,
) -> Result<SimilarHits, SimilarFailure> {
    ids.retain(|&id| id != reference);
    let folder = view.parent(reference);
    // Stable: same folder first, index order otherwise.
    ids.sort_by_key(|&id| view.parent(id) != folder);
    let reference_run = hash_ids(view, &[reference], cancel)?;
    let Some(target) = reference_run.hashes.first().copied().flatten() else {
        return Err(SimilarFailure::Invalid(QueryError {
            message: "this image cannot be read or decoded".to_owned(),
            kind: "SimilarReferenceUnreadable".to_owned(),
            start: 0,
            end: 0,
            suggestion: None,
        }));
    };
    let run = hash_ids(view, &ids, cancel)?;
    let mut matches: Vec<(u32, usize)> = run
        .hashes
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let d = h.map(|h: ImageHash| h.distance(target))?;
            (d <= REFERENCE_MAX_DISTANCE).then_some((d, i))
        })
        .collect();
    matches.sort_unstable();
    let found = matches.len();
    let truncated = found > limit;
    matches.truncate(limit);
    let generation = view.generation();
    let mut items = Vec::with_capacity(matches.len());
    for (d, i) in matches {
        if cancel.load(Ordering::Relaxed) {
            return Err(SimilarFailure::Search(SearchFailure::Cancelled));
        }
        let mut item = match query {
            Some(q) => build_item(view, generation, ids[i], q),
            None => build_row(view, generation, ids[i], |_| Vec::new()),
        };
        item.similarity = Some(similarity_percent(d));
        items.push(item);
    }
    Ok(SimilarHits {
        items,
        truncated,
        summary: summary(&run, 0, found),
        incomplete: run.incomplete,
    })
}

/// The images most like the result `id` (an image), closest first: every indexed image (only
/// under `scope` if given) within [`REFERENCE_MAX_DISTANCE`] bits, the image itself left out,
/// each row with `similarity`; `limit` rows at most (default [`DEFAULT_LIMIT`]). Like `search`
/// it cancels the previous search (and a newer one cancels it), runs on the blocking pool and
/// stops at [`TIME_BUDGET`] with `similarIncomplete: true`. Rejects (English) a stale id, a
/// folder or a file that is not a supported image, and an image that cannot be decoded.
#[tauri::command]
pub async fn find_similar(
    state: State<'_, AppState>,
    id: u64,
    request_id: u64,
    scope: Option<String>,
    limit: Option<u32>,
) -> Result<SearchResponse, String> {
    let started = Instant::now();
    let scope = SearchScope::parse(scope.as_deref())?;
    let cancel = state.begin_search();
    let (view, entry) = resolve_entry(&state.indexer, id)?;
    if !is_image(&view, entry) {
        return Err("only images (jpg, png, webp, gif, bmp, tiff) can be compared".to_owned());
    }
    let limit = limit.map_or(DEFAULT_LIMIT, |l| l as usize).min(MAX_LIMIT);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let ids = image_ids(&view, &image_query(), scope.as_ref(), &cancel)?;
        rank_against(&view, entry, ids, limit, None, &cancel)
    })
    .await
    .map_err(|e| format!("similar-image task failed: {e}"))?;
    let mut response = SearchResponse {
        request_id,
        items: Vec::new(),
        truncated: false,
        elapsed_ms: 0.0,
        error: None,
        cancelled: false,
        dupe: None,
        similar: None,
        similar_incomplete: false,
    };
    match outcome {
        Ok(hits) => hits.fill(&mut response),
        Err(SimilarFailure::Invalid(e)) if e.kind == "SimilarReferenceUnreadable" => {
            return Err(e.message);
        }
        Err(SimilarFailure::Invalid(e)) => response.error = Some(e),
        Err(SimilarFailure::Search(SearchFailure::Cancelled)) => response.cancelled = true,
        Err(SimilarFailure::Search(SearchFailure::Failed(e))) => return Err(e),
    }
    response.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::walk_options;
    use athrix_fs_index::{LiveIndex, walk};
    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
    use std::fs;
    use std::path::Path;

    fn split(text: &str) -> Result<Option<(String, Option<String>)>, String> {
        let ast = athrix_query::parse(text).unwrap();
        split_similar(text, &ast)
            .map(|s| {
                s.map(|s| {
                    (
                        format!("{:?}", s.rest.root.map(|r| r.span)),
                        s.reference.map(|r| r.0),
                    )
                })
            })
            .map_err(|e| e.kind)
    }

    #[test]
    fn similar_is_split_off_the_top_level_and_rejected_elsewhere() {
        assert_eq!(split("ext:jpg").unwrap(), None);
        assert_eq!(split("similar:").unwrap(), Some(("None".into(), None)));
        assert_eq!(split("مشابه:").unwrap(), Some(("None".into(), None)));
        let (_, reference) = split("similar:beach.jpg infolder:x").unwrap().unwrap();
        assert_eq!(reference.as_deref(), Some("beach.jpg"));
        let ast = athrix_query::parse("similar: a b").unwrap();
        let rest = split_similar("similar: a b", &ast).unwrap().unwrap().rest;
        assert!(matches!(rest.root.unwrap().kind, ExprKind::And(xs) if xs.len() == 2));
        assert_eq!(split("a | similar:").unwrap_err(), "SimilarNotTopLevel");
        assert_eq!(split("-similar: a").unwrap_err(), "SimilarNotTopLevel");
        assert_eq!(split("<similar: a>").unwrap_err(), "SimilarNotTopLevel");
        assert_eq!(split("similar: similar:").unwrap_err(), "SimilarRepeated");
        assert_eq!(split("similar: dupe:").unwrap_err(), "SimilarWithDupe");
    }

    /// A smooth picture whose layout depends on `seed`.
    fn picture(w: u32, h: u32, seed: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy, s) = (x as f32 / w as f32, y as f32 / h as f32, seed as f32);
            let v = ((fx * (3.0 + s) + fy * (2.0 + s * 0.7)).sin() + (fx * fy * (5.0 + s)).cos())
                * 60.0
                + 128.0;
            let v = v.clamp(0.0, 255.0) as u8;
            Rgb([v, v / 2, 255 - v])
        }))
    }

    fn save(path: &Path, image: &DynamicImage, format: ImageFormat) {
        image.save_with_format(path, format).unwrap();
    }

    /// A walked index of a temp folder with two near-identical pairs and a stranger.
    fn fixture() -> (tempfile::TempDir, IndexView) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("photos/small")).unwrap();
        let beach = picture(400, 300, 1);
        save(&root.join("photos/beach.png"), &beach, ImageFormat::Png);
        save(
            &root.join("photos/small/beach copy.jpg"),
            &beach.resize_exact(200, 150, image::imageops::FilterType::Triangle),
            ImageFormat::Jpeg,
        );
        let city = picture(300, 300, 4);
        save(&root.join("city.png"), &city, ImageFormat::Png);
        save(&root.join("city.bmp"), &city, ImageFormat::Bmp);
        save(
            &root.join("forest.png"),
            &picture(300, 200, 9),
            ImageFormat::Png,
        );
        fs::write(root.join("broken.jpg"), b"\xFF\xD8 not really").unwrap();
        fs::write(root.join("notes.txt"), b"not an image").unwrap();
        let roots = vec![root.to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let view = IndexView::new(1, live.snapshot(), &roots);
        (dir, view)
    }

    fn run(view: &IndexView, text: &str, scope: Option<&SearchScope>, limit: usize) -> SimilarHits {
        let ast = athrix_query::parse(text).unwrap();
        let split = split_similar(text, &ast).unwrap().unwrap();
        match similar_search(view, text, &split, limit, scope, &AtomicBool::new(false)) {
            Ok(h) => h,
            Err(e) => panic!("{text}: {e:?}"),
        }
    }

    #[test]
    fn groups_near_identical_images() {
        let (dir, view) = fixture();
        let h = run(&view, "similar:", None, 100);
        let mut rows: Vec<(u32, String)> = h
            .items
            .iter()
            .map(|i| (i.similar_group.unwrap(), i.name.clone()))
            .collect();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(h.items.iter().all(|i| i.similarity.unwrap() >= 87));
        // Rows of a group are adjacent.
        assert_eq!(rows[0].0, rows[1].0);
        assert_eq!(rows[2].0, rows[3].0);
        rows.sort();
        let names: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
        assert!(names.contains(&"beach.png") && names.contains(&"beach copy.jpg"));
        assert!(names.contains(&"city.png") && names.contains(&"city.bmp"));
        assert_eq!(h.summary.images, 6);
        assert_eq!(h.summary.checked, 6);
        assert_eq!(h.summary.skipped, 1);
        assert_eq!((h.summary.groups, h.summary.found), (2, 4));
        assert!(!h.incomplete && !h.truncated);

        // Other filters and the scope narrow the candidates; the limit cuts.
        assert_eq!(run(&view, "similar: ext:png", None, 100).items.len(), 0);
        let photos = SearchScope::parse(Some(&dir.path().join("photos").to_string_lossy()))
            .unwrap()
            .unwrap();
        let h = run(&view, "مشابه:", Some(&photos), 100);
        assert_eq!(h.items.len(), 2);
        let h = run(&view, "similar:", None, 3);
        assert_eq!(h.items.len(), 3);
        assert!(h.truncated);
    }

    #[test]
    fn a_named_reference_ranks_the_others() {
        let (_dir, view) = fixture();
        let h = run(&view, "similar:beach.png", None, 100);
        let names: Vec<&str> = h.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["beach copy.jpg"]);
        assert!(h.items[0].similar_group.is_none());
        let text = "similar:nothing-like-this.jpg";
        let ast = athrix_query::parse(text).unwrap();
        let split = split_similar(text, &ast).unwrap().unwrap();
        match similar_search(&view, text, &split, 10, None, &AtomicBool::new(false)) {
            Err(SimilarFailure::Invalid(e)) => assert_eq!(e.kind, "SimilarReferenceNotFound"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rank_against_a_result_and_errors() {
        let (_dir, view) = fixture();
        let images = image_ids(&view, &image_query(), None, &AtomicBool::new(false)).unwrap();
        let find = |name: &str| *images.iter().find(|&&id| view.name(id) == name).unwrap();
        let cancel = AtomicBool::new(false);
        let h = rank_against(&view, find("city.bmp"), images.clone(), 10, None, &cancel).unwrap();
        assert_eq!(h.items.len(), 1);
        assert_eq!(h.items[0].name, "city.png");
        assert_eq!(h.items[0].similarity, Some(100));
        match rank_against(&view, find("broken.jpg"), images.clone(), 10, None, &cancel) {
            Err(SimilarFailure::Invalid(e)) => assert_eq!(e.kind, "SimilarReferenceUnreadable"),
            other => panic!("{other:?}"),
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            rank_against(&view, find("city.bmp"), images, 10, None, &cancel),
            Err(SimilarFailure::Search(SearchFailure::Cancelled))
        ));
    }

    #[test]
    fn content_with_similar_is_rejected() {
        let (_dir, view) = fixture();
        let text = "similar: content:x";
        let ast = athrix_query::parse(text).unwrap();
        let split = split_similar(text, &ast).unwrap().unwrap();
        match similar_search(&view, text, &split, 10, None, &AtomicBool::new(false)) {
            Err(SimilarFailure::Invalid(e)) => assert_eq!(e.kind, "SimilarWithContent"),
            other => panic!("{other:?}"),
        }
    }
}
