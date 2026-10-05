//! Name search: ngram recall via Tantivy, then exact substring post-filter.
use crate::index::{BODY_QUERY, content_fields, fields};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, Occur, Query, TermQuery,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TokenStream;
use tantivy::{IndexReader, TantivyDocument, Term};

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
    // Only what the page needs is read when the index can answer every filter
    // itself: the date and size filters need a `stat` of each candidate, and the
    // content index knows no folder, extension or type. Otherwise every
    // candidate up to the ceiling is read, as before. Reading 5 000 stored
    // documents for a page of 50 was most of a query's cost, and on a cold
    // disk thousands of scattered reads.
    let stat_filters = filter.mtime.is_some() || filter.size.is_some();
    let path_filters = filter.under.is_some() || !filter.ext.is_empty() || !filter.mime.is_empty();
    let reads_content = matches!(mode, "content" | "both");
    let paged = mode != "list" && !stat_filters && !(reads_content && path_filters);
    let fetch = if paged {
        // Room for names the post-filter turns away and files gone since.
        page.offset
            .saturating_add(page.limit.saturating_mul(2))
            .min(CANDIDATE_CEILING)
    } else {
        CANDIDATE_CEILING
    };
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
    // without reading documents; "both" adds the two counts, so a file that
    // matches by name and by content counts twice and the total can run
    // over, never under, what paging will reach.
    let counted = if paged {
        Some(match mode {
            "content" => count_content(content_reader()?, q, CANDIDATE_CEILING)?,
            "both" => (count_names(indexes.name, q, &Filter::default(), CANDIDATE_CEILING)?
                + count_content(content_reader()?, q, CANDIDATE_CEILING)?)
            .min(CANDIDATE_CEILING),
            _ => count_names(indexes.name, q, filter, CANDIDATE_CEILING)?,
        })
    } else {
        None
    };

    // Cheap, path-only filters first — shrink the set before any `stat`.
    let under = filter.under.as_deref().map(|d| d.trim_end_matches('/'));
    let exts: Vec<String> = filter
        .ext
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let mut kept: Vec<Hit> = base
        .into_iter()
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
        .collect();

    // A content hit carries no type of its own: the name index has it.
    let names = indexes.name.searcher();
    let name_fields = fields(names.index())?;
    if !filter.mime.is_empty() {
        kept.retain_mut(|h| {
            if h.mime.is_empty() {
                h.mime = indexed_mime(&names, &name_fields, &h.path);
            }
            filter
                .mime
                .iter()
                .any(|wanted| crate::mime::matches(&h.mime, wanted))
        });
    }

    // Expensive filters (date/size) need metadata: stat each survivor once.
    if filter.mtime.is_some() || filter.size.is_some() {
        kept.retain(|h| {
            let Some((mt, sz)) = crate::state::meta(Path::new(&h.path)) else {
                return false; // vanished between index and query
            };
            filter.mtime.is_none_or(|[lo, hi]| mt >= lo && mt <= hi)
                && filter.size.is_none_or(|[lo, hi]| sz >= lo && sz <= hi)
        });
    }

    let total = counted.map_or(kept.len(), |counted| counted.max(kept.len()));
    let next_offset = (page.offset + page.limit < total).then_some(page.offset + page.limit);

    // Page, then enrich only the visible window with metadata.
    let mut hits: Vec<Hit> = kept
        .into_iter()
        .skip(page.offset)
        .take(page.limit)
        .collect();
    // A hit that will not stat is one of two very different things, and the
    // caller renders `available == false` as "offline · on <device>". Deciding
    // that by existence alone announced every *deleted* file as living on an
    // unplugged disk named after the user's home directory — a wrong answer that
    // reads like a right one. Only a source that is kept while unreachable
    // (removable/network) may claim it; anything else is simply gone and is
    // dropped from the results.
    let mut offline_sources = OfflineSources::default();
    hits.retain_mut(|h| {
        h.ext = ext_of(&h.path);
        if h.mime.is_empty() {
            h.mime = indexed_mime(&names, &name_fields, &h.path);
        }
        if let Some((mt, sz)) = crate::state::meta(Path::new(&h.path)) {
            h.mtime = mt;
            h.size = sz;
            h.available = true;
            return true;
        }
        offline_sources.is_retained_offline(Path::new(&h.path))
    });
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

/// Answers "is this path on a source we keep catalogued while it is unreachable?"
/// once per source instead of once per hit.
///
/// [`settings::is_available`] reads the source directory, so asking per result
/// would mean a `read_dir` for every missing file in a page.
#[derive(Default)]
struct OfflineSources {
    /// Source id → whether it is retained *and* currently unreachable.
    answered: std::collections::HashMap<String, bool>,
}

impl OfflineSources {
    fn is_retained_offline(&mut self, path: &Path) -> bool {
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

/// Substring filename search. Returns up to `limit` paths whose file name contains
/// `q` (case-insensitive). Queries shorter than 2 chars yield nothing (ngram floor).
pub fn search(reader: &IndexReader, q: &str, filter: &Filter, limit: usize) -> Result<Vec<Hit>> {
    // One searcher, taken once: building an `IndexReader` per query re-opens and
    // mmaps every segment and rebuilds the term dictionaries. Tantivy's own docs
    // say a project should create at most one reader per index; the daemon now
    // holds them for its whole life and this only takes a snapshot.
    let searcher = reader.searcher();
    let index = searcher.index();
    let f = fields(index)?;
    let words = crate::index::name_words(q);
    if words.is_empty() {
        return Ok(Vec::new());
    }

    let Some(query) = name_query(&f, &words, filter) else {
        return Ok(Vec::new());
    };

    // Over-fetch for the post-filter, but cap the candidate pool so a huge
    // `limit` (e.g. -a) doesn't allocate an enormous TopDocs heap.
    let candidate_limit = limit.saturating_mul(4).clamp(40, 50_000);
    // The day of the last change breaks ties, and only ties: among equal
    // matches — the hundreds of "1.png" — the newest first instead of the
    // index's order. Measured as a stronger factor on 165 known-item queries
    // it moved 42 of them, as many down as up.
    let candidates = searcher.search(
        &query,
        &TopDocs::with_limit(candidate_limit).tweak_score(
            move |segment: &tantivy::SegmentReader| {
                let days = segment
                    .fast_fields()
                    .u64("mtime_day")
                    .ok()
                    .map(|column| column.first_or_default_col(0));
                move |doc: tantivy::DocId, score: tantivy::Score| {
                    (score, days.as_ref().map_or(0, |days| days.get_val(doc)))
                }
            },
        ),
    )?;

    let mut ranked: Vec<(NameMatch, Hit)> = Vec::with_capacity(limit.min(1024));
    let mut seen_paths = HashSet::with_capacity(limit.min(1024));
    for ((score, _), addr) in candidates {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) else {
            continue;
        };
        if seen_paths.contains(path) {
            continue;
        }
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The grams only say the letters are there; this says how.
        let Some(quality) = name_match(&name, &words) else {
            continue;
        };
        seen_paths.insert(path.to_string());
        let source = doc
            .get_first(f.source)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        ranked.push((
            quality,
            Hit {
                path: path.to_string(),
                name,
                score,
                source,
                mime: stored_mime(&doc, &f),
                ..Default::default()
            },
        ));
        if ranked.len() >= limit {
            break;
        }
    }
    // Best match first; within one kind of match, the index's order (a
    // shorter name first) stays.
    ranked.sort_by_key(|(quality, _)| std::cmp::Reverse(*quality));
    Ok(ranked.into_iter().map(|(_, hit)| hit).collect())
}

/// The query for names holding every word in `words`, narrowed by the index
/// clauses of `filter`; `None` when no word gives the index anything to look
/// up (a single CJK character).
///
/// Each word is matched on its own, so a name holds all of them in any order
/// (`raio bruno` finds `Raio-X Bruno ….pdf`). The grams are cut from the same
/// text the field indexes (`index::name_text`), spaces at word edges included.
/// Should clauses rank, they never widen: an exact name, then words that
/// start or end where the query does, are lifted above the rest, so they are
/// among the candidates even when only a page of them is read.
fn name_query(f: &crate::index::Fields, words: &[String], filter: &Filter) -> Option<BooleanQuery> {
    let term = |field, text: &str| -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::Basic,
        ))
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for word in words {
        let chars: Vec<char> = word.chars().collect();
        if is_whole_word_query(word) {
            // One or two letters stand for a name or a word that is exactly them:
            // "go" is "GO.pdf" or "Contrato GO.pdf", not "Google.pdf".
            let grams = if chars.len() == 1 {
                vec![format!(" {word} ")]
            } else {
                vec![format!(" {word}"), format!("{word} ")]
            };
            for gram in grams {
                clauses.push((Occur::Must, term(f.name, &gram)));
            }
            continue;
        }
        // Three-letter grams only: they imply the two-letter ones, whose
        // posting lists are the longest reads on a cold disk. One CJK
        // character is a gram of its own (`index::NameGrams`).
        let mut grams: Vec<String> = if chars.len() < 3 {
            vec![word.clone()]
        } else {
            chars.windows(3).map(|w| w.iter().collect()).collect()
        };
        grams.sort_unstable();
        grams.dedup();
        for gram in &grams {
            clauses.push((Occur::Must, term(f.name, gram)));
        }
        if chars.len() >= 2 {
            let start: String = chars[..2].iter().collect();
            let end: String = chars[chars.len() - 2..].iter().collect();
            for edge in [format!(" {start}"), format!("{end} ")] {
                clauses.push((
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(term(f.name, &edge), WORD_EDGE_SCORE)),
                ));
            }
        }
    }
    if !clauses.iter().any(|(occur, _)| *occur == Occur::Must) {
        return None;
    }
    clauses.push((
        Occur::Should,
        Box::new(ConstScoreQuery::new(
            term(f.stem, &words.join(" ")),
            WHOLE_NAME_SCORE,
        )),
    ));
    // The folder and the extensions narrow the index's own answer: filtered
    // only afterwards, the candidate ceiling was spent on whatever matched
    // the words, and `pr` found no video among 5 000 other files.
    clauses.extend(filter_clauses(f, filter));
    Some(BooleanQuery::new(clauses))
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

/// How many names answer `q` under `filter`, up to `cap`, from the index
/// alone: the grams say a name holds the letters, so this can count a rare
/// name the post-filter turns away.
fn count_names(reader: &IndexReader, q: &str, filter: &Filter, cap: usize) -> Result<usize> {
    let searcher = reader.searcher();
    let f = fields(searcher.index())?;
    match name_query(&f, &crate::index::name_words(q), filter) {
        Some(query) => count_capped(&searcher, &query, cap),
        None => Ok(0),
    }
}

/// Score that lifts a name that is exactly the query (see [`name_query`]).
const WHOLE_NAME_SCORE: f32 = 100_000.0;

/// Score added for each query word whose start or end is a word edge in the
/// name — far above what the grams' own BM25 adds, so it orders kinds of match
/// before name length does.
const WORD_EDGE_SCORE: f32 = 1000.0;

/// Whether `word` (folded) stands for a whole word of a name: one or two
/// letters of a spaced script. Two CJK characters are a word on their own and
/// are searched inside names like any longer word.
fn is_whole_word_query(word: &str) -> bool {
    word.chars().count() <= 2 && !word.chars().any(crate::cjk_tokenizer::is_unspaced)
}

/// How a name answers the query: the whole name (without its extension) is
/// the query; then the sum, over the query words, of 3 for a word of the name,
/// 2 for the start of one, 1 for a part of one. Ordered best last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NameMatch {
    whole_name: bool,
    words: u32,
}

/// `None` when the name does not hold every query word the way it must: a
/// short word as a word of its own, a longer one anywhere.
fn name_match(name: &str, words: &[String]) -> Option<NameMatch> {
    let text = crate::index::name_text(name);
    let name_words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    let mut quality = 0;
    for word in words {
        quality += if name_words.iter().any(|w| w == word) {
            3
        } else if is_whole_word_query(word) {
            return None;
        } else if name_words.iter().any(|w| w.starts_with(word.as_str())) {
            2
        } else if text.contains(word.as_str()) {
            1
        } else {
            return None;
        };
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(NameMatch {
        whole_name: crate::index::name_words(&stem) == words,
        words: quality,
    })
}

/// The index clauses for `filter.under`, `filter.ext` and `filter.mime`, none
/// for a filter without them: the folder is one term of the `dir` field, the
/// extensions and types terms of their own fields. `run_query` still checks
/// them afterwards, for the content modes that do not narrow by them.
fn filter_clauses(f: &crate::index::Fields, filter: &Filter) -> Vec<(Occur, Box<dyn Query>)> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    // The root holds everything and has no term of its own.
    if let Some(dir) = filter.under.as_deref().map(|d| d.trim_end_matches('/'))
        && !dir.is_empty()
    {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(f.dir, dir),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if !filter.ext.is_empty() {
        let any_ext: Vec<(Occur, Box<dyn Query>)> = filter
            .ext
            .iter()
            .map(|ext| {
                let ext = ext.trim_start_matches('.').to_lowercase();
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.ext, &ext),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(any_ext))));
    }
    // A type or a `major/*` group: the document holds both as terms.
    if !filter.mime.is_empty() {
        let any_mime: Vec<(Occur, Box<dyn Query>)> = filter
            .mime
            .iter()
            .map(|mime| {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.mime, mime),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(any_mime))));
    }
    clauses
}

/// The `list` mode: what the name index holds under `filter.under` with one
/// of `filter.ext`, up to `limit`. Both are answered by the index itself, so
/// the ceiling counts matches rather than a sample of the whole index: cut
/// first and filtered after, "every video in ~/Videos" came back empty from
/// an index of 170 000 files whose first 5 000 held none.
///
pub fn list_matching(reader: &IndexReader, filter: &Filter, limit: usize) -> Result<Vec<Hit>> {
    let clauses = filter_clauses(&fields(reader.searcher().index())?, filter);
    if clauses.is_empty() {
        return list_all(reader, limit);
    }
    collect_paths(reader, &BooleanQuery::new(clauses), limit)
}

/// Everything the name index holds, up to `limit`, in no particular order.
///
/// The order is the caller's business: `run_query` sorts what survives the
/// filters, and asking tantivy to rank documents against a query that is not
/// a query would only spend time producing a ranking nobody reads.
pub fn list_all(reader: &IndexReader, limit: usize) -> Result<Vec<Hit>> {
    collect_paths(reader, &AllQuery, limit)
}

/// The paths `query` matches, up to `limit`, as unranked hits.
fn collect_paths(reader: &IndexReader, query: &dyn Query, limit: usize) -> Result<Vec<Hit>> {
    let searcher = reader.searcher();
    let f = fields(searcher.index())?;
    let found = searcher.search(query, &TopDocs::with_limit(limit).order_by_score())?;
    let mut hits = Vec::with_capacity(found.len());
    let mut seen_paths = HashSet::with_capacity(found.len());
    for (_, addr) in found {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) else {
            continue;
        };
        if !seen_paths.insert(path.to_string()) {
            continue;
        }
        hits.push(Hit {
            path: path.to_string(),
            name: Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            source: doc
                .get_first(f.source)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            mime: stored_mime(&doc, &f),
            ..Default::default()
        });
    }
    Ok(hits)
}

/// The exact media type a name document stores; empty for none.
fn stored_mime(doc: &TantivyDocument, f: &crate::index::Fields) -> String {
    doc.get_first(f.mime)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The media type the name index holds for `path`, for a hit that came
/// through the content index; empty when it has none.
fn indexed_mime(searcher: &tantivy::Searcher, f: &crate::index::Fields, path: &str) -> String {
    let query = TermQuery::new(
        Term::from_field_text(f.path, path),
        IndexRecordOption::Basic,
    );
    searcher
        .search(&query, &TopDocs::with_limit(1).order_by_score())
        .ok()
        .and_then(|top| top.first().map(|(_, addr)| *addr))
        .and_then(|addr| searcher.doc::<TantivyDocument>(addr).ok())
        .map(|doc| stored_mime(&doc, f))
        .unwrap_or_default()
}

/// Full-text content search over the `body` field (word match, ranked by BM25).
pub fn search_content(reader: &IndexReader, q: &str, limit: usize) -> Result<Vec<Hit>> {
    let searcher = reader.searcher();
    let index = searcher.index();
    if !crate::settings::content_index_enabled() {
        return Ok(Vec::new());
    }
    let f = content_fields(index)?;
    let Some(query) = content_query(&searcher, q)? else {
        return Ok(Vec::new());
    };

    let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;

    let mut hits = Vec::with_capacity(top.len());
    let mut seen_paths = HashSet::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        if let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) {
            if !seen_paths.insert(path.to_string()) {
                continue;
            }
            let name = Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            hits.push(Hit {
                path: path.to_string(),
                name,
                score,
                source: crate::settings::source_name(Path::new(path)),
                // Found inside the file, not by its name. Said here rather than
                // only in `search_both`, so a plain `content` query carries it
                // too and the GUI can explain a result whose filename has
                // nothing to do with what was typed.
                match_kind: MatchKind::Body,
                ..Default::default()
            });
        }
    }
    Ok(hits)
}

/// The query for documents holding every word of `q`, the last one as typed
/// so far (see [`word_query`]); `None` for a query with no word.
fn content_query(searcher: &tantivy::Searcher, q: &str) -> Result<Option<BooleanQuery>> {
    let index = searcher.index();
    let f = content_fields(index)?;
    // Reader policy follows the index it opened, not the current writer's
    // memory profile/config. Resolve once, rather than rereading configuration
    // for every term; requesting frequencies from Basic cannot invent them.
    let schema = index.schema();
    let indexing = match schema.get_field_entry(f.body).field_type() {
        tantivy::schema::FieldType::Str(options) => options.get_indexing_options(),
        _ => None,
    }
    .context("body must be an indexed text field")?;
    let record = if indexing.index_option().has_freq() {
        IndexRecordOption::WithFreqs
    } else {
        IndexRecordOption::Basic
    };
    let mut analyzer = index
        .tokenizers()
        .get(BODY_QUERY)
        .context("body query tokenizer")?;
    let mut words: Vec<String> = Vec::new();
    let mut stream = analyzer.token_stream(q);
    while stream.advance() {
        words.push(stream.token().text.clone());
    }
    let Some(last) = words.pop() else {
        return Ok(None);
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(words.len() + 1);
    for word in &words {
        clauses.push((
            Occur::Must,
            word_query(searcher, f.body, word, record, false)?,
        ));
    }
    clauses.push((
        Occur::Must,
        word_query(searcher, f.body, &last, record, true)?,
    ));
    Ok(Some(BooleanQuery::new(clauses)))
}

/// Shortest last word read as a prefix: shorter ones begin too many words.
const MIN_PREFIX_CHARS: usize = 3;
/// Most indexed words a prefix stands for.
const MAX_PREFIX_TERMS: usize = 32;
/// Most documents, summed over the words a prefix stands for, read for it —
/// the bound that matters on a cold disk, where reads follow the postings.
const PREFIX_DOC_BUDGET: u64 = 50_000;
/// Indexed words looked at per segment for a prefix, against a very short
/// one in a large vocabulary.
const MAX_SCANNED_TERMS: usize = 20_000;

/// One word of a content query: the word itself, ranked first, or its plural
/// or singular (see [`inflections`]) — "relatorios" finds "relatorio", "acao"
/// finds "acoes". The last word is also read as typed so far: it finds any
/// indexed word it begins — as-you-type in a launcher — the most frequent
/// first (see [`words_beginning`]). A Russian or Ukrainian word also matches
/// its other case endings (see [`slavic_stem`]). Every form keeps its BM25
/// score. A CJK bigram or character is matched exactly, and a last word
/// shorter than [`MIN_PREFIX_CHARS`] begins nothing.
fn word_query(
    searcher: &tantivy::Searcher,
    field: tantivy::schema::Field,
    word: &str,
    record: IndexRecordOption,
    last: bool,
) -> Result<Box<dyn Query>> {
    let exact = TermQuery::new(Term::from_field_text(field, word), record);
    if word.chars().any(crate::cjk_tokenizer::is_unspaced) {
        return Ok(Box::new(exact));
    }
    // Only forms the index holds: a query term for a word no document has
    // would cost a lookup for nothing on every segment.
    let mut forms: Vec<String> = inflections(word)
        .into_iter()
        .filter(|form| {
            searcher.segment_readers().iter().any(|segment| {
                segment.inverted_index(field).is_ok_and(|inverted| {
                    inverted
                        .terms()
                        .get(form.as_bytes())
                        .is_ok_and(|info| info.is_some())
                })
            })
        })
        .collect();
    if let Some(stem) = slavic_stem(word) {
        forms.extend(words_beginning(searcher, field, stem, |rest| {
            rest.is_empty() || SLAVIC_ENDINGS.contains(&rest)
        })?);
    }
    if last && word.chars().count() >= MIN_PREFIX_CHARS {
        forms.extend(words_beginning(searcher, field, word, |rest| {
            !rest.is_empty()
        })?);
    }
    Ok(with_forms(exact, forms, field, record))
}

/// `exact`, ranked first, or any of `forms`.
fn with_forms(
    exact: TermQuery,
    mut forms: Vec<String>,
    field: tantivy::schema::Field,
    record: IndexRecordOption,
) -> Box<dyn Query> {
    if forms.is_empty() {
        return Box::new(exact);
    }
    forms.sort_unstable();
    forms.dedup();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(
        Occur::Should,
        Box::new(BoostQuery::new(Box::new(exact), EXACT_WORD_BOOST)),
    )];
    for form in forms {
        let term = Term::from_field_text(field, &form);
        clauses.push((Occur::Should, Box::new(TermQuery::new(term, record))));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// The indexed words that begin with `prefix` and go on with a rest `keep`
/// accepts, the most frequent first, within [`MAX_PREFIX_TERMS`] and
/// [`PREFIX_DOC_BUDGET`].
fn words_beginning(
    searcher: &tantivy::Searcher,
    field: tantivy::schema::Field,
    prefix: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let upper = prefix_end(prefix.as_bytes());
    let mut doc_freqs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(field)?;
        let mut range = inverted.terms().range().ge(prefix.as_bytes());
        if let Some(upper) = &upper {
            range = range.lt(upper);
        }
        let mut terms = range.into_stream()?;
        let mut scanned = 0;
        while scanned < MAX_SCANNED_TERMS && terms.advance() {
            scanned += 1;
            if let Ok(text) = std::str::from_utf8(terms.key())
                && text.strip_prefix(prefix).is_some_and(&keep)
            {
                *doc_freqs.entry(text.to_owned()).or_default() += u64::from(terms.value().doc_freq);
            }
        }
    }
    let mut expansions: Vec<(String, u64)> = doc_freqs.into_iter().collect();
    expansions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut forms = Vec::new();
    let mut read = 0u64;
    for (text, doc_freq) in expansions.into_iter().take(MAX_PREFIX_TERMS) {
        if read > 0 && read + doc_freq > PREFIX_DOC_BUDGET {
            break;
        }
        read += doc_freq;
        forms.push(text);
    }
    Ok(forms)
}

/// The plural and singular forms of a folded Portuguese, Spanish or English
/// word, by suffix: "acao" ↔ "acoes", "responsavel" ↔ "responsaveis",
/// "imagem" ↔ "imagens", "flor" ↔ "flores", "relatorio" ↔ "relatorios".
///
/// Snowball stemming was measured on this index's own vocabulary first: on
/// folded words it joined none of the -ões/-ais/-eis/-ns plurals to their
/// singulars and merged "casa" with "caso", "casado" and "case". These rules
/// joined every such pair the vocabulary held (670 -ões, 421 -ais, 261 -eis,
/// 194 -ns, 93 % of -s), in both directions. A form counts only if it has four
/// letters or more (against "tres" → "tre", "pais" → "pai"), and it ranks below
/// the word as typed.
fn inflections(word: &str) -> Vec<String> {
    const PLURAL_TO_SINGULAR: &[(&str, &[&str])] = &[
        ("oes", &["ao"]),
        ("aes", &["ao"]),
        ("aos", &["ao"]),
        ("ais", &["al"]),
        ("eis", &["el", "il"]),
        ("ois", &["ol"]),
        ("uis", &["ul"]),
        ("ns", &["m"]),
        ("res", &["r"]),
        ("zes", &["z"]),
        ("ses", &["s"]),
        ("xes", &["x"]),
        ("ches", &["ch"]),
        ("shes", &["sh"]),
        ("s", &[""]),
    ];
    const SINGULAR_TO_PLURAL: &[(&str, &[&str])] = &[
        ("ao", &["oes", "aes", "aos"]),
        ("al", &["ais"]),
        ("el", &["eis"]),
        ("il", &["eis"]),
        ("ol", &["ois"]),
        ("ul", &["uis"]),
        ("m", &["ns"]),
        ("r", &["res"]),
        ("z", &["zes"]),
        ("s", &["ses"]),
        ("x", &["xes"]),
        ("ch", &["ches"]),
        ("sh", &["shes"]),
        ("", &["s"]),
    ];
    let mut forms: Vec<String> = Vec::new();
    if !word.is_ascii() || word.len() < 4 {
        return forms;
    }
    for (from, tos) in PLURAL_TO_SINGULAR.iter().chain(SINGULAR_TO_PLURAL) {
        let Some(base) = word.strip_suffix(from) else {
            continue;
        };
        if base.len() < 2 {
            continue;
        }
        for to in *tos {
            let form = format!("{base}{to}");
            if form.len() >= 4 && form != word && !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    forms
}

/// Russian and Ukrainian noun, adjective and participle endings, folded ("ё"
/// is "е"): "квартал", "квартала", "квартале", "кварталами" are one word.
const SLAVIC_ENDINGS: &[&str] = &[
    "а", "я", "о", "е", "ы", "и", "у", "ю", "ь", "і", "ї", "є", "ом", "ем", "ой", "ей", "ою", "ею",
    "ам", "ям", "ах", "ях", "ов", "ев", "ів", "їв", "ий", "ій", "ия", "ие", "ии", "ию", "ью", "ья",
    "ье", "ьи", "ый", "ая", "яя", "ое", "ее", "ые", "ым", "им", "ім", "ую", "юю", "ых", "их", "іх",
    "ої", "ові", "еві", "ами", "ями", "ого", "его", "ього", "ому", "ему", "ьому", "ыми", "ими",
    "іми", "ием", "иям", "иях", "ьям", "ьях", "ьев", "иями", "ьями",
];

/// The stem of a folded Russian or Ukrainian word: the word without the
/// longest of [`SLAVIC_ENDINGS`] that leaves four letters or more, or the word
/// itself ("квартал"). A shorter stem joins unrelated words: "сто" (a
/// hundred) would find "стой" (stop) and "стою" (I stand).
fn slavic_stem(word: &str) -> Option<&str> {
    if !word
        .chars()
        .all(|c| matches!(c, 'а'..='я' | 'і' | 'ї' | 'є' | 'ґ'))
    {
        return None;
    }
    let stem = SLAVIC_ENDINGS
        .iter()
        .filter_map(|ending| word.strip_suffix(ending))
        .filter(|stem| stem.chars().count() >= 4)
        .min_by_key(|stem| stem.len())
        .unwrap_or(word);
    (stem.chars().count() >= 4).then_some(stem)
}

/// The word as typed weighs this much more than the words it begins.
const EXACT_WORD_BOOST: f32 = 2.0;

/// The first byte string after every string starting with `prefix`; `None`
/// when there is none (all 0xFF).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// How many documents hold `q`'s words, up to `cap`, from the postings alone.
fn count_content(reader: &IndexReader, q: &str, cap: usize) -> Result<usize> {
    let searcher = reader.searcher();
    match content_query(&searcher, q)? {
        Some(query) => count_capped(&searcher, &query, cap),
        None => Ok(0),
    }
}

/// One file while the two searches are being merged, with the score each index
/// gave it kept apart.
///
/// The two are BM25 over different indexes and different fields, so they are not
/// points on one scale and adding them means nothing. Keeping them apart until
/// the sort is what lets each tier be ordered by the evidence that tier actually
/// has.
struct Candidate {
    hit: Hit,
    name_match: Option<NameMatch>,
    name_score: Option<f32>,
    body_score: Option<f32>,
}

impl Candidate {
    /// Which band this file sorts in; lower comes first.
    ///
    /// Both indexes agreeing about a file beats a filename match, which beats a
    /// match found only inside the file. This is the order the function has
    /// always promised; what changed is that it is now the FIRST sort key rather
    /// than something inferred from arithmetic on incomparable scores.
    fn tier(&self) -> u8 {
        match self.hit.match_kind {
            MatchKind::Both => 0,
            MatchKind::Name => 1,
            MatchKind::Body => 2,
        }
    }

    /// The hit, carrying the relevance of its own band.
    ///
    /// A name or both match reports its filename score, a body match its content
    /// score. Never a blend: a number that mixes the two would be a scale no
    /// caller can reason about, which is how the old ranking lost the name score
    /// in the first place.
    fn into_hit(mut self) -> Hit {
        self.hit.score = self.name_score.or(self.body_score).unwrap_or_default();
        self.hit
    }
}

/// Filename ∪ content matches, ranked by band and then by relevance within it:
/// files matching in **both** name and content rank highest, then name-only,
/// then content-only.
///
/// **The order is total, so it is the same on every run for the same input.** It
/// used to be decided by `HashMap` iteration: every filename hit had its BM25
/// overwritten with a constant, so all name-only results tied and a stable sort
/// preserved whatever order the merge happened to hand it — a different one each
/// process. The same query could answer in two orders with nothing having changed
/// on disk.
///
/// Within `Both`, the filename score leads and the content score is secondary
/// evidence: a strong filename hit must not be pushed down by a weak body match.
///
/// The final tie-break is the path — raw, not folded, because determinism is all
/// it has to provide and folding would allocate a second string per candidate for
/// a cosmetic ordering nobody sees. **That tie-break carries far more weight than
/// it looks like it should**, and it is worth knowing why: the name field is
/// indexed as ngrams with [`IndexRecordOption::Basic`], which stores no term
/// frequencies, so BM25 over it gives every document matching the same grams the
/// SAME score. Measured — `alpha.md` and `alpha_alpha.md` both score 1.5297492
/// for `alpha`. The name band is therefore mostly one big tie, and the path is
/// what actually orders it. The content band does rank, because the body field
/// keeps frequencies.
pub fn search_both(
    reader: &IndexReader,
    content_reader: &IndexReader,
    q: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    use std::collections::HashMap;

    let names = search(reader, q, &Filter::default(), limit)?;
    let contents = search_content(content_reader, q, limit)?;

    let mut merged: HashMap<String, Candidate> =
        HashMap::with_capacity(names.len() + contents.len());
    // The kind of name match (whole name, word, start of a word, part) leads
    // the name score inside a band, as it does in a name search.
    let words = crate::index::name_words(q);
    for hit in names {
        let name_score = hit.score;
        let name_match = name_match(&hit.name, &words);
        merged.insert(
            hit.path.clone(),
            Candidate {
                hit,
                name_match,
                name_score: Some(name_score),
                body_score: None,
            },
        );
    }
    for hit in contents {
        match merged.get_mut(&hit.path) {
            // Both indexes answered. The name hit is the one kept, because its
            // `source` came from the indexed document rather than from a lookup.
            Some(candidate) => {
                candidate.hit.match_kind = MatchKind::Both;
                candidate.body_score = Some(hit.score);
            }
            None => {
                let body_score = hit.score;
                merged.insert(
                    hit.path.clone(),
                    Candidate {
                        hit,
                        name_match: None,
                        name_score: None,
                        body_score: Some(body_score),
                    },
                );
            }
        }
    }

    let mut candidates: Vec<Candidate> = merged.into_values().collect();
    // Descending on both scores, ascending on the path: a hit missing a score
    // for a band it is not in compares as zero, which never matters because the
    // band is compared first.
    let score_of = |score: Option<f32>| score.unwrap_or(f32::NEG_INFINITY);
    candidates.sort_by(|a, b| {
        a.tier()
            .cmp(&b.tier())
            .then_with(|| b.name_match.cmp(&a.name_match))
            .then_with(|| score_of(b.name_score).total_cmp(&score_of(a.name_score)))
            .then_with(|| score_of(b.body_score).total_cmp(&score_of(a.body_score)))
            .then_with(|| a.hit.path.cmp(&b.hit.path))
    });

    Ok(candidates
        .into_iter()
        .take(limit)
        .map(Candidate::into_hit)
        .collect())
}

/// How many entries are cataloged (alive indexed documents — files and the
/// visible directories the scan indexes — with deletions excluded).
pub fn count(reader: &IndexReader) -> Result<u64> {
    crate::index::document_count(reader)
}

/// Every cataloged path, sorted and deduped. `under` keeps only paths
/// inside that directory (or equal to it); `limit` caps the output.
pub fn list_paths(reader: &IndexReader, limit: usize, under: Option<&str>) -> Result<Vec<String>> {
    let searcher = reader.searcher();
    let f = fields(searcher.index())?;
    let under = under.map(|d| d.trim_end_matches('/'));
    let mut paths: Vec<String> = Vec::new();
    // Walk each segment's document store directly; `alive_bitset` skips the
    // tombstones the content pass leaves behind when it upserts a doc.
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(0)?;
        for doc in store.iter::<TantivyDocument>(seg.alive_bitset()) {
            let doc = doc?;
            let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(dir) = under {
                let inside =
                    path == dir || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));
                if !inside {
                    continue;
                }
            }
            paths.push(path.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths.truncate(limit);
    Ok(paths)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::index::{fields, open_or_create};
    use tantivy::doc;

    fn index_with(label: &str, paths: &[&str]) -> tantivy::IndexReader {
        let dir = std::env::temp_dir().join(format!("lsearch-test-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = open_or_create(&dir).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        for p in paths {
            crate::scan::add_name_only(&writer, &f, Path::new(p), None).unwrap();
        }
        writer.commit().unwrap();
        index.reader().unwrap()
    }

    fn content_index_with(label: &str, entries: &[(&str, &str)]) -> tantivy::IndexReader {
        content_index_with_profile(
            label,
            entries,
            tantivy::schema::IndexRecordOption::WithFreqs,
        )
    }

    fn content_index_with_profile(
        label: &str,
        entries: &[(&str, &str)],
        record: tantivy::schema::IndexRecordOption,
    ) -> tantivy::IndexReader {
        let dir = std::env::temp_dir().join(format!(
            "lsearch-test-{}-{label}-content",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let index = crate::index::create_content_fixture(&dir, record).unwrap();
        let f = content_fields(&index).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        for (path, body) in entries {
            writer
                .add_document(doc!(f.path => *path, f.body => *body))
                .unwrap();
        }
        writer.commit().unwrap();
        index.reader().unwrap()
    }

    #[test]
    fn substring_case_insensitive() {
        let index = index_with(
            "subs",
            &["/a/Report_2024.md", "/b/hello.txt", "/c/photo.JPG"],
        );
        let hits = search(&index, "report", &Filter::default(), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Report_2024.md");
        assert_eq!(
            search(&index, "JPG", &Filter::default(), 10).unwrap().len(),
            1
        );
        assert_eq!(
            search(&index, "jpg", &Filter::default(), 10).unwrap().len(),
            1
        );
    }

    /// "Every PDF in my home folder" has no words to search for.
    ///
    /// The name search refuses a query under two characters, because the ngram
    /// index cannot narrow one — so a caller narrowing purely by filter had no
    /// way in at all, and the file manager's Library would have had to walk
    /// the disk itself beside an index that already knew the answer.
    #[test]
    fn the_list_mode_answers_a_query_with_no_words_in_it() {
        let root = std::env::temp_dir().join(format!("lsearch-list-{}", std::process::id()));
        let mine = root.join("me");
        let other = root.join("elsewhere");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        // Real files: a hit whose file will not `stat` is dropped as gone, so
        // a fixture of invented paths would prove nothing either way.
        let wanted = mine.join("a.pdf");
        let paths = [wanted.clone(), mine.join("b.png"), other.join("c.pdf")];
        for path in &paths {
            std::fs::write(path, b"x").unwrap();
        }
        let listed: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let index = index_with(
            "list-mode",
            &listed.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let filter = Filter {
            under: Some(mine.to_string_lossy().into_owned()),
            ext: vec!["pdf".to_string()],
            ..Default::default()
        };
        let run = |mode: &str| {
            run_query(
                QueryIndexes {
                    name: &index,
                    content: None,
                },
                "",
                mode,
                &filter,
                &Page {
                    offset: 0,
                    limit: 50,
                },
                false,
                None,
            )
            .unwrap()
        };
        let outcome = run("list");
        let found: Vec<&str> = outcome.hits.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(
            found,
            [wanted.to_string_lossy().as_ref()],
            "the png and the pdf outside the folder are filtered out"
        );
        // The same filter through the name search finds nothing, which is the
        // whole reason the mode exists.
        assert!(run("name").hits.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The folder and the extensions narrow the index before the ceiling: a
    /// sample cut first (the first `limit` documents of all) held none of
    /// the videos, and the listing came back empty.
    #[test]
    fn a_type_filter_finds_videos_by_what_they_are() {
        let dir = std::env::temp_dir().join(format!("lsearch-mime-query-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut stream = vec![0_u8; 188 * 4];
        stream.iter_mut().step_by(188).for_each(|byte| *byte = 0x47);
        let files: [(&str, &[u8]); 4] = [
            ("clip.ts", &stream),
            ("recording", &stream),
            ("app.ts", b"export const clip = 1;\n"),
            ("film.mp4", b"not read: the name tells"),
        ];
        let paths: Vec<String> = files
            .iter()
            .map(|(name, bytes)| {
                let path = dir.join(name);
                std::fs::write(&path, bytes).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("mime-filter", &refs);
        let filter = Filter {
            mime: vec!["video/*".into()],
            ..Default::default()
        };
        let outcome = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "",
            "list",
            &filter,
            &Page {
                offset: 0,
                limit: 50,
            },
            false,
            None,
        )
        .unwrap();
        let mut found: Vec<(String, String)> = outcome
            .hits
            .iter()
            .map(|hit| (hit.name.clone(), hit.mime.clone()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("clip.ts".to_string(), "video/mp2t".to_string()),
                ("film.mp4".to_string(), "video/mp4".to_string()),
                ("recording".to_string(), "video/mp2t".to_string()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn names(hits: Vec<Hit>) -> Vec<String> {
        hits.into_iter().map(|hit| hit.name).collect()
    }

    /// One or two letters find the names that are exactly them, then the names
    /// where they stand as a word — never a name that merely contains them.
    #[test]
    fn short_queries_find_whole_names_then_whole_words() {
        let index = index_with(
            "short-words",
            &[
                "/h/Contrato GO.pdf",
                "/h/Google.pdf",
                "/h/algoritmo.odt",
                "/h/GO.pdf",
                "/h/Relatório-SP-2026.odt",
                "/h/a.txt",
                "/h/casa.txt",
            ],
        );
        let found = names(search(&index, "go", &Filter::default(), 20).unwrap());
        assert_eq!(found, ["GO.pdf", "Contrato GO.pdf"]);
        assert_eq!(
            names(search(&index, "SP", &Filter::default(), 20).unwrap()),
            ["Relatório-SP-2026.odt"]
        );
        assert_eq!(
            names(search(&index, "a", &Filter::default(), 20).unwrap()),
            ["a.txt"]
        );
    }

    /// A name is found the way a person types it, whatever script, accents,
    /// decomposition or letter forms it was written with.
    #[test]
    fn names_match_across_scripts_and_letter_forms() {
        let index = index_with(
            "scripts",
            &[
                "/h/Straße Bericht.txt",
                "/h/İstanbul raporu.txt",
                "/h/Отчёт о продажах.txt",
                "/h/Αναφορά πωλήσεων.txt",
                "/h/Łódź.pdf",
                "/h/Relato\u{301}rio de vendas.pdf",
            ],
        );
        for (query, name) in [
            ("strasse", "Straße Bericht.txt"),
            ("istanbul", "İstanbul raporu.txt"),
            ("отчет", "Отчёт о продажах.txt"),
            ("αναφορα", "Αναφορά πωλήσεων.txt"),
            ("lodz", "Łódź.pdf"),
            ("relatorio", "Relato\u{301}rio de vendas.pdf"),
        ] {
            assert_eq!(
                names(search(&index, query, &Filter::default(), 20).unwrap()),
                [name],
                "query {query:?}"
            );
        }
    }

    /// The whole name first, then names holding the word, then names where a
    /// word starts with it, then the rest.
    #[test]
    fn names_rank_by_how_they_match() {
        let index = index_with(
            "name-rank",
            &[
                "/h/xrelatoriox.txt",
                "/h/pre-relatorios-antigos.pdf",
                "/h/meu relatorio de vendas.pdf",
                "/h/relatorio a.md",
                // The whole name, though longer than "relatorio a.md".
                "/h/relatorio.markdown",
            ],
        );
        assert_eq!(
            names(search(&index, "relatorio", &Filter::default(), 20).unwrap()),
            [
                "relatorio.markdown",
                "relatorio a.md",
                "meu relatorio de vendas.pdf",
                "pre-relatorios-antigos.pdf",
                "xrelatoriox.txt"
            ]
        );
    }

    /// Among names that match equally, the one changed most recently first.
    #[test]
    fn equal_names_rank_newest_first() {
        let dir = std::env::temp_dir().join(format!("lsearch-recent-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let day = std::time::Duration::from_secs(86_400);
        let now = std::time::SystemTime::now();
        let mut paths = Vec::new();
        for (folder, age_days) in [("old", 400), ("new", 1), ("mid", 30)] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
            let path = dir.join(folder).join("1.png");
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(now - day * age_days).unwrap();
            paths.push(path.to_string_lossy().into_owned());
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("recent", &refs);
        let found: Vec<String> = search(&index, "1", &Filter::default(), 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        assert_eq!(
            found,
            [paths[1].clone(), paths[2].clone(), paths[0].clone()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_name_search_finds_videos_past_a_ceiling_of_other_files() {
        let mut paths: Vec<String> = (0..40)
            .map(|i| format!("/home/u/src/prompt-{i}.rs"))
            .collect();
        paths.extend([
            "/home/u/Videos/Promo.mkv".to_string(),
            "/home/u/Videos/prores.mov".to_string(),
            "/home/u/Music/prologue.mp4".to_string(),
        ]);
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("name-ceiling", &refs);
        let filter = Filter {
            under: Some("/home/u/Videos".into()),
            ext: vec!["mkv".into(), "mov".into(), "mp4".into()],
            ..Default::default()
        };
        // A ceiling smaller than the other files that match the word.
        let mut found: Vec<String> = search(&index, "pro", &filter, 5)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        found.sort();
        assert_eq!(
            found,
            ["/home/u/Videos/Promo.mkv", "/home/u/Videos/prores.mov"]
        );
        // No filter, no narrowing: the words alone decide.
        assert_eq!(
            search(&index, "prompt", &Filter::default(), 50)
                .unwrap()
                .len(),
            40
        );
    }

    #[test]
    fn list_finds_matches_past_a_ceiling_of_other_files() {
        let mut paths: Vec<String> = (0..40)
            .map(|i| format!("/home/u/docs/note-{i}.txt"))
            .collect();
        paths.extend([
            "/home/u/Videos/show.mp4".to_string(),
            "/home/u/Videos/sub/film.MKV".to_string(),
            "/home/u/Videos/cover.png".to_string(),
            "/home/u/Videos/almost.mp4.part".to_string(),
            "/home/u/Videos2/other.mp4".to_string(),
            "/home/u/Videos-old/old.mp4".to_string(),
            "/home/u/docs/talk.mp4".to_string(),
        ]);
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("list-ceiling", &refs);
        let filter = Filter {
            under: Some("/home/u/Videos/".into()),
            ext: vec!["mp4".into(), "mkv".into()],
            ..Default::default()
        };
        // A ceiling smaller than the files before the videos.
        let mut found: Vec<String> = list_matching(&index, &filter, 5)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        found.sort();
        // `almost.mp4.part` is a `.part`, whatever its name holds; `Videos2`
        // and `Videos-old` only begin like the folder.
        assert_eq!(
            found,
            ["/home/u/Videos/show.mp4", "/home/u/Videos/sub/film.MKV"]
        );
        let outcome = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "",
            "list",
            &filter,
            &Page {
                offset: 0,
                limit: 50,
            },
            false,
            None,
        )
        .unwrap();
        let mut exact: Vec<&str> = outcome.hits.iter().map(|hit| hit.path.as_str()).collect();
        exact.sort_unstable();
        assert!(
            exact.iter().all(|path| !path.ends_with(".part")),
            "{exact:?}"
        );
    }

    #[test]
    fn name_search_dedups_stale_duplicate_paths() {
        let index = index_with(
            "dup",
            &[
                "/a/wallpaper-example.png",
                "/a/wallpaper-example.png",
                "/b/wallpaper-other.png",
            ],
        );
        let hits = search(&index, "wallpaper", &Filter::default(), 10).unwrap();
        let mut paths: Vec<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["/a/wallpaper-example.png", "/b/wallpaper-other.png"]
        );
    }

    /// Several words match a name that holds them all, in any order and with
    /// anything in between. `raio bruno` used to find nothing while `raio` and
    /// `bruno` each found the file.
    #[test]
    fn every_typed_word_matches_separately() {
        let index = index_with(
            "words",
            &[
                "/d/Raio-X Bruno Goncalves 29 de outubro de 2021 (3).pdf",
                "/d/Bruno currículo.pdf",
            ],
        );
        let paths = |q: &str| -> Vec<String> {
            let mut found: Vec<String> = search(&index, q, &Filter::default(), 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect();
            found.sort();
            found
        };
        let raio_x = "/d/Raio-X Bruno Goncalves 29 de outubro de 2021 (3).pdf".to_string();
        assert_eq!(paths("raio bruno"), vec![raio_x.clone()]);
        // Order is irrelevant, and so is what sits between the words.
        assert_eq!(paths("bruno raio"), vec![raio_x.clone()]);
        assert_eq!(paths("raio 2021"), vec![raio_x.clone()]);
        // Every word still has to be there.
        assert!(paths("raio inexistente").is_empty());
        // A word shared by both files keeps both.
        assert_eq!(paths("bruno").len(), 2);
    }

    #[test]
    fn rejects_short_and_misses() {
        let index = index_with("short", &["/a/hello.txt"]);
        assert!(
            search(&index, "h", &Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            search(&index, "zzzz", &Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn no_false_positive_from_ngram_recall() {
        // "ab" + "cd" grams both present but "abcd" is not a substring of either name.
        let index = index_with("fp", &["/x/ab_xx.txt", "/y/cd_yy.txt"]);
        assert!(
            search(&index, "abcd", &Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn content_word_search() {
        let dir = std::env::temp_dir().join(format!("lsearch-test-{}-body", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = index_with("body-name", &["/n/report.md"]);
        let content_index =
            content_index_with("body", &[("/n/report.md", "the quick brown fox jumps")]);

        assert_eq!(
            search_content(&content_index, "quick", 10).unwrap().len(),
            1
        );
        assert_eq!(
            search_content(&content_index, "QUICK", 10).unwrap().len(),
            1
        ); // lowercased
        assert!(
            search_content(&content_index, "missing", 10)
                .unwrap()
                .is_empty()
        );
        // content word is not a filename match
        assert!(
            search(&index, "quick", &Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn both_ranks_name_and_content_highest() {
        let dir = std::env::temp_dir().join(format!("lsearch-test-{}-rank", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = index_with(
            "rank-name",
            &["/x/alpha_report.md", "/y/other.md", "/z/alpha_notes.md"],
        );
        let content_index = content_index_with(
            "rank",
            &[
                ("/x/alpha_report.md", "nothing here"),
                ("/y/other.md", "alpha alpha alpha mention"),
                ("/z/alpha_notes.md", "an alpha note"),
            ],
        );
        let _ = dir;
        let hits = search_both(&index, &content_index, "alpha", 10).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        // both-match ranks first; name-only above content-only.
        assert_eq!(paths[0], "/z/alpha_notes.md");
        let name_only = paths
            .iter()
            .position(|p| *p == "/x/alpha_report.md")
            .unwrap();
        let content_only = paths.iter().position(|p| *p == "/y/other.md").unwrap();
        assert!(name_only < content_only);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Relevance survives the merge inside every band, and the band a hit is in
    /// is reported to the client.
    ///
    /// The old merge overwrote each filename hit's BM25 with a constant, so every
    /// name-only result tied at exactly the same score. This asserts the thing
    /// that constant destroyed: two name-only files whose names differ in how
    /// well they match come back in that order, and the same for two body-only
    /// files.
    #[test]
    fn both_keeps_relevance_inside_each_band() {
        // Three name matches for "alpha", one of which also matches in content.
        let index = index_with(
            "band-name",
            &["/x/alpha.md", "/y/alpha_alpha.md", "/z/both_alpha.md"],
        );
        let content_index = content_index_with(
            "band",
            &[
                ("/z/both_alpha.md", "alpha"),
                // Two body-only files, one mentioning the word far more often:
                // BM25 must still separate them.
                ("/q/plain.md", "alpha"),
                ("/r/many.md", "alpha alpha alpha alpha alpha alpha"),
            ],
        );
        let hits = search_both(&index, &content_index, "alpha", 10).unwrap();

        // The band is on the wire, and it is right for each kind.
        let kind_of = |path: &str| {
            hits.iter()
                .find(|hit| hit.path == path)
                .unwrap_or_else(|| panic!("{path} is missing from {hits:?}"))
                .match_kind
        };
        assert_eq!(kind_of("/z/both_alpha.md"), MatchKind::Both);
        assert_eq!(kind_of("/x/alpha.md"), MatchKind::Name);
        assert_eq!(kind_of("/r/many.md"), MatchKind::Body);

        // The bands do not interleave.
        let tier = |hit: &Hit| match hit.match_kind {
            MatchKind::Both => 0,
            MatchKind::Name => 1,
            MatchKind::Body => 2,
        };
        let tiers: Vec<u8> = hits.iter().map(tier).collect();
        assert!(
            tiers.windows(2).all(|pair| pair[0] <= pair[1]),
            "a band leaked into another: {tiers:?}"
        );

        // Within the body band, the file mentioning the word six times outranks
        // the one mentioning it once. This is the assertion the old code could
        // not pass for names, and it must not regress for bodies either.
        let position = |path: &str| hits.iter().position(|hit| hit.path == path).unwrap();
        assert!(
            position("/r/many.md") < position("/q/plain.md"),
            "content relevance was flattened: {hits:?}"
        );

        // **The assertion the old constant made impossible.** `search` alone IS
        // the filename-relevance order, so the name band of a merged result has
        // to agree with it. Overwriting each name score with `NAME_WEIGHT` tied
        // them all, and a tie is decided by whatever order the merge happened to
        // hand the sort — so this agreement was luck, not ranking. Derived from
        // `search` rather than hardcoded, so the test follows BM25 if it changes.
        let name_only_order: Vec<String> = search(&index, "alpha", &Filter::default(), 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .filter(|path| kind_of(path) == MatchKind::Name)
            .collect();
        let merged_name_order: Vec<String> = hits
            .iter()
            .filter(|hit| hit.match_kind == MatchKind::Name)
            .map(|hit| hit.path.clone())
            .collect();
        assert_eq!(
            merged_name_order, name_only_order,
            "the merge reordered the name band away from filename relevance"
        );

        // **And this is the assertion that actually fails on the old merge.**
        // Agreement about ORDER was luck there: two name hits tied at exactly
        // `NAME_WEIGHT` and the merge happened to hand them over in relevance
        // order. The scores themselves cannot be lucky — flattened, two files
        // whose names match differently report the same number. `search` says
        // what the real spread is, so the test asserts the merge reproduces it
        // rather than hardcoding BM25 output.
        let name_relevance = |path: &str| {
            search(&index, "alpha", &Filter::default(), 10)
                .unwrap()
                .into_iter()
                .find(|hit| hit.path == path)
                .map(|hit| hit.score)
                .unwrap_or_else(|| panic!("{path} is not a name match at all"))
        };
        for hit in hits.iter().filter(|hit| hit.match_kind == MatchKind::Name) {
            assert_eq!(
                hit.score,
                name_relevance(&hit.path),
                "{} lost its filename relevance in the merge",
                hit.path
            );
        }
    }

    /// Two files matching in both indexes are ordered by their filename score
    /// first, and only then by what the body said.
    ///
    /// This is the one ordering rule inside a band that the merge has to get
    /// right on its own — the brief says the filename leads and the body is
    /// secondary evidence — and nothing else exercised it.
    #[test]
    fn between_two_both_matches_the_filename_leads() {
        let index = index_with("pair-name", &["/a/omega.md", "/b/omega.md"]);
        let content_index = content_index_with(
            "pair",
            &[
                // Same name relevance (the ngram field stores no frequencies), and
                // very different body relevance.
                ("/a/omega.md", "omega"),
                ("/b/omega.md", "omega omega omega omega omega omega"),
            ],
        );
        let hits = search_both(&index, &content_index, "omega", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.match_kind == MatchKind::Both));
        // The name scores tie, so the body score decides — and it decides in the
        // right direction.
        assert_eq!(
            hits[0].path, "/b/omega.md",
            "the stronger body evidence did not break the name tie: {hits:?}"
        );
        // Both report their FILENAME score, never a blend: that is what makes the
        // number comparable with the rest of the name band.
        let name_score = search(&index, "omega", &Filter::default(), 10).unwrap()[0].score;
        assert!(
            hits.iter().all(|hit| hit.score == name_score),
            "a Both hit reported something other than its filename relevance: {hits:?}"
        );
    }

    /// A compact index has no term frequencies. Its body relevance still must
    /// survive the merge, but repeated words are not evidence it stored.
    #[test]
    fn basic_content_preserves_bands_and_its_own_relevance() {
        let names = index_with("basic-band-name", &["/a/alpha.md", "/b/alpha.md"]);
        let bodies = content_index_with_profile(
            "basic-band",
            &[
                ("/a/alpha.md", "alpha"),
                ("/c/one.md", "alpha"),
                ("/d/many.md", "alpha alpha alpha alpha"),
            ],
            tantivy::schema::IndexRecordOption::Basic,
        );
        let hits = search_both(&names, &bodies, "alpha", 10).unwrap();
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].path, "/a/alpha.md");
        assert_eq!(hits[0].match_kind, MatchKind::Both);
        assert_eq!(hits[1].path, "/b/alpha.md");
        assert_eq!(hits[1].match_kind, MatchKind::Name);
        let standalone = search_content(&bodies, "alpha", 10).unwrap();
        let expected: Vec<_> = standalone
            .iter()
            .filter(|h| h.path != "/a/alpha.md")
            .map(|h| (&h.path, h.score))
            .collect();
        let actual: Vec<_> = hits
            .iter()
            .filter(|h| h.match_kind == MatchKind::Body)
            .map(|h| (&h.path, h.score))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn basic_content_ties_have_a_stable_path_order() {
        let names = index_with("basic-tie-name", &["/b/omega.md", "/a/omega.md"]);
        let bodies = content_index_with_profile(
            "basic-tie",
            &[("/b/omega.md", "omega"), ("/a/omega.md", "omega")],
            tantivy::schema::IndexRecordOption::Basic,
        );
        for _ in 0..8 {
            let hits = search_both(&names, &bodies, "omega", 10).unwrap();
            assert_eq!(
                hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
                ["/a/omega.md", "/b/omega.md"]
            );
            assert!(hits.iter().all(|h| h.match_kind == MatchKind::Both));
        }
    }

    /// The same query gives the same order every time, ties included.
    ///
    /// The old ranking tied every name-only hit at one score and then let a
    /// stable sort preserve `HashMap` iteration order — which differs per
    /// process, so two runs of one query could disagree with nothing having
    /// changed. Three files whose names match identically well is exactly that
    /// case, so the tie-break has to decide it, and decide it the same way twice.
    #[test]
    fn both_orders_ties_the_same_way_every_run() {
        let index = index_with("tie-name", &["/a/tie.md", "/b/tie.md", "/c/tie.md"]);
        let content_index = content_index_with("tie", &[("/unrelated/x.md", "nothing")]);

        let order_of = || -> Vec<String> {
            search_both(&index, &content_index, "tie", 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect()
        };
        let first = order_of();
        assert_eq!(first.len(), 3);
        for _ in 0..8 {
            assert_eq!(first, order_of(), "the same query answered in two orders");
        }
        // Deterministic AND explainable: equal relevance falls back to the path.
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "the tie-break is not the path: {first:?}");
    }

    /// A word that matches a name and a body in the same file yields one row, and
    /// a multi-term query still ranks by band.
    #[test]
    fn both_never_lists_one_file_twice() {
        let index = index_with("once-name", &["/n/projeto alpha beta.md"]);
        let content_index =
            content_index_with("once", &[("/n/projeto alpha beta.md", "alpha beta gamma")]);

        let hits = search_both(&index, &content_index, "alpha", 10).unwrap();
        assert_eq!(hits.len(), 1, "the same file came back twice: {hits:?}");
        assert_eq!(hits[0].match_kind, MatchKind::Both);

        // Two terms, both present in the name and the body.
        let hits = search_both(&index, &content_index, "alpha beta", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].match_kind, MatchKind::Both);
    }

    /// Accents fold on both sides of the merge, and the band is still right.
    #[test]
    fn both_folds_accents_on_each_side() {
        let index = index_with("acc-both-name", &["/n/relatório.md", "/n/outro.md"]);
        let content_index =
            content_index_with("acc-both", &[("/n/outro.md", "um relatório de Gonçalves")]);

        // Typed without accents, matching a name with them and a body with them.
        let hits = search_both(&index, &content_index, "relatorio", 10).unwrap();
        let kind_of = |path: &str| {
            hits.iter()
                .find(|hit| hit.path == path)
                .unwrap_or_else(|| panic!("{path} missing from {hits:?}"))
                .match_kind
        };
        assert_eq!(kind_of("/n/relatório.md"), MatchKind::Name);
        assert_eq!(kind_of("/n/outro.md"), MatchKind::Body);
        // Name band first, as always.
        assert_eq!(hits[0].path, "/n/relatório.md");
    }

    /// A broad query stays bounded by the limit it was given.
    #[test]
    fn both_stays_within_its_limit() {
        let owned: Vec<String> = (0..40).map(|n| format!("/w/wide_{n:02}.md")).collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let index = index_with("wide-name", &refs);
        let bodies: Vec<(&str, &str)> = refs.iter().map(|path| (*path, "wide")).collect();
        let content_index = content_index_with("wide", &bodies);

        for limit in [1, 5, 12] {
            let hits = search_both(&index, &content_index, "wide", limit).unwrap();
            assert!(
                hits.len() <= limit,
                "asked for {limit} and got {}",
                hits.len()
            );
        }
    }

    #[test]
    fn count_and_list_cataloged_paths() {
        let index = index_with("list", &["/c/three.rs", "/a/one.txt", "/b/two.md"]);
        // count = every cataloged file.
        assert_eq!(count(&index).unwrap(), 3);
        // list = all paths, sorted.
        let all = list_paths(&index, 100, None).unwrap();
        assert_eq!(all, vec!["/a/one.txt", "/b/two.md", "/c/three.rs"]);
        // --under scopes to a directory subtree.
        assert_eq!(
            list_paths(&index, 100, Some("/b")).unwrap(),
            vec!["/b/two.md"]
        );
        // -n caps the output.
        assert_eq!(list_paths(&index, 1, None).unwrap(), vec!["/a/one.txt"]);
    }

    /// The last word is read as typed so far: it finds the words it begins,
    /// and a document holding exactly that word ranks first. Earlier words, and
    /// a last word under three letters, match whole words only.
    #[test]
    fn the_last_content_word_is_a_prefix() {
        let content = content_index_with(
            "prefix",
            &[
                ("/d/plural.txt", "os relatórios e os contratos assinados"),
                ("/d/exact.txt", "o relatorio do mes"),
                ("/d/other.txt", "religiao e cultura"),
            ],
        );
        let paths = |q: &str| -> Vec<String> {
            search_content(&content, q, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect()
        };
        assert_eq!(paths("relatorio"), ["/d/exact.txt", "/d/plural.txt"]);
        assert_eq!(paths("contrato"), ["/d/plural.txt"]);
        assert_eq!(paths("os rela"), ["/d/plural.txt"]);
        assert!(paths("re").is_empty());
        // Only the last word begins others: "rel" is no word of plural.txt.
        assert!(paths("rel contratos").is_empty());
    }

    /// The spellings people treat as one find each other, because content words
    /// are folded like names: Russian е/ё, Greek with or without its accent,
    /// Arabic alef with or without hamza and with or without the article, a
    /// decomposed accent, the Turkish İ; one CJK character finds the words it
    /// opens.
    #[test]
    fn content_spellings_find_each_other() {
        let content = content_index_with(
            "spellings",
            &[
                ("/d/ru.txt", "Отчёт о продажах и ёлка"),
                ("/d/ru2.txt", "елка без точек"),
                ("/d/el.txt", "Οι πωλήσεις αυξήθηκαν κατά είκοσι"),
                ("/d/ar.txt", "ارتفعت المبيعات وقال أحمد"),
                ("/d/zh.txt", "本季度的销售额增长了"),
                ("/d/nfd.txt", "relato\u{301}rio decomposto"),
                ("/d/tr.txt", "İstanbul'da satışlar arttı"),
            ],
        );
        let paths = |q: &str| -> Vec<String> {
            let mut found: Vec<String> = search_content(&content, q, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect();
            found.sort();
            found
        };
        assert_eq!(paths("отчет"), ["/d/ru.txt"]);
        assert_eq!(paths("елка"), ["/d/ru.txt", "/d/ru2.txt"]);
        assert_eq!(paths("ёлка"), ["/d/ru.txt", "/d/ru2.txt"]);
        assert_eq!(paths("πωλησεις"), ["/d/el.txt"]);
        assert_eq!(paths("ΑΥΞΗΘΗΚΑΝ"), ["/d/el.txt"]);
        assert_eq!(paths("مبيعات"), ["/d/ar.txt"]);
        assert_eq!(paths("احمد"), ["/d/ar.txt"]);
        assert_eq!(paths("销"), ["/d/zh.txt"]);
        assert_eq!(paths("relatorio"), ["/d/nfd.txt"]);
        assert_eq!(paths("istanbul"), ["/d/tr.txt"]);
    }

    /// One CJK character finds the names it stands in, wherever it stands.
    #[test]
    fn one_cjk_character_finds_names() {
        let index = index_with(
            "cjk-one",
            &["/h/季度销售报告2026.txt", "/h/会議の議事録.txt"],
        );
        for (query, name) in [
            ("报", "季度销售报告2026.txt"),
            ("告", "季度销售报告2026.txt"),
            ("季", "季度销售报告2026.txt"),
            ("録", "会議の議事録.txt"),
        ] {
            assert_eq!(
                names(search(&index, query, &Filter::default(), 10).unwrap()),
                [name],
                "query {query:?}"
            );
        }
    }

    /// One CJK character is a term of its own: it is found wherever it stands,
    /// however many different characters follow it elsewhere (the names held
    /// to be scanned for the grams it opens stopped at 64).
    #[test]
    fn one_cjk_character_is_found_wherever_it_stands() {
        let mut paths: Vec<String> = (0..70)
            .map(|i| format!("/h/报{}.txt", char::from_u32(0x4E00 + i).unwrap()))
            .collect();
        paths.push("/h/报龥.txt".to_owned());
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("cjk-many", &refs);
        let found = names(search(&index, "报", &Filter::default(), 100).unwrap());
        assert_eq!(found.len(), 71);
        assert!(found.iter().any(|name| name == "报龥.txt"));

        let content = content_index_with(
            "cjk-one-content",
            &[
                ("/h/a.txt", "本季度的销售额增长了。"),
                ("/h/b.txt", "销售部门"),
            ],
        );
        for (query, expected) in [
            ("额", vec!["/h/a.txt"]),
            ("销", vec!["/h/a.txt", "/h/b.txt"]),
            ("售额", vec!["/h/a.txt"]),
        ] {
            let mut got: Vec<String> = search_content(&content, query, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect();
            got.sort();
            assert_eq!(got, expected, "query {query:?}");
        }
    }

    /// A Russian or Ukrainian word finds its other case endings, wherever it
    /// stands in the query, and not words that only begin like it.
    #[test]
    fn slavic_words_find_their_other_endings() {
        let content = content_index_with(
            "slavic",
            &[
                ("/h/a.txt", "Продажи выросли в этом квартале."),
                ("/h/b.txt", "Отчёт по кварталам и продажам."),
                ("/h/c.txt", "Квартальный отчёт."),
                ("/h/d.txt", "Звіт про продажі за квартал."),
            ],
        );
        for (query, expected) in [
            ("квартал отчет", vec!["/h/b.txt"]),
            ("квартала", vec!["/h/a.txt", "/h/b.txt", "/h/d.txt"]),
            ("продажа квартал", vec!["/h/a.txt", "/h/b.txt", "/h/d.txt"]),
        ] {
            let mut got: Vec<String> = search_content(&content, query, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect();
            got.sort();
            assert_eq!(got, expected, "query {query:?}");
        }
        assert_eq!(slavic_stem("сто"), None);
        assert_eq!(slavic_stem("кварталами"), Some("квартал"));
    }

    /// Every word finds its plural or singular, in both directions and whatever
    /// its place; the form as typed ranks first; a short word is left alone.
    #[test]
    fn content_words_find_their_plural_and_singular() {
        let content = content_index_with(
            "inflection",
            &[
                ("/d/singular.txt", "o relatório da ação do responsável"),
                ("/d/plural.txt", "os relatórios das ações dos responsáveis"),
                ("/d/again.txt", "relatórios e ações, imagens e flores"),
                ("/d/mail.txt", "mail server mail queue"),
                ("/d/mail2.txt", "another mail"),
            ],
        );
        let paths = |q: &str| -> Vec<String> {
            search_content(&content, q, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.path)
                .collect()
        };
        let mut found = paths("relatorios acoes");
        assert_eq!(found.len(), 3, "{found:?}");
        assert_ne!(found[0], "/d/singular.txt", "the form as typed ranks first");
        found.sort();
        assert_eq!(found, ["/d/again.txt", "/d/plural.txt", "/d/singular.txt"]);
        let found = paths("acao responsavel");
        assert_eq!(found[0], "/d/singular.txt");
        assert!(found.contains(&"/d/plural.txt".to_string()), "{found:?}");
        assert!(paths("imagem flor").contains(&"/d/again.txt".to_string()));
        // "mais" is no plural of "mail".
        assert!(paths("mais servidor").is_empty());
    }

    /// A page holds only its own hits; the total still counts every match,
    /// from the postings, though only the page's candidates were read.
    #[test]
    fn a_page_counts_all_matches() {
        let dir = std::env::temp_dir().join(format!("lsearch-paged-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let owned: Vec<String> = (0..30)
            .map(|i| {
                let path = dir.join(format!("nota {i}.txt"));
                std::fs::write(&path, b"x").unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let index = index_with("paged", &refs);
        let outcome = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "nota",
            "name",
            &Filter::default(),
            &Page {
                offset: 0,
                limit: 5,
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(outcome.hits.len(), 5);
        assert_eq!(outcome.total, 30);
        assert_eq!(outcome.next_offset, Some(5));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_query_filters_paginates_and_enriches() {
        let dir = std::env::temp_dir().join(format!("lsearch-rq-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let p_txt = dir.join("item_one.txt");
        let p_md = dir.join("sub/item_two.md");
        let p_big = dir.join("item_three.txt");
        std::fs::write(&p_txt, b"12345").unwrap(); // 5 bytes
        std::fs::write(&p_md, b"hello").unwrap();
        std::fs::write(&p_big, vec![0u8; 2000]).unwrap(); // 2000 bytes
        let owned: Vec<String> = [&p_txt, &p_md, &p_big]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let index = index_with("rq", &refs);

        // Base: "item" matches all three; metadata is resolved server-side.
        let all = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "item",
            "name",
            &Filter::default(),
            &Page::default(),
            false,
            None,
        )
        .unwrap();
        assert_eq!(all.total, 3);
        let txt = all
            .hits
            .iter()
            .find(|h| h.path == p_txt.to_string_lossy())
            .unwrap();
        assert_eq!(txt.ext, "txt");
        assert_eq!(txt.size, 5);
        assert!(txt.mtime > 0);

        // ext filter (path-only) → the two .txt files.
        let f = Filter {
            ext: vec!["txt".into()],
            ..Default::default()
        };
        assert_eq!(
            run_query(
                QueryIndexes {
                    name: &index,
                    content: None,
                },
                "item",
                "name",
                &f,
                &Page::default(),
                false,
                None,
            )
            .unwrap()
            .total,
            2
        );

        // under filter (path-only) → only the file in sub/.
        let f = Filter {
            under: Some(dir.join("sub").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let u = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "item",
            "name",
            &f,
            &Page::default(),
            false,
            None,
        )
        .unwrap();
        assert_eq!(u.total, 1);
        assert!(u.hits[0].path.ends_with("item_two.md"));

        // size filter (stat) → excludes the 2000-byte file.
        let f = Filter {
            size: Some([0, 10]),
            ..Default::default()
        };
        assert_eq!(
            run_query(
                QueryIndexes {
                    name: &index,
                    content: None,
                },
                "item",
                "name",
                &f,
                &Page::default(),
                false,
                None,
            )
            .unwrap()
            .total,
            2
        );

        // Pagination: limit 2 → first page has next_offset; second page ends.
        let p0 = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "item",
            "name",
            &Filter::default(),
            &Page {
                offset: 0,
                limit: 2,
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(p0.hits.len(), 2);
        assert_eq!(p0.total, 3);
        assert_eq!(p0.next_offset, Some(2));
        let p1 = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "item",
            "name",
            &Filter::default(),
            &Page {
                offset: 2,
                limit: 2,
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(p1.hits.len(), 1);
        assert_eq!(p1.next_offset, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn accent_insensitive() {
        let dir = std::env::temp_dir().join(format!("lsearch-test-{}-acc", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = index_with("acc-name", &["/n/relatório Gonçalves.md"]);
        let content_index = content_index_with(
            "acc",
            &[(
                "/n/relatório Gonçalves.md",
                "documento de Bruno Gonçalves Antônio",
            )],
        );

        // content folds accents both ways
        assert_eq!(
            search_content(&content_index, "goncalves", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_content(&content_index, "antônio", 10).unwrap().len(),
            1
        );
        // name (ngram) folds too
        assert_eq!(
            search(&index, "relatorio", &Filter::default(), 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(&index, "goncalves", &Filter::default(), 10)
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A deleted file used to come back labelled `offline · on <name>`, which
    /// reads as "it lives on a disk you unplugged". With no removable or network
    /// source configured, a path that will not stat is simply gone.
    #[test]
    fn a_deleted_file_is_dropped_not_reported_as_offline() {
        let dir = std::env::temp_dir().join(format!("lsearch-offline-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let kept = dir.join("nota_viva.txt");
        let gone = dir.join("nota_morta.txt");
        std::fs::write(&kept, b"x").unwrap();
        std::fs::write(&gone, b"x").unwrap();

        let index = index_with(
            "offline",
            &[
                kept.to_string_lossy().as_ref(),
                gone.to_string_lossy().as_ref(),
            ],
        );
        // Both are catalogued; only one still exists on disk.
        std::fs::remove_file(&gone).unwrap();

        let outcome = run_query(
            QueryIndexes {
                name: &index,
                content: None,
            },
            "nota",
            "name",
            &Filter::default(),
            &Page::default(),
            false,
            None,
        )
        .unwrap();

        assert_eq!(outcome.hits.len(), 1, "the deleted file must not be listed");
        assert_eq!(outcome.hits[0].path, kept.to_string_lossy());
        assert!(
            outcome.hits[0].available,
            "a file that stats is available, never flagged offline"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
