//! Content extraction. Plain text in-process; PDF via a `pdftotext` subprocess
//! (argv array, hard wall-clock timeout, capped output), which is faster and
//! lighter than in-process Rust PDF libraries (DESIGN.md, "Extraction").
//! Word 97-2003 `.doc` the same way through `antiword`.
mod epub;

pub(crate) use crate::extract_pdf::read_pdf;
use epub::read_epub;
use std::io::Read;
use std::path::Path;

/// Adaptive cap on extracted text per file, sized by the machine's memory to
/// bound index and memory cost. `BIG_SEARCH_EXTRACT_MAX_MB` or config
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
    matches!(ext_of(path).as_deref(), Some(e) if is_text_like_ext(e) || is_office_ext(e) || is_titled_ext(e))
}

/// Formats read whole by their own reader, which also gives the title the file
/// declares when it has one: EPUB books, saved e-mail, RTF and Word 97-2003.
fn is_titled_ext(ext: &str) -> bool {
    matches!(ext, "epub" | "eml" | "rtf" | "doc")
}

/// The text of an EPUB, e-mail, RTF or `.doc` file and the title it declares.
fn read_titled(path: &Path, ext: &str) -> Option<(String, Option<String>)> {
    match ext {
        "epub" => read_epub(path),
        "eml" => mail_text(&read_capped_bytes(path)?),
        "rtf" => rtf_text(&read_capped_bytes(path)?),
        "doc" => read_doc(path),
        _ => None,
    }
}

/// A `.doc` by what it holds: a Word 97-2003 file (OLE2) through `antiword`,
/// RTF, which Word also saves under `.doc` and `antiword` refuses, or plain text
/// (old README-style `.doc` files).
fn read_doc(path: &Path) -> Option<(String, Option<String>)> {
    if exceeds_input_cap(path) {
        return None;
    }
    let mut magic = [0u8; 8];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut magic)
        .ok()?;
    if magic == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        crate::extract_pdf::read_word(path).map(|text| (text, None))
    } else if magic.starts_with(b"{\\rtf") {
        rtf_text(&read_capped_bytes(path)?)
    } else {
        read_text(path).map(|text| (text, None))
    }
}

/// The text and subject of an e-mail message held in memory (`.eml` bytes).
pub fn mail_text(bytes: &[u8]) -> Option<(String, Option<String>)> {
    crate::extract_mail::read_eml(bytes)
}

/// The text and declared title of an RTF document held in memory.
pub fn rtf_text(bytes: &[u8]) -> Option<(String, Option<String>)> {
    crate::extract_rtf::read_rtf(bytes)
}

/// A text-like file's bytes, when it is within the text input cap; read up to
/// the extraction cap.
fn read_capped_bytes(path: &Path) -> Option<Vec<u8>> {
    let size = text_input_size(path)?;
    let cap = max_extract_bytes();
    let mut buf = Vec::with_capacity(size.min(cap as u64) as usize);
    std::fs::File::open(path)
        .ok()?
        .take(cap as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
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
    } else if is_titled_ext(&ext) {
        read_titled(path, &ext).map(|(text, _)| text)
    } else {
        None
    }
}

/// Like [`content`], but office documents keep their paragraphs as lines and their
/// words whole. For readers that need the document's layout (`big-docinfo` looks
/// for a title line); the search index keeps using [`content`].
pub fn content_with_lines(path: &Path) -> Option<String> {
    let ext = ext_of(path)?;
    if is_office_ext(&ext) {
        if exceeds_input_cap(path) {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        office_text_with(std::io::BufReader::new(file), &ext, xml_text_lines)
    } else {
        content(path)
    }
}

/// What `big-docinfo` reads from a file: its text with paragraphs as lines, the
/// title its metadata declares, and the lines it sets in large type.
pub struct Described {
    pub text: String,
    pub declared_title: Option<String>,
    /// Largest type first; see [`crate::prominent`].
    pub prominent: Vec<String>,
}

/// [`content_with_lines`] (a PDF's opening pages only), with what the file says
/// about its own title.
pub fn described(path: &Path) -> Option<Described> {
    let ext = ext_of(path)?;
    let (text, declared_title) = match ext.as_str() {
        "pdf" => crate::extract_pdf::read_pdf_titled(path)?,
        "html" | "htm" | "xhtml" => {
            let html = content(path)?;
            (html_text(&html), html_element(&html, "title"))
        }
        e if is_titled_ext(e) => read_titled(path, e)?,
        _ => (content_with_lines(path)?, office_declared_title(path, &ext)),
    };
    let prominent = match ext.as_str() {
        "md" | "markdown" => markdown_headings(&text),
        "html" | "htm" | "xhtml" => content(path)
            .map(|html| html_element(&html, "h1").into_iter().collect())
            .unwrap_or_default(),
        _ => crate::prominent::prominent_lines(path, &ext),
    };
    Some(Described {
        text,
        declared_title,
        prominent,
    })
}

/// The `dc:title` an office document's metadata part declares.
fn office_declared_title(path: &Path, ext: &str) -> Option<String> {
    let part = match ext {
        "odt" | "ods" | "odp" => "meta.xml",
        "docx" | "xlsx" | "pptx" => "docProps/core.xml",
        _ => return None,
    };
    let xml = zip_entry_bytes(path, part)?;
    let xml = String::from_utf8_lossy(&xml);
    let (_, rest) = xml.split_once("<dc:title>")?;
    let (title, _) = rest.split_once("</dc:title>")?;
    let mut out = String::new();
    xml_text_lines(format!("<t>{title}</t>").as_bytes(), &mut out);
    let title = out.trim().to_owned();
    (!title.is_empty()).then_some(title)
}

/// ATX headings of a Markdown text, first level before second: "# Title".
fn markdown_headings(text: &str) -> Vec<String> {
    let level = |n: usize| {
        text.lines()
            .filter_map(move |line| {
                let rest = line.strip_prefix(&"#".repeat(n))?.strip_prefix(' ')?;
                let heading = rest.trim().trim_end_matches('#').trim();
                (!heading.is_empty()).then(|| heading.to_owned())
            })
            .take(3)
    };
    level(1).chain(level(2)).collect()
}

/// The text of the first `<tag>…</tag>` of an HTML page, tags inside removed.
fn html_element(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find(&format!("</{tag}"))?;
    let text = html_text(&html[start..end]);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// The readable text of HTML: scripts and styles dropped, block elements as
/// line breaks, the common entities decoded. Enough to read a saved page's
/// title and dates, not a renderer.
pub fn html_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut at = 0;
    while let Some(open) = lower[at..].find('<').map(|i| at + i) {
        out.push_str(&html[at..open]);
        let tag_end = lower[open..].find('>').map_or(html.len(), |i| open + i + 1);
        let tag = &lower[open..tag_end];
        // An unclosed script or style runs to the end, as in a browser; searching
        // again from every later opener would make the scan quadratic.
        let skip_to = ["script", "style"].iter().find_map(|name| {
            tag.starts_with(&format!("<{name}")).then(|| {
                lower[tag_end..]
                    .find(&format!("</{name}"))
                    .map_or(html.len(), |i| tag_end + i)
            })
        });
        let block = [
            "<p", "</p", "<br", "<div", "</div", "<h", "</h", "<li", "<tr", "<title", "</title",
        ]
        .iter()
        .any(|b| tag.starts_with(b));
        if block {
            out.push('\n');
        }
        at = skip_to.unwrap_or(tag_end).min(html.len());
    }
    out.push_str(&html[at.min(html.len())..]);
    decode_entities(&out)
}

/// `&amp;`, `&nbsp;`, `&#8212;`, `&#x2014;` and the like as characters.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // Only the next 11 bytes: a search to the end for every `&` is quadratic.
        // `;` is ASCII, so its index is a char boundary.
        let entity = rest.as_bytes()[1..]
            .iter()
            .take(11)
            .position(|&b| b == b';')
            .map(|end| &rest[1..=end]);
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => e
                .strip_prefix("#x")
                .or_else(|| e.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        });
        match (entity, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
/// path is fuzzable; it is the path `read_text` really takes, so what the
/// fuzzer drives is what runs.
///
/// The bytes are consumed: almost every file read is already valid UTF-8, and
/// then they simply become the string. `from_utf8_lossy(&buf).into_owned()`
/// would copy the whole file a second time even when nothing is replaced.
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
pub(crate) const MAX_ZIP_ENTRIES: usize = 4_096;

/// Slides read from one pptx. Bounds both the name `Vec` and the read loop.
const MAX_PPTX_SLIDES: usize = 512;

/// Reader-based core of `read_office`: extract concatenated body text from an
/// OOXML/ODF zip. Public so fuzz targets can drive it with arbitrary bytes
/// (`Cursor<&[u8]>`): this is the untrusted-document parsing surface.
pub fn office_text_from_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
    ext: &str,
) -> Option<String> {
    office_text_with(reader, ext, xml_text)
}

/// The capped, entry-scoped zip walk shared by both text shapes; `walk` turns one
/// XML part into text.
fn office_text_with<R: std::io::Read + std::io::Seek>(
    reader: R,
    ext: &str,
    walk: fn(&[u8], &mut String),
) -> Option<String> {
    let mut zip = zip::ZipArchive::new(reader).ok()?;

    // Per-part decompressed size is capped below, but the central directory is
    // read whole by `ZipArchive::new` before any of that: at the default 256 MiB
    // input cap an archive can declare millions of entries and cost hundreds of
    // megabytes in directory alone. Inside a 768 MiB cgroup that is an OOM kill,
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
        walk(&xml, &mut out);
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

/// Lowercase and fold `s` the way every comparison of names, pages and
/// content words does, so a person types a word the way they would write it
/// by hand:
///
/// - the text is first composed as [`compose`] says;
/// - accents and other marks go, whether the text carries them precomposed
///   ("relatório") or decomposed, as names unpacked from macOS archives do
///   ("relato\u{301}rio"); so does the dot that "İ" lowercases with;
/// - Latin letters fold through tantivy's `AsciiFoldingFilter`, the table the
///   content analyzer uses: "ß" is "ss", "Ł" is "l", "Æ" is "ae";
/// - the other scripts fold as [`fold_script_into`] says.
///
/// ASCII text is only lowercased.
#[must_use]
pub fn fold_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_lower_into(s, &mut out);
    out
}

/// The same, into a buffer the caller reuses: page after page, that is one
/// allocation instead of one per page.
pub fn fold_lower_into(s: &str, out: &mut String) {
    if s.is_ascii() {
        out.extend(s.chars().map(|c| c.to_ascii_lowercase()));
        return;
    }
    let lowered: String = compose(s).chars().flat_map(char::to_lowercase).collect();
    let mut folded = String::with_capacity(lowered.len());
    fold_script_into(&lowered, &mut folded);
    ASCII_FOLDING.with(|analyzer| {
        let mut analyzer = analyzer.borrow_mut();
        let mut stream = analyzer.token_stream(&folded);
        if stream.advance() {
            out.push_str(&stream.token().text);
        }
    });
}

/// `text` in NFKC, the form every comparison and every tokenizer starts from:
/// Korean syllables decomposed into jamo and kana with a separate voicing mark
/// (as macOS writes names), halfwidth katakana, Arabic presentation forms,
/// compatibility ideographs, fullwidth Latin and ligatures ("ﬁ") become the
/// characters people type. Text already in that form, nearly all of it, is not
/// copied.
///
/// A copy stops at [`max_extract_bytes`]: one character can expand eighteenfold
/// ("ﷺ"), and the extraction cap counts bytes before normalisation.
pub(crate) fn compose(text: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfkc_quick};
    if text.is_ascii() || is_nfkc_quick(text.chars()) == IsNormalized::Yes {
        return std::borrow::Cow::Borrowed(text);
    }
    let cap = max_extract_bytes();
    let mut out = String::with_capacity(text.len().min(cap));
    for c in text.nfkc() {
        if out.len() + c.len_utf8() > cap {
            break;
        }
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

/// The folding of lowercase text that `AsciiFoldingFilter` leaves to others:
/// combining marks dropped; Russian "ё" written "е", as it mostly is; Greek
/// without its accents and with final "ς" as "σ"; Arabic alef without hamza,
/// "ى" and Persian "ی" as "ي", Persian "ک" as "ك", "ة" as "ه" (each typed for
/// the other, by keyboard or by habit), and no vowel signs or tatweel;
/// traditional Chinese characters as their simplified forms ("會議" is "会议").
pub(crate) fn fold_script_into(lowered: &str, out: &mut String) {
    for c in lowered.chars() {
        match c {
            '\u{0300}'..='\u{036F}' | '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}' => {}
            'ё' => out.push('е'),
            'ς' => out.push('σ'),
            'ά' => out.push('α'),
            'έ' => out.push('ε'),
            'ή' => out.push('η'),
            'ί' | 'ϊ' | 'ΐ' => out.push('ι'),
            'ό' => out.push('ο'),
            'ύ' | 'ϋ' | 'ΰ' => out.push('υ'),
            'ώ' => out.push('ω'),
            'أ' | 'إ' | 'آ' | 'ٱ' => out.push('ا'),
            'ى' | 'ی' => out.push('ي'),
            'ک' => out.push('ك'),
            'ة' => out.push('ه'),
            c => out.push(crate::simplified_chinese::simplified(c)),
        }
    }
}

thread_local! {
    /// The whole text as one token through `AsciiFoldingFilter`.
    static ASCII_FOLDING: std::cell::RefCell<tantivy::tokenizer::TextAnalyzer> =
        std::cell::RefCell::new(
            tantivy::tokenizer::TextAnalyzer::builder(tantivy::tokenizer::RawTokenizer::default())
                .filter(tantivy::tokenizer::AsciiFoldingFilter)
                .build(),
        );
}

/// 1-based page (PDF) / slide (PPTX) numbers whose text contains *all* `terms`.
/// Empty for formats without fixed pages (docx/odt flow, txt, …).
///
/// One page at a time, and only the number is kept. The text of a big PDF is
/// tens of megabytes; holding every page in memory to answer with a handful of
/// integers is work this service cannot afford.
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
    use quick_xml::events::Event;
    let Some(xml) = zip_entry_bytes(path, part) else {
        return false;
    };
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut page = String::new();
    let mut run = String::new();
    let mut breaks = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e) | Event::Start(e)) if is_page_break(&e, break_tag) => {
                breaks += 1;
                end_run(&mut run, &mut page);
                on_page(&page);
                page.clear();
            }
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) => push_xml_text(&e, &mut run),
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => end_run(&mut run, &mut page),
        }
        buf.clear();
    }
    if breaks == 0 {
        return false;
    }
    end_run(&mut run, &mut page);
    on_page(&page);
    true
}

/// The slides of a pptx, in the order a person sees them.
pub(crate) fn slide_names<R: std::io::Read + std::io::Seek>(
    zip: &zip::ZipArchive<R>,
) -> Vec<String> {
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
pub(crate) fn zip_entry_bytes(path: &Path, name: &str) -> Option<Vec<u8>> {
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

/// Append all XML character data from `xml` to `out`, a space between elements.
///
/// Bytes, not text: every caller has an office part it just read, and turning
/// that into a `String` first is a copy of the whole part for nothing.
pub fn xml_text(xml: &[u8], out: &mut String) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut run = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) => push_xml_text(&e, &mut run),
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => end_run(&mut run, out),
        }
        buf.clear();
    }
    end_run(&mut run, out);
}

/// Text of an OOXML/ODF part as the document lays it out: a line per paragraph or
/// heading, line breaks and tabs kept, and the runs of a paragraph joined as
/// written: Word splits words across runs, so a space between runs would break
/// them. Character and predefined entity references are decoded. Field codes,
/// text deleted under change tracking and comments are not the document's text.
pub fn xml_text_lines(xml: &[u8], out: &mut String) {
    use quick_xml::events::Event;
    let hidden = |name: &[u8]| matches!(name, b"w:instrText" | b"w:delText" | b"office:annotation");
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut hidden_depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if hidden(e.name().as_ref()) => hidden_depth += 1,
            Ok(Event::End(e)) if hidden(e.name().as_ref()) => {
                hidden_depth = hidden_depth.saturating_sub(1)
            }
            Ok(Event::Text(_) | Event::GeneralRef(_) | Event::End(_) | Event::Empty(_))
                if hidden_depth > 0 => {}
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) => push_xml_text(&e, out),
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"w:p" | b"text:p" | b"text:h" | b"a:p" => out.push('\n'),
                b"w:tc" | b"table:table-cell" => out.push('\t'),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"w:br" | b"w:cr" | b"text:line-break" => out.push('\n'),
                b"w:tab" | b"text:tab" => out.push('\t'),
                b"text:s" => out.push(' '),
                b"w:p" | b"text:p" | b"text:h" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// The text of a text event or an entity reference, appended as written.
///
/// quick-xml reports `&amp;` and `&#243;` as events of their own, between the
/// text around them. Bytes that are not valid UTF-8 are read lossily rather
/// than dropped: an office part is UTF-8 by specification, so this only
/// happens to a damaged file, and a damaged file should still give up whatever
/// text it has.
fn push_xml_text(event: &quick_xml::events::Event<'_>, out: &mut String) {
    use quick_xml::events::Event;
    match event {
        Event::Text(t) => match t.decode() {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str(&String::from_utf8_lossy(t)),
        },
        Event::GeneralRef(r) => {
            if let Ok(Some(c)) = r.resolve_char_ref() {
                out.push(c);
            } else if let Ok(name) = r.decode() {
                out.push_str(match name.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                });
            }
        }
        _ => {}
    }
}

/// Move the text gathered inside one element to `out`, trimmed and followed by
/// a space.
fn end_run(run: &mut String, out: &mut String) {
    let text = run.trim();
    if !text.is_empty() {
        out.push_str(text);
        out.push(' ');
    }
    run.clear();
}

#[cfg(test)]
mod tests;
