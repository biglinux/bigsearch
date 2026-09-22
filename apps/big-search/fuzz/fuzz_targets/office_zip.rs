#![no_main]
//! Fuzz the OOXML/ODF extractor with arbitrary bytes as the zip archive. Exercises
//! the zip crate plus our entry selection / XML walk on malformed or hostile
//! documents — the primary untrusted-input surface for a search daemon.
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    for ext in ["docx", "odt", "ods", "odp", "pptx", "xlsx"] {
        let _ = big_search::extract::office_text_from_reader(Cursor::new(data), ext);
    }
});
