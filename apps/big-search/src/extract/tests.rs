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

/// A saved page is indexed by what a reader sees, and still tells big-docinfo
/// its title and heading.
#[test]
fn html_page_indexed_as_text() {
    let dir = temporary_extract_dir("html");
    let path = dir.join("page.html");
    std::fs::write(
        &path,
        r#"<html><head><title>Ata</title><style>.destaque{}</style></head><body><h1>Reuni&#227;o</h1><p class="destaque">Relat&#243;rio</p><table><tr><td>cela</td><td>celb</td></tr></table><p>caf&eacute; <b>for</b>te</p><script>var contador</script></body></html>"#,
    )
    .unwrap();
    let text = content(&path).unwrap();
    for word in ["Relatório", "cela", "celb", "café", "forte"] {
        assert!(
            text.split_whitespace().any(|w| w == word),
            "{word} in {text:?}"
        );
    }
    for markup in ["destaque", "contador", "class", "<p"] {
        assert!(!text.contains(markup), "{markup} in {text:?}");
    }
    let page = described(&path).unwrap();
    assert_eq!(page.declared_title.as_deref(), Some("Ata"));
    assert_eq!(page.prominent, ["Reunião"]);
    assert!(page.text.contains("Relatório"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unclosed_script_hides_the_rest() {
    assert_eq!(html_text("<p>kept<script>a<script>b<style>c"), "\nkept");
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

/// An office archive in memory holding `parts`.
fn office_zip(parts: &[(&str, &str)]) -> std::io::Cursor<Vec<u8>> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in parts {
        zw.start_file(*name, stored_zip_options()).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let mut zip = zw.finish().unwrap();
    zip.set_position(0);
    zip
}

/// What the index reads from an office file, per ECMA-376: the runs of a
/// paragraph are one text, deleted text and field codes are not the document's,
/// each `si` is one shared string and `rPh` only its phonetic reading.
#[test]
fn office_index_text_as_written() {
    let docx = r#"<w:document><w:body><w:p><w:r><w:instrText> HYPERLINK "x" </w:instrText></w:r><w:r><w:t>Re</w:t></w:r><w:r><w:delText>xx</w:delText></w:r><w:r><w:t>lat&#243;rio</w:t></w:r></w:p></w:body></w:document>"#;
    let xlsx = r#"<sst><si><t>Alfa</t></si><si><r><t>Be</t></r><r><t>ta</t></r></si><si><t>漢字</t><rPh sb="0" eb="2"><t>カンジ</t></rPh></si></sst>"#;
    let pptx = r#"<p:sld><a:p><a:r><a:t>Re</a:t></a:r><a:r><a:t>latório</a:t></a:r></a:p></p:sld>"#;
    // Writers choose the prefix; the .NET Open XML SDK writes `x:`.
    let xlsx_prefixed =
        r#"<x:sst><x:si><x:t>Fornecedor</x:t></x:si><x:si><x:t>Valor</x:t></x:si></x:sst>"#;
    let pptx_break = r#"<p:sld><a:p><a:r><a:t>Primeira</a:t></a:r><a:br/><a:r><a:t>Segunda</a:t></a:r></a:p></p:sld>"#;
    let odt_note = r#"<office:text><text:p>palavra<text:note><text:note-citation>1</text:note-citation><text:note-body><text:p>rodape</text:p></text:note-body></text:note> segue</text:p></office:text>"#;
    let docx_nested = r#"<w:document><w:body><w:p><w:r><w:t>antes</w:t></w:r><w:r><w:pict><v:textbox><w:txbxContent><w:p><w:r><w:t>caixa</w:t></w:r></w:p></w:txbxContent></v:textbox></w:pict></w:r><w:r><w:t>depois</w:t></w:r><w:r><w:br></w:br><w:t xml:space="preserve">linha </w:t></w:r><w:r><w:t>ref</w:t><w:noBreakHyphen/><w:t>hifen</w:t></w:r></w:p></w:body></w:document>"#;
    for (ext, part, xml, expected) in [
        ("docx", "word/document.xml", docx, "Relatório\n"),
        ("xlsx", "xl/sharedStrings.xml", xlsx, "Alfa\nBeta\n漢字\n"),
        ("pptx", "ppt/slides/slide1.xml", pptx, "Relatório\n"),
        (
            "xlsx",
            "xl/sharedStrings.xml",
            xlsx_prefixed,
            "Fornecedor\nValor\n",
        ),
        (
            "pptx",
            "ppt/slides/slide1.xml",
            pptx_break,
            "Primeira\nSegunda\n",
        ),
        (
            "odt",
            "content.xml",
            odt_note,
            "palavra 1\nrodape\n segue\n",
        ),
        (
            "docx",
            "word/document.xml",
            docx_nested,
            "antes caixa\ndepois\nlinha ref-hifen\n",
        ),
    ] {
        let text = office_text_from_reader(office_zip(&[(part, xml)]), ext);
        assert_eq!(text.as_deref(), Some(expected), "{ext}");
    }
}

/// ECMA-376 18.3.1.4: `t` is the cell's type. `s` indexes the shared table,
/// `n`, `b` and `e` hold no text; `inlineStr` and a formula's `str` do.
#[test]
fn xlsx_sheet_strings_indexed() {
    let sheet = r#"<x:worksheet><x:sheetData>
        <x:row><x:c t="inlineStr"><x:is><x:r><x:t>Re</x:t></x:r><x:r><x:t>lat&#243;rio</x:t></x:r><x:rPh><x:t>xx</x:t></x:rPh></x:is></x:c><x:c t="s"><x:v>0</x:v></x:c><x:c><x:v>45000</x:v></x:c><x:c t="str"><x:f>A1&amp;"!"</x:f><x:v>Total geral</x:v></x:c></x:row>
        <x:row><x:c t="b"><x:v>1</x:v></x:c><x:c t="e"><x:v>#REF!</x:v></x:c><x:c t="inlineStr"><x:is><x:t>Fim</x:t></x:is></x:c></x:row>
    </x:sheetData></x:worksheet>"#;
    let text = office_text_from_reader(office_zip(&[("xl/worksheets/sheet1.xml", sheet)]), "xlsx");
    assert_eq!(text.as_deref(), Some("Relatório\tTotal geral\nFim\n"));
}

/// A docx's headers, footers and notes follow its body; a pptx's speaker notes
/// follow its slides.
#[test]
fn office_text_outside_body_indexed() {
    let docx = [
        (
            "word/footnotes.xml",
            "<w:footnotes><w:p><w:t>Nota</w:t></w:p></w:footnotes>",
        ),
        (
            "word/footer1.xml",
            "<w:ftr><w:p><w:t>Rodapé</w:t></w:p></w:ftr>",
        ),
        (
            "word/document.xml",
            "<w:document><w:p><w:t>Corpo</w:t></w:p></w:document>",
        ),
        (
            "word/header1.xml",
            "<w:hdr><w:p><w:t>Cabeçalho</w:t></w:p></w:hdr>",
        ),
    ];
    let text = office_text_from_reader(office_zip(&docx), "docx");
    assert_eq!(text.as_deref(), Some("Corpo\nCabeçalho\nRodapé\nNota\n"));

    let pptx = [
        (
            "ppt/notesSlides/notesSlide1.xml",
            "<p:notes><a:p><a:t>Fala</a:t></a:p></p:notes>",
        ),
        (
            "ppt/slides/slide1.xml",
            "<p:sld><a:p><a:t>Slide</a:t></a:p></p:sld>",
        ),
    ];
    let text = office_text_from_reader(office_zip(&pptx), "pptx");
    assert_eq!(text.as_deref(), Some("Slide\nFala\n"));
}

/// An archive declaring more entries than [`MAX_ZIP_ENTRIES`] is refused before
/// any part is read; one at the cap is read.
#[test]
fn zip_entry_cap_refuses_one_more() {
    let docx = |entries: usize| {
        let mut parts = vec![(
            "word/document.xml".to_string(),
            "<w:p><w:t>dentro</w:t></w:p>",
        )];
        parts.extend((1..entries).map(|n| (format!("pad/{n}"), "")));
        let parts: Vec<(&str, &str)> = parts.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        office_text_from_reader(office_zip(&parts), "docx")
    };
    assert_eq!(docx(MAX_ZIP_ENTRIES).as_deref(), Some("dentro\n"));
    assert_eq!(docx(MAX_ZIP_ENTRIES + 1), None);
}

/// Parts are read until the archive's markup budget, four extraction caps, is
/// spent: a part past it is not read.
#[test]
fn archive_markup_budget_stops_reading() {
    let _lock = crate::test_env::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _cap = crate::test_env::EnvVarGuard::set("BIG_SEARCH_EXTRACT_MAX_MB", "1");
    let empty_rows = format!("<worksheet>{}</worksheet>", "<row/>".repeat(1 << 18));
    let last = r#"<worksheet><row><c t="inlineStr"><is><t>fim</t></is></c></row></worksheet>"#;
    let names: Vec<String> = (1..=6)
        .map(|n| format!("xl/worksheets/sheet{n}.xml"))
        .collect();
    let mut parts: Vec<(&str, &str)> = names[..5]
        .iter()
        .map(|n| (n.as_str(), empty_rows.as_str()))
        .collect();
    parts.push((names[5].as_str(), last));
    assert_eq!(
        office_text_from_reader(office_zip(&parts[4..]), "xlsx").as_deref(),
        Some("fim\n")
    );
    assert_eq!(office_text_from_reader(office_zip(&parts), "xlsx"), None);
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
                    <w:p><w:r><w:t>beta</w:t></w:r><w:r><w:t>word relat&#243;rio</w:t></w:r></w:p>
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

#[test]
fn pptx_slides_sort_by_number() {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in [
        "ppt/slides/slide10.xml",
        "ppt/slides/slide2.xml",
        "ppt/slides/_rels/slide1.xml.rels",
        "ppt/notesSlides/notesSlide1.xml",
        "ppt/slides/slide1.xml",
    ] {
        zw.start_file(name, stored_zip_options()).unwrap();
    }
    let zip = capped_zip(zw.finish().unwrap()).unwrap();
    assert_eq!(
        slide_names(&zip),
        [
            "ppt/slides/slide1.xml",
            "ppt/slides/slide2.xml",
            "ppt/slides/slide10.xml"
        ]
    );
}

#[test]
fn zip_entries_match_stored_bytes() {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in ["plain.xml", "QQ.xml"] {
        zw.start_file(name, stored_zip_options()).unwrap();
    }
    // Store the second name as CP437 bytes, which `by_name` decodes to "Çü".
    let mut bytes = zw.finish().unwrap().into_inner();
    for at in 0..bytes.len() - 1 {
        if bytes[at..].starts_with(b"QQ.xml") {
            bytes[at..at + 2].copy_from_slice(&[0x80, 0x81]);
        }
    }
    let mut zip = capped_zip(std::io::Cursor::new(bytes)).unwrap();
    assert!(zip.by_name("Çü.xml").is_ok());
    assert!(zip_entry(&mut zip, "Çü.xml").is_err());
    assert!(zip_entry(&mut zip, "plain.xml").is_ok());
}
