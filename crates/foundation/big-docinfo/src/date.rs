// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The date that identifies a document, chosen by the words around each date.
//!
//! A contract mentions its signature, its start, its end and the tenant's birth;
//! a bill has an issue date and a due date. Every date in the text is a
//! candidate, and the words just before it decide its role: "vencimento",
//! "due", "validez", "nascimento" push it down; "emissão", "signed", "fecha", a
//! signature line ("Porto Alegre, 22 de abril de 2026.") push it up. The best
//! candidate wins only with a positive score — otherwise no date.
//!
//! Written dates are read in every alphabetic language BigLinux ships (month
//! names from glibc, see `months.rs`), plus the `2026年10月4日` / `2026년 10월 4일`
//! forms of Chinese, Japanese and Korean. In English text a numeric date whose
//! day and month could swap (`04/10/2026`: April in the US, October in the UK) is
//! not a candidate at all.

use std::sync::LazyLock;

use regex::Regex;

use crate::months::MONTHS;
use crate::text::{fold_lower, last_chars, norm};

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,2})[/.-](\d{1,2})[/.-](\d{4})\b").expect("valid"));
static ISO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").expect("valid"));
/// "4 de outubro de 2026", "4. Oktober 2026", "4 października 2026", "a 4 de octubre de 2026".
static DAY_MONTH_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(\d{1,2})\s?[.ºo°]?\s+(?:(?i:de|of)\s+)?(\p{L}+)\.?,?\s+(?:(?i:de|del)\s+)?(\d{4})\b",
    )
    .expect("valid")
});
/// "October 4, 2026", "Oct. 4th 2026".
static MONTH_DAY_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\p{L}+)\.?\s+(\d{1,2})(?i:st|nd|rd|th)?,?\s+(\d{4})\b").expect("valid")
});
/// "2026. október 4." (Hungarian).
static YEAR_MONTH_DAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{4})\.\s+(\p{L}+)\s+(\d{1,2})\b").expect("valid"));
/// "2026年10月4日", "2026년 10월 4일".
static CJK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{4})\s*[年년]\s*(\d{1,2})\s*[月월]\s*(\d{1,2})\s*[日일]").expect("valid")
});
/// Words before a date that make it NOT the document's date (pt, es, en).
static AGAINST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"venc|validade|valid[ao] ate|nasc|inicio|termino|prazo|\bate\b|a partir|periodo|referen|leitura|coleta|vigen|proxim|previst|lancamento|entrega",
        r"|validez|valid[oa] hasta|nacim|nacid|plazo|\bhasta\b|\bfin\b",
        r"|\bdue\b|expir|valid until|valid through|birth|born|\bstart|\bend\b|deadline|\buntil\b|\bthrough\b|effective|renew|\bnext\b|\bperiod\b"
    ))
    .expect("valid")
});
/// Words before a date that make it the document's date (pt, es, en).
static FOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"emiss|emitid|\bdata\b|assinad|reuniao|realizad|publicad|enviad|aprovad|sessao|\bem\b$",
        r"|\bfecha\b|emision|firmad|celebrad|\bsesion\b",
        r"|\bdated?\b|issued|signed|meeting|\bheld\b|published|\bsent\b|approved|session|\bon\b$|\bas of\b|entered into|executed"
    ))
    .expect("valid")
});
/// "Porto Alegre, ", "Lyon, le ", "Москва, ", "北京，" — a place and a comma, alone
/// before the date: a capital in scripts with case, any letter in those without.
static SIGNATURE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // Place length is checked in code (`signature_line`): a counted repeat of
        // a Unicode class compiles into that many copies of it.
        r"^\s*(?:\p{Lu}[\p{L}\p{M} .'/-]{2,},",
        r"|[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}\p{Hebrew}][\p{L}\p{M} ]*[,，、])",
        r"\s*((em|le|den|am|el|on|the|a)\s+)?$"
    ))
    .expect("valid")
});

/// A place of at most 41 characters, a comma, and nothing else before the date.
fn signature_line(before: &str) -> bool {
    let place = before.trim_start();
    SIGNATURE_LINE.is_match(before)
        && place
            .find([',', '，', '、'])
            .is_some_and(|comma| place[..comma].chars().count() <= 41)
}

/// A date found in the text: ISO form and byte offset where it starts.
struct Found {
    iso: String,
    at: usize,
    len: usize,
}

/// Month number for a written name in any shipped language. Hebrew attaches its
/// preposition to the word ("באוקטובר", in October), so one leading letter may go.
fn month(name: &str) -> Option<u32> {
    let folded = fold_lower(name);
    let find = |n: &str| {
        MONTHS
            .binary_search_by(|(m, _)| (*m).cmp(n))
            .ok()
            .map(|i| MONTHS[i].1)
    };
    find(&folded).or_else(|| folded.strip_prefix(['ב', 'ל']).and_then(find))
}

fn candidates(text: &str) -> Vec<Found> {
    // English writes 04/10 both ways; another numeric date of the same document
    // that can only be read one way (03/27 or 29/08) tells which way it is.
    let order = if is_english(text) {
        numeric_order(text)
    } else {
        Some(DayFirst)
    };
    let mut out = Vec::new();
    let mut push = |year: &str, month: u32, day: &str, (at, len): (usize, usize)| {
        if let (Ok(y), Ok(d)) = (year.parse::<u32>(), day.parse::<u32>())
            && (1..=12).contains(&month)
            && (1..=31).contains(&d)
        {
            out.push(Found {
                iso: format!("{y:04}-{month:02}-{d:02}"),
                at,
                len,
            });
        }
    };
    let start = |c: &regex::Captures<'_>| c.get(0).map_or((0, 0), |m| (m.start(), m.len()));
    for c in NUMERIC.captures_iter(text) {
        let (Ok(first), Ok(second)) = (c[1].parse::<u32>(), c[2].parse::<u32>()) else {
            continue;
        };
        let day_first = match order {
            _ if first > 12 => true,
            _ if second > 12 => false,
            Some(DayFirst) => true,
            Some(MonthFirst) => false,
            None => continue, // swappable and nothing in the text settles it
        };
        if day_first {
            push(&c[3], second, &c[1], start(&c));
        } else {
            push(&c[3], first, &c[2], start(&c));
        }
    }
    for c in ISO.captures_iter(text) {
        if let Ok(m) = c[2].parse() {
            push(&c[1], m, &c[3], start(&c));
        }
    }
    for c in DAY_MONTH_YEAR.captures_iter(text) {
        if let Some(m) = month(&c[2]) {
            push(&c[3], m, &c[1], start(&c));
        }
    }
    for c in MONTH_DAY_YEAR.captures_iter(text) {
        if let Some(m) = month(&c[1]) {
            push(&c[3], m, &c[2], start(&c));
        }
    }
    for c in YEAR_MONTH_DAY.captures_iter(text) {
        if let Some(m) = month(&c[2]) {
            push(&c[1], m, &c[3], start(&c));
        }
    }
    for c in CJK.captures_iter(text) {
        if let Ok(m) = c[2].parse() {
            push(&c[1], m, &c[3], start(&c));
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Order {
    DayFirst,
    MonthFirst,
}
use Order::{DayFirst, MonthFirst};

/// The numeric date order the text itself proves, if it proves exactly one.
fn numeric_order(text: &str) -> Option<Order> {
    let (mut day_first, mut month_first) = (false, false);
    for c in NUMERIC.captures_iter(text) {
        let (first, second) = (
            c[1].parse::<u32>().unwrap_or(0),
            c[2].parse::<u32>().unwrap_or(0),
        );
        day_first |= first > 12 && second <= 12;
        month_first |= second > 12 && first <= 12;
    }
    match (day_first, month_first) {
        (true, false) => Some(DayFirst),
        (false, true) => Some(MonthFirst),
        _ => None,
    }
}

/// English text, told by its most common words outnumbering those of Portuguese
/// and Spanish. Only used to refuse day/month-swappable numeric dates.
fn is_english(text: &str) -> bool {
    let sample = norm(crate::text::first_chars(text, 4000));
    let (mut english, mut iberian) = (0, 0);
    for word in sample.split(' ') {
        match word {
            "the" | "and" | "of" | "to" | "is" | "for" | "that" | "with" | "this" | "by" | "be"
            | "are" => {
                english += 1;
            }
            "de" | "que" | "em" | "en" | "el" | "do" | "da" | "os" | "los" | "las" | "para"
            | "con" | "com" | "um" | "una" => {
                iberian += 1;
            }
            _ => {}
        }
    }
    english >= 5 && english > iberian * 2
}

/// The document's own date, or `None` when no candidate scores above zero.
pub fn document_date(text: &str) -> Option<String> {
    let mut best: Option<(i32, f64, String)> = None;
    for found in candidates(text) {
        let before_raw = &text[..found.at];
        let line_start = before_raw.rfind('\n').map_or(0, |i| i + 1);
        let before = norm(last_chars(before_raw, 60));
        let near = last_chars(&before, 35);
        // Position as a share of the text, in bytes: counting characters from the
        // start for every candidate would be quadratic on a long document.
        #[expect(
            clippy::cast_precision_loss,
            reason = "document lengths are far below 2^52 bytes"
        )]
        let place = found.at as f64 / text.len().max(1) as f64;
        let mut score = 0;
        if AGAINST.is_match(near) {
            score -= 3;
        }
        if FOR.is_match(near) {
            score += 2;
        }
        if signature_line(&text[line_start..found.at]) {
            score += 3;
        }
        let line_end = text[found.at..]
            .find('\n')
            .map_or(text.len(), |i| found.at + i);
        if text[line_start..found.at].trim().is_empty()
            && text[found.at + found.len..line_end]
                .trim_matches(|c: char| c.is_whitespace() || c == '.')
                .is_empty()
        {
            score += 2; // a date alone on its line heads a letter
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

/// Tuple order: score, then position, then the ISO string.
fn greater(a: &(i32, f64, String), b: &(i32, f64, String)) -> bool {
    match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => match a.1.partial_cmp(&b.1) {
            Some(std::cmp::Ordering::Equal) | None => a.2 > b.2,
            Some(order) => order == std::cmp::Ordering::Greater,
        },
        order => order == std::cmp::Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `month` binary-searches the generated table.
    #[test]
    fn month_table_is_sorted_without_repeats() {
        assert!(MONTHS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn written_dates_in_shipped_languages() {
        for (text, iso) in [
            ("Porto Alegre, 22 de abril de 2026.", "2026-04-22"),
            ("Boston, October 4, 2026", "2026-10-04"),
            ("Gdańsk, 4 października 2026", "2026-10-04"),
            ("Москва, 4 октября 2026 г.", "2026-10-04"),
            ("Helsinki, 4. lokakuuta 2026", "2026-10-04"),
            ("Budapest, 2026. október 4.", "2026-10-04"),
            ("תל אביב, 4 באוקטובר 2026", "2026-10-04"),
            ("北京，2026年10月4日", "2026-10-04"),
            ("서울, 2026년 10월 4일", "2026-10-04"),
        ] {
            assert_eq!(document_date(text).as_deref(), Some(iso), "{text}");
        }
    }

    /// In English "04/10/2026" is April or October depending on the country.
    #[test]
    fn english_swappable_numeric_dates_are_not_guessed() {
        let us = "This agreement is made by and between the parties for the lease of the apartment. Signed on 04/10/2026.";
        assert_eq!(document_date(us), None);
        let unambiguous = "This agreement is made by and between the parties for the lease of the apartment. Signed on 10/24/2026.";
        assert_eq!(document_date(unambiguous).as_deref(), Some("2026-10-24"));
    }
}
