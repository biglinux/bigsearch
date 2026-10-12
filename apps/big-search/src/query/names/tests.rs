use super::*;
use crate::query::tests::{index_with, names};

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

/// One or two letters find the names that are exactly them, then the names
/// where they stand as a word, and never a name that merely contains them.
#[test]
fn short_queries_prefer_whole_names() {
    let index = index_with(
        "short-words",
        &[
            "/h/Contrato GO.pdf",
            "/h/Google.pdf",
            "/h/algoritmo.odt",
            "/h/GO.pdf",
            "/h/Relatório-SP-2026.odt",
            "/h/a.txt",
            "/h/casa.txt",
        ],
    );
    let found = names(search(&index, "go", &Filter::default(), 20).unwrap());
    assert_eq!(found, ["GO.pdf", "Contrato GO.pdf"]);
    assert_eq!(
        names(search(&index, "SP", &Filter::default(), 20).unwrap()),
        ["Relatório-SP-2026.odt"]
    );
    assert_eq!(
        names(search(&index, "a", &Filter::default(), 20).unwrap()),
        ["a.txt"]
    );
}

/// A name is found the way a person types it, whatever script, accents,
/// decomposition or letter forms it was written with.
#[test]
fn names_match_across_letter_forms() {
    let index = index_with(
        "scripts",
        &[
            "/h/Straße Bericht.txt",
            "/h/İstanbul raporu.txt",
            "/h/Отчёт о продажах.txt",
            "/h/Αναφορά πωλήσεων.txt",
            "/h/Łódź.pdf",
            "/h/Relato\u{301}rio de vendas.pdf",
        ],
    );
    for (query, name) in [
        ("strasse", "Straße Bericht.txt"),
        ("istanbul", "İstanbul raporu.txt"),
        ("отчет", "Отчёт о продажах.txt"),
        ("αναφορα", "Αναφορά πωλήσεων.txt"),
        ("lodz", "Łódź.pdf"),
        ("relatorio", "Relato\u{301}rio de vendas.pdf"),
    ] {
        assert_eq!(
            names(search(&index, query, &Filter::default(), 20).unwrap()),
            [name],
            "query {query:?}"
        );
    }
}

/// The whole name first, then names holding the word, then names where a
/// word starts with it, then the rest.
#[test]
fn names_rank_by_how_they_match() {
    let index = index_with(
        "name-rank",
        &[
            "/h/xrelatoriox.txt",
            "/h/pre-relatorios-antigos.pdf",
            "/h/meu relatorio de vendas.pdf",
            "/h/relatorio a.md",
            // The whole name, though longer than "relatorio a.md".
            "/h/relatorio.markdown",
        ],
    );
    assert_eq!(
        names(search(&index, "relatorio", &Filter::default(), 20).unwrap()),
        [
            "relatorio.markdown",
            "relatorio a.md",
            "meu relatorio de vendas.pdf",
            "pre-relatorios-antigos.pdf",
            "xrelatoriox.txt"
        ]
    );
}

/// Among names that match equally, the one changed most recently first.
#[test]
fn equal_names_rank_newest_first() {
    let dir = std::env::temp_dir().join(format!("lsearch-recent-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let day = std::time::Duration::from_secs(86_400);
    let now = std::time::SystemTime::now();
    let mut paths = Vec::new();
    for (folder, age_days) in [("old", 400), ("new", 1), ("mid", 30)] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
        let path = dir.join(folder).join("1.png");
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(now - day * age_days).unwrap();
        paths.push(path.to_string_lossy().into_owned());
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let index = index_with("recent", &refs);
    let found: Vec<String> = search(&index, "1", &Filter::default(), 10)
        .unwrap()
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    assert_eq!(
        found,
        [paths[1].clone(), paths[2].clone(), paths[0].clone()]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn name_search_sees_past_the_ceiling() {
    let mut paths: Vec<String> = (0..40)
        .map(|i| format!("/home/u/src/prompt-{i}.rs"))
        .collect();
    paths.extend([
        "/home/u/Videos/Promo.mkv".to_string(),
        "/home/u/Videos/prores.mov".to_string(),
        "/home/u/Music/prologue.mp4".to_string(),
    ]);
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let index = index_with("name-ceiling", &refs);
    let filter = Filter {
        under: Some("/home/u/Videos".into()),
        ext: vec!["mkv".into(), "mov".into(), "mp4".into()],
        ..Default::default()
    };
    // A ceiling smaller than the other files that match the word.
    let mut found: Vec<String> = search(&index, "pro", &filter, 5)
        .unwrap()
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    found.sort();
    assert_eq!(
        found,
        ["/home/u/Videos/Promo.mkv", "/home/u/Videos/prores.mov"]
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
/// anything in between: `raio bruno` finds what `raio` and `bruno` each find.
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

/// One CJK character finds the names it stands in, wherever it stands.
#[test]
fn one_cjk_character_finds_names() {
    let index = index_with(
        "cjk-one",
        &["/h/季度销售报告2026.txt", "/h/会議の議事録.txt"],
    );
    for (query, name) in [
        ("报", "季度销售报告2026.txt"),
        ("告", "季度销售报告2026.txt"),
        ("季", "季度销售报告2026.txt"),
        ("録", "会議の議事録.txt"),
    ] {
        assert_eq!(
            names(search(&index, query, &Filter::default(), 10).unwrap()),
            [name],
            "query {query:?}"
        );
    }
}
