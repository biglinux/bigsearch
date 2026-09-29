//! Body tokenizer that makes space-less-script content searchable without a
//! segmentation dictionary — CJK (Han / Kana / Hangul) plus the SE-Asian
//! abugidas (Thai / Lao / Khmer / Myanmar). Those scripts write without inter-
//! word spaces, so a plain word tokenizer indexes a whole sentence as one
//! (often over-long, dropped) token. Here such a run emits **overlapping
//! bigrams** (中文内容 → 中文, 文内, 内容), so a query of ≥2 chars matches by
//! substring — the same effect the ngram name field already gives. An isolated
//! char is a unigram. Everything else is split into alphanumeric words exactly
//! like `SimpleTokenizer`, so Latin / Cyrillic / Hebrew / Arabic / etc. (all
//! space-separated) behave unchanged.
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Scripts written without inter-word spaces — Han/kana/Hangul plus the
/// space-less SE-Asian abugidas — get bigram tokenization. Punctuation and
/// symbols are treated as separators (not folded into runs).
fn is_unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0E7F      // Thai
        | 0x0E80..=0x0EFF    // Lao
        | 0x1000..=0x109F    // Myanmar (Burmese)
        | 0x1780..=0x17FF    // Khmer
        | 0x3400..=0x4DBF    // CJK Ext A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        | 0x20000..=0x2A6DF  // CJK Ext B
        | 0x3040..=0x309F    // Hiragana
        | 0x30A0..=0x30FF    // Katakana
        | 0xAC00..=0xD7AF,   // Hangul syllables
    )
}

#[cfg(test)]
fn mk(text: String, from: usize, to: usize, position: usize) -> Token {
    Token {
        offset_from: from,
        offset_to: to,
        position,
        text,
        position_length: 1,
    }
}

/// The reference implementation, kept only for the tests.
///
/// It builds every token of the text up front, which is what the streaming
/// version replaced: a 32 MiB body cost 925 MiB of resident memory and 1.6 s
/// here. It stays as the thing the streaming tokenizer is proved equal to,
/// token for token, offset for offset.
#[cfg(test)]
fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut tokens = Vec::new();
    let mut position = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let (off, c) = chars[i];
        if is_unspaced(c) {
            // Collect the whole CJK run.
            let mut j = i;
            while j < chars.len() && is_unspaced(chars[j].1) {
                j += 1;
            }
            let run = &chars[i..j];
            if run.len() == 1 {
                // An isolated CJK char → unigram (so a 1-char query can match it).
                tokens.push(mk(c.to_string(), off, off + c.len_utf8(), position));
                position += 1;
            } else {
                // Overlapping bigrams only — NO trailing unigram. A query of ≥2
                // chars tokenizes to the same bigrams the doc holds; a trailing
                // unigram would add a Must clause for a standalone char the doc
                // only carries inside a bigram, killing the match.
                for pair in run.windows(2) {
                    let (b0, c0) = pair[0];
                    let (_, c1) = pair[1];
                    let to = pair[1].0 + c1.len_utf8();
                    tokens.push(mk([c0, c1].iter().collect(), b0, to, position));
                    position += 1;
                }
            }
            i = j;
        } else if c.is_alphanumeric() {
            // A non-CJK alphanumeric word run (stops at the next CJK char or
            // non-alphanumeric), mirroring SimpleTokenizer.
            let start = off;
            let mut word = String::new();
            while i < chars.len() {
                let (_, cj) = chars[i];
                if is_unspaced(cj) || !cj.is_alphanumeric() {
                    break;
                }
                word.push(cj);
                i += 1;
            }
            let end = chars.get(i).map_or(text.len(), |&(b, _)| b);
            tokens.push(mk(word, start, end, position));
            position += 1;
        } else {
            i += 1; // separator
        }
    }
    tokens
}

/// One token at a time, holding nothing but the cursor and the token itself.
///
/// The whole point is what is *not* here: no vector of every character in the
/// document, and no `String` per token. A 32 MiB body used to need about 925 MiB
/// of memory to be tokenized, on a service whose whole budget is 63 MiB.
///
/// The cursor is a byte offset rather than a character iterator because most
/// bodies are mostly ASCII: a separator or a letter below 0x80 is decided from
/// its byte, and a word is copied into the token as one slice. Decoding and
/// pushing every character one by one made this the hottest function of a
/// content pass.
pub struct CjkTokenStream<'a> {
    text: &'a str,
    /// Byte offset of the next character to read; always a character boundary.
    cursor: usize,
    /// The last character of the space-less run being read, and whether that run
    /// has already produced a bigram. A run of one character owes a unigram; a
    /// longer one must not get a trailing unigram, which would add a clause no
    /// document can satisfy.
    run: Option<(usize, char, bool)>,
    position: usize,
    token: Token,
}

impl CjkTokenStream<'_> {
    fn emit(&mut self, from: usize, to: usize) {
        self.token.offset_from = from;
        self.token.offset_to = to;
        self.token.position = self.position;
        self.position += 1;
    }

    fn char_at(&self, offset: usize) -> Option<char> {
        self.text[offset..].chars().next()
    }

    /// Where the word whose remaining characters start at `from` ends: the next
    /// space-less or non-alphanumeric character, or the end of the text.
    fn word_end(&self, from: usize) -> usize {
        let bytes = self.text.as_bytes();
        let mut end = from;
        while let Some(&byte) = bytes.get(end) {
            if byte.is_ascii() {
                if !byte.is_ascii_alphanumeric() {
                    break;
                }
                end += 1;
                continue;
            }
            match self.char_at(end) {
                Some(next) if !is_unspaced(next) && next.is_alphanumeric() => {
                    end += next.len_utf8();
                }
                _ => break,
            }
        }
        end
    }
}

impl TokenStream for CjkTokenStream<'_> {
    fn advance(&mut self) -> bool {
        loop {
            if let Some((offset, current, emitted)) = self.run {
                match self.char_at(self.cursor) {
                    Some(next) if is_unspaced(next) => {
                        let next_offset = self.cursor;
                        self.cursor += next.len_utf8();
                        self.run = Some((next_offset, next, true));
                        self.token.text.clear();
                        self.token.text.push(current);
                        self.token.text.push(next);
                        self.emit(offset, self.cursor);
                        return true;
                    }
                    _ => {
                        self.run = None;
                        if emitted {
                            continue;
                        }
                        self.token.text.clear();
                        self.token.text.push(current);
                        self.emit(offset, offset + current.len_utf8());
                        return true;
                    }
                }
            }
            let offset = self.cursor;
            let Some(&byte) = self.text.as_bytes().get(offset) else {
                return false;
            };
            if byte.is_ascii() && !byte.is_ascii_alphanumeric() {
                self.cursor += 1;
                continue; // separator
            }
            let Some(current) = self.char_at(offset) else {
                return false;
            };
            self.cursor += current.len_utf8();
            if is_unspaced(current) {
                self.run = Some((offset, current, false));
                continue;
            }
            if !current.is_alphanumeric() {
                continue; // separator
            }
            // Where the word ends is where the next character begins, or the end
            // of the text — the same byte offset the old tokenizer reported.
            let end = self.word_end(self.cursor);
            self.cursor = end;
            self.token.text.clear();
            self.token.text.push_str(&self.text[offset..end]);
            self.emit(offset, end);
            return true;
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// Body analyzer base tokenizer: CJK-bigram aware, word-splitting otherwise.
#[derive(Clone, Default)]
pub struct CjkFriendlyTokenizer;

impl Tokenizer for CjkFriendlyTokenizer {
    type TokenStream<'a> = CjkTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CjkTokenStream {
            text,
            cursor: 0,
            run: None,
            position: 0,
            token: Token {
                offset_from: 0,
                offset_to: 0,
                position: 0,
                text: String::with_capacity(16),
                position_length: 1,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streamed(text: &str) -> Vec<Token> {
        let mut tokenizer = CjkFriendlyTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().clone());
        }
        out
    }

    fn texts(text: &str) -> Vec<String> {
        streamed(text).into_iter().map(|t| t.text).collect()
    }

    /// The streaming tokenizer and the one it replaced must agree completely:
    /// same texts, same byte offsets, same positions. An offset that moves by
    /// one byte is a highlight pointing at the wrong character; a position that
    /// moves breaks phrase queries.
    #[test]
    fn the_stream_says_exactly_what_the_old_tokenizer_said() {
        let corpus = [
            "",
            " ",
            "hello world",
            "relatorio_final-2026.txt",
            "avaliação neuropsicológica às três",
            "Привет мир",
            "مرحبا بالعالم",
            "שלום עולם",
            "中文内容",
            "文",
            "文 abc",
            "报告report2026中文",
            "日本語のテキスト",
            "한국어 문서",
            "ทดสอบภาษาไทย",
            "ພາສາລາວ",
            "ភាសាខ្មែរ",
            "မြန်မာဘာသာ",
            "emoji 🙂 e pontuação!!! ...",
            "a\tb\nc\r\nd",
            "中a文b内c容",
            "  中文  english  中文  ",
        ];
        for text in corpus {
            let expected = tokenize(text);
            let got = streamed(text);
            assert_eq!(got.len(), expected.len(), "count for {text:?}");
            for (got, expected) in got.iter().zip(expected.iter()) {
                assert_eq!(got.text, expected.text, "text for {text:?}");
                assert_eq!(got.offset_from, expected.offset_from, "from for {text:?}");
                assert_eq!(got.offset_to, expected.offset_to, "to for {text:?}");
                assert_eq!(got.position, expected.position, "position for {text:?}");
                assert_eq!(
                    got.position_length, expected.position_length,
                    "length for {text:?}"
                );
            }
        }
    }

    /// Long input, every script mixed, checked the same way: a bug in the run
    /// bookkeeping only shows up where runs meet each other repeatedly.
    #[test]
    fn the_two_agree_on_a_long_mixed_body() {
        let mut text = String::new();
        for round in 0..2000 {
            text.push_str("contrato 2026 中文内容 relatório ทดสอบ 文 ");
            text.push_str(&round.to_string());
            text.push_str("日本語!! ");
        }
        let expected = tokenize(&text);
        let got = streamed(&text);
        assert_eq!(got.len(), expected.len());
        assert!(got.len() > 10_000, "corpus too small: {}", got.len());
        for (got, expected) in got.iter().zip(expected.iter()) {
            assert_eq!(
                (&got.text, got.offset_from, got.offset_to, got.position),
                (
                    &expected.text,
                    expected.offset_from,
                    expected.offset_to,
                    expected.position
                )
            );
        }
    }

    #[test]
    fn cjk_run_yields_overlapping_bigrams() {
        assert_eq!(texts("中文内容"), ["中文", "文内", "内容"]);
    }

    #[test]
    fn latin_and_hebrew_split_into_words_unchanged() {
        assert_eq!(texts("hello world"), ["hello", "world"]);
        assert_eq!(texts("שלום עולם"), ["שלום", "עולם"]);
    }

    #[test]
    fn mixed_script_separates_cjk_from_words() {
        // "报告report" → CJK bigram then the Latin word.
        assert_eq!(texts("报告report"), ["报告", "report"]);
    }

    #[test]
    fn single_cjk_char_is_a_unigram() {
        assert_eq!(texts("文 abc"), ["文", "abc"]);
    }

    #[test]
    fn thai_run_is_bigrammed() {
        // Space-less Thai → overlapping bigrams (substring-searchable).
        assert_eq!(texts("ทดสอบ"), ["ทด", "ดส", "สอ", "อบ"]);
    }

    #[test]
    fn full_body_analyzer_keeps_cjk_bigrams() {
        use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RemoveLongFilter, TextAnalyzer};
        let mut analyzer = TextAnalyzer::builder(CjkFriendlyTokenizer)
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build();
        let mut stream = analyzer.token_stream("这是中文");
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        assert!(
            out.contains(&"这是".to_string()),
            "filters dropped CJK: {out:?}"
        );
    }
}
