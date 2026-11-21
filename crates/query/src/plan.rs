//! Query planning: splitting a parsed [`Query`] between the name index and the content index.
//!
//! The name index (`athrix-fs-index`) evaluates names, paths and metadata; the content index
//! (`athrix-content-index`) evaluates the text inside files. A query that uses `content:` /
//! `محتوى:` needs both, so [`plan`] splits it into:
//!
//! - a **name part**: an ordinary [`Query`] with every `content:` filter removed, ready for
//!   `athrix_fs_index::Query::compile`; and
//! - a **content part**: a [`ContentQuery`] whose [`ContentQuery::to_query_text`] is the
//!   string `ContentIndex::search` takes (words ANDed, `"phrase"`, `-word` exclusion).
//!
//! # Semantics
//!
//! Results are the files that satisfy **both** parts (the name part is a filter on the same
//! files the content part searches):
//!
//! | query | meaning |
//! |---|---|
//! | `تقرير content:ميزانية` | NAME matches `تقرير` AND CONTENT matches `ميزانية` |
//! | `content:"خطة العمل" ext:docx` | a `.docx` whose content has the phrase «خطة العمل» |
//! | `محتوى:عقد محتوى:إيجار` | content has both words (several `content:` filters are ANDed) |
//! | `report -content:draft` | NAME matches `report`, CONTENT does not contain `draft` |
//! | `<report content:x> ext:pdf` | AND groups are flattened: same as `report content:x ext:pdf` |
//! | `!!content:x` | double negation cancels: same as `content:x` |
//! | `content:x` | content only: any file whose content matches |
//!
//! A `content:` value is always searched as one clause: `content:عقد` is a word,
//! `content:"عقد الإيجار"` a phrase. `content:عقد ايجار` is `content:عقد` AND the *name*
//! word `ايجار` (a filter value ends at whitespace); quote the value to search both words in
//! the content. Files that are not in the content index (not extracted yet, or a format
//! without text) never match a positive `content:` and always pass a negated one.
//!
//! ## Rejected combinations ([`PlanError`])
//!
//! Only conjunctions across the two sources are sound to evaluate as "filter one index by the
//! other". Everything that would need a union across sources is rejected with a typed error:
//!
//! - [`PlanErrorKind::ContentInOr`]: `a | content:b`, `content:a | content:b`,
//!   `<x | content:y> z` — a `content:` anywhere under an OR.
//! - [`PlanErrorKind::NegatedContentGroup`]: `!<content:a ext:pdf>`, `!<content:a content:b>` —
//!   NOT over a group that contains `content:` together with anything else (by De Morgan
//!   this is an OR). NOT directly on one `content:` filter is fine.
//! - [`PlanErrorKind::OnlyExcludedContent`]: `-content:draft` alone — "every file without
//!   this word" has nothing to start from; add a name word or filter (`ext:docx -content:x`).
//!
//! # "Plain words search both" mode
//!
//! With [`PlanOptions::plain_words_search_content`] on, a query **without** `content:` whose
//! words are all plain (top-level AND of words, quoted phrases, negated words and filters; no
//! wildcards `*` `?`, no anchors `^` `$`, no words under an OR) becomes
//! [`SearchPlan::NameOrContent`]: the files whose NAME matches the whole query, **plus** the
//! files whose CONTENT matches the words and whose name/metadata match the remaining filters.
//! `تقرير ext:pdf` = (name has تقرير AND ext pdf) OR (content has تقرير AND ext pdf). The two
//! result lists are fused by rank (see `athrix-ranking`). Queries that do not qualify keep
//! [`SearchPlan::Name`]; an explicit `content:` always wins over the mode.

use std::fmt;

use crate::ast::{Expr, ExprKind, Filter, FilterName, FilterValue, Query, Span, Term};

/// Planner switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanOptions {
    /// Plain words (without `content:`) search names **and** contents, and the two result
    /// lists are fused. Off by default: plain words search names only.
    pub plain_words_search_content: bool,
}

/// How a query is executed. Build it with [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPlan {
    /// The query is empty: show nothing (or the default view).
    Empty,
    /// Only the name index is involved: run the query as is.
    Name(Query),
    /// Files matching `name` (all files when `None`) AND `content`.
    NameAndContent {
        /// The name-index part, or `None` when the query has only `content:` filters.
        name: Option<Query>,
        /// The content-index part. If it has no positive clause
        /// ([`ContentQuery::has_positive`] is false), `name` is always `Some`.
        content: ContentQuery,
    },
    /// "Plain words search both": files whose NAME matches `name`, plus files whose CONTENT
    /// matches `content` and whose name/metadata match `filters` (`None` = no restriction).
    NameOrContent {
        /// The whole query, run on the name index.
        name: Query,
        /// The query's non-word filters (`ext:` …), restricting the content branch.
        filters: Option<Query>,
        /// The query's words as a content query (always has a positive clause).
        content: ContentQuery,
    },
}

impl SearchPlan {
    /// True when the content index is needed.
    pub fn uses_content(&self) -> bool {
        matches!(
            self,
            SearchPlan::NameAndContent { .. } | SearchPlan::NameOrContent { .. }
        )
    }

    /// The positive plain words that the file NAME is expected to match (top-level words and
    /// phrases of the name part), for an "exact name" ranking boost. Empty for content-only
    /// queries.
    pub fn name_words(&self) -> Vec<&str> {
        let q = match self {
            SearchPlan::Empty => return Vec::new(),
            SearchPlan::Name(q) | SearchPlan::NameOrContent { name: q, .. } => Some(q),
            SearchPlan::NameAndContent { name, .. } => name.as_ref(),
        };
        let mut out = Vec::new();
        if let Some(root) = q.and_then(|q| q.root.as_ref()) {
            for e in conjuncts(root) {
                if let ExprKind::Term(t) = &e.kind {
                    out.push(t.text.as_str());
                }
            }
        }
        out
    }
}

/// The content-index part of a plan: clauses ANDed together.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContentQuery {
    /// The clauses, in query order.
    pub clauses: Vec<ContentClause>,
}

/// One clause of a [`ContentQuery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentClause {
    /// The text to find (a word or a phrase), without quotes.
    pub text: String,
    /// True for `-content:x` / `!content:x` / `-word` (in plain-words mode): must NOT match.
    pub negated: bool,
    /// Where it came from in the query string (the whole filter or term).
    pub span: Span,
}

impl ContentQuery {
    /// True when at least one clause must match. A query without one cannot be run on its
    /// own (the content index returns nothing for it): it only excludes.
    pub fn has_positive(&self) -> bool {
        self.clauses.iter().any(|c| !c.negated)
    }

    /// The positive clauses.
    pub fn positive(&self) -> impl Iterator<Item = &ContentClause> {
        self.clauses.iter().filter(|c| !c.negated)
    }

    /// The negated clauses.
    pub fn excluded(&self) -> impl Iterator<Item = &ContentClause> {
        self.clauses.iter().filter(|c| c.negated)
    }

    /// The text for `ContentIndex::search`: every clause as a quoted phrase (a one-word
    /// phrase is the same as the word), negated ones prefixed with `-`, joined by spaces.
    /// `"ميزانية" -"مسودة"`.
    pub fn to_query_text(&self) -> String {
        let mut out = String::new();
        for c in &self.clauses {
            push_clause(&mut out, &c.text, c.negated);
        }
        out
    }
}

impl ContentClause {
    /// This clause alone as positive content-query text (`"x"`), e.g. to look up the files
    /// an exclusion removes.
    pub fn to_positive_text(&self) -> String {
        let mut out = String::new();
        push_clause(&mut out, &self.text, false);
        out
    }
}

fn push_clause(out: &mut String, text: &str, negated: bool) {
    if !out.is_empty() {
        out.push(' ');
    }
    if negated {
        out.push('-');
    }
    out.push('"');
    // The content syntax has no escapes; a `"` cannot reach here from the parser, but stay
    // safe and treat one as a word separator.
    out.extend(text.chars().map(|c| if c == '"' { ' ' } else { c }));
    out.push('"');
}

impl fmt::Display for ContentQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_query_text())
    }
}

/// Why a query cannot be planned. Stable codes the UI can map to localized text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PlanErrorKind {
    /// `content:` under an OR (`a | content:b`): a union across the two indexes.
    ContentInOr,
    /// NOT over a group containing `content:` and anything else (`!<content:a ext:pdf>`).
    NegatedContentGroup,
    /// Only negated `content:` filters, with nothing to search for.
    OnlyExcludedContent,
}

/// A query that parses but whose combination of sources is not supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// The problem category.
    pub kind: PlanErrorKind,
    /// The offending `content:` filter.
    pub span: Span,
    /// The enclosing OR / NOT expression (equal to `span` for
    /// [`PlanErrorKind::OnlyExcludedContent`]).
    pub group_span: Span,
    /// A friendly English explanation.
    pub message: String,
}

impl PlanError {
    fn new(kind: PlanErrorKind, span: Span, group_span: Span) -> Self {
        let message = match kind {
            PlanErrorKind::ContentInOr => {
                "`content:` cannot be combined with `|` (OR) yet; \
                 search the alternatives one at a time"
            }
            PlanErrorKind::NegatedContentGroup => {
                "`!`/`-` before a group with `content:` is not supported; \
                 negate the `content:` filter itself, for example `-content:draft`"
            }
            PlanErrorKind::OnlyExcludedContent => {
                "a query with only excluded `content:` has nothing to search for; \
                 add a word or filter, for example `ext:docx -content:draft`"
            }
        };
        PlanError {
            kind,
            span,
            group_span,
            message: message.to_string(),
        }
    }

    /// Byte offset where the problem starts.
    pub fn position(&self) -> usize {
        self.span.start
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unsupported query at position {}: {}",
            self.span.start, self.message
        )
    }
}

impl std::error::Error for PlanError {}

impl From<PlanError> for athrix_core::Error {
    fn from(e: PlanError) -> Self {
        athrix_core::Error::InvalidQuery {
            position: e.span.start,
            message: e.message,
        }
    }
}

/// Splits `query` between the name index and the content index. See the module docs for the
/// semantics and the rejected combinations.
pub fn plan(query: &Query, options: &PlanOptions) -> Result<SearchPlan, PlanError> {
    let Some(root) = &query.root else {
        return Ok(SearchPlan::Empty);
    };
    if first_content(root).is_none() {
        let plain = options
            .plain_words_search_content
            .then(|| plain_words_plan(query, root))
            .flatten();
        return Ok(plain.unwrap_or_else(|| SearchPlan::Name(query.clone())));
    }

    let mut name: Vec<Expr> = Vec::new();
    let mut content = ContentQuery::default();
    let mut stack: Vec<&Expr> = conjuncts(root);
    stack.reverse();
    while let Some(e) = stack.pop() {
        if first_content(e).is_none() {
            name.push(e.clone());
            continue;
        }
        match &unwrap_groups(e).kind {
            ExprKind::Filter(f) if f.name == FilterName::Content => {
                push_content(&mut content, f, e.span, false);
            }
            ExprKind::Not(inner) => {
                let inner = unwrap_groups(inner);
                match &inner.kind {
                    ExprKind::Filter(f) if f.name == FilterName::Content => {
                        push_content(&mut content, f, e.span, true);
                    }
                    // `!!x` is `x`: plan `x` as a conjunct.
                    ExprKind::Not(x) => {
                        let mut more = conjuncts(x);
                        more.reverse();
                        stack.extend(more);
                    }
                    _ => {
                        let c = first_content(inner).unwrap_or(inner.span);
                        return Err(match first_content_under_or(inner) {
                            Some((c, or)) => PlanError::new(PlanErrorKind::ContentInOr, c, or),
                            None => PlanError::new(PlanErrorKind::NegatedContentGroup, c, e.span),
                        });
                    }
                }
            }
            _ => {
                // Contains content but is neither a content filter, a NOT nor an AND (those
                // were flattened): an OR somewhere at the top.
                let (c, or) = first_content_under_or(e)
                    .unwrap_or_else(|| (first_content(e).unwrap_or(e.span), e.span));
                return Err(PlanError::new(PlanErrorKind::ContentInOr, c, or));
            }
        }
    }

    if !content.has_positive() && name.is_empty() {
        let span = content.clauses.first().map_or(root.span, |c| c.span);
        return Err(PlanError::new(
            PlanErrorKind::OnlyExcludedContent,
            span,
            span,
        ));
    }
    Ok(SearchPlan::NameAndContent {
        name: and_query(name),
        content,
    })
}

fn push_content(content: &mut ContentQuery, f: &Filter, span: Span, negated: bool) {
    if let FilterValue::Text(t) = &f.value {
        content.clauses.push(ContentClause {
            text: t.clone(),
            negated,
            span,
        });
    }
}

/// `plain words search both`: `None` when the query does not qualify.
fn plain_words_plan(query: &Query, root: &Expr) -> Option<SearchPlan> {
    let mut filters = Vec::new();
    let mut content = ContentQuery::default();
    for e in conjuncts(root) {
        let inner = unwrap_groups(e);
        match &inner.kind {
            ExprKind::Term(t) => content.clauses.push(plain_clause(t, e.span, false)?),
            ExprKind::Not(x) => match &unwrap_groups(x).kind {
                ExprKind::Term(t) => content.clauses.push(plain_clause(t, e.span, true)?),
                _ if has_term(x) => return None,
                _ => filters.push(e.clone()),
            },
            _ if has_term(inner) => return None,
            _ => filters.push(e.clone()),
        }
    }
    content.has_positive().then(|| SearchPlan::NameOrContent {
        name: query.clone(),
        filters: and_query(filters),
        content,
    })
}

fn plain_clause(t: &Term, span: Span, negated: bool) -> Option<ContentClause> {
    let special = t.text.contains(['*', '?', '^', '$']);
    let has_text = t.text.chars().any(char::is_alphanumeric);
    (!special && has_text).then(|| ContentClause {
        text: t.text.clone(),
        negated,
        span,
    })
}

fn has_term(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Term(_) => true,
        ExprKind::Filter(_) => false,
        ExprKind::Not(x) | ExprKind::Group(x) => has_term(x),
        ExprKind::And(v) | ExprKind::Or(v) => v.iter().any(has_term),
    }
}

/// The top-level AND children of `e`, with groups and nested ANDs flattened.
fn conjuncts(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match &e.kind {
            ExprKind::Group(x) => stack.push(x),
            ExprKind::And(v) => stack.extend(v.iter().rev()),
            _ => out.push(e),
        }
    }
    out
}

fn unwrap_groups(mut e: &Expr) -> &Expr {
    while let ExprKind::Group(x) = &e.kind {
        e = x;
    }
    e
}

/// Span of the first `content:` filter in `e`.
fn first_content(e: &Expr) -> Option<Span> {
    match &e.kind {
        ExprKind::Filter(f) if f.name == FilterName::Content => Some(e.span),
        ExprKind::Term(_) | ExprKind::Filter(_) => None,
        ExprKind::Not(x) | ExprKind::Group(x) => first_content(x),
        ExprKind::And(v) | ExprKind::Or(v) => v.iter().find_map(first_content),
    }
}

/// The first `content:` filter under an OR, with that OR's span.
fn first_content_under_or(e: &Expr) -> Option<(Span, Span)> {
    match &e.kind {
        ExprKind::Term(_) | ExprKind::Filter(_) => None,
        ExprKind::Not(x) | ExprKind::Group(x) => first_content_under_or(x),
        ExprKind::And(v) => v.iter().find_map(first_content_under_or),
        ExprKind::Or(v) => v.iter().find_map(first_content).map(|c| (c, e.span)),
    }
}

/// `None` for no expressions, the expression itself for one, an AND otherwise.
fn and_query(mut exprs: Vec<Expr>) -> Option<Query> {
    let root = match exprs.len() {
        0 => return None,
        1 => exprs.pop()?,
        _ => {
            let span = exprs
                .iter()
                .map(|e| e.span)
                .reduce(Span::to)
                .unwrap_or_default();
            Expr {
                kind: ExprKind::And(exprs),
                span,
            }
        }
    };
    Some(Query { root: Some(root) })
}
