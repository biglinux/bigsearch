# Fuzz targets

Coverage-guided fuzzing (libFuzzer via `cargo-fuzz`) for the untrusted-input
surface: content extraction from documents in the indexed tree.

Requires a **nightly** toolchain (libFuzzer needs `-Zsanitizer`); the crate root
pins stable, so always invoke through the `+nightly` proxy:

```sh
cargo +nightly fuzz build
cargo +nightly fuzz run xml_text     -- -max_total_time=300
cargo +nightly fuzz run office_zip   -- -max_total_time=300
cargo +nightly fuzz run text_bytes   -- -max_total_time=300
```

| Target | Exercises |
|--------|-----------|
| `xml_text`   | `extract::xml_text` — the quick-xml character-data walk |
| `office_zip` | `extract::office_text_from_reader` — zip parsing + entry selection + XML walk on arbitrary bytes as the archive (the primary hostile-document path) |
| `text_bytes` | `extract::text_from_bytes` — NUL/binary rejection + lossy-UTF-8 decode |

Each asserts the parser never panics, hangs, or over-allocates. `extract.rs` caps
the text taken from one file; the harness checks the cap holds for any input.

Seed `office_zip` with real `.docx`/`.odt`/`.pptx` files in
`corpus/office_zip/` to reach deep into the XML walk faster (random bytes rarely
form a valid zip header).
