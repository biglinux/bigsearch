//! Query engine: name, content and combined searches, then the filters,
//! paging and metadata a client sees.
use crate::index::fields;
use anyhow::{Context, Result};
use std::path::Path;
use tantivy::IndexReader;
use tantivy::query::Query;

mod both;
mod content;
mod listing;
mod names;
#[cfg(all(test, not(miri)))]
mod tests;

pub use both::search_both;
use content::count_content;
pub use content::search_content;
pub use listing::{count, list_all, list_matching, list_paths};
pub use names::search;
use names::{count_names, indexed_mime};

// Wire types (Hit/Filter/Page/QueryOutcome) live in the shared `big-indexd-client`
// crate so the daemon and its GUI clients cannot drift; the daemon adds the engine.
pub use big_indexd_client::{Filter, Hit, MatchKind, Page, QueryOutcome};

/// Ceiling on candidates pulled from the index before filtering/paging. Bounds the
/// daemon's work (and `stat` count) per request regardless of how broad the query is.
///
/// From the shared crate rather than written here: it caps `QueryOutcome::total`,
/// so a client has to know it to tell an exact count from a floor, and one fact
/// in two files is a fact that can drift.
use big_indexd_client::CANDIDATE_CEILING;

/// Longest query read, in characters; the rest is ignored. The query comes
/// from any same-user client, and every word becomes terms to look up.
const MAX_QUERY_CHARS: usize = 256;

pub struct QueryIndexes<'a> {
    pub name: &'a IndexReader,
    pub content: Option<&'a IndexReader>,
}

/// Run a full query: base search → cheap path filters → `stat`-based date/size
/// filters → page → metadata enrichment. Everything the GUI would otherwise do
/// (and leak over) happens here, server-side and bounded.
pub fn run_query(
    indexes: QueryIndexes<'_>,
    q: &str,
    mode: &str,
    filter: &Filter,
    page: &Page,
    include_origin: bool,
    origin_reader: Option<&crate::origin::OriginReader>,
) -> Result<QueryOutcome> {
    let q = match q.char_indices().nth(MAX_QUERY_CHARS) {
        Some((end, _)) => &q[..end],
        None => q,
    };
    let (paged, fetch) = candidates_to_read(mode, filter, page);
    let (base, counted) = search_by_mode(&indexes, q, mode, filter, paged, fetch)?;

    let mut kept = keep_path_matches(base, filter);

    // A content hit carries no type of its own: the name index has it.
    let names = indexes.name.searcher();
    let name_fields = fields(names.index())?;
    keep_type_matches(&mut kept, filter, &names, &name_fields);
    keep_stat_matches(&mut kept, filter);

    let total = counted.map_or(kept.len(), |counted| counted.max(kept.len()));
    let end = page.offset.saturating_add(page.limit);
    let next_offset = (end < total).then_some(end);

    // Page, then enrich only the visible window with metadata.
    let mut hits: Vec<Hit> = kept
        .into_iter()
        .skip(page.offset)
        .take(page.limit)
        .collect();
    enrich_page(&mut hits, &names, &name_fields);
    if include_origin && let Some(origin_reader) = origin_reader {
        origin_reader.attach_summaries(&mut hits);
    }

    Ok(QueryOutcome {
        hits,
        total,
        next_offset,
        content_pending: 0, // the daemon overwrites this from its live gauge
    })
}

/// The base search of `mode`, and the count of a paged query whose page did
/// not read every match.
fn search_by_mode(
    indexes: &QueryIndexes<'_>,
    q: &str,
    mode: &str,
    filter: &Filter,
    paged: bool,
    fetch: usize,
) -> Result<(Vec<Hit>, Option<usize>)> {
    let content_reader = || {
        indexes
            .content
            .context("content index required for content search")
    };
    let base = match mode {
        "content" => search_content(content_reader()?, q, fetch)?,
        "both" => search_both(indexes.name, content_reader()?, q, fetch)?,
        // Everything the index holds, for a caller that is narrowing by
        // filter rather than by words: "every PDF under my home folder" has
        // no text to search for, and the name search refuses a query shorter
        // than two letters because the ngram index cannot narrow one.
        "list" => list_matching(indexes.name, filter, CANDIDATE_CEILING)?,
        _ => search(indexes.name, q, filter, fetch)?,
    };
    // The count of a paged query comes from the postings, up to the ceiling,
    // without reading documents. Names are counted before the exact-match
    // post-filter, so it can run over what paging reaches. A search that
    // returned fewer candidates than it asked for has read every match, and
    // its own count is exact.
    let counted = if paged && base.len() >= fetch {
        Some(match mode {
            "content" => count_content(content_reader()?, q, CANDIDATE_CEILING)?,
            _ => count_names(indexes.name, q, filter, CANDIDATE_CEILING)?,
        })
    } else {
        None
    };
    Ok((base, counted))
}

/// Whether the query is paged, and how many candidates the base search reads.
///
/// Only what the page needs is read when the index can answer every filter
/// itself: the date and size filters need a `stat` of each candidate, and the
/// content index knows no folder, extension or type. Otherwise every
/// candidate up to the ceiling is read. Reading every stored candidate for
/// one page would be most of a query's cost, and on a cold disk thousands
/// of scattered reads.
///
/// "both" always reads to the ceiling: a file is ranked first when it
/// matches by name and by content, and a page that reads fewer candidates
/// may see only one of the two, so its order would change with the offset.
fn candidates_to_read(mode: &str, filter: &Filter, page: &Page) -> (bool, usize) {
    let stat_filters = filter.mtime.is_some() || filter.size.is_some();
    let path_filters = filter.under.is_some() || !filter.ext.is_empty() || !filter.mime.is_empty();
    let paged =
        !matches!(mode, "list" | "both") && !stat_filters && !(mode == "content" && path_filters);
    let fetch = if paged {
        // Room for names the post-filter turns away and files gone since.
        page.offset
            .saturating_add(page.limit.saturating_mul(2))
            .min(CANDIDATE_CEILING)
    } else {
        CANDIDATE_CEILING
    };
    (paged, fetch)
}

/// The hits inside `filter.under` with one of `filter.ext`. Cheap, path-only
/// filters run first: they shrink the set before any `stat`.
fn keep_path_matches(base: Vec<Hit>, filter: &Filter) -> Vec<Hit> {
    let under = filter.under.as_deref().map(|d| d.trim_end_matches('/'));
    let exts: Vec<String> = filter
        .ext
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    base.into_iter()
        .filter(|h| under.is_none_or(|d| is_under(&h.path, d)))
        // The extension is read once per candidate, not once per wanted
        // extension: `ext_of` builds a lowercase `String`, and inside `any` that
        // happened again for every extension in the filter.
        .filter(|h| {
            exts.is_empty() || {
                let ext = Path::new(&h.path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default();
                exts.iter().any(|wanted| wanted.eq_ignore_ascii_case(ext))
            }
        })
        .collect()
}

/// The hits of one of `filter.mime`, typed from the name index when the hit
/// carries no type.
fn keep_type_matches(
    kept: &mut Vec<Hit>,
    filter: &Filter,
    names: &tantivy::Searcher,
    name_fields: &crate::index::Fields,
) {
    if !filter.mime.is_empty() {
        kept.retain_mut(|h| {
            if h.mime.is_empty() {
                h.mime = indexed_mime(names, name_fields, &h.path);
            }
            filter
                .mime
                .iter()
                .any(|wanted| crate::mime::matches(&h.mime, wanted))
        });
    }
}

/// Expensive filters (date/size) need metadata: stat each survivor once.
fn keep_stat_matches(kept: &mut Vec<Hit>, filter: &Filter) {
    if filter.mtime.is_some() || filter.size.is_some() {
        kept.retain(|h| {
            let Some((mt, sz)) = crate::state::meta(Path::new(&h.path)) else {
                return false; // vanished between index and query
            };
            filter.mtime.is_none_or(|[lo, hi]| mt >= lo && mt <= hi)
                && filter.size.is_none_or(|[lo, hi]| sz >= lo && sz <= hi)
        });
    }
}

/// Fills in the extension, type and metadata of the visible page.
///
/// A hit that will not stat is one of two very different things, and the
/// caller renders `available == false` as "offline · on <device>". Deciding
/// that by existence alone would announce every *deleted* file as living on
/// an unplugged disk named after the user's home directory. Only a source
/// that is kept while unreachable (removable/network) may claim it; anything
/// else is simply gone and is dropped from the results.
fn enrich_page(hits: &mut Vec<Hit>, names: &tantivy::Searcher, name_fields: &crate::index::Fields) {
    let mut offline_sources = OfflineSources::default();
    hits.retain_mut(|h| {
        h.ext = ext_of(&h.path);
        if h.mime.is_empty() {
            h.mime = indexed_mime(names, name_fields, &h.path);
        }
        if let Some((mt, sz)) = crate::state::meta(Path::new(&h.path)) {
            h.mtime = mt;
            h.size = sz;
            h.available = true;
            return true;
        }
        offline_sources.is_retained_offline(Path::new(&h.path))
    });
}

/// Answers "is this path on a source we keep catalogued while it is unreachable?"
/// once per source instead of once per hit.
///
/// [`settings::is_available`] reads the source directory, so asking per result
/// would mean a `read_dir` for every missing file in a page.
#[derive(Default)]
pub(crate) struct OfflineSources {
    /// Source id → whether it is retained *and* currently unreachable.
    answered: std::collections::HashMap<String, bool>,
}

impl OfflineSources {
    pub(crate) fn is_retained_offline(&mut self, path: &Path) -> bool {
        let Some(source) = crate::settings::sources().owner(path) else {
            return false; // no configured owner → nothing claims it
        };
        if !source.retained_when_unavailable() {
            return false; // a normal source: a missing file is a deleted file
        }
        *self
            .answered
            .entry(source.id.clone())
            .or_insert_with(|| !crate::settings::is_available(source))
    }
}

/// Lowercase extension of `path` without the dot; empty when there is none.
fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Whether `path` is inside directory `dir` (or is `dir` itself).
fn is_under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Matches of `query` in the index, counted up to `cap`: the posting lists
/// are walked, nothing is scored and no document is read.
fn count_capped(searcher: &tantivy::Searcher, query: &dyn Query, cap: usize) -> Result<usize> {
    let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(
        searcher,
    ))?;
    let mut total = 0usize;
    for segment in searcher.segment_readers() {
        total += weight.count(segment)? as usize;
        if total >= cap {
            return Ok(cap);
        }
    }
    Ok(total)
}
