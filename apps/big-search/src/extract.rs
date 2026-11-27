//! Content extraction. Plain text in-process; PDF via a `pdftotext` subprocess
//! (argv array, hard wall-clock timeout, capped output), which is faster and
//! lighter than in-process Rust PDF libraries (DESIGN.md, "Extraction").
//! Word 97-2003 `.doc` the same way through `antiword`.
mod epub;

pub(crate) use crate::extract_pdf::read_pdf;
use crate::fold::{fold_lower, fold_lower_into};
use epub::read_epub;
use std::io::Read;
use std::path::Path;

/// `path` opened for reading when it is a regular file, a link followed: the
/// readers' one way to a file's bytes. A `stat` taken before cannot see a FIFO
/// renamed into place since, and a plain open of one waits for a writer.
pub(crate) fn open_regular(path: &Path) -> Option<std::fs::File> {
    open_regular_with(path, 0).ok()
}

/// Why [`open_regular_with`] gave no file.
pub(crate) enum Unopened {
    /// Not a regular file, or not readable: read it again once it changes.
    Refused,
    /// Under another process's write lease, or no descriptor free: a later pass may read it.
    Busy,
}

impl From<std::io::Error> for Unopened {
    fn from(error: std::io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::EWOULDBLOCK | libc::EMFILE | libc::ENFILE) => Self::Busy,
            _ => Self::Refused,
        }
    }
}

/// Set once a reopen has found no `/proc/self/fd`.
static PROC_FD_MISSING_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// [`open_regular`] with `flags` added to the first open, such as `O_NOFOLLOW`.
pub(crate) fn open_regular_with(
    path: &Path,
    flags: libc::c_int,
) -> Result<std::fs::File, Unopened> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // `O_PATH` runs no FIFO or device open. The non-blocking reopen fails at once
    // under a write lease and, while open, bars one, so the blocking reopen beside
    // it cannot wait.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags | libc::O_PATH)
        .open(path)?;
    if !handle.metadata()?.is_file() {
        return Err(Unopened::Refused);
    }
    let reopen_path = format!("/proc/self/fd/{}", handle.as_raw_fd());
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&reopen_path)
        .and_then(|_lease_guard| std::fs::File::open(&reopen_path))
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound
                && !PROC_FD_MISSING_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                log::warn!("documents are skipped unread: /proc/self/fd is unavailable ({error})");
            }
            error.into()
        })
}

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

/// Pages indexed by their readable text, not their markup.
fn is_html_ext(ext: &str) -> bool {
    matches!(ext, "html" | "htm" | "xhtml")
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
    open_regular(path)?.read_exact(&mut magic).ok()?;
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
    // Sized up front: a text file is read whole, and growth copies it.
    let mut buf = Vec::with_capacity(size.min(cap as u64) as usize);
    open_regular(path)?
        .take(cap as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// The extension of `path` without the dot, ASCII-lowercased; `None` when it
/// has none or it is not UTF-8.
pub(crate) fn ext_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// Extracted text for `path`, or `None` if not eligible / not extractable.
pub fn content(path: &Path) -> Option<String> {
    let ext = ext_of(path)?;
    if is_html_ext(&ext) {
        read_text(path).map(|html| html_text(&html))
    } else if is_text_ext(&ext) {
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

/// What `big-docinfo` reads from a file: its text with paragraphs as lines, the
/// title its metadata declares, and the lines it sets in large type.
pub struct Described {
    pub text: String,
    pub declared_title: Option<String>,
    /// Largest type first; see [`crate::prominent`].
    pub prominent: Vec<String>,
}

/// [`content`] (a PDF's opening pages only), with what the file says about its
/// own title.
pub fn described(path: &Path) -> Option<Described> {
    let ext = ext_of(path)?;
    if is_html_ext(&ext) {
        let html = read_text(path)?;
        return Some(Described {
            text: html_text(&html),
            declared_title: html_element(&html, "title"),
            prominent: html_element(&html, "h1").into_iter().collect(),
        });
    }
    let (text, declared_title) = match ext.as_str() {
        "pdf" => crate::extract_pdf::read_pdf_titled(path)?,
        e if is_titled_ext(e) => read_titled(path, e)?,
        _ => (content(path)?, office_declared_title(path, &ext)),
    };
    let prominent = match ext.as_str() {
        "md" | "markdown" => markdown_headings(&text),
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

/// Elements that sit inside a line of text: the others separate words.
const INLINE_TAGS: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "dfn", "em", "font", "i", "kbd",
    "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var", "wbr",
];

/// The readable text of HTML: scripts and styles dropped, block elements as
/// line breaks, entities decoded. Enough to read a saved page's
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
        let name = tag
            .trim_start_matches(['<', '/'])
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default();
        // What an image or a link says of itself is read too; markup is not.
        for attribute in [" alt=", " title="] {
            let Some(found) = tag.find(attribute) else {
                continue;
            };
            let value = &html[open + found + attribute.len()..tag_end];
            if let Some(quote @ ('"' | '\'')) = value.chars().next()
                && let Some(len) = value[1..].find(quote)
            {
                out.push(' ');
                out.push_str(&value[1..=len]);
                out.push(' ');
            }
        }
        if block {
            out.push('\n');
        } else if skip_to.is_none() && !name.is_empty() && !INLINE_TAGS.contains(&name) {
            // A table cell, a list or any other box ends the word before it.
            out.push(' ');
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
        // Only as far as the longest HTML5 name: a search to the end for every
        // `&` is quadratic. `;` is ASCII, so its index is a char boundary.
        let entity = rest.as_bytes()[1..]
            .iter()
            .take(33)
            .position(|&b| b == b';')
            .map(|end| &rest[1..=end]);
        let decoded = |e: &str| -> Option<std::borrow::Cow<'static, str>> {
            if e == "nbsp" {
                return Some(" ".into());
            }
            match e.strip_prefix('#') {
                Some(number) => number
                    .strip_prefix(['x', 'X'])
                    .map_or_else(
                        || number.parse().ok(),
                        |hex| u32::from_str_radix(hex, 16).ok(),
                    )
                    .and_then(char::from_u32)
                    .map(|c| c.to_string().into()),
                None => quick_xml::escape::resolve_html5_entity(e).map(Into::into),
            }
        };
        match entity.and_then(|e| Some((e, decoded(e)?))) {
            Some((e, text)) => {
                out.push_str(&text);
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
/// PDF via `pdftotext`, HTML as its readable text, anything else `text/*` as
/// plain text. Used by the metadata pass so an extension-less document is still
/// full-text searchable.
pub fn content_for_mime(path: &Path, mime: &str) -> Option<String> {
    if mime == "application/pdf" {
        read_pdf(path)
    } else if mime == "text/html" {
        read_text(path).map(|html| html_text(&html))
    } else if mime.starts_with("text/") {
        read_text(path)
    } else {
        None
    }
}

/// Read a text file up to the adaptive extraction cap; reject binary NULs.
fn read_text(path: &Path) -> Option<String> {
    text_from_bytes(read_capped_bytes(path)?).map(|mut text| {
        truncate_to_char_boundary(&mut text, max_extract_bytes());
        text
    })
}

/// The size of a text file, or `None` when it is over the input cap.
fn text_input_size(path: &Path) -> Option<u64> {
    let size = std::fs::metadata(path).ok()?.len();
    let cap = crate::settings::text_max_input_bytes();
    (cap == 0 || size <= cap).then_some(size)
}

/// A blob with no text in it: empty, or holding a NUL byte early on.
fn looks_binary(buf: &[u8]) -> bool {
    buf.is_empty() || buf.iter().take(8192).any(|&b| b == 0)
}

/// `bytes` as text, each invalid UTF-8 sequence as U+FFFD; valid text, nearly
/// all of it, becomes the string without a copy. `String::from_utf8_lossy_owned`
/// validates through `Utf8Chunks`, without `from_utf8`'s word-at-a-time ASCII
/// path.
pub(crate) fn lossy_utf8(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(std::string::FromUtf8Error::into_utf8_lossy)
}

/// Decode a text blob: reject empty or binary (NUL in the first chunk), else
/// UTF-8, lossily where it has to be. Split out from `read_text` so the decode
/// path is fuzzable; it is the path `read_text` really takes, so what the
/// fuzzer drives is what runs.
///
/// The bytes are consumed so valid UTF-8, almost every file, becomes the
/// string without a second copy.
pub fn text_from_bytes(buf: Vec<u8>) -> Option<String> {
    if looks_binary(&buf) {
        return None;
    }
    Some(lossy_utf8(buf))
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

/// Extract text from an OOXML/ODF zip by reading its body XML part(s) as
/// [`xml_text_lines`] lays them out. Capped + entry-scoped (zip-bomb safe).
fn read_office(path: &Path, ext: &str) -> Option<String> {
    if exceeds_input_cap(path) {
        return None;
    }
    let file = open_regular(path)?;
    office_text_from_reader(std::io::BufReader::new(file), ext)
}

fn exceeds_input_cap(path: &Path) -> bool {
    let cap = crate::settings::office_max_input_bytes();
    cap > 0
        && std::fs::metadata(path)
            .map(|m| m.len() > cap)
            .unwrap_or(false)
}

/// Entries an office archive may declare before we refuse it. Generous against
/// real documents (a heavy pptx is a few hundred), tight against a bomb.
pub(crate) const MAX_ZIP_ENTRIES: usize = 4_096;

/// Markup read from one office archive, in extraction caps.
const ARCHIVE_MARKUP_CAPS: usize = 4;

/// An office or EPUB archive, refused past [`MAX_ZIP_ENTRIES`]: `ZipArchive::new`
/// reads the whole central directory, which a crafted file inflates to hundreds
/// of megabytes before any per-part cap applies.
#[expect(
    clippy::disallowed_methods,
    reason = "the owner of opening an archive with its entry cap"
)]
pub(crate) fn capped_zip<R: std::io::Read + std::io::Seek>(
    reader: R,
) -> Option<zip::ZipArchive<R>> {
    let zip = zip::ZipArchive::new(reader).ok()?;
    (zip.len() <= MAX_ZIP_ENTRIES).then_some(zip)
}

/// The archive at `path`, as [`capped_zip`] accepts it.
pub(crate) fn open_archive(
    path: &Path,
) -> Option<zip::ZipArchive<std::io::BufReader<std::fs::File>>> {
    capped_zip(std::io::BufReader::new(open_regular(path)?))
}

/// Slides, worksheets, speaker notes, headers or footers read from one archive,
/// each kind on its own. Bounds both the name `Vec` and the read loop.
const MAX_NUMBERED_PARTS: usize = 512;

/// The parts of an office archive that hold its text, in reading order.
fn text_parts<R: std::io::Read + std::io::Seek>(
    zip: &zip::ZipArchive<R>,
    ext: &str,
) -> Option<Vec<String>> {
    let parts = match ext {
        "docx" => {
            let mut parts = vec!["word/document.xml".to_string()];
            parts.extend(numbered_parts(zip, "word/header"));
            parts.extend(numbered_parts(zip, "word/footer"));
            parts.extend(["word/footnotes.xml".into(), "word/endnotes.xml".into()]);
            parts
        }
        "odt" | "ods" | "odp" => vec!["content.xml".to_string()],
        // Most strings sit in the shared table; a sheet written by a streaming
        // writer holds its own.
        "xlsx" => std::iter::once("xl/sharedStrings.xml".to_string())
            .chain(numbered_parts(zip, XLSX_SHEET_PREFIX))
            .collect(),
        // Numeric order, the same order `for_each_pptx_slide` reads them in: a
        // deck's text and its page numbers must describe the same slides. The
        // speaker notes follow, outside any slide's page.
        "pptx" => {
            let mut parts = slide_names(zip);
            parts.extend(numbered_parts(zip, "ppt/notesSlides/notesSlide"));
            parts
        }
        _ => return None,
    };
    Some(parts)
}

/// Reader-based core of `read_office`: extract body text from an OOXML/ODF zip.
/// Public so fuzz targets can drive it with arbitrary bytes (`Cursor<&[u8]>`):
/// this is the untrusted-document parsing surface.
pub fn office_text_from_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
    ext: &str,
) -> Option<String> {
    let mut zip = capped_zip(reader)?;
    let parts = text_parts(&zip, ext)?;

    let cap = max_extract_bytes();
    // Markup decompressed across all parts: parts of markup with no text would
    // otherwise each cost a full cap of parsing.
    let mut budget = cap.saturating_mul(ARCHIVE_MARKUP_CAPS);
    let mut out = String::new();
    for part in parts {
        if budget == 0 {
            break;
        }
        let Ok(mut entry) = zip_entry(&mut zip, &part) else {
            continue;
        };
        let mut xml = Vec::new();
        let read = entry
            .by_ref()
            .take(cap.min(budget) as u64)
            .read_to_end(&mut xml);
        budget = budget.saturating_sub(xml.len());
        if read.is_err() {
            continue;
        }
        if part.starts_with(XLSX_SHEET_PREFIX) {
            xlsx_sheet_text(&xml, &mut out);
        } else {
            xml_text_lines(&xml, &mut out);
        }
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
            let (part, break_tag): (&str, &str) = if ext == "docx" {
                ("word/document.xml", "w:lastRenderedPageBreak")
            } else {
                ("content.xml", "text:soft-page-break")
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
    let Some(mut zip) = open_archive(path) else {
        return;
    };
    let names = slide_names(&zip);
    let mut xml = Vec::new();
    let mut text = String::new();
    for name in names {
        // Every slide is announced, readable or not: the caller counts these
        // calls to number the pages, and skipping one would move every slide
        // after it up by one.
        text.clear();
        if let Ok(mut entry) = zip_entry(&mut zip, &name) {
            xml.clear();
            if entry
                .by_ref()
                .take(cap as u64)
                .read_to_end(&mut xml)
                .is_ok()
            {
                xml_text_lines(&xml, &mut text);
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
    break_tag: &str,
    on_page: &mut dyn FnMut(&str),
) -> bool {
    let Some(xml) = zip_entry_bytes(path, part) else {
        return false;
    };
    let mut page = String::new();
    if office_xml_pages(&xml, Some(break_tag), &mut page, on_page) == 0 {
        return false;
    }
    on_page(&page);
    true
}

/// The slides of a pptx, in the order a person sees them.
pub(crate) fn slide_names<R: std::io::Read + std::io::Seek>(
    zip: &zip::ZipArchive<R>,
) -> Vec<String> {
    numbered_parts(zip, "ppt/slides/slide")
}

const XLSX_SHEET_PREFIX: &str = "xl/worksheets/sheet";

/// The parts named `{prefix}N.xml`, in the order of N, at most
/// [`MAX_NUMBERED_PARTS`].
fn numbered_parts<R: std::io::Read + std::io::Seek>(
    zip: &zip::ZipArchive<R>,
    prefix: &str,
) -> Vec<String> {
    // A name the archive cannot decode is no part of ours: they are ASCII.
    let mut names: Vec<String> = zip
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
        .map(String::from)
        .collect();
    // Numerically: `slide10.xml` sorts before `slide2.xml` as text.
    names.sort_by_key(|n| {
        n.trim_start_matches(prefix)
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(0)
    });
    names.truncate(MAX_NUMBERED_PARTS);
    names
}

/// Entry `name` of `zip`, matched on the bytes it is stored under, so that a
/// missing name costs one hash lookup: on a miss, `ZipArchive::by_name`
/// decodes every entry's name, which a crafted archive would repeat for each
/// name its manifest asks for.
fn zip_entry<'a, R: std::io::Read + std::io::Seek>(
    zip: &'a mut zip::ZipArchive<R>,
    name: &str,
) -> zip::result::ZipResult<zip::read::ZipFile<'a, R>> {
    let index = zip
        .index_for_name_raw(name.as_bytes())
        .ok_or(zip::result::ZipError::FileNotFound)?;
    zip.by_index(index)
}

/// Bytes of a zip entry (capped), or `None`.
pub(crate) fn zip_entry_bytes(path: &Path, name: &str) -> Option<Vec<u8>> {
    let cap = max_extract_bytes();
    let mut zip = open_archive(path)?;
    let mut entry = zip_entry(&mut zip, name).ok()?;
    let mut xml = Vec::new();
    entry.by_ref().take(cap as u64).read_to_end(&mut xml).ok()?;
    Some(xml)
}

/// True if `e` is a cached page-break marker or an explicit `w:br w:type="page"`.
fn is_page_break(e: &quick_xml::events::BytesStart, break_tag: &str) -> bool {
    let name = e.name();
    if name.as_ref() == break_tag {
        return true;
    }
    if name.as_ref() == "w:br" {
        return e
            .attributes()
            .flatten()
            .any(|a| a.key.as_ref() == "w:type" && a.value.as_ref() == "page");
    }
    false
}

/// Text of an OOXML/ODF part as the document lays it out: a line per paragraph or
/// heading, line breaks and tabs kept, and the runs of a paragraph joined as
/// written: Word splits words across runs, so a space between runs would break
/// them. Character and predefined entity references are decoded. Field codes,
/// text deleted under change tracking, comments and the phonetic readings of a
/// spreadsheet's strings are not the document's text.
pub fn xml_text_lines(xml: &[u8], out: &mut String) {
    office_xml_pages(xml, None, out, &mut |_| {});
}

/// [`xml_text_lines`], cut into pages at `break_tag` and at `w:br w:type="page"`
/// when a tag is given: `on_page` gets the text of each page before a break, and
/// `page` keeps the text after the last one. Returns how many breaks there were.
fn office_xml_pages(
    xml: &[u8],
    break_tag: Option<&str>,
    page: &mut String,
    on_page: &mut dyn FnMut(&str),
) -> usize {
    use quick_xml::events::Event;
    let hidden = |name: &str| matches!(name, "instrText" | "delText" | "annotation" | "rPh");
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut hidden_depth = 0usize;
    let mut breaks = 0;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if hidden(e.local_name().as_ref()) => hidden_depth += 1,
            Ok(Event::End(e)) if hidden(e.local_name().as_ref()) => {
                hidden_depth = hidden_depth.saturating_sub(1)
            }
            Ok(
                Event::Text(_)
                | Event::GeneralRef(_)
                | Event::Start(_)
                | Event::End(_)
                | Event::Empty(_),
            ) if hidden_depth > 0 => {}
            Ok(Event::Empty(e) | Event::Start(e))
                if break_tag.is_some_and(|tag| is_page_break(&e, tag)) =>
            {
                breaks += 1;
                on_page(page);
                page.clear();
            }
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) => push_xml_text(&e, page),
            Ok(e @ (Event::Start(_) | Event::End(_) | Event::Empty(_))) => {
                // An opening only separates what came before it from what it holds.
                if let Some(mark) = layout_char(&e)
                    && (!matches!(e, Event::Start(_))
                        || !page.is_empty() && !page.ends_with(char::is_whitespace))
                {
                    page.push(mark);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    breaks
}

/// The character an element stands for in the laid-out text: a line, a tab, a
/// space or a hyphen. Matched without the prefix, which writers choose freely
/// (`x:si`, `w:p`, `a:br`).
fn layout_char(event: &quick_xml::events::Event<'_>) -> Option<char> {
    use quick_xml::events::Event;
    match event {
        // A note, a text box or a paragraph inside another paragraph.
        Event::Start(e) => match e.local_name().as_ref() {
            "p" | "h" => Some('\n'),
            "note" | "txbxContent" | "text-box" => Some(' '),
            _ => None,
        },
        Event::End(e) => match e.local_name().as_ref() {
            // `si` is one string of a spreadsheet's shared string table.
            "p" | "h" | "si" | "br" | "cr" | "line-break" => Some('\n'),
            "tc" | "table-cell" => Some('\t'),
            _ => None,
        },
        Event::Empty(e) => match e.local_name().as_ref() {
            "br" | "cr" | "line-break" | "p" | "h" => Some('\n'),
            "tab" => Some('\t'),
            "s" => Some(' '),
            "noBreakHyphen" => Some('-'),
            _ => None,
        },
        _ => None,
    }
}

/// The strings a worksheet holds itself (inline strings, text results of
/// formulas), a tab after each cell and a line per row. Element names are
/// matched without the prefix some writers add (`x:c`).
fn xlsx_sheet_text(xml: &[u8], out: &mut String) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    // The open cell's type says it holds a string of its own.
    let mut text_cell = false;
    // Inside that string's `<t>` or `<v>`.
    let mut reading = false;
    let mut phonetic_depth = 0usize;
    let mut cell_start = out.len();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "c" => {
                    text_cell = e.attributes().flatten().any(|a| {
                        a.key.local_name().as_ref() == "t"
                            && matches!(a.value.as_ref(), "inlineStr" | "str")
                    });
                    cell_start = out.len();
                }
                "rPh" => phonetic_depth += 1,
                "t" | "v" => reading = text_cell && phonetic_depth == 0,
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "c" => {
                    if out.len() > cell_start {
                        out.push('\t');
                    }
                    text_cell = false;
                }
                "row" => {
                    if out.ends_with('\t') {
                        out.pop();
                        out.push('\n');
                    }
                }
                "rPh" => phonetic_depth = phonetic_depth.saturating_sub(1),
                "t" | "v" => reading = false,
                _ => {}
            },
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) if reading => push_xml_text(&e, out),
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// The text of a text event or an entity reference, appended as written.
///
/// quick-xml reports `&amp;` and `&#243;` as events of their own, between the
/// text around them. The reader stops at bytes that are not valid UTF-8; an
/// office part is UTF-8 by specification, so only a damaged file loses the text
/// after the damage.
pub(crate) fn push_xml_text(event: &quick_xml::events::Event<'_>, out: &mut String) {
    use quick_xml::events::Event;
    match event {
        Event::Text(t) => out.push_str(t),
        Event::GeneralRef(r) => {
            if let Ok(Some(c)) = r.resolve_char_ref() {
                out.push(c);
            } else {
                out.push_str(match &**r {
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

#[cfg(test)]
mod tests;
