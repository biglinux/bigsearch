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

#[cfg(test)]
mod tests {
    use super::*;

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
