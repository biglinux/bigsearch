#![no_main]
//! Fuzz the HTML reader used for web pages, EPUB chapters and mail parts: tags,
//! entities, unclosed script and style. It must not panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(html) = std::str::from_utf8(data) {
        let _ = big_search::extract::html_text(html);
    }
});
