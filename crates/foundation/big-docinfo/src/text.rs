// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Text plumbing shared by the finders: accent folding, normalized words, lines.
//!
//! Positions in the rules are counted in characters, not bytes, because that is
//! how the reference implementation counts them; the helpers here do the
//! conversion so the finders can stay readable.

/// Lowercase and fold the accents, ordinal marks, superscripts and typographic
/// ligatures of Portuguese, Spanish and English text (PDF text is full of `ﬁ`).
pub fn fold_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' | 'ª' => out.push('a'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'í' | 'ì' | 'î' | 'ï' => out.push('i'),
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'º' | 'ᵒ' => out.push('o'),
            'ú' | 'ù' | 'û' | 'ü' => out.push('u'),
            'ç' => out.push('c'),
            'ñ' => out.push('n'),
            'ý' | 'ÿ' => out.push('y'),
            '¹' => out.push('1'),
            '²' => out.push('2'),
            '³' => out.push('3'),
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            other => out.push(other),
        }
    }
    out
}

/// A word character as the reference's regular expressions see it.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Folded words separated by single spaces: what the rules match against.
pub fn norm(s: &str) -> String {
    let folded = fold_lower(s);
    let mut out = String::with_capacity(folded.len());
    for word in folded
        .split(|c: char| !is_word(c))
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Lines split on every line boundary Python's `str.splitlines` knows: PDF text
/// separates pages with a form feed, which `str::lines` would keep.
pub fn lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Non-empty lines, trimmed. Zero-width characters count as blank: editors pad
/// with them, and a line of nothing but U+200B is not a title.
pub fn content_lines(s: &str) -> Vec<&str> {
    lines(s)
        .into_iter()
        .map(|l| {
            l.trim_matches(|c: char| {
                c.is_whitespace() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
            })
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// The first `n` characters of `s`.
pub fn first_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// The last `n` characters of `s`.
pub fn last_chars(s: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    s.char_indices()
        .rev()
        .nth(n - 1)
        .map_or(s, |(i, _)| &s[i..])
}

/// Number of whitespace-separated words.
pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}
