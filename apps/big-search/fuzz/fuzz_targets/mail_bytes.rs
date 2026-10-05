#![no_main]
//! Fuzz the reader of an e-mail message: MIME parts, base64, quoted-printable, encoded headers. It must not panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = big_search::extract::mail_text(data);
});
