//! Case and script folding: the one form names, pages and content words are
//! compared in.
use crate::extract::max_extract_bytes;

/// Lowercase and fold `s` the way every comparison of names, pages and
/// content words does, so a person types a word the way they would write it
/// by hand:
///
/// - the text is first composed as `compose` says;
/// - accents and other marks go, whether the text carries them precomposed
///   ("relatório") or decomposed, as names unpacked from macOS archives do
///   ("relato\u{301}rio"); so does the dot that "İ" lowercases with;
/// - Latin letters fold through tantivy's `AsciiFoldingFilter`, the table the
///   content analyzer uses: "ß" is "ss", "Ł" is "l", "Æ" is "ae";
/// - the other scripts fold as `fold_script_into` says.
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
/// ("ﷺ"), and the extraction cap counts bytes before normalization.
///
/// Only the runs of non-ASCII text are checked and normalized, each with the
/// ASCII character before it: NFKC leaves ASCII as it is and composes nothing
/// into an ASCII character from its left, so a cut before one changes nothing.
pub(crate) fn compose(text: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfkc_quick};
    let bytes = text.as_bytes();
    let cap = max_extract_bytes();
    // Created at the first run that changes; `text[copied..]` is not in it yet.
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut at = 0;
    while let Some(non_ascii) = first_non_ascii(&bytes[at..]).map(|i| at + i) {
        // The ASCII character before a run may take its combining marks.
        let start = non_ascii.saturating_sub(1).max(at);
        let end = bytes[non_ascii..]
            .iter()
            .position(u8::is_ascii)
            .map_or(bytes.len(), |i| non_ascii + i);
        let run = &text[start..end];
        if is_nfkc_quick(run.chars()) != IsNormalized::Yes {
            let out = out.get_or_insert_with(|| String::with_capacity(text.len().min(cap)));
            if !push_str_within_cap(out, &text[copied..start], cap)
                || !push_within_cap(out, run.nfkc(), cap)
            {
                return std::borrow::Cow::Owned(std::mem::take(out));
            }
            copied = end;
        }
        at = end;
    }
    match out {
        None => std::borrow::Cow::Borrowed(text),
        Some(mut out) => {
            push_str_within_cap(&mut out, &text[copied..], cap);
            std::borrow::Cow::Owned(out)
        }
    }
}

/// [`push_within_cap`] for text already normalized: whole when it fits.
fn push_str_within_cap(out: &mut String, text: &str, cap: usize) -> bool {
    if out.len() + text.len() <= cap {
        out.push_str(text);
        return true;
    }
    push_within_cap(out, text.chars(), cap)
}

/// Pushes `chars` while they fit in `cap` bytes; false at the first that does
/// not, which is left out with all after it.
fn push_within_cap(out: &mut String, chars: impl Iterator<Item = char>, cap: usize) -> bool {
    for c in chars {
        if out.len() + c.len_utf8() > cap {
            return false;
        }
        out.push(c);
    }
    true
}

/// The index of the first byte of `bytes` above ASCII, 16 bytes at a time
/// until the block that holds it.
fn first_non_ascii(bytes: &[u8]) -> Option<usize> {
    let (blocks, _) = bytes.as_chunks::<16>();
    let ascii_blocks = blocks
        .iter()
        .position(|block| !block.is_ascii())
        .unwrap_or(blocks.len());
    let skipped = ascii_blocks * 16;
    bytes[skipped..]
        .iter()
        .position(|b| !b.is_ascii())
        .map(|i| skipped + i)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Oracle: `nfkc()` over the whole text, which `compose` cuts into runs.
    /// Every string of up to four characters from marks of several combining
    /// classes, Hangul jamo, compatibility characters and ASCII.
    #[test]
    fn composed_runs_equal_whole_text_nfkc() {
        use unicode_normalization::UnicodeNormalization;
        const ALPHABET: [char; 22] = [
            'a', 'e', 'n', ' ', 'é', '\u{0301}', '\u{0327}', '\u{0323}', '\u{0315}', '\u{0345}',
            '\u{1100}', '\u{1161}', '\u{11A8}', '\u{AC00}', '\u{FB01}', '\u{2026}', '\u{00A0}',
            '\u{FF21}', '\u{304B}', '\u{3099}', '\u{0F73}', '\u{1E0B}',
        ];
        let mut text = String::new();
        let mut digits = [0usize; 4];
        for len in 1..=digits.len() {
            let combinations = ALPHABET.len().pow(u32::try_from(len).unwrap());
            for mut n in 0..combinations {
                text.clear();
                for digit in &mut digits[..len] {
                    *digit = n % ALPHABET.len();
                    n /= ALPHABET.len();
                    text.push(ALPHABET[*digit]);
                }
                let whole: String = text.nfkc().collect();
                assert_eq!(compose(&text), whole, "{}", text.escape_unicode());
            }
        }
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
}
