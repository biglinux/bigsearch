//! Listing the name index without words to search for.
use super::names::{filter_clauses, stored_mime};
use super::{Filter, Hit, file_name, is_under};
use crate::index::fields;
use anyhow::Result;
use std::collections::HashSet;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Query};
use tantivy::schema::Value;
use tantivy::{IndexReader, TantivyDocument};

/// The `list` and `largest` modes with a type filter: what the name index
/// holds matching `filter`, up to `limit`, in no particular order. The index answers the
/// filter itself, so the ceiling counts matches rather than a sample of the
/// whole index: cut first and filtered after, "every video in ~/Videos" would
/// come back empty whenever the first files of the index held none.
pub fn list_matching(reader: &IndexReader, filter: &Filter, limit: usize) -> Result<Vec<Hit>> {
    let clauses = filter_clauses(&fields(reader.searcher().index())?, filter);
    if clauses.is_empty() {
        return collect_paths(reader, &AllQuery, limit);
    }
    collect_paths(reader, &BooleanQuery::new(clauses), limit)
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
            name: file_name(path),
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

/// How many entries are cataloged (alive indexed documents: files and the
/// visible directories the scan indexes, with deletions excluded).
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
            if under.is_some_and(|dir| path != dir && !is_under(path, dir)) {
                continue;
            }
            paths.push(path.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths.truncate(limit);
    Ok(paths)
}
