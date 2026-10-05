#![no_main]
//! Fuzz the reader of an RTF document: groups, control words, hex and Unicode escapes. It must not panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = big_search::extract::rtf_text(data);
});
