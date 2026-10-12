use super::*;
use std::io::Write;

#[test]
fn pages_and_markdown_name_titles() {
    assert_eq!(
        decode_entities("A &amp; B &#8212; C &#x2014; D &nbsp;&bogus; &"),
        "A & B — C — D  &bogus; &"
    );
    let html = "<html><head><title>Shader Setup &#8212; Nsight</title><style>p{}</style></head>\
                    <body><H1>Shader <b>Setup</b></h1><p>Use the &lt;debugger&gt;.</p><script>x<y</script></body></html>";
    assert_eq!(
        html_element(html, "title").as_deref(),
        Some("Shader Setup — Nsight")
    );
    assert_eq!(html_element(html, "h1").as_deref(), Some("Shader Setup"));
    let text = html_text(html);
    assert!(text.contains("Use the <debugger>.") && !text.contains("p{}") && !text.contains("x<y"));
    assert_eq!(
        markdown_headings("intro\n## Usage\n# Getting started #\n#hashtag\n# Second"),
        ["Getting started", "Second", "Usage"]
    );
}

#[test]
fn unclosed_script_hides_the_rest() {
    assert_eq!(html_text("<p>kept<script>a<script>b<style>c"), "\nkept");
}

/// References are part of the word around them; elements separate words.
#[test]
fn office_text_keeps_references_in_words() {
    let docx = br#"<w:p><w:r><w:t>relat&#243;rio</w:t></w:r></w:p><w:p><w:r><w:t>Lopes &amp; Filhos</w:t></w:r></w:p>"#;
    let mut out = String::new();
    xml_text(docx, &mut out);
    assert_eq!(out, "relatório Lopes & Filhos ");
}

#[test]
fn composed_text_stops_at_extraction_cap() {
    let _lock = crate::test_env::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _cap = crate::test_env::EnvVarGuard::set("BIG_SEARCH_EXTRACT_MAX_MB", "1");
    let cap = max_extract_bytes();
    // U+FDFA is 3 bytes and 33 under NFKC.
    let text = "\u{FDFA}".repeat(cap / 3);
    let composed = compose(&text);
    assert!(composed.len() <= cap && composed.len() > cap - 33);
}

/// Paragraphs become lines, a word Word split across runs stays whole, and
/// entity references come back as characters.
#[test]
fn office_lines_keep_whole_words() {
    let docx = br#"<w:document><w:body><w:p><w:r><w:instrText> XE "index" </w:instrText></w:r><w:r><w:t>Re</w:t></w:r><w:r><w:delText>xx</w:delText></w:r><w:r><w:t>lat&#243;rio</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Lopes &amp; Filhos</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>Curitiba</w:t></w:r></w:p></w:body></w:document>"#;
    let mut out = String::new();
    xml_text_lines(docx, &mut out);
    assert_eq!(out, "Relatório\nLopes & Filhos\nCuritiba\n");

    let odt = br#"<office:text><text:h>Ata da reuni&#227;o<office:annotation><text:p>nota</text:p></office:annotation></text:h><text:p>Pauta<text:tab/>1<text:s/>item</text:p></office:text>"#;
    let mut out = String::new();
    xml_text_lines(odt, &mut out);
    assert_eq!(out, "Ata da reunião\nPauta\t1 item\n");
}

/// What a person types matches what the name or page writes, in every script
/// the folding covers; other scripts pass through untouched.
#[test]
fn folding_matches_what_people_type() {
    for (written, typed) in [
        ("Relatório FINAL 2026", "relatorio final 2026"),
        ("Relato\u{301}rio", "relatorio"),
        ("ÇÃO çao ÑÖÜ", "cao cao nou"),
        ("Straße GROSS", "strasse gross"),
        ("Łódź Ørsted Æble", "lodz orsted aeble"),
        ("İstanbul ılık", "istanbul ilik"),
        ("Отчёт ЁЛКА", "отчет елка"),
        ("Αναφορά ΟΔΟΣ οδός", "αναφορα οδοσ οδοσ"),
        ("أحمد مُحَمَّد مستشفى", "احمد محمد مستشفي"),
        ("مدرسة کتاب فارسی", "مدرسه كتاب فارسي"),
        ("中文内容 ABC", "中文内容 abc"),
        ("會議記錄 资料庫", "会议记录 资料库"),
        ("\u{1112}\u{1161}\u{11AB}\u{1100}\u{116E}\u{11A8}", "한국"),
        ("テ\u{3099}ータ ﾃﾞｰﾀ", "データ データ"),
        ("ＡＢＣ１２ ﬁle", "abc12 file"),
        ("ﻣﺮﺣﺒﺎ \u{F900}", "مرحبا 岂"),
        ("🙂 EMOJI", "🙂 emoji"),
        ("", ""),
    ] {
        assert_eq!(fold_lower(written), typed, "for {written:?}");
    }
}

fn temporary_extract_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lsearch-ex-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn stored_zip_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
}

#[test]
fn text_file_and_binary_reject() {
    let dir = temporary_extract_dir("txt");
    let txt = dir.join("note.txt");
    std::fs::write(&txt, b"hello brave world").unwrap();
    assert!(content(&txt).unwrap().contains("brave"));

    let bin = dir.join("blob.txt");
    std::fs::write(&bin, [0u8, 1, 2, 3, 0, 9]).unwrap();
    assert!(content(&bin).is_none()); // NUL → binary

    let unknown = dir.join("thing.xyz");
    std::fs::write(&unknown, b"data").unwrap();
    assert!(content(&unknown).is_none()); // not content-eligible
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn office_skips_oversized_input() {
    let _lock = crate::test_env::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temporary_extract_dir("cap");
    let path = dir.join("huge.docx");
    let _cap = crate::test_env::EnvVarGuard::set("BIG_SEARCH_CONTENT_MAX_MB", "1");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(2 * 1024 * 1024).unwrap();
    drop(file);
    assert!(exceeds_input_cap(&path));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn text_skips_oversized_input() {
    let _lock = crate::test_env::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temporary_extract_dir("textcap");
    let path = dir.join("huge.txt");
    let _cap = crate::test_env::EnvVarGuard::set("BIG_SEARCH_TEXT_MAX_MB", "1");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(2 * 1024 * 1024).unwrap();
    drop(file);
    assert!(text_input_size(&path).is_none());
    assert!(content(&path).is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn docx_zip_xml_extraction() {
    let dir = temporary_extract_dir("docx");
    let path = dir.join("doc.docx");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file("word/document.xml", stored_zip_options())
            .unwrap();
        zw.write_all(
            br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r>
                    <w:t>quarterly budget review</w:t></w:r></w:p></w:body></w:document>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    assert!(is_content_path(&path));
    let text = content(&path).unwrap();
    assert!(text.contains("quarterly"));
    assert!(text.contains("budget"));
    std::fs::remove_dir_all(&dir).ok();
}

/// An EPUB reads in its spine's order, from the package `container.xml`
/// names, with the title its metadata declares.
#[test]
fn epub_reads_spine_in_order() {
    let dir = temporary_extract_dir("epub");
    let path = dir.join("livro.epub");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let mut add = |name: &str, body: &str| {
            zw.start_file(name, stored_zip_options()).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        };
        add("mimetype", "application/epub+zip");
        add(
            "META-INF/container.xml",
            r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
        );
        add(
            "OEBPS/content.opf",
            r#"<package><metadata><dc:title>Dom Casmurro &amp; Cia</dc:title></metadata>
                <manifest><opf:item id="c2" href="cap%202.xhtml"/><item id="c1" href="cap1.xhtml"/></manifest>
                <spine><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c1"/></spine></package>"#,
        );
        add(
            "OEBPS/cap1.xhtml",
            "<html><body><h1>Do título</h1><p>Uma noite destas&#8230;</p></body></html>",
        );
        add(
            "OEBPS/cap 2.xhtml",
            "<html><body><p>Do livro</p></body></html>",
        );
        zw.finish().unwrap();
    }
    assert!(is_content_path(&path));
    let (text, title) = read_epub(&path).unwrap();
    assert_eq!(title.as_deref(), Some("Dom Casmurro & Cia"));
    // A chapter the spine lists twice is read once.
    assert_eq!(text.matches("Do título").count(), 1, "{text:?}");
    let (first, second) = (
        text.find("Do título").unwrap(),
        text.find("Do livro").unwrap(),
    );
    assert!(
        first < second && text.contains("Uma noite destas…"),
        "{text:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn docx_page_split_by_rendered_break() {
    let dir = temporary_extract_dir("docxpages");
    let path = dir.join("p.docx");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file("word/document.xml", stored_zip_options())
            .unwrap();
        zw.write_all(
            br#"<w:document><w:body>
                    <w:p><w:r><w:t>alphaword first</w:t></w:r></w:p>
                    <w:r><w:lastRenderedPageBreak/></w:r>
                    <w:p><w:r><w:t>betaword relat&#243;rio</w:t></w:r></w:p>
                </w:body></w:document>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    assert_eq!(pages_with(&path, &["alphaword".to_string()]), vec![1]);
    assert_eq!(pages_with(&path, &["betaword".to_string()]), vec![2]);
    assert_eq!(pages_with(&path, &["relatorio".to_string()]), vec![2]);
    assert!(pages_with(&path, &["missing".to_string()]).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn docx_without_breaks_reports_no_page() {
    let dir = temporary_extract_dir("docxnobreak");
    let path = dir.join("nb.docx");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file("word/document.xml", stored_zip_options())
            .unwrap();
        zw.write_all(br#"<w:document><w:body><w:p><w:r><w:t>soloword present</w:t></w:r></w:p></w:body></w:document>"#)
                .unwrap();
        zw.finish().unwrap();
    }
    // term is present, but no cached page breaks → no page reported (no guess).
    assert!(pages_with(&path, &["soloword".to_string()]).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
