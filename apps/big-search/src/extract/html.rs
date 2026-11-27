//! HTML as text: what web pages, EPUB chapters and mail parts are indexed as,
//! and the title and first heading a page is described by.

/// The text of the first `<tag>…</tag>` of an HTML page, tags inside removed.
pub(super) fn html_element(html: &str, tag: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let open = find_ignore_ascii_case(bytes, format!("<{tag}").as_bytes())?;
    let start = open + memchr::memchr(b'>', &bytes[open..])? + 1;
    let end = start + find_ignore_ascii_case(&bytes[start..], format!("</{tag}").as_bytes())?;
    let text = html_text(&html[start..end]);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Whether `name`, lowercase, is an element that sits inside a line of text:
/// the others separate words.
fn is_inline_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"a" | b"abbr"
            | b"b"
            | b"bdi"
            | b"bdo"
            | b"cite"
            | b"code"
            | b"data"
            | b"dfn"
            | b"em"
            | b"font"
            | b"i"
            | b"kbd"
            | b"mark"
            | b"q"
            | b"s"
            | b"samp"
            | b"small"
            | b"span"
            | b"strong"
            | b"sub"
            | b"sup"
            | b"time"
            | b"u"
            | b"var"
            | b"wbr"
    )
}

/// Openings, lowercase, of the elements that end a line: a prefix, so `<h`
/// also takes `<head>` and `<hr>`.
const BLOCK_TAG_OPENINGS: &[&[u8]] = &[
    b"<p", b"</p", b"<br", b"<div", b"</div", b"<h", b"</h", b"<li", b"<tr", b"<title", b"</title",
];

/// Elements whose content is not text, by opening and closing prefix,
/// lowercase.
const SKIPPED_ELEMENTS: [(&[u8], &[u8]); 2] = [(b"<script", b"</script"), (b"<style", b"</style")];

/// First position of `needle` in `haystack`, ASCII case ignored. The first
/// byte of `needle` has no case, so a byte search finds every candidate.
fn find_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memchr_iter(needle[0], haystack).find(|&at| {
        haystack
            .get(at..at + needle.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(needle))
    })
}

/// The readable text of HTML: scripts and styles dropped, block elements as
/// line breaks, entities decoded. Enough to read a saved page's
/// title and dates, not a renderer.
///
/// Markup is matched by bytes with ASCII case ignored: every byte it looks for
/// is ASCII, so each position found is a char boundary.
pub fn html_text(html: &str) -> String {
    let bytes = html.as_bytes();
    // What is kept is cut from the page, so its length nearly always fits.
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(open) = memchr::memchr(b'<', &bytes[at..]).map(|i| at + i) {
        out.push_str(&html[at..open]);
        let tag_end = memchr::memchr(b'>', &bytes[open..]).map_or(html.len(), |i| open + i + 1);
        let tag = &bytes[open..tag_end];
        // The tag's start, lowercased once: every opening compared fits in it,
        // and the NUL padding of a shorter tag matches none of them.
        let mut head = [0u8; 8];
        let head_len = tag.len().min(head.len());
        head[..head_len].copy_from_slice(&tag[..head_len]);
        head.make_ascii_lowercase();
        // An unclosed script or style runs to the end, as in a browser; searching
        // again from every later opener would make the scan quadratic.
        let skip_to = SKIPPED_ELEMENTS.iter().find_map(|&(opening, closing)| {
            head.starts_with(opening).then(|| {
                find_ignore_ascii_case(&bytes[tag_end..], closing)
                    .map_or(html.len(), |i| tag_end + i)
            })
        });
        let block = BLOCK_TAG_OPENINGS
            .iter()
            .any(|opening| head.starts_with(opening));
        let name_start = tag
            .iter()
            .position(|&b| b != b'<' && b != b'/')
            .unwrap_or(tag.len());
        let name = &tag[name_start..];
        let name = &name[..name
            .iter()
            .position(|b| !b.is_ascii_alphanumeric())
            .unwrap_or(name.len())];
        // No inline element's name is longer than the buffer.
        let mut lowered = [0u8; 8];
        let inline = name.len() <= lowered.len() && {
            let lowered = &mut lowered[..name.len()];
            lowered.copy_from_slice(name);
            lowered.make_ascii_lowercase();
            is_inline_element(lowered)
        };
        push_described_text(&html[open..tag_end], &mut out);
        if block {
            out.push('\n');
        } else if skip_to.is_none() && !name.is_empty() && !inline {
            // A table cell, a list or any other box ends the word before it.
            out.push(' ');
        }
        at = skip_to.unwrap_or(tag_end).min(html.len());
    }
    out.push_str(&html[at.min(html.len())..]);
    decode_entities(out)
}

/// What an image or a link says of itself, the quoted `alt` and `title` of
/// `tag`, each between spaces; the rest of the markup is not text.
fn push_described_text(tag: &str, out: &mut String) {
    for attribute in [&b" alt="[..], b" title="] {
        let Some(found) = find_ignore_ascii_case(tag.as_bytes(), attribute) else {
            continue;
        };
        let value = &tag[found + attribute.len()..];
        if let Some(&quote @ (b'"' | b'\'')) = value.as_bytes().first()
            && let Some(len) = memchr::memchr(quote, &value.as_bytes()[1..])
        {
            out.push(' ');
            out.push_str(&value[1..=len]);
            out.push(' ');
        }
    }
}

/// `&amp;`, `&nbsp;`, `&#8212;`, `&#x2014;` and the like as characters; text
/// without an `&` is returned as it is.
pub(super) fn decode_entities(text: String) -> String {
    let Some(first) = memchr::memchr(b'&', text.as_bytes()) else {
        return text;
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut rest = &text[first..];
    while let Some(at) = memchr::memchr(b'&', rest.as_bytes()) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // Only as far as the longest HTML5 name: a search to the end for every
        // `&` is quadratic. `;` is ASCII, so its index is a char boundary.
        let entity = rest.as_bytes()[1..]
            .iter()
            .take(33)
            .position(|&b| b == b';')
            .map(|end| &rest[1..=end]);
        match entity {
            Some(entity) if push_entity(entity, &mut out) => rest = &rest[entity.len() + 2..],
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Pushes the character an entity's name or number (between `&` and `;`)
/// stands for; false, pushing nothing, when it stands for none.
fn push_entity(entity: &str, out: &mut String) -> bool {
    if entity == "nbsp" {
        out.push(' ');
        return true;
    }
    if let Some(number) = entity.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => number.parse().ok(),
        };
        let Some(c) = code.and_then(char::from_u32) else {
            return false;
        };
        out.push(c);
        return true;
    }
    let Some(text) = quick_xml::escape::resolve_html5_entity(entity) else {
        return false;
    };
    out.push_str(text);
    true
}
