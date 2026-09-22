//! Content extraction. Plain text in-process; PDF via a `pdftotext` subprocess
//! (argv array, hard wall-clock timeout, capped output) — chosen over in-process
//! Rust PDF libs after the P3.1 benchmark (4× faster, 12× less memory, robust).
//! bigagents: app-local-subprocess - PDF streaming needs capped pipe reads and a process group.
pub(crate) use crate::extract_pdf::read_pdf;
use std::io::Read;
use std::path::Path;

/// Adaptive cap on extracted text per file — bounds index + memory cost without
/// the old fixed 1 MiB ceiling. `BIG_SEARCH_EXTRACT_MAX_MB` or config
/// `[defaults].extract_max_mb` can override the automatic value.
pub fn max_extract_bytes() -> usize {
    crate::settings::extract_max_bytes()
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        // documents / data
        "txt" | "text" | "md" | "markdown" | "rst" | "org" | "log"
            | "csv" | "tsv" | "tex" | "bib" | "json" | "jsonl" | "ndjson"
            | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "properties" | "env" | "desktop"
            // markup / web
            | "html" | "htm" | "xhtml" | "xml" | "svg" | "css" | "scss" | "sass"
            // subtitles
            | "srt" | "vtt"
            // code
            | "rs" | "py" | "vala" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp"
            | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "sh" | "bash" | "zsh" | "fish"
            | "go" | "java" | "kt" | "kts" | "rb" | "php" | "pl" | "pm" | "lua" | "sql"
            | "swift" | "dart" | "hs" | "ml" | "r" | "jl" | "nim" | "zig"
    )
}

fn is_office_ext(ext: &str) -> bool {
    matches!(ext, "docx" | "odt" | "ods" | "odp" | "pptx" | "xlsx")
}

pub(crate) fn is_text_like_ext(ext: &str) -> bool {
    is_text_ext(ext) || ext == "pdf"
}

/// Whether `path` is a file we attempt content extraction for.
pub fn is_content_path(path: &Path) -> bool {
    matches!(ext_of(path).as_deref(), Some(e) if is_text_like_ext(e) || is_office_ext(e))
}

fn ext_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// Extracted text for `path`, or `None` if not eligible / not extractable.
pub fn content(path: &Path) -> Option<String> {
    let ext = ext_of(path)?;
    if is_text_ext(&ext) {
        read_text(path)
    } else if ext == "pdf" {
        read_pdf(path)
    } else if is_office_ext(&ext) {
        read_office(path, &ext)
    } else {
        None
    }
}

/// Extract text for a file identified by content sniffing (no usable extension):
/// PDF via `pdftotext`, anything `text/*` as plain text. Used by the metadata
/// pass so an extension-less document is still full-text searchable.
pub fn content_for_mime(path: &Path, mime: &str) -> Option<String> {
    if mime == "application/pdf" {
        read_pdf(path)
    } else if mime.starts_with("text/") {
        read_text(path)
    } else {
        None
    }
}

/// Read a text file up to the adaptive extraction cap; reject binary NULs.
fn read_text(path: &Path) -> Option<String> {
    let size = text_input_size(path)?;
    let cap = max_extract_bytes();
    // The buffer is the right size from the start. Growing it costs a copy of
    // everything read so far at each step, and a text file is read whole.
    let mut buf = Vec::with_capacity(size.min(cap as u64) as usize);
    std::fs::File::open(path)
        .ok()?
        .take(cap as u64)
        .read_to_end(&mut buf)
        .ok()?;
    text_from_bytes(buf).map(|mut text| {
        truncate_to_char_boundary(&mut text, cap);
        text
    })
}

/// The size of a text file, or `None` when it is over the input cap.
///
/// One `stat`, answering both questions: whether to read the file at all, and
/// how big a buffer it needs.
fn text_input_size(path: &Path) -> Option<u64> {
    let size = std::fs::metadata(path).ok()?.len();
    let cap = text_max_input_bytes();
    (cap == 0 || size <= cap).then_some(size)
}

/// A blob with no text in it: empty, or holding a NUL byte early on.
fn looks_binary(buf: &[u8]) -> bool {
    buf.is_empty() || buf.iter().take(8192).any(|&b| b == 0)
}

/// Decode a text blob: reject empty or binary (NUL in the first chunk), else
/// UTF-8, lossily where it has to be. Split out from `read_text` so the decode
/// path is fuzzable — and it is the path `read_text` really takes, so what the
/// fuzzer drives is what runs.
///
/// The bytes are consumed: almost every file read is already valid UTF-8, and
/// then they simply become the string. `from_utf8_lossy(&buf).into_owned()`
/// copied the whole file a second time even when nothing was replaced.
pub fn text_from_bytes(buf: Vec<u8>) -> Option<String> {
    if looks_binary(&buf) {
        return None;
    }
    Some(match String::from_utf8(buf) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    })
}

pub(crate) fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Extract text from an OOXML/ODF zip by reading its body XML part(s) and
/// concatenating all character data. Capped + entry-scoped (zip-bomb safe).
fn read_office(path: &Path, ext: &str) -> Option<String> {
    if exceeds_input_cap(path) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    office_text_from_reader(std::io::BufReader::new(file), ext)
}

fn exceeds_input_cap(path: &Path) -> bool {
    let cap = max_input_bytes();
    cap > 0
        && std::fs::metadata(path)
            .map(|m| m.len() > cap)
            .unwrap_or(false)
}

fn max_input_bytes() -> u64 {
    crate::settings::office_max_input_bytes()
}

fn text_max_input_bytes() -> u64 {
    crate::settings::text_max_input_bytes()
}

/// Entries an office archive may declare before we refuse it. Generous against
/// real documents (a heavy pptx is a few hundred), tight against a bomb.
const MAX_ZIP_ENTRIES: usize = 4_096;

/// Slides read from one pptx. Bounds both the name `Vec` and the read loop.
const MAX_PPTX_SLIDES: usize = 512;

/// Reader-based core of `read_office`: extract concatenated body text from an
/// OOXML/ODF zip. Public so fuzz targets can drive it with arbitrary bytes
/// (`Cursor<&[u8]>`) — the untrusted-document parsing surface.
pub fn office_text_from_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
    ext: &str,
) -> Option<String> {
    let mut zip = zip::ZipArchive::new(reader).ok()?;

    // Per-part decompressed size is capped below, but the central directory is
    // read whole by `ZipArchive::new` before any of that: at the default 256 MiB
    // input cap an archive can declare millions of entries and cost hundreds of
    // megabytes in directory alone — inside a 768 MiB cgroup that is an OOM kill,
    // and since the content-state row is only written after the batch completes,
    // the same file is retried on every restart. A real document has tens.
    if zip.len() > MAX_ZIP_ENTRIES {
        log::debug!("skipping office file: {} zip entries", zip.len());
        return None;
    }

    let parts: Vec<String> = match ext {
        "docx" => vec!["word/document.xml".to_string()],
        "odt" | "ods" | "odp" => vec!["content.xml".to_string()],
        "xlsx" => vec!["xl/sharedStrings.xml".to_string()],
        // Numeric order, the same order `for_each_pptx_slide` reads them in: a
        // deck's text and its page numbers must describe the same slides.
        "pptx" => slide_names(&zip),
        _ => return None,
    };

    let cap = max_extract_bytes();
    let mut out = String::new();
    for part in parts {
        let Ok(mut entry) = zip.by_name(&part) else {
            continue;
        };
        let mut xml = Vec::new();
        if entry
            .by_ref()
            .take(cap as u64)
            .read_to_end(&mut xml)
            .is_err()
        {
            continue;
        }
        xml_text(&xml, &mut out);
        if out.len() >= cap {
            truncate_to_char_boundary(&mut out, cap);
            break;
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Lowercase and strip accents in one pass over the text.
///
/// Everything that compares text does `fold(&s.to_lowercase())`, which walks the
/// string twice and builds two strings of it. Page text runs to hundreds of
/// kilobytes, so the second copy is real work for nothing.
///
/// Two details keep the result identical to the two-step version:
///
/// - ASCII text needs no folding at all, so it is only lowercased;
/// - Greek capital sigma lowercases differently at the end of a word, which is a
///   rule about the string and not about the character. That case is rare enough
///   to notice while passing and redo properly.
#[must_use]
pub fn fold_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_lower_into(s, &mut out);
    out
}

/// The same, into a buffer the caller reuses — page after page, that is one
/// allocation instead of one per page.
pub fn fold_lower_into(s: &str, out: &mut String) {
    if s.is_ascii() {
        out.extend(s.chars().map(|c| c.to_ascii_lowercase()));
    } else if s.contains('Σ') {
        out.push_str(&fold(&s.to_lowercase()));
    } else {
        for c in s.chars() {
            out.extend(c.to_lowercase().map(fold_char));
        }
    }
}

/// Fold common Latin accents to ASCII (mirrors the analyzers' `AsciiFoldingFilter`)
/// so substring/page matching is accent-insensitive. Expects lowercase input.
#[must_use]
fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        'ý' | 'ÿ' => 'y',
        other => other,
    }
}

/// 1-based page (PDF) / slide (PPTX) numbers whose text contains *all* `terms`.
/// Empty for formats without fixed pages (docx/odt flow, txt, …).
///
/// One page at a time, and only the number is kept. Holding every page of a
/// 3000-page document in memory to answer with a handful of integers is the kind
/// of work this service cannot afford: the text of a big PDF is tens of
/// megabytes, and it used to exist three times over — the bytes, a copy of them,
/// and a `String` per page.
pub fn pages_with(path: &Path, terms: &[String]) -> Vec<usize> {
    let Some(ext) = ext_of(path) else {
        return Vec::new();
    };
    let folded: Vec<String> = terms
        .iter()
        .map(|t| fold_lower(t))
        .filter(|t| !t.is_empty())
        .collect();
    if folded.is_empty() {
        return Vec::new();
    }
    let mut pages = PageMatcher {
        terms: &folded,
        folded_page: String::new(),
        page: 0,
        hits: Vec::new(),
    };
    match ext.as_str() {
        "pdf" => crate::extract_pdf::for_each_pdf_page(path, &mut |text| pages.test(text)),
        "pptx" => for_each_pptx_slide(path, &mut |text| pages.test(text)),
        "docx" | "odt" | "ods" | "odp" => {
            let (part, break_tag): (&str, &[u8]) = if ext == "docx" {
                ("word/document.xml", b"w:lastRenderedPageBreak")
            } else {
                ("content.xml", b"text:soft-page-break")
            };
            // No break markers → page layout unknown (the app didn't cache it).
            // Report no page rather than guessing "page 1". PDF and PPTX page
            // numbers stay reliable.
            if !for_each_office_page(path, part, break_tag, &mut |text| pages.test(text)) {
                return Vec::new();
            }
        }
        _ => return Vec::new(),
    }
    pages.hits
}

/// Counts pages as they go past and keeps the numbers of the ones that hold
/// every term. The folded text of the current page is the only text it holds.
struct PageMatcher<'a> {
    terms: &'a [String],
    folded_page: String,
    page: usize,
    hits: Vec<usize>,
}

impl PageMatcher<'_> {
    fn test(&mut self, text: &str) {
        self.page += 1;
        self.folded_page.clear();
        fold_lower_into(text, &mut self.folded_page);
        if self
            .terms
            .iter()
            .all(|term| self.folded_page.contains(term.as_str()))
        {
            self.hits.push(self.page);
        }
    }
}

/// Hand each pptx slide's text to `on_page`, one slide at a time.
fn for_each_pptx_slide(path: &Path, on_page: &mut dyn FnMut(&str)) {
    let cap = max_extract_bytes();
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(mut zip) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return;
    };
    if zip.len() > MAX_ZIP_ENTRIES {
        return;
    }
    let names = slide_names(&zip);
    let mut xml = Vec::new();
    let mut text = String::new();
    for name in names {
        // Every slide is announced, readable or not: the caller counts these
        // calls to number the pages, and skipping one would move every slide
        // after it up by one.
        text.clear();
        if let Ok(mut entry) = zip.by_name(&name) {
            xml.clear();
            if entry
                .by_ref()
                .take(cap as u64)
                .read_to_end(&mut xml)
                .is_ok()
            {
                xml_text(&xml, &mut text);
            }
        }
        on_page(&text);
    }
}

/// Split an OOXML/ODF body part into pages using the layout page-break markers the
/// authoring app cached at last save (`w:lastRenderedPageBreak`, `text:soft-page-break`)
/// plus explicit `w:br w:type="page"`. No rendering.
///
/// Returns whether the document declared any break at all: without one there is
/// no page layout to report, which is not the same as a document of one page.
fn for_each_office_page(
    path: &Path,
    part: &str,
    break_tag: &[u8],
    on_page: &mut dyn FnMut(&str),
) -> bool {
    let Some(xml) = zip_entry_bytes(path, part) else {
        return false;
    };
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut page = String::new();
    let mut breaks = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Empty(e)) | Ok(quick_xml::events::Event::Start(e))
                if is_page_break(&e, break_tag) =>
            {
                breaks += 1;
                on_page(&page);
                page.clear();
            }
            Ok(quick_xml::events::Event::Text(t)) => {
                append_decoded(&t, &mut page, false);
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    if breaks == 0 {
        return false;
    }
    on_page(&page);
    true
}

/// The slides of a pptx, in the order a person sees them.
fn slide_names<R: std::io::Read + std::io::Seek>(zip: &zip::ZipArchive<R>) -> Vec<String> {
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(String::from)
        .collect();
    // Numerically: `slide10.xml` sorts before `slide2.xml` as text.
    names.sort_by_key(|n| {
        n.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(0)
    });
    names.truncate(MAX_PPTX_SLIDES);
    names
}

/// Bytes of a zip entry (capped), or `None`.
///
/// Bytes, not text: the XML parser reads bytes, so turning the part into a
/// `String` first only buys a second copy of a part that can be tens of
/// megabytes.
fn zip_entry_bytes(path: &Path, name: &str) -> Option<Vec<u8>> {
    let cap = max_extract_bytes();
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return None;
    }
    let mut entry = zip.by_name(name).ok()?;
    let mut xml = Vec::new();
    entry.by_ref().take(cap as u64).read_to_end(&mut xml).ok()?;
    Some(xml)
}

/// True if `e` is a cached page-break marker or an explicit `w:br w:type="page"`.
fn is_page_break(e: &quick_xml::events::BytesStart, break_tag: &[u8]) -> bool {
    let name = e.name();
    if name.as_ref() == break_tag {
        return true;
    }
    if name.as_ref() == b"w:br" {
        return e
            .attributes()
            .flatten()
            .any(|a| a.key.as_ref() == b"w:type" && a.value.as_ref() == b"page");
    }
    false
}

/// Append all XML character data from `xml` to `out`, space-separated.
///
/// Bytes, not text: every caller has an office part it just read, and turning
/// that into a `String` first is a copy of the whole part for nothing.
pub fn xml_text(xml: &[u8], out: &mut String) {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Text(e)) => append_decoded(&e, out, true),
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// Append one text event, trimmed and followed by a space.
///
/// Bytes that are not valid UTF-8 are read lossily rather than dropped: an
/// office part is UTF-8 by specification, so this only happens to a damaged
/// file, and a damaged file should still give up whatever text it has.
fn append_decoded(event: &quick_xml::events::BytesText<'_>, out: &mut String, skip_empty: bool) {
    let decoded = event.decode().map_or_else(
        |_| String::from_utf8_lossy(event).into_owned(),
        |t| t.into_owned(),
    );
    let text = decoded.trim();
    if skip_empty && text.is_empty() {
        return;
    }
    out.push_str(text);
    out.push(' ');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The one-pass version has to answer exactly what the two-step one did,
    /// including the Greek sigma that changes shape at the end of a word.
    #[test]
    fn one_pass_folding_matches_the_two_step_version() {
        for text in [
            "",
            "Relatorio FINAL 2026",
            "Avaliação Neuropsicológica ÀS TRÊS",
            "ÇÃO çao ÑÖÜ",
            "ΣΟΦΟΣ ΟΔΟΣ",
            "Straße GROSS",
            "İstanbul TÜRKÇE",
            "中文内容 ABC",
            "🙂 EMOJI Ölçü",
        ] {
            assert_eq!(
                fold_lower(text),
                fold(&text.to_lowercase()),
                "differs for {text:?}"
            );
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
    fn office_extractors_skip_files_above_input_cap() {
        let dir = temporary_extract_dir("cap");
        let path = dir.join("huge.docx");
        let previous = std::env::var_os("BIG_SEARCH_CONTENT_MAX_MB");
        unsafe {
            std::env::set_var("BIG_SEARCH_CONTENT_MAX_MB", "1");
        }
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(2 * 1024 * 1024).unwrap();
        drop(file);
        assert!(exceeds_input_cap(&path));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("BIG_SEARCH_CONTENT_MAX_MB", value),
                None => std::env::remove_var("BIG_SEARCH_CONTENT_MAX_MB"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn text_extraction_skips_files_above_text_input_cap() {
        let dir = temporary_extract_dir("textcap");
        let path = dir.join("huge.txt");
        let previous = std::env::var_os("BIG_SEARCH_TEXT_MAX_MB");
        unsafe {
            std::env::set_var("BIG_SEARCH_TEXT_MAX_MB", "1");
        }
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(2 * 1024 * 1024).unwrap();
        drop(file);
        assert!(text_input_size(&path).is_none());
        assert!(content(&path).is_none());
        unsafe {
            match previous {
                Some(value) => std::env::set_var("BIG_SEARCH_TEXT_MAX_MB", value),
                None => std::env::remove_var("BIG_SEARCH_TEXT_MAX_MB"),
            }
        }
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
                    <w:p><w:r><w:t>betaword second</w:t></w:r></w:p>
                </w:body></w:document>"#,
            )
            .unwrap();
            zw.finish().unwrap();
        }
        assert_eq!(pages_with(&path, &["alphaword".to_string()]), vec![1]);
        assert_eq!(pages_with(&path, &["betaword".to_string()]), vec![2]);
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
}
