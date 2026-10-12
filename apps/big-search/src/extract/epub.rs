//! EPUB text: the package document named by `META-INF/container.xml`, then
//! each spine chapter in reading order, read as HTML by [`super::html_text`].
use super::{
    MAX_ZIP_ENTRIES, decode_entities, exceeds_input_cap, html_text, max_extract_bytes,
    push_xml_text, truncate_to_char_boundary,
};
use std::io::Read;
use std::path::Path;

/// Spine documents read from one EPUB, against a book built to be endless.
const MAX_EPUB_PARTS: usize = 2_000;

/// An EPUB's text, its chapters in reading order, and its `dc:title`.
///
/// `META-INF/container.xml` names the package document; its manifest maps ids
/// to files and its spine orders them. Each chapter is XHTML, read as HTML.
pub(super) fn read_epub(path: &Path) -> Option<(String, Option<String>)> {
    if exceeds_input_cap(path) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return None;
    }
    let cap = max_extract_bytes();
    let mut part = |name: &str, limit: usize| -> Option<String> {
        let mut entry = zip.by_name(name).ok()?;
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    };
    let container = part("META-INF/container.xml", cap)?;
    let opf_path = epub_package(container.as_bytes())?;
    let opf = part(&opf_path, cap)?;
    let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let (title, spine) = epub_spine(opf.as_bytes());
    let mut text = String::new();
    // Chapters of markup alone yield no text, so the output cap never stops a
    // spine of them; this bounds what the whole book may inflate.
    let mut inflate_budget = cap.saturating_mul(2);
    let mut seen = std::collections::HashSet::new();
    for href in spine.iter().take(MAX_EPUB_PARTS) {
        if !seen.insert(href.as_str()) {
            continue;
        }
        let name = if base.is_empty() {
            percent_decoded(href)
        } else {
            format!("{base}/{}", percent_decoded(href))
        };
        if let Some(chapter) = part(&name, cap.min(inflate_budget)) {
            inflate_budget -= chapter.len().min(inflate_budget);
            text.push_str(&html_text(&chapter));
            text.push('\n');
        }
        if text.len() >= cap || inflate_budget == 0 {
            truncate_to_char_boundary(&mut text, cap);
            break;
        }
    }
    (!text.trim().is_empty()).then_some((text, title))
}

/// The local name of an XML element, without its namespace prefix.
fn local_name(e: &quick_xml::events::BytesStart<'_>) -> Vec<u8> {
    e.local_name().as_ref().to_vec()
}

fn xml_attribute(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| decode_entities(&String::from_utf8_lossy(&a.value)))
}

/// The package document `META-INF/container.xml` points to.
fn epub_package(container: &[u8]) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(container);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) if local_name(&e) == b"rootfile" => {
                return xml_attribute(&e, b"full-path");
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// A package document's `dc:title` and its spine: the chapter files in reading order.
fn epub_spine(opf: &[u8]) -> (Option<String>, Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(opf);
    let mut buf = Vec::new();
    let (mut hrefs, mut order) = (std::collections::HashMap::new(), Vec::new());
    let (mut title, mut in_title) = (None::<String>, false);
    let mut title_text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => match local_name(&e).as_slice() {
                b"item" => {
                    if let (Some(id), Some(href)) =
                        (xml_attribute(&e, b"id"), xml_attribute(&e, b"href"))
                    {
                        hrefs.insert(id, href);
                    }
                }
                b"itemref" => order.extend(xml_attribute(&e, b"idref")),
                b"title" if title.is_none() => in_title = true,
                _ => {}
            },
            Ok(e @ (Event::Text(_) | Event::GeneralRef(_))) if in_title => {
                push_xml_text(&e, &mut title_text);
            }
            Ok(Event::End(e)) if in_title && e.local_name().as_ref() == b"title" => {
                in_title = false;
                let text = title_text.trim();
                if !text.is_empty() {
                    title = Some(text.to_owned());
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    let spine = order
        .iter()
        .filter_map(|id| hrefs.get(id).cloned())
        .collect();
    (title, spine)
}

/// "%20" and the like in an EPUB's file references.
fn percent_decoded(href: &str) -> String {
    let bytes = href.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = href
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
