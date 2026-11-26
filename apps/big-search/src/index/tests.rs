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
    // A name index measured on a desktop: 1 of 3 documents dead.
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
    // Both schema reads below resolve the mode another test may be setting.
    let _lock = crate::test_env::ENV_LOCK.lock().unwrap();
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
        let dir = crate::test_env::scratch_dir(&format!("big-search-read-profile-{record:?}"));
        let index = create_content_fixture(&dir, record).unwrap();
        let fields = content_fields(&index).unwrap();
        let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(fields.path => "/sample.txt", fields.body => "CAFÉ 東京"))
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

/// Guards: persisted data: under `auto` the service (sized by its memory
/// limit) and a shell command (by the machine's RAM) open the same index.
#[test]
fn auto_keeps_stored_content_detail() {
    use crate::test_env::{ENV_LOCK, EnvVarGuard};
    let _lock = ENV_LOCK.lock().unwrap();
    for (stored, explicit, explicit_record) in [
        (
            IndexRecordOption::Basic,
            "freqs",
            IndexRecordOption::WithFreqs,
        ),
        (
            IndexRecordOption::WithFreqs,
            "basic",
            IndexRecordOption::Basic,
        ),
    ] {
        let dir = crate::test_env::scratch_dir(&format!("bs-content-detail-{stored:?}"));
        let index = create_content_fixture(&dir, stored).unwrap();
        let fields = content_fields(&index).unwrap();
        let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(fields.path => "/kept.txt", fields.body => "kept"))
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        drop(index);

        let auto = EnvVarGuard::set("BIG_SEARCH_CONTENT_INDEX_MODE", "auto");
        let reopened = open_content_or_create(&dir).unwrap();
        assert_eq!(reopened.schema(), build_content_schema_for(stored));
        assert_eq!(document_count(&reopened.reader().unwrap()), 1, "{stored:?}");
        drop(reopened);
        drop(auto);

        let _explicit = EnvVarGuard::set("BIG_SEARCH_CONTENT_INDEX_MODE", explicit);
        let rebuilt = open_content_or_create(&dir).unwrap();
        assert_eq!(rebuilt.schema(), build_content_schema_for(explicit_record));
        assert_eq!(document_count(&rebuilt.reader().unwrap()), 0, "{explicit}");
        drop(rebuilt);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn corrupt_index_is_recreated() {
    let dir = crate::test_env::scratch_dir("bs-index-corrupt");
    std::fs::write(dir.join("meta.json"), b"not tantivy metadata").unwrap();

    let index = open_or_create(&dir).unwrap();

    assert_eq!(document_count(&index.reader().unwrap()), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deflated_index_repair_adds_missing_paths() {
    let dir = crate::test_env::scratch_dir("bs-index-repair");

    let index = open_or_create(&dir.join("idx")).unwrap();
    let f = fields(&index).unwrap();
    let mut writer = bulk_writer(&index).unwrap();
    crate::scan::add_name_only(&writer, &f, Path::new("/catalog/keep-alpha.txt"), None).unwrap();
    writer.commit().unwrap();
    drop(writer);

    let mut state =
        State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE).unwrap();
    state.set(Path::new("/catalog/keep-alpha.txt"), 10, 1);
    state.set(Path::new("/catalog/missing-beta.txt"), 20, 2);
    state.set(Path::new("/catalog/missing-gamma.txt"), 30, 3);
    state.persist().unwrap();
    let mut state = State::load(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);

    assert_eq!(
        repair_deflated_against_state(&index, &mut state).unwrap(),
        2
    );
    assert_eq!(document_count(&index.reader().unwrap()), state.len() as u64);
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

    let _ = std::fs::remove_dir_all(&dir);
}
