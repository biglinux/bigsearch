use super::*;
use crate::query::Filter;
use crate::query::names::search;
use crate::query::tests::{content_index_with, index_with, names};

#[test]
fn content_word_search() {
    let dir = std::env::temp_dir().join(format!("lsearch-test-{}-body", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let index = index_with("body-name", &["/n/report.md"]);
    let content_index =
        content_index_with("body", &[("/n/report.md", "the quick brown fox jumps")]);

    assert_eq!(
        search_content(&content_index, "quick", 10).unwrap().len(),
        1
    );
    assert_eq!(
        search_content(&content_index, "QUICK", 10).unwrap().len(),
        1
    ); // lowercased
    assert!(
        search_content(&content_index, "missing", 10)
            .unwrap()
            .is_empty()
    );
    // content word is not a filename match
    assert!(
        search(&index, "quick", &Filter::default(), 10)
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The last word is read as typed so far: it finds the words it begins,
/// and a document holding exactly that word ranks first. Earlier words, and
/// a last word under three letters, match whole words only.
#[test]
fn last_content_word_is_prefix() {
    let content = content_index_with(
        "prefix",
        &[
            ("/d/plural.txt", "os relatórios e os contratos assinados"),
            ("/d/exact.txt", "o relatorio do mes"),
            ("/d/other.txt", "religiao e cultura"),
        ],
    );
    let paths = |q: &str| -> Vec<String> {
        search_content(&content, q, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect()
    };
    assert_eq!(paths("relatorio"), ["/d/exact.txt", "/d/plural.txt"]);
    assert_eq!(paths("contrato"), ["/d/plural.txt"]);
    assert_eq!(paths("os rela"), ["/d/plural.txt"]);
    assert!(paths("re").is_empty());
    // Only the last word begins others: "rel" is no word of plural.txt.
    assert!(paths("rel contratos").is_empty());
}

/// The spellings people treat as one find each other, because content words
/// are folded like names: Russian е/ё, Greek with or without its accent,
/// Arabic alef with or without hamza and with or without the article, a
/// decomposed accent, the Turkish İ; one CJK character finds the words it
/// opens.
#[test]
fn content_spellings_find_each_other() {
    let content = content_index_with(
        "spellings",
        &[
            ("/d/ru.txt", "Отчёт о продажах и ёлка"),
            ("/d/ru2.txt", "елка без точек"),
            ("/d/el.txt", "Οι πωλήσεις αυξήθηκαν κατά είκοσι"),
            ("/d/ar.txt", "ارتفعت المبيعات وقال أحمد"),
            ("/d/zh.txt", "本季度的销售额增长了"),
            ("/d/nfd.txt", "relato\u{301}rio decomposto"),
            ("/d/tr.txt", "İstanbul'da satışlar arttı"),
        ],
    );
    let paths = |q: &str| -> Vec<String> {
        let mut found: Vec<String> = search_content(&content, q, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        found.sort();
        found
    };
    assert_eq!(paths("отчет"), ["/d/ru.txt"]);
    assert_eq!(paths("елка"), ["/d/ru.txt", "/d/ru2.txt"]);
    assert_eq!(paths("ёлка"), ["/d/ru.txt", "/d/ru2.txt"]);
    assert_eq!(paths("πωλησεις"), ["/d/el.txt"]);
    assert_eq!(paths("ΑΥΞΗΘΗΚΑΝ"), ["/d/el.txt"]);
    assert_eq!(paths("مبيعات"), ["/d/ar.txt"]);
    assert_eq!(paths("احمد"), ["/d/ar.txt"]);
    assert_eq!(paths("销"), ["/d/zh.txt"]);
    assert_eq!(paths("relatorio"), ["/d/nfd.txt"]);
    assert_eq!(paths("istanbul"), ["/d/tr.txt"]);
}

/// One CJK character is a term of its own: it is found wherever it stands,
/// however many different characters follow it elsewhere (expanding it to
/// the grams it begins stopped at 64).
#[test]
fn single_cjk_character_is_found() {
    let mut paths: Vec<String> = (0..70)
        .map(|i| format!("/h/报{}.txt", char::from_u32(0x4E00 + i).unwrap()))
        .collect();
    paths.push("/h/报龥.txt".to_owned());
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let index = index_with("cjk-many", &refs);
    let found = names(search(&index, "报", &Filter::default(), 100).unwrap());
    assert_eq!(found.len(), 71);
    assert!(found.iter().any(|name| name == "报龥.txt"));

    let content = content_index_with(
        "cjk-one-content",
        &[
            ("/h/a.txt", "本季度的销售额增长了。"),
            ("/h/b.txt", "销售部门"),
        ],
    );
    for (query, expected) in [
        ("额", vec!["/h/a.txt"]),
        ("销", vec!["/h/a.txt", "/h/b.txt"]),
        ("售额", vec!["/h/a.txt"]),
    ] {
        let mut got: Vec<String> = search_content(&content, query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        got.sort();
        assert_eq!(got, expected, "query {query:?}");
    }
}

/// An Arabic word typed so far, article included, finds the indexed word
/// that lost its article.
#[test]
fn typed_arabic_word_keeps_article() {
    let content = content_index_with(
        "arabic-typing",
        &[("/h/a.txt", "إعادة الاتصال"), ("/h/b.txt", "مدرسة")],
    );
    for (query, expected) in [("اعادة الات", "/h/a.txt"), ("مدرسه", "/h/b.txt")] {
        let got: Vec<String> = search_content(&content, query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        assert_eq!(got, [expected], "query {query:?}");
    }
}

/// A Russian or Ukrainian word finds its other case endings, wherever it
/// stands in the query, and not words that only begin like it.
#[test]
fn slavic_words_find_their_other_endings() {
    let content = content_index_with(
        "slavic",
        &[
            ("/h/a.txt", "Продажи выросли в этом квартале."),
            ("/h/b.txt", "Отчёт по кварталам и продажам."),
            ("/h/c.txt", "Квартальный отчёт."),
            ("/h/d.txt", "Звіт про продажі за квартал."),
        ],
    );
    for (query, expected) in [
        ("квартал отчет", vec!["/h/b.txt"]),
        ("квартала", vec!["/h/a.txt", "/h/b.txt", "/h/d.txt"]),
        ("продажа квартал", vec!["/h/a.txt", "/h/b.txt", "/h/d.txt"]),
    ] {
        let mut got: Vec<String> = search_content(&content, query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        got.sort();
        assert_eq!(got, expected, "query {query:?}");
    }
    assert_eq!(slavic_stem("сто"), None);
    assert_eq!(slavic_stem("кварталами"), Some("квартал"));
}

/// Every word finds its plural or singular, in both directions and whatever
/// its place; the form as typed ranks first; a short word is left alone.
#[test]
fn content_words_match_plural_and_singular() {
    let content = content_index_with(
        "inflection",
        &[
            ("/d/singular.txt", "o relatório da ação do responsável"),
            ("/d/plural.txt", "os relatórios das ações dos responsáveis"),
            ("/d/again.txt", "relatórios e ações, imagens e flores"),
            ("/d/mail.txt", "mail server mail queue"),
            ("/d/mail2.txt", "another mail"),
        ],
    );
    let paths = |q: &str| -> Vec<String> {
        search_content(&content, q, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect()
    };
    let mut found = paths("relatorios acoes");
    assert_eq!(found.len(), 3, "{found:?}");
    assert_ne!(found[0], "/d/singular.txt", "the form as typed ranks first");
    found.sort();
    assert_eq!(found, ["/d/again.txt", "/d/plural.txt", "/d/singular.txt"]);
    let found = paths("acao responsavel");
    assert_eq!(found[0], "/d/singular.txt");
    assert!(found.contains(&"/d/plural.txt".to_string()), "{found:?}");
    assert!(paths("imagem flor").contains(&"/d/again.txt".to_string()));
    // "mais" is no plural of "mail".
    assert!(paths("mais servidor").is_empty());
}

#[test]
fn accent_insensitive() {
    let dir = std::env::temp_dir().join(format!("lsearch-test-{}-acc", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let index = index_with("acc-name", &["/n/relatório Gonçalves.md"]);
    let content_index = content_index_with(
        "acc",
        &[(
            "/n/relatório Gonçalves.md",
            "documento de Bruno Gonçalves Antônio",
        )],
    );

    // content folds accents both ways
    assert_eq!(
        search_content(&content_index, "goncalves", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        search_content(&content_index, "antônio", 10).unwrap().len(),
        1
    );
    // name (ngram) folds too
    assert_eq!(
        search(&index, "relatorio", &Filter::default(), 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        search(&index, "goncalves", &Filter::default(), 10)
            .unwrap()
            .len(),
        1
    );
    std::fs::remove_dir_all(&dir).ok();
}
