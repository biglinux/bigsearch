#![no_main]
//! Fuzz the document reader with arbitrary text: it must not panic, and the name
//! it suggests must stay a safe single path component with the extension kept.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let info = big_docinfo::analyze(text, "pdf");
    let name = &info.suggested_name;
    assert!(!name.contains(['/', '\0']), "unsafe name {name:?}");
    assert!(name.ends_with(".pdf") && name.len() > ".pdf".len(), "extension lost in {name:?}");
    assert!(name.chars().count() <= 70, "name too long: {name:?}");
});
