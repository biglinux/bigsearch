// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The date that identifies a document, chosen by the words around each date.
//!
//! A contract mentions its signature, its start, its end and the tenant's birth;
//! a bill has an issue date and a due date. Every date in the text is a
//! candidate, and the words just before it decide its role: "vencimento",
//! "validade", "nascimento" push it down; "emissão", "assinado", "reunião" and a
//! signature line ("Porto Alegre, 22 de abril de 2026.") push it up. The best
//! candidate wins only with a positive score — otherwise no date.

use std::sync::LazyLock;

use regex::Regex;

use crate::text::{last_chars, norm};

const MONTHS: &[(&str, u32)] = &[
    ("janeiro", 1),
    ("fevereiro", 2),
    ("marco", 3),
    ("abril", 4),
    ("maio", 5),
    ("junho", 6),
    ("julho", 7),
    ("agosto", 8),
    ("setembro", 9),
    ("outubro", 10),
    ("novembro", 11),
    ("dezembro", 12),
    ("enero", 1),
    ("febrero", 2),
    ("marzo", 3),
    ("mayo", 5),
    ("junio", 6),
    ("julio", 7),
    ("septiembre", 9),
    ("octubre", 10),
    ("noviembre", 11),
    ("diciembre", 12),
    ("setiembre", 9),
];

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,2})[/.-](\d{1,2})[/.-](\d{4})\b").expect("valid"));
static ISO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").expect("valid"));
static WRITTEN: LazyLock<Regex> = LazyLock::new(|| {
    let names: Vec<&str> = MONTHS
        .iter()
        .map(|&(m, _)| if m == "marco" { "mar[cç]o" } else { m })
        .collect();
    let pattern = format!(
        r"(?i)\b(\d{{1,2}})\s?[ºo°]?\s+de\s+({})\s+de\s+(\d{{4}})\b",
        names.join("|")
    );
    Regex::new(&pattern).expect("valid")
});
/// Words before a date that make it NOT the document's date.
static AGAINST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"venc|validade|valid[ao] ate|nasc|inicio|termino|prazo|\bate\b|a partir|periodo|referen|leitura|coleta|vigen|proxim|previst|lancamento|entrega",
    )
    .expect("valid")
});
/// Words before a date that make it the document's date.
static FOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"emiss|emitid|\bdata\b|assinad|reuniao|realizad|publicad|enviad|aprovad|sessao|\bem\b$",
    )
    .expect("valid")
});
/// "Porto Alegre, " — a place and a comma, alone on the line before the date.
static SIGNATURE_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[A-ZÀ-Ý][\wÀ-ÿ .'/-]{2,40},\s*(em\s+)?$").expect("valid"));

/// A date found in the text: ISO form and byte offset where it starts.
struct Found {
    iso: String,
    at: usize,
}

fn candidates(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut push = |year: &str, month: u32, day: &str, at: usize| {
        if let (Ok(y), Ok(d)) = (year.parse::<u32>(), day.parse::<u32>())
            && (1..=12).contains(&month)
            && (1..=31).contains(&d)
        {
            out.push(Found {
                iso: format!("{y:04}-{month:02}-{d:02}"),
                at,
            });
        }
    };
    for c in NUMERIC.captures_iter(text) {
        if let Ok(month) = c[2].parse() {
            push(&c[3], month, &c[1], c.get(0).map_or(0, |m| m.start()));
        }
    }
    for c in ISO.captures_iter(text) {
        if let Ok(month) = c[2].parse() {
            push(&c[1], month, &c[3], c.get(0).map_or(0, |m| m.start()));
        }
    }
    for c in WRITTEN.captures_iter(text) {
        let name = norm(&c[2]);
        if let Some(&(_, month)) = MONTHS.iter().find(|(m, _)| *m == name) {
            push(&c[3], month, &c[1], c.get(0).map_or(0, |m| m.start()));
        }
    }
    out
}

/// The document's own date, or `None` when no candidate scores above zero.
pub fn document_date(text: &str) -> Option<String> {
    let total_chars = text.chars().count().max(1);
    let mut best: Option<(i32, f64, String)> = None;
    for found in candidates(text) {
        let before_raw = &text[..found.at];
        let char_pos = before_raw.chars().count();
        let line_start = before_raw.rfind('\n').map_or(0, |i| i + 1);
        let before = norm(last_chars(before_raw, 60));
        let near = last_chars(&before, 35);
        #[expect(
            clippy::cast_precision_loss,
            reason = "document lengths are far below 2^52 characters"
        )]
        let place = char_pos as f64 / total_chars as f64;
        let mut score = 0;
        if AGAINST.is_match(near) {
            score -= 3;
        }
        if FOR.is_match(near) {
            score += 2;
        }
        if SIGNATURE_LINE.is_match(&text[line_start..found.at]) {
            score += 3;
        }
        if !(0.15..=0.85).contains(&place) {
            score += 1;
        }
        let candidate = (score, place, found.iso);
        if best.as_ref().is_none_or(|b| greater(&candidate, b)) {
            best = Some(candidate);
        }
    }
    best.filter(|(score, _, _)| *score >= 1)
        .map(|(_, _, iso)| iso)
}

/// Tuple order of the reference: score, then position, then the ISO string.
fn greater(a: &(i32, f64, String), b: &(i32, f64, String)) -> bool {
    match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => match a.1.partial_cmp(&b.1) {
            Some(std::cmp::Ordering::Equal) | None => a.2 > b.2,
            Some(order) => order == std::cmp::Ordering::Greater,
        },
        order => order == std::cmp::Ordering::Greater,
    }
}
