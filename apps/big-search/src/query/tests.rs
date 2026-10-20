use super::*;
use crate::catalogue::{CatalogueReader, NAME_NAMESPACE, State};
use crate::index::{content_fields, fields, open_or_create};
use std::path::PathBuf;
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

/// A reader of a catalogue that does not exist, for queries that never reach it.
fn no_catalogue() -> CatalogueReader {
    CatalogueReader::new(Path::new("/nonexistent/big-search/state.sqlite"))
}

/// A catalogue recording `files` with the given modification times and
/// sizes, in a fresh directory under `dir`.
fn catalogue_with(dir: &Path, files: &[(&Path, i64, u64)]) -> CatalogueReader {
    let db = dir.join("state/state.sqlite");
    let mut names = State::empty(&db, NAME_NAMESPACE);
    for (path, mtime, size) in files {
        names.set(path, *mtime, *size);
    }
    names.persist().unwrap();
    CatalogueReader::new(&db)
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
            catalogue: &no_catalogue(),
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
    let catalogue = catalogue_with(
        &root,
        &paths
            .iter()
            .map(|path| (path.as_path(), 1, 1))
            .collect::<Vec<_>>(),
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
                catalogue: &catalogue,
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

/// The `largest` mode is the `list` mode ordered by recorded size.
#[test]
fn largest_pages_by_size() {
    let root = std::env::temp_dir().join(format!("lsearch-largest-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    // Recorded (mtime, size), in neither name nor time order.
    let recorded = [
        ("small.iso", 100, 10),
        ("huge.iso", 300, 900),
        ("tie-b.iso", 200, 500),
        ("tie-a.iso", 400, 500),
        ("recent.iso", 9_000, 2_000),
    ];
    let paths: Vec<PathBuf> = recorded
        .iter()
        .map(|(name, _, _)| {
            let path = root.join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        })
        .collect();
    let catalogue = catalogue_with(
        &root,
        &paths
            .iter()
            .zip(recorded)
            .map(|(path, (_, mtime, size))| (path.as_path(), mtime, size))
            .collect::<Vec<_>>(),
    );
    let owned: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let index = index_with(
        "largest",
        &owned.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    // "Large and old": everything not changed since 1000.
    let filter = Filter {
        under: Some(root.to_string_lossy().into_owned()),
        mtime: Some([i64::MIN, 1_000]),
        ..Default::default()
    };
    let page = |offset, limit| {
        run_query(
            QueryIndexes {
                name: &index,
                content: None,
                catalogue: &catalogue,
            },
            "",
            "largest",
            &filter,
            &Page { offset, limit },
            false,
            None,
        )
        .unwrap()
    };
    let names = |outcome: &QueryOutcome| -> Vec<String> {
        outcome.hits.iter().map(|hit| hit.name.clone()).collect()
    };

    let first = page(0, 3);
    // Same size: the path breaks the tie.
    assert_eq!(names(&first), ["huge.iso", "tie-a.iso", "tie-b.iso"]);
    assert_eq!((first.total, first.next_offset), (4, Some(3)));
    let last = page(3, 3);
    assert_eq!(names(&last), ["small.iso"]);
    assert_eq!((last.total, last.next_offset), (4, None));
    std::fs::remove_dir_all(&root).ok();
}

/// A type filter finds videos by what they are, extension or not.
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
}

/// The `list` mode answers newest first by the catalogue's recorded times,
/// counts every match, and pages through all of them.
#[test]
fn list_pages_newest_first() {
    let root = std::env::temp_dir().join(format!("lsearch-newest-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let pictures = root.join("Pictures");
    std::fs::create_dir_all(&pictures).unwrap();
    // Recorded times, not in name or creation order.
    let recorded = [
        ("c.jpg", 300),
        ("a.jpg", 500),
        ("e.png", 100),
        ("b.JPG", 400),
        ("d.jpg", 200),
    ];
    let files: Vec<(PathBuf, i64)> = recorded
        .iter()
        .map(|(name, mtime)| {
            let path = pictures.join(name);
            std::fs::write(&path, b"x").unwrap();
            (path, *mtime)
        })
        .collect();
    let catalogue = catalogue_with(
        &root,
        &files
            .iter()
            .map(|(path, mtime)| (path.as_path(), *mtime, 1))
            .collect::<Vec<_>>(),
    );
    let owned: Vec<String> = files
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    let index = index_with(
        "newest",
        &owned.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let filter = Filter {
        under: Some(pictures.to_string_lossy().into_owned()),
        ext: vec!["jpg".into(), "png".into()],
        ..Default::default()
    };
    let page = |offset, limit| {
        run_query(
            QueryIndexes {
                name: &index,
                content: None,
                catalogue: &catalogue,
            },
            "",
            "list",
            &filter,
            &Page { offset, limit },
            false,
            None,
        )
        .unwrap()
    };
    let names = |outcome: &QueryOutcome| -> Vec<String> {
        outcome.hits.iter().map(|hit| hit.name.clone()).collect()
    };

    let first = page(0, 2);
    assert_eq!(names(&first), ["a.jpg", "b.JPG"]);
    assert_eq!((first.total, first.next_offset), (5, Some(2)));
    // Enriched like any page: the file's own stat and extension.
    assert!(first.hits.iter().all(|hit| hit.available && hit.size == 1));
    assert_eq!(first.hits[1].ext, "jpg");
    let second = page(2, 2);
    assert_eq!(names(&second), ["c.jpg", "d.jpg"]);
    assert_eq!((second.total, second.next_offset), (5, Some(4)));
    let last = page(4, 2);
    assert_eq!(names(&last), ["e.png"]);
    assert_eq!((last.total, last.next_offset), (5, None));

    // A file deleted before the watcher noticed is dropped from its page,
    // and the next page still starts after the rows this one covered.
    std::fs::remove_file(pictures.join("b.JPG")).unwrap();
    let first = page(0, 2);
    assert_eq!(names(&first), ["a.jpg"]);
    assert_eq!((first.total, first.next_offset), (5, Some(2)));
    assert_eq!(names(&page(2, 2)), ["c.jpg", "d.jpg"]);
    std::fs::remove_dir_all(&root).ok();
}

/// `under` keeps what is inside the folder and not the folder itself, as
/// the catalogue's range does.
#[test]
fn under_leaves_the_folder_out() {
    let kept = |under: &str| -> Vec<String> {
        let hits = ["/home/a", "/home/a/x.jpg", "/home/ab/y.jpg"].map(|path| Hit {
            path: path.to_string(),
            ..Default::default()
        });
        let filter = Filter {
            under: Some(under.to_string()),
            ..Default::default()
        };
        keep_path_matches(hits.into(), &filter)
            .into_iter()
            .map(|hit| hit.path)
            .collect()
    };
    assert_eq!(kept("/home/a/"), ["/home/a/x.jpg"]);
    assert_eq!(kept("/").len(), 3, "the root holds everything");
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
            catalogue: &no_catalogue(),
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
