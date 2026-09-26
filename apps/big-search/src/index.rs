//! Tantivy indexes: a compact name index (`path`, `name`, `source`) and a
//! separate content index (`path`, `body`).
use crate::cjk_tokenizer::CjkFriendlyTokenizer;
use crate::state::State;
use anyhow::{Context, Result};
use std::path::Path;
use tantivy::indexer::IndexWriterOptions;
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, NgramTokenizer, RemoveLongFilter, TextAnalyzer,
};
use tantivy::{Index, IndexWriter, TantivyDocument};

/// Registered analyzer for the `name` field (ngram substring, accent-folded).
pub const NGRAM: &str = "name_ngram";
/// Registered analyzer for the `body` field: CJK-bigram aware, word-splitting
/// otherwise, accent-folded. Bumped from `body_text` (SimpleTokenizer) so the
/// on-disk schema differs → the index auto-rebuilds with the new analyzer.
pub const BODY: &str = "body_cjk";
const DEFAULT_BACKGROUND_WRITER_MEMORY_MB: usize = 15;
const DEFAULT_BULK_WRITER_MEMORY_MB: usize = 32;
const MIN_WRITER_MEMORY_MB: usize = 15;
const MAX_INDEX_TO_STATE_EXTRA_PERCENT: u64 = 20;
const MAX_INDEX_TO_STATE_SURPLUS: u64 = 10_000;
const MIN_REPAIRABLE_NAME_DEFICIT: u64 = 1_000;

/// Posting detail used for the content body field. The schema changes when this
/// setting changes, so the content sidecar is recreated cleanly on next open.
pub fn content_record_option() -> IndexRecordOption {
    match crate::settings::resolved_content_index_mode() {
        crate::settings::ResolvedContentIndexMode::Basic => IndexRecordOption::Basic,
        crate::settings::ResolvedContentIndexMode::Freqs => IndexRecordOption::WithFreqs,
    }
}

/// Build the name schema: `path` (stored identity) + `name` (ngram-indexed for substring).
fn build_name_schema() -> Schema {
    let mut sb = Schema::builder();
    sb.add_text_field("path", raw_identifier_options(true));
    let name_indexing = TextFieldIndexing::default()
        .set_tokenizer(NGRAM)
        .set_index_option(IndexRecordOption::Basic)
        .set_fieldnorms(false);
    sb.add_text_field(
        "name",
        TextOptions::default().set_indexing_options(name_indexing),
    );
    // Source/device display name (stored, untokenised) so a hit can report which
    // device it lives on — kept even when that device is offline.
    sb.add_text_field("source", TextOptions::default().set_stored());
    // Every folder above the file, one exact term each, and its lowercase
    // extension: "under ~/Videos" and "videos only" are then one posting list
    // each, where a range over the paths walked every file below the folder
    // and the name grams of `.mp4` also matched `x.mp4.part`.
    sb.add_text_field("dir", raw_identifier_options(false));
    sb.add_text_field("ext", raw_identifier_options(false));
    // The media type (see `mime`), then its `major/*` group: "every video" is
    // one term, extension or not. Stored for the hits to carry.
    sb.add_text_field("mime", raw_identifier_options(true));
    sb.build()
}

/// Build the content schema: `path` identity + full-text `body`.
fn build_content_schema() -> Schema {
    build_content_schema_for(content_record_option())
}

fn build_content_schema_for(record: IndexRecordOption) -> Schema {
    let mut sb = Schema::builder();
    sb.add_text_field("path", raw_identifier_options(true));
    let body_indexing = TextFieldIndexing::default()
        .set_tokenizer(BODY)
        .set_index_option(record);
    sb.add_text_field(
        "body",
        TextOptions::default().set_indexing_options(body_indexing),
    );
    sb.build()
}

fn raw_identifier_options(stored: bool) -> TextOptions {
    let indexing = TextFieldIndexing::default()
        .set_tokenizer("raw")
        .set_index_option(IndexRecordOption::Basic)
        .set_fieldnorms(false);
    let options = TextOptions::default().set_indexing_options(indexing);
    if stored {
        options.set_stored()
    } else {
        options
    }
}

/// Register the name ngram analyzer.
fn register_name_tokenizer(index: &Index) -> Result<()> {
    let ngram = NgramTokenizer::all_ngrams(2, 3).context("ngram tokenizer")?;
    index.tokenizers().register(
        NGRAM,
        TextAnalyzer::builder(ngram)
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build(),
    );
    Ok(())
}

/// Register the content word analyzer.
fn register_body_tokenizer(index: &Index) {
    index.tokenizers().register(
        BODY,
        TextAnalyzer::builder(CjkFriendlyTokenizer)
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build(),
    );
}

/// Resolved field handles for the name index schema.
pub struct Fields {
    pub path: Field,
    pub name: Field,
    pub source: Field,
    pub dir: Field,
    pub ext: Field,
    pub mime: Field,
}

/// Resolved field handles for the content index schema.
pub struct ContentFields {
    pub path: Field,
    pub body: Field,
}

pub fn fields(index: &Index) -> Result<Fields> {
    let schema = index.schema();
    Ok(Fields {
        path: schema.get_field("path").context("field path")?,
        name: schema.get_field("name").context("field name")?,
        source: schema.get_field("source").context("field source")?,
        dir: schema.get_field("dir").context("field dir")?,
        ext: schema.get_field("ext").context("field ext")?,
        mime: schema.get_field("mime").context("field mime")?,
    })
}

pub fn content_fields(index: &Index) -> Result<ContentFields> {
    let schema = index.schema();
    Ok(ContentFields {
        path: schema.get_field("path").context("field path")?,
        body: schema.get_field("body").context("field body")?,
    })
}

/// Open an existing index without ever repairing it, for commands that only read.
///
/// [`open_or_create`] deletes and recreates on a schema mismatch or an unreadable
/// index. That is right for the daemon, which owns the data, but wrong for a
/// query: `count`, `list` and the daemonless `search` fallback would silently
/// wipe the catalogue — and a query racing a daemon that holds the writer lock
/// but has not yet bound its socket would do it underneath the daemon. A read
/// command must fail instead.
pub fn open_read_only(dir: &Path) -> Result<Index> {
    let index = open_existing(dir).with_context(|| {
        format!(
            "no usable index at {} (is the daemon running?)",
            dir.display()
        )
    })?;
    if index.schema() == build_name_schema() {
        register_name_tokenizer(&index)?;
    } else if [IndexRecordOption::Basic, IndexRecordOption::WithFreqs]
        .into_iter()
        .any(|record| index.schema() == build_content_schema_for(record))
    {
        // A read-only client must follow the stored schema. Its current RAM,
        // cgroup or configuration may differ from the writer's profile; that
        // changes future builds, not how an existing content index is queried.
        register_body_tokenizer(&index);
    }
    Ok(index)
}

/// Open the on-disk index, creating it on first use. Tokenizer is always registered.
pub fn open_or_create(dir: &Path) -> Result<Index> {
    let index = open_or_create_with_schema(dir, build_name_schema)?;
    register_name_tokenizer(&index)?;
    Ok(index)
}

/// Open the on-disk content index, creating it on first use.
pub fn open_content_or_create(dir: &Path) -> Result<Index> {
    let index = open_or_create_with_schema(dir, build_content_schema)?;
    register_body_tokenizer(&index);
    Ok(index)
}

/// Test fixtures select posting detail explicitly rather than depending on RAM
/// or mutating process-wide environment while other tests are running.
#[cfg(test)]
pub(crate) fn create_content_fixture(dir: &Path, record: IndexRecordOption) -> Result<Index> {
    std::fs::create_dir_all(dir)?;
    let index = create_in(dir, build_content_schema_for(record))?;
    register_body_tokenizer(&index);
    Ok(index)
}

fn open_or_create_with_schema(dir: &Path, build_schema: fn() -> Schema) -> Result<Index> {
    std::fs::create_dir_all(dir).with_context(|| format!("create index dir {}", dir.display()))?;
    match open_existing(dir) {
        // Reuse only if the on-disk schema matches the current one; otherwise the
        // schema evolved (e.g. a new field) → rebuild from scratch.
        Ok(index) if index.schema() == build_schema() => Ok(index),
        Ok(_) => recreate(dir, build_schema),
        Err(error) if is_non_empty_dir(dir) => {
            log::warn!(
                "failed to open index at {}; recreating it: {error:#}",
                dir.display()
            );
            recreate(dir, build_schema)
        }
        Err(_) => create_in(dir, build_schema()),
    }
}

/// Open the index and prove it is actually readable.
///
/// Writes skip `fsync` (see [`crate::nosync_dir`]), so a power cut can leave
/// `meta.json` naming segments whose bytes never landed. That index parses but
/// explodes on the first query, which is the one failure the caller cannot
/// recover from. Building a searcher here opens every segment, so the damage
/// surfaces as an open error and the caller recreates the index instead.
fn open_existing(dir: &Path) -> Result<Index> {
    let index = Index::open(crate::nosync_dir::NoFsyncDirectory::open(dir)?)?;
    index
        .reader()
        .context("open index reader")?
        .searcher()
        .segment_readers();
    Ok(index)
}

fn create_in(dir: &Path, schema: Schema) -> Result<Index> {
    Index::create(
        crate::nosync_dir::NoFsyncDirectory::open(dir)?,
        schema,
        tantivy::IndexSettings::default(),
    )
    .context("create index")
}

fn is_non_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

pub fn recreate_on_disk(dir: &Path) -> Result<Index> {
    let index = recreate(dir, build_name_schema)?;
    register_name_tokenizer(&index)?;
    Ok(index)
}

pub fn recreate_content_on_disk(dir: &Path) -> Result<Index> {
    let index = recreate(dir, build_content_schema)?;
    register_body_tokenizer(&index);
    Ok(index)
}

pub fn is_content_state_synced(dir: &Path) -> bool {
    content_state_sync_marker(dir).exists()
}

pub fn mark_content_state_synced(dir: &Path) -> Result<()> {
    let marker = content_state_sync_marker(dir);
    if marker.exists() {
        return Ok(()); // already marked — avoid a disk write per backfill pass
    }
    std::fs::write(marker, b"1\n")
        .with_context(|| format!("write content sync marker {}", dir.display()))
}

fn content_state_sync_marker(dir: &Path) -> std::path::PathBuf {
    dir.join(".content-state-synced")
}

/// Whether the index has already been swept for documents no state row
/// references. The sweep walks every live document, so repeating it on a
/// catalogue that is already clean is pure cost.
///
/// The marker is only trustworthy because exactly one thing creates such
/// documents — [`crate::content::clear_content_state`], which drops every row
/// and leaves the documents behind — and that function clears this marker. A
/// "done once" flag without that coupling would suppress the repair precisely
/// when it became necessary again.
pub fn is_unreferenced_pruned(dir: &Path) -> bool {
    unreferenced_prune_marker(dir).exists()
}

pub fn mark_unreferenced_pruned(dir: &Path) -> Result<()> {
    let marker = unreferenced_prune_marker(dir);
    if marker.exists() {
        return Ok(());
    }
    std::fs::write(marker, b"1\n")
        .with_context(|| format!("write unreferenced-prune marker {}", dir.display()))
}

/// Called when content state is cleared: the documents it abandons must be
/// swept again.
pub fn clear_unreferenced_pruned(dir: &Path) {
    let _ = std::fs::remove_file(unreferenced_prune_marker(dir));
}

fn unreferenced_prune_marker(dir: &Path) -> std::path::PathBuf {
    dir.join(".unreferenced-pruned")
}

fn recreate(dir: &Path, build_schema: fn() -> Schema) -> Result<Index> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        } else {
            std::fs::remove_file(&path).ok();
        }
    }
    create_in(dir, build_schema()).context("recreate index")
}

pub fn background_writer(index: &Index) -> Result<IndexWriter<TantivyDocument>> {
    low_impact_writer(
        index,
        writer_memory_budget_mb(
            "BIG_SEARCH_BACKGROUND_WRITER_MB",
            DEFAULT_BACKGROUND_WRITER_MEMORY_MB,
        ),
    )
}

pub fn bulk_writer(index: &Index) -> Result<IndexWriter<TantivyDocument>> {
    low_impact_writer(
        index,
        writer_memory_budget_mb("BIG_SEARCH_BULK_WRITER_MB", DEFAULT_BULK_WRITER_MEMORY_MB),
    )
}

pub fn content_writer(index: &Index) -> Result<IndexWriter<TantivyDocument>> {
    low_impact_writer(
        index,
        writer_memory_budget_mb(
            "BIG_SEARCH_CONTENT_WRITER_MB",
            DEFAULT_BACKGROUND_WRITER_MEMORY_MB,
        ),
    )
}

fn writer_memory_budget_mb(env_name: &str, default_mb: usize) -> usize {
    let adaptive_default = if crate::settings::low_memory_machine() {
        match default_mb {
            DEFAULT_BULK_WRITER_MEMORY_MB => 16,
            _ => MIN_WRITER_MEMORY_MB,
        }
    } else {
        default_mb
    };
    std::env::var(env_name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|megabytes| *megabytes >= MIN_WRITER_MEMORY_MB)
        .unwrap_or(adaptive_default)
        .saturating_mul(1 << 20)
}

fn low_impact_writer(index: &Index, memory_budget: usize) -> Result<IndexWriter<TantivyDocument>> {
    let options = IndexWriterOptions::builder()
        .num_worker_threads(1)
        .num_merge_threads(1)
        .memory_budget_per_thread(memory_budget)
        .build();
    index
        .writer_with_options(options)
        .context("open low-impact index writer")
}

pub fn document_count(reader: &tantivy::IndexReader) -> Result<u64> {
    Ok(reader.searcher().num_docs())
}

/// Visit the `path` of every live document. Resolves the field by name rather
/// than through [`fields`] so it serves the content index too — that schema has
/// no `name`/`source`, and the orphan sweep has to walk it.
pub fn for_each_indexed_path(
    index: &Index,
    mut visit: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let path_field = index.schema().get_field("path").context("field path")?;
    let searcher = index.reader()?.searcher();
    for segment in searcher.segment_readers() {
        let store = segment.get_store_reader(0)?;
        for doc in store.iter::<TantivyDocument>(segment.alive_bitset()) {
            let doc = doc?;
            let Some(path) = doc.get_first(path_field).and_then(|value| value.as_str()) else {
                continue;
            };
            visit(Path::new(path))?;
        }
    }
    Ok(())
}

pub fn max_repairable_name_deficit(cached_paths: u64) -> u64 {
    cached_paths
        .saturating_div(100)
        .max(MIN_REPAIRABLE_NAME_DEFICIT)
}

pub fn repair_deflated_against_state(index: &Index, state: &mut State) -> Result<u64> {
    let f = fields(index)?;
    state.begin_scan()?;
    for_each_indexed_path(index, |path| state.mark_seen(path))?;

    let mut writer = bulk_writer(index)?;
    let mut repaired = 0u64;
    let mut after: Option<String> = None;
    loop {
        let missing = state.unseen_paths_after(after.as_deref(), 2048)?;
        if missing.is_empty() {
            break;
        }
        after = missing
            .last()
            .map(|path| path.to_string_lossy().into_owned());
        for path in missing {
            crate::scan::add_name_only(&writer, &f, &path)?;
            repaired = repaired.saturating_add(1);
        }
    }
    writer.commit().context("commit repaired name index")?;
    Ok(repaired)
}

pub fn is_inflated_document_count(indexed_documents: u64, cached_paths: u64) -> bool {
    if cached_paths == 0 {
        return indexed_documents > 0;
    }
    let allowed_surplus = cached_paths
        .saturating_mul(MAX_INDEX_TO_STATE_EXTRA_PERCENT)
        .saturating_div(100)
        .max(MAX_INDEX_TO_STATE_SURPLUS);
    indexed_documents > cached_paths.saturating_add(allowed_surplus)
}

pub fn is_deflated_document_count(indexed_documents: u64, cached_paths: u64) -> bool {
    cached_paths > indexed_documents
}

pub fn is_divergent_document_count(indexed_documents: u64, cached_paths: u64) -> bool {
    is_inflated_document_count(indexed_documents, cached_paths)
        || is_deflated_document_count(indexed_documents, cached_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::FieldType;

    #[test]
    fn inflated_document_count_allows_small_drift_only() {
        assert!(!is_inflated_document_count(0, 0));
        assert!(is_inflated_document_count(1, 0));
        assert!(!is_inflated_document_count(120_000, 100_000));
        assert!(is_inflated_document_count(120_001, 100_000));
    }

    #[test]
    fn schema_avoids_position_indexing() {
        let schema = build_name_schema();
        let path = schema.get_field("path").unwrap();
        let name = schema.get_field("name").unwrap();
        let source = schema.get_field("source").unwrap();

        let path_entry = schema.get_field_entry(path);
        assert!(path_entry.is_indexed());
        assert!(path_entry.is_stored());
        assert!(!path_entry.has_fieldnorms());

        let name_entry = schema.get_field_entry(name);
        let name_record = match name_entry.field_type() {
            FieldType::Str(options) => options.get_indexing_options().unwrap().index_option(),
            other => panic!("name should be text, got {other:?}"),
        };
        let source_entry = schema.get_field_entry(source);

        assert_eq!(name_record, IndexRecordOption::Basic);
        assert!(!name_entry.has_fieldnorms());
        assert!(!source_entry.is_indexed());
        assert!(source_entry.is_stored());
    }

    #[test]
    fn content_schema_keeps_body_out_of_name_index() {
        let name_schema = build_name_schema();
        assert!(name_schema.get_field("body").is_err());

        let content_schema = build_content_schema();
        let body = content_schema.get_field("body").unwrap();
        let body_record = match content_schema.get_field_entry(body).field_type() {
            FieldType::Str(options) => options.get_indexing_options().unwrap().index_option(),
            other => panic!("body should be text, got {other:?}"),
        };
        assert_eq!(body_record, content_record_option());
    }

    #[test]
    fn read_only_content_uses_stored_profile_without_rewriting_it() {
        for record in [IndexRecordOption::Basic, IndexRecordOption::WithFreqs] {
            let dir = std::env::temp_dir().join(format!(
                "big-search-read-profile-{}-{record:?}",
                std::process::id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            let index = create_content_fixture(&dir, record).unwrap();
            let fields = content_fields(&index).unwrap();
            let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
            writer
                .add_document(
                    tantivy::doc!(fields.path => "/sample.txt", fields.body => "CAFÉ 東京"),
                )
                .unwrap();
            writer.commit().unwrap();
            writer.wait_merging_threads().unwrap();
            let before = std::fs::read(dir.join("meta.json")).unwrap();
            drop(index);
            let read_only = open_read_only(&dir).unwrap();
            assert_eq!(read_only.schema(), build_content_schema_for(record));
            let reader = read_only.reader().unwrap();
            for query in ["cafe", "東京"] {
                let hits = crate::query::search_content(&reader, query, 10).unwrap();
                assert_eq!(hits.len(), 1, "{record:?}: {query}");
                assert_eq!(hits[0].path, "/sample.txt");
            }
            assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
            drop(reader);
            drop(read_only);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn open_or_create_recreates_corrupt_non_empty_index() {
        let dir = std::env::temp_dir().join(format!("bs-index-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"not tantivy metadata").unwrap();

        let index = open_or_create(&dir).unwrap();

        assert_eq!(document_count(&index.reader().unwrap()).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repair_deflated_index_adds_missing_state_paths_without_wipe() {
        let dir = std::env::temp_dir().join(format!("bs-index-repair-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let index = open_or_create(&dir.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = bulk_writer(&index).unwrap();
        crate::scan::add_name_only(&writer, &f, Path::new("/catalog/keep-alpha.txt")).unwrap();
        writer.commit().unwrap();
        drop(writer);

        let mut state = State::empty(dir.join("state.json"));
        state.set(Path::new("/catalog/keep-alpha.txt"), 10, 1);
        state.set(Path::new("/catalog/missing-beta.txt"), 20, 2);
        state.set(Path::new("/catalog/missing-gamma.txt"), 30, 3);
        state.persist().unwrap();
        let mut state = State::load(dir.join("state.json"));

        assert_eq!(
            repair_deflated_against_state(&index, &mut state).unwrap(),
            2
        );
        assert_eq!(
            document_count(&index.reader().unwrap()).unwrap(),
            state.len() as u64
        );
        assert_eq!(
            crate::query::search(
                &index.reader().unwrap(),
                "alpha",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()[0]
                .path,
            "/catalog/keep-alpha.txt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
