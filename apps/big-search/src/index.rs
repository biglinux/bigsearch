//! Tantivy indexes: a compact name index (`path`, `name`, `source`) and a
//! separate content index (`path`, `body`).
use crate::catalogue::State;
use crate::cjk_tokenizer::CjkFriendlyTokenizer;
use anyhow::{Context, Result};
use std::path::Path;
use tantivy::indexer::IndexWriterOptions;
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenFilter,
    TokenStream, Tokenizer,
};
use tantivy::{Index, IndexWriter, TantivyDocument};

/// Registered analyzer for the `name` field: [`NameGrams`] of [`name_text`].
/// Its name changes with what it emits (last: `name_grams`, before NFKC and
/// single CJK characters), so the on-disk schema differs and the name index
/// rebuilds.
pub const NGRAM: &str = "name_nfkc";
/// Registered analyzer for the `body` field: CJK-bigram aware with every CJK
/// character alone too, word-splitting otherwise, folded like names
/// ([`ScriptFold`] then ASCII folding). Renamed the same way (last:
/// `body_folded`), so the content index rebuilds with it.
pub const BODY: &str = "body_nfkc";
/// The analyzer content queries go through: [`BODY`] without the single
/// characters of a CJK run (see [`CjkFriendlyTokenizer`]).
pub const BODY_QUERY: &str = "body_nfkc_query";
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
    // Field norms make a shorter name rank above a longer one with the same
    // grams, which is most often the one meant: "go" -> "GO.pdf" first.
    let name_indexing = TextFieldIndexing::default()
        .set_tokenizer(NGRAM)
        .set_index_option(IndexRecordOption::Basic);
    sb.add_text_field(
        "name",
        TextOptions::default().set_indexing_options(name_indexing),
    );
    // Source/device display name (stored, untokenised) so a hit can report which
    // device it lives on, kept even when that device is offline.
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
    // The whole name without its extension, folded, as one term (see
    // `stem_key`): the exact name ranks first even when only a page of
    // candidates is read.
    sb.add_text_field("stem", raw_identifier_options(false));
    // The day the file last changed, for ranking recent files first among
    // equal matches. A day and not the second, so saving a file again the same
    // day leaves its name document alone (see `scan::name_doc_stale`).
    sb.add_u64_field("mtime_day", NumericOptions::default().set_fast());
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

/// Register the name analyzer: plain grams, because [`name_text`] already
/// folded the text. Folding after cutting the grams changed their length
/// ("ß" -> "ss") and broke every gram across the letter.
fn register_name_tokenizer(index: &Index) {
    index
        .tokenizers()
        .register(NGRAM, TextAnalyzer::builder(NameGrams).build());
}

/// The `name` field's grams: every two- and three-character slice of
/// [`name_text`], and each character of a space-less script alone, since one
/// Chinese or Korean character is often a word.
#[derive(Clone)]
struct NameGrams;

impl Tokenizer for NameGrams {
    type TokenStream<'a> = NameGramStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> NameGramStream<'a> {
        NameGramStream {
            text,
            start: 0,
            length: 0,
            token: Token {
                position: 0,
                ..Token::default()
            },
        }
    }
}

/// Like tantivy's `NgramTokenizer`, every token is at position 0: the field
/// keeps no positions.
struct NameGramStream<'a> {
    text: &'a str,
    /// Byte offset of the character the next grams start at.
    start: usize,
    /// Characters in the gram last considered at `start`.
    length: usize,
    token: Token,
}

impl TokenStream for NameGramStream<'_> {
    fn advance(&mut self) -> bool {
        while let Some(first) = self.text[self.start..].chars().next() {
            self.length += 1;
            if self.length == 1 && !crate::cjk_tokenizer::is_unspaced(first) {
                continue;
            }
            if self.length <= 3
                && let Some((at, last)) =
                    self.text[self.start..].char_indices().nth(self.length - 1)
            {
                let end = self.start + at + last.len_utf8();
                self.token.offset_from = self.start;
                self.token.offset_to = end;
                self.token.text.clear();
                self.token.text.push_str(&self.text[self.start..end]);
                return true;
            }
            self.start += first.len_utf8();
            self.length = 0;
        }
        false
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// The text the `name` field indexes and every name comparison reads: folded
/// (see [`crate::extract::fold_lower`]), separators as spaces, and a space at
/// each end. The spaces make the edges of words grams of their own (" go"
/// exists only where a word starts with "go", "go " only where one ends with
/// it), so one or two letters find the names that are those letters
/// ("GO.pdf", "Contrato GO.pdf") instead of every name containing them.
pub fn name_text(name: &str) -> String {
    let mut text = String::with_capacity(name.len() + 2);
    text.push(' ');
    for word in name_words(name) {
        text.push_str(&word);
        text.push(' ');
    }
    text
}

/// The `stem` term of a file name: its words without the extension, folded and
/// joined by single spaces, what a query for exactly that name joins to.
pub fn stem_key(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    name_words(&stem).join(" ")
}

/// The folded words of a name or of a name query, split at separators.
pub fn name_words(text: &str) -> Vec<String> {
    crate::extract::fold_lower(text)
        .split(is_name_separator)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What separates the words of a file name: spaces, ASCII punctuation
/// ("relatorio_final-v2.pdf") and the common dashes, quotes and CJK marks.
fn is_name_separator(c: char) -> bool {
    c.is_whitespace()
        || c.is_ascii_punctuation()
        || matches!(
            c,
            '–' | '—'
                | '‐'
                | '·'
                | '•'
                | '«'
                | '»'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '、'
                | '。'
                | '，'
                | '（'
                | '）'
                | '【'
                | '】'
                | '「'
                | '」'
        )
}

/// Register the content word analyzers, for the index and for queries.
fn register_body_tokenizer(index: &Index) {
    for (name, unigrams) in [(BODY, true), (BODY_QUERY, false)] {
        index.tokenizers().register(
            name,
            TextAnalyzer::builder(CjkFriendlyTokenizer { unigrams })
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(ScriptFold)
                .filter(AsciiFoldingFilter)
                .build(),
        );
    }
}

/// The folding of [`crate::extract::fold_script_into`] for content words, so a
/// word is indexed and queried the way names are compared: "отчет" finds
/// "Отчёт", "αναφορα" "Αναφορά", "احمد" "أحمد", a decomposed accent its
/// precomposed twin. An Arabic word also loses its article (ال, وال, بال,
/// كال, فال, لل) when three letters or more remain, so "مبيعات" and
/// "المبيعات" are one word.
#[derive(Clone)]
pub struct ScriptFold;

/// The Arabic article and the prepositions that fuse with it, longest first.
pub(crate) const ARABIC_ARTICLES: &[&str] = &["وال", "بال", "كال", "فال", "ال", "لل"];

impl TokenFilter for ScriptFold {
    type Tokenizer<T: Tokenizer> = ScriptFoldFilter<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        ScriptFoldFilter {
            tokenizer,
            buffer: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct ScriptFoldFilter<T> {
    tokenizer: T,
    buffer: String,
}

impl<T: Tokenizer> Tokenizer for ScriptFoldFilter<T> {
    type TokenStream<'a> = ScriptFoldStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        ScriptFoldStream {
            tail: self.tokenizer.token_stream(text),
            buffer: &mut self.buffer,
        }
    }
}

pub struct ScriptFoldStream<'a, T> {
    buffer: &'a mut String,
    tail: T,
}

impl<T: TokenStream> TokenStream for ScriptFoldStream<'_, T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let text = &self.tail.token().text;
        if text.is_ascii() {
            return true;
        }
        self.buffer.clear();
        crate::extract::fold_script_into(text, self.buffer);
        if self
            .buffer
            .starts_with(|c: char| ('\u{0600}'..='\u{06FF}').contains(&c))
            && let Some(rest) = ARABIC_ARTICLES
                .iter()
                .find_map(|article| self.buffer.strip_prefix(article))
                .filter(|rest| rest.chars().count() >= 3)
        {
            *self.buffer = rest.to_owned();
        }
        std::mem::swap(&mut self.tail.token_mut().text, self.buffer);
        true
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

/// Resolved field handles for the name index schema.
pub struct Fields {
    pub path: Field,
    pub name: Field,
    pub source: Field,
    pub dir: Field,
    pub ext: Field,
    pub mime: Field,
    pub stem: Field,
    pub mtime_day: Field,
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
        stem: schema.get_field("stem").context("field stem")?,
        mtime_day: schema.get_field("mtime_day").context("field mtime_day")?,
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
/// wipe the catalogue, and a query racing a daemon that holds the writer lock
/// but has not yet bound its socket would do it underneath the daemon. A read
/// command must fail instead.
pub fn open_read_only(dir: &Path) -> Result<Index> {
    open_read_only_with(dir, crate::nosync_dir::NoFsyncDirectory::open)
}

/// [`open_read_only`] for searching: mapped for small random reads (see
/// [`NoFsyncDirectory::open_for_queries`]). Commands that walk every document
/// (`count`, `list`) keep [`open_read_only`] and the kernel's read-ahead.
///
/// [`NoFsyncDirectory::open_for_queries`]: crate::nosync_dir::NoFsyncDirectory::open_for_queries
pub fn open_for_queries(dir: &Path) -> Result<Index> {
    open_read_only_with(dir, crate::nosync_dir::NoFsyncDirectory::open_for_queries)
}

/// A reader for answering queries on an index from [`open_for_queries`]: its
/// cache of decompressed store blocks holds 8 per segment instead of tantivy's
/// 100. A query reads only the documents of its page, a few blocks.
pub fn query_reader(index: &Index) -> Result<tantivy::IndexReader> {
    Ok(index
        .reader_builder()
        .doc_store_cache_num_blocks(8)
        .try_into()?)
}

fn open_read_only_with(
    dir: &Path,
    directory: fn(&Path) -> tantivy::Result<crate::nosync_dir::NoFsyncDirectory>,
) -> Result<Index> {
    let index = open_existing_with(dir, directory).with_context(|| {
        format!(
            "no usable index at {} (is the daemon running?)",
            dir.display()
        )
    })?;
    if index.schema() == build_name_schema() {
        register_name_tokenizer(&index);
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
    register_name_tokenizer(&index);
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
    open_existing_with(dir, crate::nosync_dir::NoFsyncDirectory::open)
}

fn open_existing_with(
    dir: &Path,
    directory: fn(&Path) -> tantivy::Result<crate::nosync_dir::NoFsyncDirectory>,
) -> Result<Index> {
    let index = Index::open(directory(dir)?)?;
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
    register_name_tokenizer(&index);
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
        return Ok(()); // already marked: avoid a disk write per backfill pass
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
/// documents ([`crate::content::clear_content_state`], which drops every row
/// and leaves the documents behind) and that function clears this marker. A
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

/// Whether merging every segment into one is worth what it costs, given each
/// segment's `(documents, deleted)`: when a tenth of the index has changed.
/// A new file lands outside the largest segment, a removed one is a deletion,
/// and an edit is both, so the larger of the two counts the changed files,
/// each once.
///
/// One segment answers a cold query with a fraction of the disk reads, and a
/// merged index is smaller. Deleted documents only leave a segment when it is
/// merged, and tantivy's merge policy never merges a large segment for its
/// deletes: the daemon's largest segments carry many dead documents. Each merge
/// rewrites the whole index, so it waits until a tenth has changed, which keeps
/// merge writes a small share of what the edits themselves write.
pub fn worth_compacting(segments: &[(u32, u32)]) -> bool {
    let total: u64 = segments.iter().map(|&(docs, _)| u64::from(docs)).sum();
    let Some(&(largest, _)) = segments.iter().max_by_key(|&&(docs, _)| docs) else {
        return false;
    };
    let outside = total - u64::from(largest);
    let deleted: u64 = segments
        .iter()
        .map(|&(_, deleted)| u64::from(deleted))
        .sum();
    let changed = outside.max(deleted);
    changed > 0 && changed * 10 >= total
}

pub fn document_count(reader: &tantivy::IndexReader) -> Result<u64> {
    Ok(reader.searcher().num_docs())
}

/// Visit the `path` of every live document. Resolves the field by name rather
/// than through [`fields`] so it serves the content index too: that schema has
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
    for_each_indexed_path(index, |path| state.mark_seen(path, None))?;

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
            crate::scan::add_name_only(&writer, &f, &path, None)?;
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

    fn grams(text: &str) -> Vec<String> {
        let mut tokenizer = NameGrams;
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    /// The name grams are tantivy's 2- and 3-grams, plus each CJK character.
    #[test]
    fn name_grams_add_cjk_characters() {
        for text in [" relatorio final ", " go ", " a ", "", " ab"] {
            let mut ngram = tantivy::tokenizer::NgramTokenizer::all_ngrams(2, 3).unwrap();
            let mut stream = ngram.token_stream(text);
            let mut expected = Vec::new();
            while stream.advance() {
                expected.push(stream.token().text.clone());
            }
            assert_eq!(grams(text), expected, "for {text:?}");
        }
        assert_eq!(
            grams(" 报告 x"),
            [
                " 报", " 报告", "报", "报告", "报告 ", "告", "告 ", "告 x", " x"
            ]
        );
    }

    #[test]
    fn compacts_after_a_tenth_changes() {
        assert!(!worth_compacting(&[]));
        assert!(!worth_compacting(&[(1000, 0)]));
        assert!(!worth_compacting(&[(1000, 50), (40, 0)]));
        // 60 files edited: 60 deleted and 60 new versions, 6 % of 1060.
        assert!(!worth_compacting(&[(1000, 60), (60, 0)]));
        assert!(worth_compacting(&[(1000, 120), (120, 0)]));
        assert!(worth_compacting(&[(1000, 100)]));
        assert!(worth_compacting(&[(1000, 0), (70, 0), (50, 0)]));
        // The daemon's own name index before the change: 1 of 3 documents dead.
        assert!(worth_compacting(&[
            (102_746, 34_296),
            (18_215, 851),
            (16_389, 9),
            (7_403, 1),
            (1, 0)
        ]));
    }

    #[test]
    fn inflated_count_allows_small_drift() {
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
        // Norms rank a shorter name first among names with the same grams.
        assert!(name_entry.has_fieldnorms());
        assert!(!source_entry.is_indexed());
        assert!(source_entry.is_stored());
    }

    #[test]
    fn body_stays_out_of_name_index() {
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
    fn read_only_content_keeps_stored_profile() {
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
    fn corrupt_index_is_recreated() {
        let dir = std::env::temp_dir().join(format!("bs-index-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"not tantivy metadata").unwrap();

        let index = open_or_create(&dir).unwrap();

        assert_eq!(document_count(&index.reader().unwrap()).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deflated_index_repair_adds_missing_paths() {
        let dir = std::env::temp_dir().join(format!("bs-index-repair-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let index = open_or_create(&dir.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = bulk_writer(&index).unwrap();
        crate::scan::add_name_only(&writer, &f, Path::new("/catalog/keep-alpha.txt"), None)
            .unwrap();
        writer.commit().unwrap();
        drop(writer);

        let mut state = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        state.set(Path::new("/catalog/keep-alpha.txt"), 10, 1);
        state.set(Path::new("/catalog/missing-beta.txt"), 20, 2);
        state.set(Path::new("/catalog/missing-gamma.txt"), 30, 3);
        state.persist().unwrap();
        let mut state = State::load(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);

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
