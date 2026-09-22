#![no_main]
//! Fuzz the XML character-data extractor (quick-xml path). Arbitrary input must
//! never panic, hang unboundedly, or over-allocate.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The bytes as they come, valid UTF-8 or not: the invalid case is a real
    // branch of the decoder and gating on `from_utf8` would never reach it.
    let mut out = String::new();
    big_search::extract::xml_text(data, &mut out);
});
