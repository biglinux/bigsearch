//! Name search: ngram recall via Tantivy, then exact substring post-filter.
use super::{Filter, Hit, count_capped};
use crate::index::fields;
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{IndexReader, TantivyDocument, Term};

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
    let candidates = newest_first_on_ties(&searcher, &query, candidate_limit)?;

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

/// The top `limit` matches of `query`, each with its score and the day of
/// its last change.
///
/// The day breaks ties, and only ties: among equal matches (the hundreds of
/// "1.png") the newest first instead of the index's order. As a stronger
/// factor it moves known-item queries as often down as up.
fn newest_first_on_ties(
    searcher: &tantivy::Searcher,
    query: &BooleanQuery,
    limit: usize,
) -> Result<Vec<((tantivy::Score, u64), tantivy::DocAddress)>> {
    Ok(searcher.search(
        query,
        &TopDocs::with_limit(limit).tweak_score(move |segment: &tantivy::SegmentReader| {
            let days = segment
                .fast_fields()
                .u64("mtime_day")
                .ok()
                .map(|column| column.first_or_default_col(0));
            move |doc: tantivy::DocId, score: tantivy::Score| {
                (score, days.as_ref().map_or(0, |days| days.get_val(doc)))
            }
        }),
    )?)
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
    // only afterwards, the candidate ceiling would be spent on whatever
    // matched the words, and `pr` would find no video among thousands of
    // other files.
    clauses.extend(filter_clauses(f, filter));
    Some(BooleanQuery::new(clauses))
}

/// How many names answer `q` under `filter`, up to `cap`, from the index
/// alone: the grams say a name holds the letters, so this can count a rare
/// name the post-filter turns away.
pub(super) fn count_names(
    reader: &IndexReader,
    q: &str,
    filter: &Filter,
    cap: usize,
) -> Result<usize> {
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
/// name, far above what the grams' own BM25 adds, so it orders kinds of match
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
pub(super) struct NameMatch {
    whole_name: bool,
    words: u32,
}

/// `None` when the name does not hold every query word the way it must: a
/// short word as a word of its own, a longer one anywhere.
pub(super) fn name_match(name: &str, words: &[String]) -> Option<NameMatch> {
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
pub(super) fn filter_clauses(
    f: &crate::index::Fields,
    filter: &Filter,
) -> Vec<(Occur, Box<dyn Query>)> {
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

/// The exact media type a name document stores; empty for none.
pub(super) fn stored_mime(doc: &TantivyDocument, f: &crate::index::Fields) -> String {
    doc.get_first(f.mime)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The media type the name index holds for `path`, for a hit that came
/// through the content index; empty when it has none.
pub(super) fn indexed_mime(
    searcher: &tantivy::Searcher,
    f: &crate::index::Fields,
    path: &str,
) -> String {
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

#[cfg(all(test, not(miri)))]
mod tests;
