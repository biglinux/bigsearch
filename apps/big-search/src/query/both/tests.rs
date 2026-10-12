use super::*;
use crate::query::tests::{content_index_with, content_index_with_profile, index_with};

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
/// Two name-only files whose names differ in how well they match come back
/// in that order, and the same for two body-only files.
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
    // the one mentioning it once.
    let position = |path: &str| hits.iter().position(|hit| hit.path == path).unwrap();
    assert!(
        position("/r/many.md") < position("/q/plain.md"),
        "content relevance was flattened: {hits:?}"
    );

    assert_name_band_follows_search(&index, &hits);
}

/// The name band of a merged `alpha` result keeps the order and the scores `search`
/// gives the same files.
fn assert_name_band_follows_search(index: &tantivy::IndexReader, hits: &[Hit]) {
    let kind_of = |path: &str| {
        hits.iter()
            .find(|hit| hit.path == path)
            .unwrap_or_else(|| panic!("{path} is missing from {hits:?}"))
            .match_kind
    };
    // `search` alone is the filename-relevance order, so the name band of a
    // merged result has to agree with it. Derived from `search` rather than
    // hardcoded, so the test follows BM25 if it changes.
    let name_only_order: Vec<String> = search(index, "alpha", &Filter::default(), 10)
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

    // An order can agree by luck when scores tie; the scores themselves
    // cannot. Each name hit must report the score `search` gives it, so two
    // files whose names match differently report different numbers.
    let name_relevance = |path: &str| {
        search(index, "alpha", &Filter::default(), 10)
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
/// right on its own: the filename leads and the body is secondary evidence.
#[test]
fn filename_leads_between_both_matches() {
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
    // The name scores tie, so the body score decides, in the right
    // direction.
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
fn basic_content_keeps_band_relevance() {
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
fn basic_content_ties_follow_path() {
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
/// Three files whose names match identically well tie on score, and
/// `HashMap` iteration order differs per process, so the tie-break has to
/// decide it, and decide it the same way twice.
#[test]
fn both_orders_ties_stably() {
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
