//! The text of an RTF document, and the title its `{\info{\title …}}` declares.
//!
//! RTF is text with control words: `\par` ends a paragraph, `\'e9` is a byte in
//! the document's ANSI code page (read as Windows-1252, the common one), `\u233`
//! a Unicode character followed by `\ucN` fallback characters to skip. Groups
//! that are not text (fonts, colors, styles, pictures, embedded objects and
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

/// Deepest group nesting kept apart from its parent.
const MAX_GROUP_DEPTH: usize = 1024;

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

/// The document's own group: text kept, one fallback character after a `\u`.
const GROUP_START: Group = Group {
    skip: false,
    title: false,
    fallback: 1,
};

/// The text of an RTF document and its declared title, or `None` when the bytes
/// are not RTF.
pub(crate) fn read_rtf(bytes: &[u8]) -> Option<(String, Option<String>)> {
    if !bytes.starts_with(b"{\\rtf") {
        return None;
    }
    let mut reader = RtfReader::default();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                // Past the cap a group is read as part of its parent: nesting
                // that deep is no document's, and each level costs memory.
                if reader.stack.len() < MAX_GROUP_DEPTH {
                    reader.stack.push(reader.group);
                } else {
                    if reader.uncounted == 0 {
                        reader.past_cap = reader.group;
                    }
                    reader.uncounted += 1;
                }
                // "{\*\dest …}": a destination a reader may ignore.
                if bytes[i + 1..].starts_with(b"\\*") {
                    reader.group.skip = true;
                }
                i += 1;
            }
            b'}' => {
                if reader.uncounted > 0 {
                    reader.uncounted -= 1;
                    if reader.uncounted == 0 {
                        reader.group = reader.past_cap;
                    }
                } else {
                    reader.group = reader.stack.pop().unwrap_or(reader.group);
                }
                i += 1;
            }
            b'\\' => i += 1 + reader.control(&bytes[i + 1..]),
            b'\r' | b'\n' => i += 1,
            _ => {
                // Plain text up to the next control character, as UTF-8 if it is.
                let end = bytes[i..]
                    .iter()
                    .position(|b| matches!(b, b'{' | b'}' | b'\\' | b'\r' | b'\n'))
                    .map_or(bytes.len(), |n| i + n);
                for c in String::from_utf8_lossy(&bytes[i..end]).chars() {
                    reader.push_unless_fallback(c);
                }
                i = end;
            }
        }
    }
    let title = reader.title.trim().to_owned();
    let text = reader.text;
    (!text.trim().is_empty()).then(|| (text, (!title.is_empty()).then_some(title)))
}

/// What has been read so far, and the state of the group being read.
struct RtfReader {
    text: String,
    title: String,
    stack: Vec<Group>,
    group: Group,
    /// Groups opened past [`MAX_GROUP_DEPTH`] and not closed yet.
    uncounted: usize,
    /// The group the first of them opened in, back when the last one closes.
    past_cap: Group,
    /// Fallback characters still to drop after a `\u`.
    pending_skip: usize,
}

impl Default for RtfReader {
    fn default() -> Self {
        Self {
            text: String::new(),
            title: String::new(),
            stack: Vec::new(),
            group: GROUP_START,
            uncounted: 0,
            past_cap: GROUP_START,
            pending_skip: 0,
        }
    }
}

impl RtfReader {
    fn push(&mut self, c: char) {
        if self.group.title {
            self.title.push(c);
        } else if !self.group.skip {
            self.text.push(c);
        }
    }

    /// A character that may be one of the fallbacks a `\u` asked to drop.
    fn push_unless_fallback(&mut self, c: char) {
        if self.pending_skip > 0 {
            self.pending_skip -= 1;
        } else {
            self.push(c);
        }
    }

    /// What follows a backslash; returns how many of `rest`'s bytes it took.
    fn control(&mut self, rest: &[u8]) -> usize {
        match rest.first() {
            Some(b'\'') => {
                // `\'hh`: one byte in Windows-1252.
                let byte = rest
                    .get(1..3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(byte) = byte {
                    let c = match byte {
                        0x80..=0x9f => CP1252[usize::from(byte - 0x80)],
                        b => char::from(b),
                    };
                    self.push_unless_fallback(c);
                }
                3
            }
            Some(c) if c.is_ascii_alphabetic() => self.control_word(rest),
            Some(&c) => {
                // A control symbol: "\{", "\}", "\\" are text; "\~" a space.
                match c {
                    b'{' | b'}' | b'\\' => self.push(char::from(c)),
                    b'~' => self.push(' '),
                    b'\n' | b'\r' => self.push('\n'),
                    _ => {}
                }
                1
            }
            None => 0,
        }
    }

    /// A control word, its optional number and the space that ends it; returns
    /// how many of `rest`'s bytes it took.
    fn control_word(&mut self, rest: &[u8]) -> usize {
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
        match word {
            "par" | "line" | "sect" | "page" | "row" => self.push('\n'),
            "tab" | "cell" => self.push('\t'),
            "endash" => self.push('–'),
            "emdash" => self.push('—'),
            "lquote" => self.push('‘'),
            "rquote" => self.push('’'),
            "ldblquote" => self.push('“'),
            "rdblquote" => self.push('”'),
            "bullet" => self.push('•'),
            "uc" => self.group.fallback = number.and_then(|n| usize::try_from(n).ok()).unwrap_or(1),
            "u" => {
                if let Some(n) = number {
                    // Negative values are the high half of a 16-bit code.
                    let code = if n < 0 { n + 65_536 } else { n };
                    if let Some(c) = u32::try_from(code).ok().and_then(char::from_u32) {
                        self.push(c);
                    }
                    self.pending_skip = self.group.fallback;
                }
            }
            "title" => self.group.title = true,
            "info" => self.group.skip = true,
            w if SKIPPED.contains(&w) => self.group.skip = true,
            _ => {}
        }
        j
    }
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

    /// Nesting past the cap is read as its parent's text, and the groups
    /// around it close where they should.
    #[test]
    fn rtf_deep_nesting_read() {
        let depth = MAX_GROUP_DEPTH * 4;
        let mut rtf = br"{\rtf1{\info{\title T}}{\fonttbl{\f0 Arial;}}".to_vec();
        rtf.extend(std::iter::repeat_n(b'{', depth));
        rtf.extend_from_slice(b"deep");
        rtf.extend(std::iter::repeat_n(b'}', depth));
        rtf.extend_from_slice(br"\par after}");
        let (text, title) = read_rtf(&rtf).expect("text");
        assert_eq!(title.as_deref(), Some("T"));
        assert_eq!(text, "deep\nafter");
    }

    /// A skipped destination opened past the cap hides only its own text.
    #[test]
    fn rtf_skip_past_cap_ends() {
        let mut rtf = br"{\rtf1".to_vec();
        rtf.extend(std::iter::repeat_n(b'{', MAX_GROUP_DEPTH - 1));
        rtf.extend_from_slice(br"{\*\ignored hidden}visible");
        rtf.extend(std::iter::repeat_n(b'}', MAX_GROUP_DEPTH));
        let (text, _) = read_rtf(&rtf).expect("text");
        assert_eq!(text, "visible");
    }
}
