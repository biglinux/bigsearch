//! The `both` mode: name and content matches merged into ranked bands.
use super::content::search_content;
use super::names::{NameMatch, name_match, search};
use super::{Filter, Hit, MatchKind};
use anyhow::Result;
use std::collections::HashMap;
use tantivy::IndexReader;

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
    /// match found only inside the file. The band is the first sort key, never
    /// inferred from arithmetic on incomparable scores.
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
    /// caller can reason about.
    fn into_hit(mut self) -> Hit {
        self.hit.score = self.name_score.or(self.body_score).unwrap_or_default();
        self.hit
    }
}

/// Filename ∪ content matches, ranked by band and then by relevance within it:
/// files matching in both name and content rank highest, then name-only,
/// then content-only.
///
/// The order is total, so it is the same on every run for the same input; a
/// tie left to the merge would follow `HashMap` iteration, which differs per
/// process.
///
/// Within `Both`, the filename score leads and the content score is secondary
/// evidence: a strong filename hit must not be pushed down by a weak body match.
///
/// The final tie-break is the path, raw rather than folded: determinism is all
/// it has to provide, and folding would allocate a second string per candidate.
/// That tie-break carries more weight than it looks: the name field is indexed
/// as ngrams with [`IndexRecordOption::Basic`], which stores no term
/// frequencies, so BM25 over it gives every document matching the same grams
/// the same score (`alpha.md` and `alpha_alpha.md` tie for `alpha`). The name
/// band is therefore mostly one big tie, and the path is what actually orders
/// it. The content band does rank, because the body field keeps frequencies.
pub fn search_both(
    reader: &IndexReader,
    content_reader: &IndexReader,
    q: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    let names = search(reader, q, &Filter::default(), limit)?;
    let contents = search_content(content_reader, q, limit)?;

    let mut candidates = merge_by_path(names, contents, q);
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

/// One candidate per path: a file both searches found keeps its name hit and
/// gains the body score.
fn merge_by_path(names: Vec<Hit>, contents: Vec<Hit>, q: &str) -> Vec<Candidate> {
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
    merged.into_values().collect()
}

#[cfg(all(test, not(miri)))]
mod tests;
