// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! An e-mail message saved as a file (`.eml`, RFC 5322 with MIME): its headers
//! and the text of its body, decoded.
//!
//! The text keeps the headers a reader looks at — Subject, From, To, Date — as
//! lines, then the body: the plain-text part when there is one, else the HTML
//! part read as text. Base64 and quoted-printable bodies and RFC 2047 encoded
//! headers (`=?UTF-8?B?…?=`) are decoded; attachments are left out. The subject
//! is the message's own title.

/// Parts read from nested `multipart/*`, against a message built to recurse.
const MAX_DEPTH: usize = 6;

/// The text of a message and its subject.
///
/// The message is read as Latin-1 — one character per byte, nothing lost — so it
/// can be split into headers and parts before each body is decoded by its own
/// charset.
pub(crate) fn read_eml(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let message: String = bytes.iter().map(|&b| char::from(b)).collect();
    let message = message.replace("\r\n", "\n");
    let (headers, body) = split_message(&message);
    let header = |name: &str| header_value(headers, name).map(|v| decode_words(&raw_text(&v)));
    let subject = header("subject").filter(|s| !s.trim().is_empty());
    let mut text = String::new();
    for (label, value) in [
        ("Subject", subject.clone()),
        ("From", header("from")),
        ("To", header("to")),
        ("Date", header("date")),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("{label}: {}\n", value.trim()));
        }
    }
    text.push('\n');
    text.push_str(&body_text(headers, body, 0).unwrap_or_default());
    (!text.trim().is_empty()).then_some((text, subject))
}

/// Headers and body: they part at the first empty line.
fn split_message(message: &str) -> (&str, &str) {
    message.split_once("\n\n").unwrap_or((message, ""))
}

/// The value of header `name` (case-insensitive), folded lines joined.
fn header_value(headers: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in headers.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(v) = value.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if value.is_some() {
            break;
        }
        if let Some((key, rest)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case(name)
        {
            value = Some(rest.trim().to_owned());
        }
    }
    value
}

/// A parameter of a header value: `boundary` of `multipart/mixed; boundary="x"`.
fn parameter(value: &str, name: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|part| {
        let (key, v) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// The readable text of a body: a plain-text part first, else an HTML one.
fn body_text(headers: &str, body: &str, depth: usize) -> Option<String> {
    let content_type = header_value(headers, "content-type").unwrap_or_else(|| "text/plain".into());
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let disposition = header_value(headers, "content-disposition").unwrap_or_default();
    if disposition
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("attachment")
    {
        return None;
    }
    if kind.starts_with("multipart/") {
        if depth == MAX_DEPTH {
            return None;
        }
        let boundary = parameter(&content_type, "boundary")?;
        let delimiter = format!("--{boundary}");
        let parts: Vec<(&str, &str)> = body
            .split(delimiter.as_str())
            .skip(1)
            .take_while(|part| !part.starts_with("--"))
            .map(|part| split_message(part.strip_prefix('\n').unwrap_or(part)))
            .collect();
        // An alternative's plain text and its HTML say the same: keep one.
        let plain = parts
            .iter()
            .find(|(h, _)| content_kind(h) == "text/plain")
            .and_then(|(h, b)| body_text(h, b, depth + 1));
        if kind == "multipart/alternative" {
            return plain.or_else(|| parts.iter().find_map(|(h, b)| body_text(h, b, depth + 1)));
        }
        let texts: Vec<String> = parts
            .iter()
            .filter_map(|(h, b)| body_text(h, b, depth + 1))
            .collect();
        return (!texts.is_empty()).then(|| texts.join("\n\n"));
    }
    if kind != "text/plain" && kind != "text/html" {
        return None;
    }
    let encoding = header_value(headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let raw = match encoding.trim() {
        "base64" => base64(body),
        "quoted-printable" => quoted_printable(&bytes_of(body)),
        _ => bytes_of(body),
    };
    let text = decode_charset(&raw, parameter(&content_type, "charset").as_deref());
    Some(if kind == "text/html" {
        crate::extract::html_text(&text)
    } else {
        text
    })
}

fn content_kind(headers: &str) -> String {
    header_value(headers, "content-type")
        .unwrap_or_else(|| "text/plain".into())
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The bytes a Latin-1 reading stands for.
fn bytes_of(latin1: &str) -> Vec<u8> {
    latin1
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// A header written raw in UTF-8, as many clients do, read back as UTF-8.
fn raw_text(latin1: &str) -> String {
    String::from_utf8(bytes_of(latin1)).unwrap_or_else(|_| latin1.to_owned())
}

/// Bytes in a charset as text: UTF-8 (lossy), else Latin-1 / Windows-1252,
/// byte for character — what old mail clients send.
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let latin = charset.is_some_and(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("8859") || c.contains("1252") || c == "latin1"
    });
    if latin {
        bytes.iter().map(|&b| char::from(b)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Longest encoded word read. RFC 2047 allows 75 characters, but some mailers
/// write a whole subject as one word; RFC 5322 still ends a line at 998. The
/// limit keeps each search for "?=" local, so a header of unterminated "=?"
/// costs linear time.
const MAX_ENCODED_WORD: usize = 998;

/// RFC 2047 encoded words in a header: "=?UTF-8?B?w4l…?=", "=?iso-8859-1?Q?Ol=E1?=".
fn decode_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut after_word = false;
    while let Some(start) = rest.find("=?") {
        let decoded = (|| {
            let mut end = (start + MAX_ENCODED_WORD).min(rest.len());
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let inner = &rest[start + 2..end];
            let (charset, inner) = inner.split_once('?')?;
            let (encoding, inner) = inner.split_once('?')?;
            let (text, _) = inner.split_once("?=")?;
            let bytes = match encoding.to_ascii_lowercase().as_str() {
                "b" => base64(text),
                "q" => quoted_printable(text.replace('_', " ").as_bytes()),
                _ => return None,
            };
            let consumed = 2 + charset.len() + 1 + encoding.len() + 1 + text.len() + 2;
            Some((decode_charset(&bytes, Some(charset)), consumed))
        })();
        let Some((word, consumed)) = decoded else {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            after_word = false;
            continue;
        };
        // Whitespace between two encoded words is not part of the text.
        let gap = &rest[..start];
        if !(after_word && gap.trim().is_empty()) {
            out.push_str(gap);
        }
        out.push_str(&word);
        rest = &rest[start + consumed..];
        after_word = true;
    }
    out.push_str(rest);
    out
}

/// Base64 decoded, ignoring line breaks and anything outside the alphabet.
fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for v in text.bytes().filter_map(value) {
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Quoted-printable decoded: "=E1" a byte, "=" at a line's end a soft break.
fn quoted_printable(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multipart_message_reads_as_headers_and_plain_text() {
        let eml = "From: =?UTF-8?B?Sm/Do28=?= <joao@example.org>\r\n\
To: maria@example.org\r\n\
Subject: =?iso-8859-1?Q?Or=E7amento_da?=\r\n =?UTF-8?Q?_reforma?=\r\n\
Date: Tue, 6 Oct 2026 10:15:00 -0300\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
--b1\r\nContent-Type: multipart/alternative; boundary=b2\r\n\r\n\
--b2\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
Segue o or=C3=A7amento, com valores =\r\natualizados.\r\n\
--b2\r\nContent-Type: text/html\r\n\r\n<p>Segue o <b>or&ccedil;amento</b></p>\r\n--b2--\r\n\
--b1\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n--b1--\r\n";
        let (text, subject) = read_eml(eml.as_bytes()).expect("text");
        assert_eq!(subject.as_deref(), Some("Orçamento da reforma"));
        assert!(
            text.starts_with("Subject: Orçamento da reforma\nFrom: João <joao@example.org>\n"),
            "{text}"
        );
        assert!(text.contains("Date: Tue, 6 Oct 2026 10:15:00 -0300"));
        assert!(
            text.contains("Segue o orçamento, com valores atualizados."),
            "{text}"
        );
        assert!(!text.contains("<p>") && !text.contains("JVBER"), "{text}");
    }

    #[test]
    fn base64_html_bodies_and_latin1_text_decode() {
        let eml = "Subject: Aviso\nContent-Type: text/html; charset=utf-8\nContent-Transfer-Encoding: base64\n\nPHA+UmV1bmnDo288L3A+\n";
        let (text, _) = read_eml(eml.as_bytes()).expect("text");
        assert!(text.contains("Reunião"), "{text}");
        let latin = b"Subject: Ola\nContent-Type: text/plain; charset=ISO-8859-1\n\nRelat\xf3rio\n";
        assert!(read_eml(latin).expect("text").0.contains("Relatório"));
    }

    #[test]
    fn overlong_encoded_word_stays_verbatim() {
        let word = format!("=?utf-8?q?{}?=", "a".repeat(MAX_ENCODED_WORD));
        assert_eq!(decode_words(&word), word);
        assert_eq!(decode_words("=?utf-8?q?Ol=C3=A1?="), "Olá");
    }
}
