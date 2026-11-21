//! Running a [`SearchPlan`] on both indexes and fusing the results.
//!
//! The executor does not know the index types: the caller supplies the two searches through
//! [`Sources`] (or plain closures with [`Closures`]). Paths are the file identity; they are
//! compared with [`crate::path_key`] (case-insensitive, `/` = `\` on Windows by default).
//!
//! # Strategy for `name AND content`
//!
//! The content index cannot (yet) be restricted to a set of paths, so the intersection is
//! done here:
//!
//! 1. **Name first.** The name part runs with [`ExecOptions::name_cap`] (default 100 000)
//!    results. A name-index search is a linear parallel scan whose cost hardly depends on how
//!    many entries match, so this is cheap (a few ms per million entries) and gives an exact
//!    membership set whenever fewer than `name_cap` files match.
//! 2. **Content with deepening.** The content part runs with a first fetch of
//!    `max(4 × limit, 200)` hits; hits whose path is not in the name set are dropped. While
//!    fewer than `limit` survive and the content index returned a full page, the fetch is
//!    repeated with 4× the size, up to [`ExecOptions::content_cap`] (default 10 000).
//! 3. If the name set was cut at `name_cap`, content hits outside it are checked one by one
//!    with [`Sources::name_matches`] (when the caller provides it) instead of being dropped.
//!
//! Bounds: at most `1 + log₄(content_cap / first fetch)` content searches (4 with the
//! defaults) and one name search. Whenever a bound may have hidden a valid result,
//! [`SearchResults::truncated`] says so; with the defaults that only happens when more than
//! 10 000 files match the content part and fewer than `limit` of the best 10 000 pass the
//! name part, or when more than 100 000 files match the name part without a verifier.
//!
//! Content first (search content, verify each hit against the name part) would avoid the
//! name scan, but it cannot stop early when the name part is selective, and the scan is not
//! the expensive step; it is used only for the overflow in step 3.
//!
//! # Excluded content only (`ext:docx -content:draft`)
//!
//! Each excluded clause is searched as a positive content query (up to
//! [`ExecOptions::exclusion_cap`] hits each); the name part then runs with
//! `limit + |excluded|` results so that at least `limit` survive when they exist.
//!
//! # Fusion
//!
//! The surviving lists are fused with RRF (see [`crate::reciprocal_rank_fusion`]):
//! the content list (BM25 order) with [`ExecOptions::content_weight`], the name list with
//! [`ExecOptions::name_weight`]. In `name AND content`, the name list takes part only when it
//! is ranked ([`NameResults::ranked`], e.g. fuzzy queries): index order carries no relevance.
//! In "plain words search both" ([`SearchPlan::NameOrContent`]) both lists always take part,
//! each fetched with `limit` results. Then [`Boosts`] are applied and the order is
//! re-sorted (stable, so equal scores keep the RRF order).

use std::collections::HashMap;

use athrix_query::{ContentQuery, Query, SearchPlan};

use crate::boost::{BoostInput, Boosts, ExactName};
use crate::frecency::FrecencyStore;
use crate::fusion::{DEFAULT_K, RankedList, reciprocal_rank_fusion};
use crate::path_key;

/// One name-index result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameHit {
    /// Full path.
    pub path: String,
    /// Modification time, Unix seconds, for the recency boost.
    pub mtime: Option<i64>,
}

/// The name-index results of one search.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameResults {
    /// Results, in the index's order.
    pub hits: Vec<NameHit>,
    /// The order is a relevance order (fuzzy scoring), not just index order.
    pub ranked: bool,
}

/// One content-index result. `S` is the caller's snippet type.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentHit<S> {
    /// Full path.
    pub path: String,
    /// The content index's score (BM25), kept for display and debugging.
    pub score: f32,
    /// Modification time, Unix seconds, for the recency boost.
    pub mtime: Option<i64>,
    /// The snippet with the matches.
    pub snippet: S,
}

/// The two searches the executor needs.
pub trait Sources {
    /// The snippet type of content hits.
    type Snippet;
    /// The error both searches return.
    type Error;

    /// Runs `query` on the name index, returning at most `limit` results.
    fn search_names(&mut self, query: &Query, limit: usize) -> Result<NameResults, Self::Error>;

    /// Runs `text` (content-query syntax: words ANDed, `"phrase"`, `-word`) on the content
    /// index, returning at most `limit` results, best first.
    fn search_content(
        &mut self,
        text: &str,
        limit: usize,
    ) -> Result<Vec<ContentHit<Self::Snippet>>, Self::Error>;

    /// Whether the file at `path` matches `query` on the name index. `Ok(None)` (the
    /// default) when the caller cannot answer; see the module docs, step 3.
    fn name_matches(&mut self, query: &Query, path: &str) -> Result<Option<bool>, Self::Error> {
        let _ = (query, path);
        Ok(None)
    }
}

/// The name search closure type of [`Closures`].
pub type NameFn<'a, E> = dyn FnMut(&Query, usize) -> Result<NameResults, E> + 'a;
/// The content search closure type of [`Closures`].
pub type ContentFn<'a, S, E> = dyn FnMut(&str, usize) -> Result<Vec<ContentHit<S>>, E> + 'a;
/// The name verifier closure type of [`Closures`].
pub type VerifyFn<'a, E> = dyn FnMut(&Query, &str) -> Result<bool, E> + 'a;

/// [`Sources`] from closures.
pub struct Closures<'a, S, E> {
    /// See [`Sources::search_names`].
    pub names: &'a mut NameFn<'a, E>,
    /// See [`Sources::search_content`].
    pub content: &'a mut ContentFn<'a, S, E>,
    /// See [`Sources::name_matches`]; `None` when not available.
    pub verify: Option<&'a mut VerifyFn<'a, E>>,
}

impl<S, E> Sources for Closures<'_, S, E> {
    type Snippet = S;
    type Error = E;

    fn search_names(&mut self, query: &Query, limit: usize) -> Result<NameResults, E> {
        (self.names)(query, limit)
    }

    fn search_content(&mut self, text: &str, limit: usize) -> Result<Vec<ContentHit<S>>, E> {
        (self.content)(text, limit)
    }

    fn name_matches(&mut self, query: &Query, path: &str) -> Result<Option<bool>, E> {
        match &mut self.verify {
            Some(v) => v(query, path).map(Some),
            None => Ok(None),
        }
    }
}

/// Executor settings. Build with [`ExecOptions::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecOptions {
    /// Results to return. Default 100.
    pub limit: usize,
    /// The RRF constant. Default 60.
    pub k: f64,
    /// Weight of the name list in fusion. Default 1.
    pub name_weight: f64,
    /// Weight of the content list in fusion. Default 1.
    pub content_weight: f64,
    /// Boosts applied after fusion.
    pub boosts: Boosts,
    /// The current time (Unix seconds) for recency and frecency. The executor never reads
    /// the clock.
    pub now: i64,
    /// Maximum name results fetched to intersect with content. Default 100 000.
    pub name_cap: usize,
    /// Maximum content results fetched while deepening. Default 10 000.
    pub content_cap: usize,
    /// Maximum content results per excluded clause. Default 10 000.
    pub exclusion_cap: usize,
    /// Compare paths case-insensitively (and `/` = `\`). Default: true on Windows.
    pub case_insensitive_paths: bool,
}

impl ExecOptions {
    /// Defaults (see the fields), with the current time `now` (Unix seconds).
    pub fn new(now: i64) -> Self {
        ExecOptions {
            limit: 100,
            k: DEFAULT_K,
            name_weight: 1.0,
            content_weight: 1.0,
            boosts: Boosts::default(),
            now,
            name_cap: 100_000,
            content_cap: 10_000,
            exclusion_cap: 10_000,
            case_insensitive_paths: cfg!(windows),
        }
    }

    fn first_content_fetch(&self) -> usize {
        self.limit
            .saturating_mul(4)
            .max(200)
            .min(self.content_cap.max(1))
    }
}

/// Which parts of the query a result matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Matched {
    /// The name part only (or a name-only query).
    Name,
    /// The content part only (`content:x` alone, or a content hit in "plain words search
    /// both").
    Content,
    /// Both: `name AND content` results, or a "plain words" result found by both.
    Both,
}

impl Matched {
    /// True for [`Matched::Content`] and [`Matched::Both`].
    pub fn has_content(self) -> bool {
        matches!(self, Matched::Content | Matched::Both)
    }

    /// True for [`Matched::Name`] and [`Matched::Both`].
    pub fn has_name(self) -> bool {
        matches!(self, Matched::Name | Matched::Both)
    }
}

/// One fused result.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultHit<S> {
    /// Full path, as the source that found it first spelled it.
    pub path: String,
    /// The final score (RRF × boosts). Only meaningful for ordering.
    pub score: f64,
    /// Which parts matched.
    pub matched: Matched,
    /// The content snippet, when the content part matched.
    pub snippet: Option<S>,
    /// The content index's score, when the content part matched.
    pub content_score: Option<f32>,
    /// Modification time, when a source reported it.
    pub mtime: Option<i64>,
}

/// Which bounds may have hidden valid results. All false = the results are exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Truncation {
    /// More than `name_cap` files matched the name part and some content hits could not be
    /// checked against it.
    pub names: bool,
    /// The content part was cut at `content_cap` before `limit` results passed the name part.
    pub content: bool,
    /// An excluded clause matched more than `exclusion_cap` files: some files that contain it
    /// may be shown.
    pub exclusions: bool,
}

impl Truncation {
    /// True when no bound was hit.
    pub fn is_complete(&self) -> bool {
        !(self.names || self.content || self.exclusions)
    }
}

/// Fused results of [`execute`].
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults<S> {
    /// Best first, at most `limit`.
    pub hits: Vec<ResultHit<S>>,
    /// Which bounds were hit.
    pub truncated: Truncation,
}

impl<S> Default for SearchResults<S> {
    fn default() -> Self {
        SearchResults {
            hits: Vec::new(),
            truncated: Truncation::default(),
        }
    }
}

struct Candidate<S> {
    path: String,
    mtime: Option<i64>,
    in_name: bool,
    content: Option<(f32, S)>,
}

/// Collects candidates by path identity.
struct Pool<S> {
    case_insensitive: bool,
    slots: HashMap<String, usize>,
    items: Vec<Candidate<S>>,
}

impl<S> Pool<S> {
    fn new(case_insensitive: bool) -> Self {
        Pool {
            case_insensitive,
            slots: HashMap::new(),
            items: Vec::new(),
        }
    }

    fn slot(&mut self, path: &str, mtime: Option<i64>) -> usize {
        let key = path_key(path, self.case_insensitive).into_owned();
        let items = &mut self.items;
        let i = *self.slots.entry(key).or_insert_with(|| {
            items.push(Candidate {
                path: path.to_string(),
                mtime: None,
                in_name: false,
                content: None,
            });
            items.len() - 1
        });
        if self.items[i].mtime.is_none() {
            self.items[i].mtime = mtime;
        }
        i
    }

    fn add_name(&mut self, h: NameHit) -> usize {
        let i = self.slot(&h.path, h.mtime);
        self.items[i].in_name = true;
        i
    }

    fn add_content(&mut self, h: ContentHit<S>) -> usize {
        let i = self.slot(&h.path, h.mtime);
        if self.items[i].content.is_none() {
            self.items[i].content = Some((h.score, h.snippet));
        }
        i
    }
}

/// Runs `plan` and fuses the results. `frecency` feeds the frecency boost (`None` = no
/// frecency). See the module docs for the strategy and bounds.
pub fn execute<Src: Sources + ?Sized>(
    plan: &SearchPlan,
    options: &ExecOptions,
    frecency: Option<&dyn FrecencyStore>,
    sources: &mut Src,
) -> Result<SearchResults<Src::Snippet>, Src::Error> {
    let o = options;
    let mut pool = Pool::new(o.case_insensitive_paths);
    let mut truncated = Truncation::default();
    let mut lists: Vec<(Vec<usize>, f64)> = Vec::new();
    let both_required;
    match plan {
        SearchPlan::Empty => return Ok(SearchResults::default()),
        SearchPlan::Name(q) => {
            both_required = false;
            let names = sources.search_names(q, o.limit)?;
            let order = names.hits.into_iter().map(|h| pool.add_name(h)).collect();
            lists.push((order, o.name_weight));
        }
        SearchPlan::NameAndContent {
            name: None,
            content,
        } => {
            both_required = false;
            let hits = sources.search_content(&content.to_query_text(), o.limit)?;
            let order = hits.into_iter().map(|h| pool.add_content(h)).collect();
            lists.push((order, o.content_weight));
        }
        SearchPlan::NameAndContent {
            name: Some(name),
            content,
        } if !content.has_positive() => {
            both_required = false;
            let order = name_minus_content(sources, name, content, o, &mut pool, &mut truncated)?;
            lists.push((order, o.name_weight));
        }
        SearchPlan::NameAndContent {
            name: Some(name),
            content,
        } => {
            both_required = true;
            let text = content.to_query_text();
            let (content_order, name_order, ranked) =
                content_within(sources, name, &text, o, &mut pool, &mut truncated)?;
            lists.push((content_order, o.content_weight));
            if ranked {
                lists.push((name_order, o.name_weight));
            }
        }
        SearchPlan::NameOrContent {
            name,
            filters,
            content,
        } => {
            both_required = false;
            let names = sources.search_names(name, o.limit)?;
            let name_order: Vec<usize> = names.hits.into_iter().map(|h| pool.add_name(h)).collect();
            let text = content.to_query_text();
            let content_order = match filters {
                None => {
                    let hits = sources.search_content(&text, o.limit)?;
                    hits.into_iter().map(|h| pool.add_content(h)).collect()
                }
                Some(f) => {
                    // Filter-only name hits must not count as name matches of the words.
                    let mut side = Pool::new(o.case_insensitive_paths);
                    let (order, _, _) =
                        content_within(sources, f, &text, o, &mut side, &mut truncated)?;
                    let mut out = Vec::with_capacity(order.len());
                    let mut items: Vec<Option<Candidate<_>>> =
                        side.items.into_iter().map(Some).collect();
                    for i in order {
                        if let Some(c) = items[i].take() {
                            let (score, snippet) = c.content.expect("content hit");
                            out.push(pool.add_content(ContentHit {
                                path: c.path,
                                score,
                                mtime: c.mtime,
                                snippet,
                            }));
                        }
                    }
                    out
                }
            };
            lists.push((name_order, o.name_weight));
            lists.push((content_order, o.content_weight));
        }
    }

    let ranked: Vec<RankedList<'_, usize>> = lists
        .iter()
        .map(|(keys, w)| RankedList::weighted(keys, *w))
        .collect();
    let fused = reciprocal_rank_fusion(&ranked, o.k);
    // Skip the per-result work of boosts that are off.
    let exact = (o.boosts.exact_name > 0.0)
        .then(|| ExactName::new(&plan.name_words()))
        .flatten();
    let frecency = frecency.filter(|_| o.boosts.frecency > 0.0);
    let mut scored: Vec<(usize, f64)> = fused
        .iter()
        .map(|f| {
            let i = lists[f.list].0[f.index];
            let c = &pool.items[i];
            let input = BoostInput {
                exact_name: exact.as_ref().is_some_and(|e| e.matches(&c.path)),
                mtime: c.mtime,
                now: o.now,
                frecency: frecency.map_or(0.0, |s| s.frecency(&c.path, o.now)),
            };
            (i, f.score * o.boosts.factor(&input))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1)); // stable: ties keep the RRF order
    scored.truncate(o.limit);

    let mut items: Vec<Option<Candidate<_>>> = pool.items.into_iter().map(Some).collect();
    let hits = scored
        .into_iter()
        .filter_map(|(i, score)| {
            let c = items[i].take()?;
            let matched = match (both_required || c.in_name, c.content.is_some()) {
                (true, true) => Matched::Both,
                (false, true) => Matched::Content,
                _ => Matched::Name,
            };
            let (content_score, snippet) = match c.content {
                Some((s, sn)) => (Some(s), Some(sn)),
                None => (None, None),
            };
            Some(ResultHit {
                path: c.path,
                score,
                matched,
                snippet,
                content_score,
                mtime: c.mtime,
            })
        })
        .collect();
    Ok(SearchResults { hits, truncated })
}

/// `name AND content` (content has a positive clause). Returns the surviving candidates in
/// content order, the same set in name order, and whether the name order is ranked.
fn content_within<Src: Sources + ?Sized>(
    sources: &mut Src,
    name: &Query,
    text: &str,
    o: &ExecOptions,
    pool: &mut Pool<Src::Snippet>,
    truncated: &mut Truncation,
) -> Result<(Vec<usize>, Vec<usize>, bool), Src::Error> {
    let name_cap = o.name_cap.max(1);
    let names = sources.search_names(name, name_cap)?;
    let name_full = names.hits.len() >= name_cap;
    let ranked = names.ranked;
    // path key → (name rank, mtime)
    let mut member: HashMap<String, (usize, Option<i64>)> =
        HashMap::with_capacity(names.hits.len());
    for (rank, h) in names.hits.into_iter().enumerate() {
        member
            .entry(path_key(&h.path, o.case_insensitive_paths).into_owned())
            .or_insert((rank, h.mtime));
    }
    let mut verified: HashMap<String, bool> = HashMap::new();

    let cap = o.content_cap.max(1);
    let mut fetch = o.first_content_fetch();
    let hits = loop {
        let hits = sources.search_content(text, fetch)?;
        let exhausted = hits.len() < fetch;
        let mut survivors = Vec::with_capacity(hits.len().min(o.limit));
        for h in hits {
            let key = path_key(&h.path, o.case_insensitive_paths).into_owned();
            let rank = match member.get(&key) {
                Some(&(rank, mtime)) => Some((rank, mtime)),
                None if name_full => {
                    let ok = match verified.get(&key) {
                        Some(&ok) => Some(ok),
                        None => {
                            let r = sources.name_matches(name, &h.path)?;
                            if let Some(ok) = r {
                                verified.insert(key, ok);
                            }
                            r
                        }
                    };
                    match ok {
                        Some(true) => Some((usize::MAX, None)),
                        Some(false) => None,
                        None => {
                            truncated.names = true;
                            None
                        }
                    }
                }
                None => None,
            };
            if let Some(r) = rank {
                survivors.push((h, r));
            }
        }
        if survivors.len() >= o.limit || exhausted || fetch >= cap {
            if survivors.len() < o.limit && !exhausted {
                truncated.content = true;
            }
            break survivors;
        }
        fetch = fetch.saturating_mul(4).min(cap);
    };

    let mut by_name: Vec<(usize, usize)> = Vec::with_capacity(hits.len());
    let mut content_order = Vec::with_capacity(hits.len());
    for (pos, (mut h, (rank, name_mtime))) in hits.into_iter().enumerate() {
        if h.mtime.is_none() {
            h.mtime = name_mtime;
        }
        let i = pool.add_content(h);
        content_order.push(i);
        by_name.push((rank, pos));
    }
    by_name.sort_unstable();
    let name_order = by_name.iter().map(|&(_, pos)| content_order[pos]).collect();
    Ok((content_order, name_order, ranked))
}

/// `name AND NOT content` (no positive content clause). Returns the name order.
fn name_minus_content<Src: Sources + ?Sized>(
    sources: &mut Src,
    name: &Query,
    content: &ContentQuery,
    o: &ExecOptions,
    pool: &mut Pool<Src::Snippet>,
    truncated: &mut Truncation,
) -> Result<Vec<usize>, Src::Error> {
    let cap = o.exclusion_cap.max(1);
    let mut excluded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for clause in content.excluded() {
        let hits = sources.search_content(&clause.to_positive_text(), cap)?;
        if hits.len() >= cap {
            truncated.exclusions = true;
        }
        excluded.extend(
            hits.into_iter()
                .map(|h| path_key(&h.path, o.case_insensitive_paths).into_owned()),
        );
    }
    let want = o.limit.saturating_add(excluded.len());
    let fetch = want.min(o.name_cap.max(1));
    let names = sources.search_names(name, fetch)?;
    let full = names.hits.len() >= fetch && fetch < want;
    let mut order = Vec::with_capacity(o.limit.min(names.hits.len()));
    for h in names.hits {
        if order.len() >= o.limit {
            break;
        }
        if !excluded.contains(path_key(&h.path, o.case_insensitive_paths).as_ref()) {
            order.push(pool.add_name(h));
        }
    }
    if full && order.len() < o.limit {
        truncated.names = true;
    }
    Ok(order)
}
