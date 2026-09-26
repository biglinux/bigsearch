//! Name search: ngram recall via Tantivy, then exact substring post-filter.
use crate::index::{BODY, NGRAM, content_fields, fields};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, TermQuery};
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
    let base = match mode {
        "content" => search_content(
            indexes
                .content
                .context("content index required for content search")?,
            q,
            CANDIDATE_CEILING,
        )?,
        "both" => search_both(
            indexes.name,
            indexes
                .content
                .context("content index required for combined search")?,
            q,
            CANDIDATE_CEILING,
        )?,
        // Everything the index holds, for a caller that is narrowing by
        // filter rather than by words: "every PDF under my home folder" has
        // no text to search for, and the name search refuses a query shorter
        // than two letters because the ngram index cannot narrow one.
        "list" => list_matching(indexes.name, filter, CANDIDATE_CEILING)?,
        _ => search(indexes.name, q, filter, CANDIDATE_CEILING)?,
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

    let total = kept.len();
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

use crate::extract::fold_lower;

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
    let needle = fold_lower(q.trim());
    if needle.len() < 2 {
        return Ok(Vec::new());
    }
    // Each word is matched on its own, so a name holds all of them in any order.
    // Treating the whole query as one substring made `raio bruno` miss
    // `Raio-X Bruno ….pdf`, which either word alone found: the space, and the
    // grams straddling it, are only in the query.
    let words: Vec<&str> = needle.split_whitespace().collect();

    // Tokenize each word with the same ngram analyzer the field uses, AND the grams.
    let mut analyzer = index.tokenizers().get(NGRAM).context("ngram tokenizer")?;
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for word in &words {
        let mut stream = analyzer.token_stream(word);
        while stream.advance() {
            let term = Term::from_field_text(f.name, stream.token().text.as_str());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }
    }
    // Only single-character words were typed: the ngram floor is 2, so the index
    // cannot narrow anything and scanning every document is not worth it.
    if clauses.is_empty() {
        return Ok(Vec::new());
    }
    // The folder and the extensions narrow the index's own answer: filtered
    // only afterwards, the candidate ceiling was spent on whatever matched
    // the words, and `pr` found no video among 5 000 other files.
    clauses.extend(filter_clauses(&f, filter));
    let query = BooleanQuery::new(clauses);

    // Over-fetch for the substring post-filter, but cap the candidate pool so a
    // huge `limit` (e.g. -a) doesn't allocate an enormous TopDocs heap.
    let candidate_limit = limit.saturating_mul(4).clamp(40, 50_000);
    let candidates = searcher.search(
        &query,
        &TopDocs::with_limit(candidate_limit).order_by_score(),
    )?;

    let mut hits = Vec::with_capacity(limit.min(1024));
    let mut seen_paths = HashSet::with_capacity(limit.min(1024));
    for (score, addr) in candidates {
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
        let folded_name = fold_lower(&name);
        if words.iter().all(|word| folded_name.contains(word)) {
            seen_paths.insert(path.to_string());
            let source = doc
                .get_first(f.source)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            hits.push(Hit {
                path: path.to_string(),
                name,
                score,
                source,
                mime: stored_mime(&doc, &f),
                ..Default::default()
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    Ok(hits)
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
    let mut analyzer = index.tokenizers().get(BODY).context("body tokenizer")?;
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut stream = analyzer.token_stream(q);
    while stream.advance() {
        let term = Term::from_field_text(f.body, stream.token().text.as_str());
        clauses.push((Occur::Must, Box::new(TermQuery::new(term, record))));
    }
    if clauses.is_empty() {
        return Ok(Vec::new());
    }
    let query = BooleanQuery::new(clauses);

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

/// One file while the two searches are being merged, with the score each index
/// gave it kept apart.
///
/// The two are BM25 over different indexes and different fields, so they are not
/// points on one scale and adding them means nothing. Keeping them apart until
/// the sort is what lets each tier be ordered by the evidence that tier actually
/// has.
struct Candidate {
    hit: Hit,
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
    for hit in names {
        let name_score = hit.score;
        merged.insert(
            hit.path.clone(),
            Candidate {
                hit,
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

    #[test]
    fn a_name_search_finds_videos_past_a_ceiling_of_other_files() {
        let mut paths: Vec<String> = (0..40)
            .map(|i| format!("/home/u/src/prompt-{i}.rs"))
            .collect();
        paths.extend([
            "/home/u/Videos/Predator.mkv".to_string(),
            "/home/u/Videos/prores.mov".to_string(),
            "/home/u/Music/prelude.mp4".to_string(),
        ]);
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let index = index_with("name-ceiling", &refs);
        let filter = Filter {
            under: Some("/home/u/Videos".into()),
            ext: vec!["mkv".into(), "mov".into(), "mp4".into()],
            ..Default::default()
        };
        // A ceiling smaller than the other files that match the word.
        let mut found: Vec<String> = search(&index, "pr", &filter, 5)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        found.sort();
        assert_eq!(
            found,
            ["/home/u/Videos/Predator.mkv", "/home/u/Videos/prores.mov"]
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
