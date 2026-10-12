// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The text of an RTF document, and the title its `{\info{\title …}}` declares.
//!
//! RTF is text with control words: `\par` ends a paragraph, `\'e9` is a byte in
//! the document's ANSI code page (read as Windows-1252, the common one), `\u233`
//! a Unicode character followed by `\ucN` fallback characters to skip. Groups
//! that are not text (fonts, colours, styles, pictures, embedded objects and
//! every `{\* …}` destination) are skipped whole.

/// Destinations whose contents are not the document's text.
const SKIPPED: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "pict",
    "object",
    "themedata",
    "colorschememapping",
    "datastore",
    "latentstyles",
    "xmlnstbl",
    "generator",
    "header",
    "footer",
    "headerl",
    "headerr",
    "footerl",
    "footerr",
    "fldinst",
    "filetbl",
    "revtbl",
    "author",
    "operator",
    "company",
    "subject",
    "keywords",
    "comment",
    "doccomm",
    "creatim",
    "revtim",
    "printim",
    "buptim",
];

/// Windows-1252 bytes 0x80–0x9F, the ones Latin-1 leaves as controls.
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// A group being read: whether its text is skipped, is the title, and how many
/// fallback characters follow a `\u`.
#[derive(Clone, Copy)]
struct Group {
    skip: bool,
    title: bool,
    fallback: usize,
}

/// The text of an RTF document and its declared title, or `None` when the bytes
/// are not RTF.
pub(crate) fn read_rtf(bytes: &[u8]) -> Option<(String, Option<String>)> {
    if !bytes.starts_with(b"{\\rtf") {
        return None;
    }
    let mut text = String::new();
    let mut title = String::new();
    let mut stack: Vec<Group> = Vec::new();
    let mut group = Group {
        skip: false,
        title: false,
        fallback: 1,
    };
    let mut pending_skip = 0usize; // fallback characters still to drop after \u
    let mut i = 0;
    let push = |c: char, group: &Group, text: &mut String, title: &mut String| {
        if group.title {
            title.push(c);
        } else if !group.skip {
            text.push(c);
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                stack.push(group);
                // "{\*\dest …}": a destination a reader may ignore.
                if bytes[i + 1..].starts_with(b"\\*") {
                    group.skip = true;
                }
                i += 1;
            }
            b'}' => {
                group = stack.pop().unwrap_or(group);
                i += 1;
            }
            b'\\' => {
                let rest = &bytes[i + 1..];
                match rest.first() {
                    Some(b'\'') => {
                        let byte = rest
                            .get(1..3)
                            .and_then(|hex| std::str::from_utf8(hex).ok())
                            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                        if let Some(byte) = byte {
                            if pending_skip > 0 {
                                pending_skip -= 1;
                            } else {
                                let c = match byte {
                                    0x80..=0x9f => CP1252[usize::from(byte - 0x80)],
                                    b => char::from(b),
                                };
                                push(c, &group, &mut text, &mut title);
                            }
                        }
                        i += 4;
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        let word_len = rest.iter().take_while(|c| c.is_ascii_alphabetic()).count();
                        let word = std::str::from_utf8(&rest[..word_len]).unwrap_or("");
                        let mut j = word_len;
                        let sign = usize::from(rest.get(j) == Some(&b'-'));
                        let digits = rest[j + sign..]
                            .iter()
                            .take_while(|c| c.is_ascii_digit())
                            .count();
                        let number = (digits > 0)
                            .then(|| {
                                std::str::from_utf8(&rest[j..j + sign + digits])
                                    .ok()?
                                    .parse::<i32>()
                                    .ok()
                            })
                            .flatten();
                        j += sign + digits;
                        if rest.get(j) == Some(&b' ') {
                            j += 1; // the space that ends a control word is not text
                        }
                        i += 1 + j;
                        match word {
                            "par" | "line" | "sect" | "page" | "row" => {
                                push('\n', &group, &mut text, &mut title);
                            }
                            "tab" | "cell" => push('\t', &group, &mut text, &mut title),
                            "endash" => push('–', &group, &mut text, &mut title),
                            "emdash" => push('—', &group, &mut text, &mut title),
                            "lquote" => push('‘', &group, &mut text, &mut title),
                            "rquote" => push('’', &group, &mut text, &mut title),
                            "ldblquote" => push('“', &group, &mut text, &mut title),
                            "rdblquote" => push('”', &group, &mut text, &mut title),
                            "bullet" => push('•', &group, &mut text, &mut title),
                            "uc" => {
                                group.fallback =
                                    number.and_then(|n| usize::try_from(n).ok()).unwrap_or(1)
                            }
                            "u" => {
                                if let Some(n) = number {
                                    // Negative values are the high half of a 16-bit code.
                                    let code = if n < 0 { n + 65_536 } else { n };
                                    if let Some(c) =
                                        u32::try_from(code).ok().and_then(char::from_u32)
                                    {
                                        push(c, &group, &mut text, &mut title);
                                    }
                                    pending_skip = group.fallback;
                                }
                            }
                            "title" => group.title = true,
                            "info" => group.skip = true,
                            w if SKIPPED.contains(&w) => group.skip = true,
                            _ => {}
                        }
                    }
                    Some(&c) => {
                        // A control symbol: "\{", "\}", "\\" are text; "\~" a space.
                        match c {
                            b'{' | b'}' | b'\\' => {
                                push(char::from(c), &group, &mut text, &mut title)
                            }
                            b'~' => push(' ', &group, &mut text, &mut title),
                            b'\n' | b'\r' => push('\n', &group, &mut text, &mut title),
                            _ => {}
                        }
                        i += 2;
                    }
                    None => i += 1,
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                // Plain text up to the next control character, as UTF-8 if it is.
                let end = bytes[i..]
                    .iter()
                    .position(|b| matches!(b, b'{' | b'}' | b'\\' | b'\r' | b'\n'))
                    .map_or(bytes.len(), |n| i + n);
                for c in String::from_utf8_lossy(&bytes[i..end]).chars() {
                    if pending_skip > 0 {
                        pending_skip -= 1;
                    } else {
                        push(c, &group, &mut text, &mut title);
                    }
                }
                i = end;
            }
        }
    }
    let title = title.trim().to_owned();
    (!text.trim().is_empty()).then(|| (text, (!title.is_empty()).then_some(title)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtf_reads_text_and_title() {
        let rtf =
            br"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}
{\info{\title Ata da reuni\'e3o}{\author Bruno}}{\*\generator Writer;}
\pard\f0\fs24 ATA DA REUNI\'C3O ORDIN\'C1RIA\par
Aos onze dias, \u8220?reuni\u227?o\u8221? \endash  pauta\tab item\par
{\pict\pngblip 89504e47}Fim\par}";
        let (text, title) = read_rtf(rtf).expect("text");
        assert_eq!(title.as_deref(), Some("Ata da reunião"));
        assert!(text.contains("ATA DA REUNIÃO ORDINÁRIA\n"), "{text:?}");
        assert!(
            text.contains("Aos onze dias, “reunião” – pauta"),
            "{text:?}"
        );
        assert!(text.contains("pauta\titem"), "{text:?}");
        assert!(
            !text.contains("Arial") && !text.contains("89504e47") && !text.contains("Bruno"),
            "{text:?}"
        );
        assert!(read_rtf(b"not rtf").is_none());
    }
}
