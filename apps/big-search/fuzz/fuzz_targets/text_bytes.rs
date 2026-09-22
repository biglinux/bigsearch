#![no_main]
//! Fuzz the plain-text decode path: NUL/binary rejection + lossy-UTF-8 decode.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = big_search::extract::text_from_bytes(data.to_vec());
});
