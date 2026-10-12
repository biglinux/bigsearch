use super::*;
use crate::index::{content_fields, fields, open_or_create};
use tantivy::{IndexReader, doc};

pub(super) fn index_with(label: &str, paths: &[&str]) -> tantivy::IndexReader {
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

pub(super) fn content_index_with(label: &str, entries: &[(&str, &str)]) -> tantivy::IndexReader {
    content_index_with_profile(
        label,
        entries,
        tantivy::schema::IndexRecordOption::WithFreqs,
    )
}

pub(super) fn content_index_with_profile(
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

pub(super) fn names(hits: Vec<Hit>) -> Vec<String> {
    hits.into_iter().map(|hit| hit.name).collect()
}

/// `run_query` over a name index alone, without origins.
fn run_names(
    index: &IndexReader,
    q: &str,
    mode: &str,
    filter: &Filter,
    page: &Page,
) -> QueryOutcome {
    run_query(
        QueryIndexes {
            name: index,
            content: None,
        },
        q,
        mode,
        filter,
        page,
        false,
        None,
    )
    .unwrap()
}

/// "Every PDF in my home folder" has no words to search for.
///
/// The name search refuses a query under two characters, because the ngram
/// index cannot narrow one, so a caller narrowing purely by filter needs
/// this mode or would have to walk the disk itself beside an index that
/// already knows the answer.
#[test]
fn list_mode_answers_wordless_queries() {
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
        run_names(
            &index,
            "",
            mode,
            &filter,
            &Page {
                offset: 0,
                limit: 50,
            },
        )
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
fn type_filter_finds_videos_by_content() {
    let dir = std::env::temp_dir().join(format!("lsearch-mime-query-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let mut stream = vec![0_u8; 188 * 4];
    stream.iter_mut().step_by(188).for_each(|byte| *byte = 0x47);
    // Only types the bytes tell: a type known by its name alone needs the
    // system's MIME database, which mime.rs tests on its own.
    let files: [(&str, &[u8]); 3] = [
        ("clip.ts", &stream),
        ("recording", &stream),
        ("app.ts", b"export const clip = 1;\n"),
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
    let outcome = run_names(
        &index,
        "",
        "list",
        &filter,
        &Page {
            offset: 0,
            limit: 50,
        },
    );
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
            ("recording".to_string(), "video/mp2t".to_string()),
        ]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_sees_past_the_ceiling() {
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
    let outcome = run_names(
        &index,
        "",
        "list",
        &filter,
        &Page {
            offset: 0,
            limit: 50,
        },
    );
    let mut exact: Vec<&str> = outcome.hits.iter().map(|hit| hit.path.as_str()).collect();
    exact.sort_unstable();
    assert!(
        exact.iter().all(|path| !path.ends_with(".part")),
        "{exact:?}"
    );
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
    let outcome = run_names(
        &index,
        "nota",
        "name",
        &Filter::default(),
        &Page {
            offset: 0,
            limit: 5,
        },
    );
    assert_eq!(outcome.hits.len(), 5);
    assert_eq!(outcome.total, 30);
    assert_eq!(outcome.next_offset, Some(5));
    std::fs::remove_dir_all(&dir).ok();
}

/// A file found by name and by content is one hit and counts once.
#[test]
fn both_mode_counts_each_file_once() {
    let dir = std::env::temp_dir().join(format!("lsearch-both-total-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let owned: Vec<String> = (0..3)
        .map(|i| {
            let path = dir.join(format!("recibo {i}.txt"));
            std::fs::write(&path, b"x").unwrap();
            path.to_string_lossy().into_owned()
        })
        .collect();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let names = index_with("both-total", &refs);
    let entries: Vec<(&str, &str)> = refs.iter().map(|p| (*p, "recibo pago")).collect();
    let content = content_index_with("both-total", &entries);
    let outcome = run_query(
        QueryIndexes {
            name: &names,
            content: Some(&content),
        },
        "recibo",
        "both",
        &Filter::default(),
        &Page {
            offset: 0,
            limit: 2,
        },
        false,
        None,
    )
    .unwrap();
    assert_eq!(outcome.hits.len(), 2);
    assert_eq!(outcome.total, 3);
    assert_eq!(outcome.next_offset, Some(2));
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
    let item = |filter: &Filter, page: &Page| run_names(&index, "item", "name", filter, page);

    // Base: "item" matches all three; metadata is resolved server-side.
    let all = item(&Filter::default(), &Page::default());
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
    assert_eq!(item(&f, &Page::default()).total, 2);

    // under filter (path-only) → only the file in sub/.
    let f = Filter {
        under: Some(dir.join("sub").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let u = item(&f, &Page::default());
    assert_eq!(u.total, 1);
    assert!(u.hits[0].path.ends_with("item_two.md"));

    // size filter (stat) → excludes the 2000-byte file.
    let f = Filter {
        size: Some([0, 10]),
        ..Default::default()
    };
    assert_eq!(item(&f, &Page::default()).total, 2);

    assert_pages_of_two(&index);

    std::fs::remove_dir_all(&dir).ok();
}

/// Pagination over the three `item` files: limit 2 → first page has
/// next_offset; second page ends.
fn assert_pages_of_two(index: &IndexReader) {
    let p0 = run_names(
        index,
        "item",
        "name",
        &Filter::default(),
        &Page {
            offset: 0,
            limit: 2,
        },
    );
    assert_eq!(p0.hits.len(), 2);
    assert_eq!(p0.total, 3);
    assert_eq!(p0.next_offset, Some(2));
    let p1 = run_names(
        index,
        "item",
        "name",
        &Filter::default(),
        &Page {
            offset: 2,
            limit: 2,
        },
    );
    assert_eq!(p1.hits.len(), 1);
    assert_eq!(p1.next_offset, None);
}

/// `offline · on <name>` reads as "it lives on a disk you unplugged". With
/// no removable or network source configured, a path that will not stat is
/// gone and is dropped.
#[test]
fn deleted_file_is_dropped() {
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

    let outcome = run_names(&index, "nota", "name", &Filter::default(), &Page::default());

    assert_eq!(outcome.hits.len(), 1, "the deleted file must not be listed");
    assert_eq!(outcome.hits[0].path, kept.to_string_lossy());
    assert!(
        outcome.hits[0].available,
        "a file that stats is available, never flagged offline"
    );

    std::fs::remove_dir_all(&dir).ok();
}
