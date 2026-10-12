# Fuzz targets

Coverage-guided fuzzing (libFuzzer through `cargo-fuzz`) of the untrusted-input
surface: extracting text from the documents in the indexed tree.

libFuzzer needs `-Zsanitizer`, so the targets build on a nightly toolchain. The
repository's `rust-toolchain.toml` pins a stable one, so always go through the
`+nightly` proxy:

```sh
cargo +nightly fuzz build
cargo +nightly fuzz run xml_text     -- -max_total_time=300
cargo +nightly fuzz run office_zip   -- -max_total_time=300
cargo +nightly fuzz run text_bytes   -- -max_total_time=300
cargo +nightly fuzz run mail_bytes   -- -max_total_time=300
cargo +nightly fuzz run rtf_bytes    -- -max_total_time=300
cargo +nightly fuzz run html_text    -- -max_total_time=300 -max_len=1048576
cargo +nightly fuzz run docinfo_text -- -max_total_time=300
```

| Target | Exercises |
|--------|-----------|
| `xml_text`   | `extract::xml_text` and `extract::xml_text_lines`: the quick-xml character-data walks |
| `office_zip` | `extract::office_text_from_reader`: zip parsing, entry selection and the XML walk, with arbitrary bytes as the archive (the main hostile-document path) |
| `text_bytes` | `extract::text_from_bytes`: NUL/binary rejection and lossy UTF-8 decoding |
| `mail_bytes` | `extract::mail_text`: an `.eml` message with MIME parts, base64, quoted-printable and encoded headers |
| `rtf_bytes` | `extract::rtf_text`: an RTF document with groups, control words, hex and Unicode escapes |
| `html_text` | `extract::html_text`: the HTML reader behind web pages, EPUB chapters and mail parts |
| `docinfo_text` | `big_docinfo::analyze`: the suggested name stays one safe path component, keeps its extension and is bounded in length |

The extraction targets only run the parser; libFuzzer reports a panic, an
input slower than `-timeout` (default 1200 s) and memory above `-rss_limit_mb`
(default 2048) as failures. Lower both to catch quadratic work and
over-allocation, for example `-timeout=10 -rss_limit_mb=512`, and raise
`-max_len` (4096 bytes for an empty corpus), which otherwise keeps inputs too
small to show either. No target checks the extraction cap. No target opens an
EPUB archive; `html_text` covers the reader its chapters go through.

Seed `office_zip` with real `.docx`, `.odt` and `.pptx` files in
`corpus/office_zip/` to reach deep into the XML walk sooner: random bytes rarely
form a valid zip header.
