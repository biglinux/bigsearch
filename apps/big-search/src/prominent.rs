// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The lines a document sets in large type at its opening: what a reader takes
//! for its title, and what `big-docinfo` weighs as one.
//!
//! Every format gives the same answer: runs of consecutive lines in one type
//! size, keeping those set clearly larger than the body text, largest first.
//! PDF: line heights on page 1 from `pdftotext -bbox-layout`. ODF and OOXML:
//! the font size each paragraph and run resolves to through its styles.
//! Presentations: the first slide's title box. Markdown and HTML: their top
//! headings, which name their own prominence.

use std::collections::HashMap;
use std::path::Path;

use quick_xml::events::{BytesStart, Event};

/// A run counts as prominent when its type is this much larger than the body.
const PROMINENT: f32 = 1.15;
/// Prominent runs handed on; a title is among the first few.
const MAX_RUNS: usize = 8;
/// Paragraphs of an office document read for type sizes: its opening.
const OPENING_PARAGRAPHS: usize = 40;
/// Text kept per run; anything longer is body, not a title.
const MAX_RUN_CHARS: usize = 300;

/// Prominent lines of `path`, largest type first; empty when nothing stands out.
pub fn prominent_lines(path: &Path, ext: &str) -> Vec<String> {
    match ext {
        // A PDF title broken over lines is one run; an office paragraph is one already.
        "pdf" => crate::extract_pdf::read_pdf_first_page_layout(path)
            .map(|xhtml| ranked(pdf_lines(&xhtml), true))
            .unwrap_or_default(),
        // Below an office document's largest type come its section headings
        // ("QUADRO RESUMO", "Introdução"): only the largest is a title there.
        "odt" => ranked(odf_lines(path), false).into_iter().take(1).collect(),
        "docx" => ranked(docx_lines(path), false)
            .into_iter()
            .take(1)
            .collect(),
        "pptx" => pptx_title(path).into_iter().collect(),
        "odp" => odp_title(path).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Runs of consecutive lines in one size (with `join`; otherwise each line is
/// its own run), kept when set larger than the body, the size that carries
/// the most characters. A largest size shared by three runs or more marks
/// section headings, and then nothing is kept. When large type is no small part of the opening
/// (scanned pages, slides saved as text), no size stands out and none is kept.
fn ranked(lines: Vec<(String, f32)>, join: bool) -> Vec<String> {
    let same = |a: f32, b: f32| (a - b).abs() <= a.max(b) * 0.03;
    let lines: Vec<(String, f32)> = lines
        .into_iter()
        .map(|(text, size)| (text.split_whitespace().collect::<Vec<_>>().join(" "), size))
        .filter(|(text, size)| !text.is_empty() && size.is_finite() && *size > 0.0)
        .collect();
    let mut weights: Vec<(f32, usize)> = Vec::new();
    for (text, size) in &lines {
        match weights.iter_mut().find(|(s, _)| same(*s, *size)) {
            Some((_, chars)) => *chars += text.chars().count(),
            None => weights.push((*size, text.chars().count())),
        }
    }
    let Some(&(body, _)) = weights.iter().max_by_key(|(_, chars)| *chars) else {
        return Vec::new();
    };
    let mut runs: Vec<(String, f32)> = Vec::new();
    for (text, size) in lines {
        match runs.last_mut() {
            Some((run, run_size)) if join && same(*run_size, size) => {
                if run.len() < MAX_RUN_CHARS {
                    run.push(' ');
                    run.push_str(&text);
                }
            }
            _ => runs.push((text, size)),
        }
    }
    let mut prominent: Vec<(String, f32)> = runs
        .into_iter()
        .filter(|(_, size)| *size >= body * PROMINENT)
        .collect();
    // Stable: among equal sizes the earlier run stays first.
    prominent.sort_by(|a, b| b.1.total_cmp(&a.1));
    // The largest size set three times or more is the style of section
    // headings ("OBJETO", "APRESENTAÇÃO", "Introduction", "Results"), not a title.
    let top = prominent.first().map_or(0.0, |(_, size)| *size);
    if prominent
        .iter()
        .filter(|(_, size)| same(*size, top))
        .count()
        >= 3
    {
        return Vec::new();
    }
    prominent
        .into_iter()
        .take(MAX_RUNS)
        .map(|(text, _)| text)
        .collect()
}

fn attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// Text of a text event or an entity reference, decoded.
fn push_event_text(event: &Event<'_>, out: &mut String) {
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

/// Appends a text event to a paragraph; visible text raises the paragraph's
/// `largest` size to the `size` it is set in.
fn push_sized_text(event: &Event<'_>, text: &mut String, largest: &mut f32, size: f32) {
    let before = text.len();
    push_event_text(event, text);
    if text[before..].chars().any(|c| !c.is_whitespace()) {
        *largest = largest.max(size);
    }
}

/// Keeps a finished paragraph unless it is blank; `true` once the opening
/// paragraphs are all collected.
fn keep_opening(lines: &mut Vec<(String, f32)>, text: String, size: f32) -> bool {
    if !text.trim().is_empty() {
        lines.push((text, size));
    }
    lines.len() == OPENING_PARAGRAPHS
}

/// Lines of page 1 with their heights. A line taller than it is wide runs
/// vertically (a publisher's stamp up the margin) and is left out.
fn pdf_lines(xhtml: &[u8]) -> Vec<(String, f32)> {
    let mut reader = quick_xml::Reader::from_reader(xhtml);
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    let mut line: Option<(f32, f32)> = None; // (height, width)
    let mut in_word = false;
    let mut text = String::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) if e.name().as_ref() == b"line" => {
                let coord = |n: &[u8]| attr(e, n).and_then(|v| v.parse::<f32>().ok());
                line = match (
                    coord(b"xMin"),
                    coord(b"yMin"),
                    coord(b"xMax"),
                    coord(b"yMax"),
                ) {
                    (Some(x0), Some(y0), Some(x1), Some(y1)) => Some((y1 - y0, x1 - x0)),
                    _ => None,
                };
                text.clear();
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"word" => in_word = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"word" => {
                in_word = false;
                text.push(' ');
            }
            Ok(ev @ (Event::Text(_) | Event::GeneralRef(_))) if in_word => {
                push_event_text(ev, &mut text);
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"line" => {
                if let Some((height, width)) = line.take()
                    && height <= width
                {
                    lines.push((std::mem::take(&mut text), height));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    lines
}

/// A font size in points from an ODF length ("18pt", "0.5in", "130%" of `parent`).
fn odf_size(value: &str, parent: f32) -> Option<f32> {
    let number = |suffix: &str| value.strip_suffix(suffix)?.trim().parse::<f32>().ok();
    number("pt")
        .or_else(|| number("%").map(|p| p / 100.0 * parent))
        .or_else(|| number("cm").map(|v| v * 28.35))
        .or_else(|| number("mm").map(|v| v * 2.835))
        .or_else(|| number("in").map(|v| v * 72.0))
        .or_else(|| number("px").map(|v| v * 0.75))
}

/// Named styles: parent and own size (absolute, or relative as a raw value).
#[derive(Default)]
struct Styles {
    by_name: HashMap<String, (Option<String>, Option<String>)>,
    default: Option<f32>,
}

impl Styles {
    /// The size `name` resolves to, through its parents, down to the default.
    fn size(&self, name: Option<&str>, fallback: f32) -> f32 {
        let base = self.default.unwrap_or(fallback);
        // Sizes relative to a parent need the parent first: walk up, then apply
        // back down. Depth-capped against a style that names itself.
        let mut chain = Vec::new();
        let mut current = name.map(str::to_owned);
        while let Some(style) = current.take() {
            if chain.len() == 16 {
                break;
            }
            let Some((parent, size)) = self.by_name.get(&style) else {
                break;
            };
            chain.push(size.clone());
            current = parent.clone();
        }
        let mut size = base;
        for own in chain.into_iter().rev().flatten() {
            size = odf_size(&own, size).unwrap_or(size);
        }
        size
    }
}

/// ODF styles from one part (`styles.xml`, or the automatic ones in `content.xml`).
fn read_odf_styles(xml: &[u8], styles: &mut Styles) {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut current: Option<String> = None;
    let mut in_default = false;
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e) | Event::Empty(e)) => match e.name().as_ref() {
                b"style:style" => {
                    if let Some(name) = attr(e, b"style:name") {
                        styles
                            .by_name
                            .insert(name.clone(), (attr(e, b"style:parent-style-name"), None));
                        current = matches!(event, Ok(Event::Start(_))).then_some(name);
                    }
                }
                b"style:default-style" => {
                    in_default = matches!(event, Ok(Event::Start(_)))
                        && attr(e, b"style:family").as_deref() == Some("paragraph");
                }
                b"style:text-properties" => {
                    if let Some(size) = attr(e, b"fo:font-size") {
                        if in_default {
                            styles.default = odf_size(&size, 12.0);
                        } else if let Some(entry) =
                            current.as_ref().and_then(|n| styles.by_name.get_mut(n))
                        {
                            entry.1 = Some(size);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"style:style" => current = None,
                b"style:default-style" => in_default = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// Opening paragraphs of an ODF text with the largest size any of their text
/// is set in (a span may be larger than its paragraph).
fn odf_lines(path: &Path) -> Vec<(String, f32)> {
    let mut styles = Styles::default();
    if let Some(xml) = crate::extract::zip_entry_bytes(path, "styles.xml") {
        read_odf_styles(&xml, &mut styles);
    }
    let Some(content) = crate::extract::zip_entry_bytes(path, "content.xml") else {
        return Vec::new();
    };
    read_odf_styles(&content, &mut styles);
    odf_opening_paragraphs(&content, &styles)
}

/// The paragraph walk of [`odf_lines`] over `content.xml`; annotations are skipped.
fn odf_opening_paragraphs(content: &[u8], styles: &Styles) -> Vec<(String, f32)> {
    let mut reader = quick_xml::Reader::from_reader(content);
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    let mut paragraph: Option<(String, f32, f32)> = None; // text, own size, largest used
    let mut spans: Vec<f32> = Vec::new();
    let mut hidden = 0usize;
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) if e.name().as_ref() == b"office:annotation" => hidden += 1,
            Ok(Event::End(e)) if e.name().as_ref() == b"office:annotation" => {
                hidden = hidden.saturating_sub(1);
            }
            _ if hidden > 0 => {}
            Ok(Event::Start(e)) if matches!(e.name().as_ref(), b"text:p" | b"text:h") => {
                let size = styles.size(attr(e, b"text:style-name").as_deref(), 12.0);
                paragraph = Some((String::new(), size, 0.0));
                spans.clear();
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"text:span" => {
                spans.push(odf_span_size(e, styles, &spans, paragraph.as_ref()));
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"text:span" => {
                spans.pop();
            }
            Ok(ev @ (Event::Text(_) | Event::GeneralRef(_))) => {
                if let Some((text, own, largest)) = paragraph.as_mut() {
                    push_sized_text(ev, text, largest, spans.last().copied().unwrap_or(*own));
                }
            }
            Ok(Event::Empty(e))
                if matches!(
                    e.name().as_ref(),
                    b"text:s" | b"text:tab" | b"text:line-break"
                ) =>
            {
                if let Some((text, _, _)) = paragraph.as_mut() {
                    text.push(' ');
                }
            }
            Ok(Event::End(e)) if matches!(e.name().as_ref(), b"text:p" | b"text:h") => {
                if let Some((text, _, largest)) = paragraph.take()
                    && keep_opening(&mut lines, text, largest)
                {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    lines
}

/// The size a `text:span` sets, relative to the span or paragraph it sits in.
fn odf_span_size(
    e: &BytesStart<'_>,
    styles: &Styles,
    spans: &[f32],
    paragraph: Option<&(String, f32, f32)>,
) -> f32 {
    let outer = spans.last().copied().or(paragraph.map(|p| p.1));
    attr(e, b"text:style-name")
        .and_then(|n| styles.by_name.get(&n).and_then(|s| s.1.clone()))
        .and_then(|v| odf_size(&v, outer.unwrap_or(12.0)))
        .or(outer)
        .unwrap_or(12.0)
}

/// A docx style id to its `w:basedOn` style and its size in half-points.
type DocxStyles = HashMap<String, (Option<String>, Option<f32>)>;

/// OOXML styles: each style's base and size in half-points; the document default.
fn read_docx_styles(xml: &[u8]) -> (DocxStyles, f32) {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut styles = HashMap::new();
    let mut default = 20.0; // Word's own default when the file names none: 10 pt
    let mut current: Option<String> = None;
    let mut in_defaults = false;
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e) | Event::Empty(e)) => match e.name().as_ref() {
                b"w:style" => {
                    current = attr(e, b"w:styleId");
                    if let Some(id) = &current {
                        styles.insert(id.clone(), (None, None));
                    }
                }
                b"w:rPrDefault" => in_defaults = true,
                b"w:basedOn" => {
                    if let (Some(id), Some(base)) = (&current, attr(e, b"w:val"))
                        && let Some(style) = styles.get_mut(id)
                    {
                        style.0 = Some(base);
                    }
                }
                b"w:sz" => {
                    let size = attr(e, b"w:val").and_then(|v| v.parse::<f32>().ok());
                    if in_defaults {
                        default = size.unwrap_or(default);
                    } else if let Some(style) = current.as_ref().and_then(|id| styles.get_mut(id)) {
                        style.1 = size;
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"w:style" => current = None,
                b"w:rPrDefault" => in_defaults = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (styles, default)
}

/// Opening paragraphs of a docx with the largest size their text is set in:
/// direct run formatting, else the run's or the paragraph's style.
fn docx_lines(path: &Path) -> Vec<(String, f32)> {
    let (styles, default) = crate::extract::zip_entry_bytes(path, "word/styles.xml")
        .map(|xml| read_docx_styles(&xml))
        .unwrap_or_else(|| (HashMap::new(), 20.0));
    let Some(xml) = crate::extract::zip_entry_bytes(path, "word/document.xml") else {
        return Vec::new();
    };
    docx_opening_paragraphs(&xml, &styles, default)
}

/// The size a docx style sets, inherited through at most 16 `w:basedOn` steps.
fn docx_style_size(styles: &DocxStyles, id: Option<&str>) -> Option<f32> {
    let mut current = id.map(str::to_owned);
    for _ in 0..16 {
        let Some(style) = current.as_ref().and_then(|c| styles.get(c)) else {
            break;
        };
        if let Some(size) = style.1 {
            return Some(size);
        }
        current = style.0.clone();
    }
    None
}

/// The paragraph walk of [`docx_lines`] over `word/document.xml`; sizes are in
/// half-points until a paragraph is kept.
fn docx_opening_paragraphs(xml: &[u8], styles: &DocxStyles, default: f32) -> Vec<(String, f32)> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    let mut paragraph: Option<(String, Option<f32>, f32)> = None; // text, style size, largest
    let mut run: Option<f32> = None; // size set on the current run
    let (mut in_ppr, mut in_text) = (false, false);
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:p" => {
                paragraph = Some((String::new(), None, 0.0));
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:pPr" => in_ppr = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:pPr" => in_ppr = false,
            Ok(Event::Start(e) | Event::Empty(e)) if e.name().as_ref() == b"w:pStyle" => {
                if let Some((_, style, _)) = paragraph.as_mut() {
                    *style = docx_style_size(styles, attr(e, b"w:val").as_deref());
                }
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:r" => run = None,
            // The paragraph mark's own size (inside w:pPr) is not its text's.
            Ok(Event::Start(e) | Event::Empty(e))
                if !in_ppr && e.name().as_ref() == b"w:rStyle" =>
            {
                run = run.or(docx_style_size(styles, attr(e, b"w:val").as_deref()));
            }
            Ok(Event::Start(e) | Event::Empty(e)) if !in_ppr && e.name().as_ref() == b"w:sz" => {
                run = attr(e, b"w:val").and_then(|v| v.parse().ok()).or(run);
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:t" => in_text = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:t" => in_text = false,
            Ok(ev @ (Event::Text(_) | Event::GeneralRef(_))) if in_text => {
                if let Some((text, style, largest)) = paragraph.as_mut() {
                    push_sized_text(ev, text, largest, run.or(*style).unwrap_or(default));
                }
            }
            Ok(Event::Empty(e)) if matches!(e.name().as_ref(), b"w:tab" | b"w:br" | b"w:cr") => {
                if let Some((text, _, _)) = paragraph.as_mut() {
                    text.push(' ');
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"w:p" => {
                if let Some((text, _, largest)) = paragraph.take()
                    && keep_opening(&mut lines, text, largest / 2.0)
                {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    lines
}

/// The text of the first slide's title placeholder.
fn pptx_title(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    if zip.len() > crate::extract::MAX_ZIP_ENTRIES {
        return None;
    }
    let first = crate::extract::slide_names(&zip).into_iter().next()?;
    let xml = crate::extract::zip_entry_bytes(path, &first)?;
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let (mut depth, mut shape_start, mut is_title, mut in_text) = (0usize, None, false, false);
    let mut text = String::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) => {
                depth += 1;
                match e.name().as_ref() {
                    b"p:sp" => {
                        shape_start = Some(depth);
                        is_title = false;
                        text.clear();
                    }
                    b"a:t" => in_text = true,
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"p:ph" => {
                is_title |= matches!(attr(e, b"type").as_deref(), Some("title" | "ctrTitle"));
            }
            Ok(ev @ (Event::Text(_) | Event::GeneralRef(_))) if in_text => {
                push_event_text(ev, &mut text)
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"a:t" => in_text = false,
                    b"a:p" => text.push(' '),
                    b"p:sp" if shape_start == Some(depth) => {
                        if is_title && !text.trim().is_empty() {
                            return Some(text.split_whitespace().collect::<Vec<_>>().join(" "));
                        }
                        shape_start = None;
                    }
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// The text of the first page's title frame in an ODF presentation.
fn odp_title(path: &Path) -> Option<String> {
    let xml = crate::extract::zip_entry_bytes(path, "content.xml")?;
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let (mut in_title, mut pages) = (false, 0);
    let mut text = String::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) if e.name().as_ref() == b"draw:page" => {
                pages += 1;
                if pages > 1 {
                    return None;
                }
            }
            Ok(Event::Start(e)) if e.name().as_ref() == b"draw:frame" => {
                in_title = attr(e, b"presentation:class").as_deref() == Some("title");
            }
            Ok(ev @ (Event::Text(_) | Event::GeneralRef(_))) if in_title => {
                push_event_text(ev, &mut text)
            }
            Ok(Event::End(e)) if in_title && e.name().as_ref() == b"text:p" => text.push(' '),
            Ok(Event::End(e)) if e.name().as_ref() == b"draw:frame" && in_title => {
                let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
                return (!title.is_empty()).then_some(title);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn larger_runs_rank_by_size() {
        let lines = vec![
            ("CARDIOLOGY".to_owned(), 9.0),
            (
                "Early mobilization after cardiac surgery and".to_owned(),
                20.0,
            ),
            ("length of hospital stay".to_owned(), 20.0),
            ("Ana Souza, Rui Lima".to_owned(), 11.0),
            (
                "Patients who leave bed early may recover faster than those who stay.".to_owned(),
                10.0,
            ),
            (
                "We randomized adults to early or usual mobilization and compared stays."
                    .to_owned(),
                10.0,
            ),
            (
                "Early mobilization shortened the stay by more than a day.".to_owned(),
                10.0,
            ),
            (
                "Falls and readmissions were no more frequent in that group.".to_owned(),
                10.0,
            ),
        ];
        assert_eq!(
            ranked(lines, true),
            ["Early mobilization after cardiac surgery and length of hospital stay"]
        );
        // Section headings in the largest size: no title stands out.
        let sections = ["OBJETO", "APRESENTAÇÃO", "ACOMPANHAMENTO"]
            .iter()
            .map(|h| ((*h).to_owned(), 14.0))
            .chain((0..6).map(|i| (format!("Corpo do plano de trabalho, parágrafo {i}."), 10.0)))
            .collect();
        assert!(ranked(sections, false).is_empty());
        // One size throughout: nothing stands out.
        assert!(
            ranked(
                vec![("Ata".to_owned(), 12.0), ("Texto".to_owned(), 12.0)],
                true
            )
            .is_empty()
        );
    }

    #[test]
    fn pdf_lines_skip_vertical_stamps() {
        let xhtml = br#"<doc><page><flow><block>
<line xMin="10" yMin="10" xMax="20" yMax="300"><word>Downloaded</word></line>
<line xMin="50" yMin="100" xMax="400" yMax="121"><word>Fish</word><word>&amp;</word><word>chips</word></line>
</block></flow></page></doc>"#;
        assert_eq!(pdf_lines(xhtml), [("Fish & chips ".to_owned(), 21.0)]);
    }

    #[test]
    fn docx_styles_give_half_point_sizes() {
        let (styles, default) = read_docx_styles(
            br#"<w:styles><w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="Title"><w:basedOn w:val="Normal"/><w:rPr><w:sz w:val="56"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Subtitle"><w:basedOn w:val="Title"/></w:style></w:styles>"#,
        );
        assert!((default - 22.0).abs() < f32::EPSILON);
        assert_eq!(styles["Title"], (Some("Normal".to_owned()), Some(56.0)));
        assert_eq!(styles["Subtitle"], (Some("Title".to_owned()), None));
    }

    #[test]
    fn odf_sizes_resolve_through_parents() {
        let mut styles = Styles::default();
        read_odf_styles(
            br#"<office:styles><style:default-style style:family="paragraph"><style:text-properties fo:font-size="12pt"/></style:default-style>
<style:style style:name="Heading" style:family="paragraph"><style:text-properties fo:font-size="14pt"/></style:style>
<style:style style:name="Heading_20_1" style:parent-style-name="Heading" style:family="paragraph"><style:text-properties fo:font-size="130%"/></style:style>
<style:style style:name="Body" style:family="paragraph"/></office:styles>"#,
            &mut styles,
        );
        assert!((styles.size(Some("Heading_20_1"), 12.0) - 18.2).abs() < 0.01);
        assert!((styles.size(Some("Body"), 12.0) - 12.0).abs() < 0.01);
        assert!((styles.size(Some("missing"), 12.0) - 12.0).abs() < 0.01);
    }
}
