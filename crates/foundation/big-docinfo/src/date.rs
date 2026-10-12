// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The date that identifies a document, chosen by the words around each date.
//!
//! A contract mentions its signature, its start, its end and the tenant's birth;
//! a bill has an issue date and a due date. Every date in the text is a
//! candidate, and the words just before it on its line decide its role:
//! "vencimento", "due", "validez", "nascimento", a law it cites or a letter it
//! answers push it down; "emissão", "signed", "fecha", a weekday dateline, a
//! signature line ("Porto Alegre, 22 de abril de 2026.") or the formal "aos onze
//! dias do mês de maio…" push it up. The best candidate wins only with a positive
//! score, the earliest among equals; otherwise the first electronic signature
//! stamp, if any, dates the document.
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
/// The formal date of minutes, deeds and declarations: "aos 25 dias de fevereiro
/// de 2025", "aos onze dias do mês de maio do ano de dois mil e dezoito", "a los
/// veinte días del mes de marzo de", "this 12th day of March, 2026". The year
/// after the match is read in code, in digits or in words.
static DAYS_OF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // Day words are Latin letters ("vinte e um", "veintidós"); a small class,
        // bounded, keeps the automaton small and the scan after "this" short.
        r"(?i:\baos|\ba los|\bthis|\bon the)\s+((?:[a-zA-Z\u{C0}-\u{FF}\d]+[ -]){0,4}?[a-zA-Z\u{C0}-\u{FF}\d]+?)(?i:st|nd|rd|th)?\s+",
        r"(?i:dias?|días?|day)\s+(?i:do\s+m[eê]s\s+de|del\s+mes\s+de|de|of)\s+(\p{L}+),?\s+",
        r"(?i:do\s+ano\s+de|del\s+a[ñn]o|de|of)?\s*"
    ))
    .expect("valid")
});
/// Words before a date that make it NOT the document's date (pt, es, en).
static AGAINST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"venc|validade|valid[ao] ate|nasc|\bdn\b|inicio|termino|prazo|\bate\b|a partir|periodo|referen|leitura|coleta|vigen|proxim|previst|lancamento|entrega",
        r"|validez|valid[oa] hasta|nacim|nacid|plazo|\bhasta\b|\bfin\b",
        r"|\bdue\b|expir|valid until|valid through|birth|born|\bstart|\bend\b|deadline|\buntil\b|\bthrough\b|effective|renew|\bnext\b|\bperiod\b",
        // Another document the text answers or amends: its date is not this one's.
        r"|original|de origem|anterior|\bsu[ae]? (carta|oficio|escrito|solicitud|mensagem|e ?mail)|solicitud",
        r"|\byour\b|previous|\bprior\b|preceding|in response|beginning|commenc|launch"
    ))
    .expect("valid")
});
/// An article's history line ("Submitted …; accepted for publication …"): its
/// dates date the article only when nothing better does, whatever "em" or
/// "publication" says next to them.
static HISTORY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"submet|submitted|received|recebid|recibid|aceit[oa]|accepted|aceptad[oa]")
        .expect("valid")
});
/// Words before a date that make it the document's date (pt, es, en).
static FOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"emiss|emitid|\bdata\b|assinad|reuniao|realizad|publicad|enviad|aprovad|sessao|\bem\b$",
        r"|\bfecha\b|emision|firmad|celebrad|\bsesion\b",
        r"|\bdated?\b|issued|signed|meeting|\bheld\b|published|\bsent\b|approved|session|\bon\b$|\bas of\b|entered into|executed",
        // A weekday heads the dateline of gazettes, letters and e-mails; after a
        // preposition ("no sábado", "on Friday") it is an event in the text.
        r"|(?:^|(?:[a-z]{3,}|\d+) )(?:(?:segunda|terca|quarta|quinta|sexta) feira|sabado|domingo",
        r"|lunes|martes|miercoles|jueves|viernes",
        r"|monday|tuesday|wednesday|thursday|friday|saturday|sunday)$"
    ))
    .expect("valid")
});
/// An electronic signature stamp ("Documento assinado eletronicamente por …, em",
/// "Firmado por … el"): repeated on every page and by every signer, so it only
/// dates a document that offers no other date, and then the first one does.
static STAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eletronicamente|electronicamente|firmado por|firma electronica|digitally signed|signed by")
        .expect("valid")
});
/// "Downloaded from … by guest on", "acessado em": the date of a copy, which a
/// publisher stamps on every page, never the document's.
static COPY_STAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)downloaded|accessed|retrieved|baixado|acess[oa]|descargado|consultado|recuperado|\.pdf (?:by|por)\b",
    )
    .expect("valid")
});
/// The header a browser prints on a saved page: "18/12/2024, 23:13", alone on
/// its line. It dates the printout, not the page.
static PRINT_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\d{1,2}/\d{1,2}/\d{2,4},?\s+\d{1,2}:\d{2}\s*$").expect("valid")
});
/// An identity document a party is described by, anywhere on the date's line:
/// "portador da Carteira Nacional de Habilitação nº 0485…, expedida em".
static IDENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"habilitacao|carteira de identidade|\brg\b|passaporte|passport|driver s license")
        .expect("valid")
});
/// A law, decree or order named by its number just before its date ("Lei nº
/// 13.844, de", "Decreto Municipal nº 2.812 de", "Reglamento (UE) 2016/679 …
/// de"): the date of the act, which is the document's own only in its title.
static CITED_ACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:lei|decreto|portaria|resolucao|instrucao normativa|medida provisoria|emenda|acordao|deliberacao",
        r"|oficio|despacho|memorando|parecer|nota tecnica|contrato|convenio",
        r"|ley|orden|reglamento|resolucion|directiva|expediente",
        r"|act|law|order|regulation|memorandum|contract)\b",
        // its number, possibly coded ("1asec ri e no 565"), then up to a few words
        // ("publicada no dou") before the "de" that introduces the date…
        r"(?:(?: [a-z0-9]+){0,6}? (?:n[o]? )?\d[\d ]*(?:[a-z]+ ){0,6}de",
        // …or no number at all ("pelo decreto de").
        r"|(?: [a-z]+)? de)$"
    ))
    .expect("valid")
});
/// "Porto Alegre, ", "Lyon, le ", "Москва, ", "北京，" — a place and a comma, alone
/// before the date: a capital in scripts with case, any letter in those without.
static SIGNATURE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // Place length is checked in code (`signature_line`): a counted repeat of
        // a Unicode class compiles into that many copies of it.
        // "Porto Alegre, ", "Goiânia, GO, " (a state's two letters may follow the city)
        r"^\s*(?:\p{Lu}[\p{L}\p{M} .'/-]{2,}(?:,\s*[A-Z]{2})?,",
        r"|[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}\p{Hebrew}][\p{L}\p{M} ]*[,，、])",
        r"\s*((em|le|den|am|el|on|the|a)\s+)?$"
    ))
    .expect("valid")
});

/// A place of at most 41 characters, a comma, and nothing else before the date.
fn signature_line(before: &str) -> bool {
    // Even indented by a layout extractor, such a line is far shorter; the
    // bound keeps a long line from being matched once per date on it.
    if before.len() > 512 {
        return false;
    }
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
    /// Written out in the formal way ("aos onze dias do mês de…").
    formal: bool,
}

/// Value of a number written in Portuguese or Spanish words ("dois mil e dezoito",
/// "veinticinco"), or in digits; `None` for anything else.
fn number(words: &[&str]) -> Option<u32> {
    if let [digits] = words
        && let Ok(n) = digits.parse()
    {
        return Some(n);
    }
    let (mut total, mut current, mut any) = (0, 0, false);
    for word in words {
        let word = fold_lower(word);
        let value = match word.as_str() {
            "e" | "y" => continue,
            "mil" => {
                total += current.max(1) * 1000;
                current = 0;
                any = true;
                continue;
            }
            "um" | "uma" | "un" | "uno" | "una" | "primeiro" | "primero" => 1,
            "dois" | "duas" | "dos" => 2,
            "tres" => 3,
            "quatro" | "cuatro" => 4,
            "cinco" => 5,
            "seis" => 6,
            "sete" | "siete" => 7,
            "oito" | "ocho" => 8,
            "nove" | "nueve" => 9,
            "dez" | "diez" => 10,
            "onze" | "once" => 11,
            "doze" | "doce" => 12,
            "treze" | "trece" => 13,
            "catorze" | "quatorze" | "catorce" => 14,
            "quinze" | "quince" => 15,
            "dezesseis" | "dezasseis" | "dieciseis" => 16,
            "dezessete" | "dezassete" | "diecisiete" => 17,
            "dezoito" | "dieciocho" => 18,
            "dezenove" | "dezanove" | "diecinueve" => 19,
            "vinte" | "veinte" => 20,
            "veintiun" | "veintiuno" => 21,
            "veintidos" => 22,
            "veintitres" => 23,
            "veinticuatro" => 24,
            "veinticinco" => 25,
            "veintiseis" => 26,
            "veintisiete" => 27,
            "veintiocho" => 28,
            "veintinueve" => 29,
            "trinta" | "treinta" => 30,
            "quarenta" | "cuarenta" => 40,
            "cinquenta" | "cincuenta" => 50,
            "sessenta" | "sesenta" => 60,
            "setenta" => 70,
            "oitenta" | "ochenta" => 80,
            "noventa" => 90,
            "cem" | "cento" | "cien" | "ciento" => 100,
            "duzentos" | "doscientos" => 200,
            "trezentos" | "trescientos" => 300,
            "quatrocentos" | "cuatrocientos" => 400,
            "quinhentos" | "quinientos" => 500,
            "seiscentos" | "seiscientos" => 600,
            "setecentos" | "setecientos" => 700,
            "oitocentos" | "ochocientos" => 800,
            "novecentos" | "novecientos" => 900,
            _ => return None,
        };
        current += value;
        any = true;
    }
    any.then_some(total + current)
}

/// The year that opens `rest`: four digits, or number words up to the first word
/// that is not one ("dois mil e dezoito, com início às…").
fn leading_year(rest: &str) -> Option<(u32, usize)> {
    let mut words = Vec::new();
    let mut end = 0;
    for (at, token) in rest.split_inclusive([' ', '\n', '\t']).scan(0, |at, t| {
        let start = *at;
        *at += t.len();
        Some((start, t))
    }) {
        let word = token.trim().trim_end_matches([',', '.', ';', ':']);
        if word.is_empty() {
            continue;
        }
        if words.is_empty() && word.len() == 4 && word.bytes().all(|b| b.is_ascii_digit()) {
            return word.parse().ok().map(|y| (y, at + token.trim_end().len()));
        }
        // "mil novecentos e noventa e nove" is the longest year in words.
        if words.len() == 8 || (number(&[word]).is_none() && !matches!(word, "e" | "y")) {
            break;
        }
        words.push(word);
        end = at + token.trim_end().len();
        if word.len() < token.trim().len() {
            break; // punctuation closes the year
        }
    }
    while words.last().is_some_and(|w| matches!(*w, "e" | "y")) {
        words.pop();
    }
    number(&words)
        .filter(|y| (1000..=2999).contains(y))
        .map(|y| (y, end))
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
            && is_calendar_date(y, month, d)
        {
            out.push(Found {
                iso: format!("{y:04}-{month:02}-{d:02}"),
                at,
                len,
                formal: false,
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
    for c in DAYS_OF.captures_iter(text) {
        let whole = c.get(0).map_or(0..0, |m| m.range());
        let day_words: Vec<&str> = c[1].split([' ', '-']).collect();
        if let (Some(day), Some(m), Some((year, len))) = (
            (day_words.len() <= 5).then(|| number(&day_words)).flatten(),
            month(&c[2]),
            leading_year(&text[whole.end..]),
        ) && is_calendar_date(year, m, day)
        {
            out.push(Found {
                iso: format!("{year:04}-{m:02}-{day:02}"),
                at: whole.start,
                len: whole.len() + len,
                formal: true,
            });
        }
    }
    out
}

/// A day that exists, in the years a document can carry: "31 de abril" is a
/// typo or a misread, and a wrong date is worse than none.
fn is_calendar_date(year: u32, month: u32, day: u32) -> bool {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1000..=2999).contains(&year) && (1..=days).contains(&day)
}

/// Whether `line` is little more than a date: "17 de setembro de 2026",
/// "Setembro de 2026", "October 2026".
pub fn is_date_line(line: &str) -> bool {
    let line = line.trim();
    let words: Vec<&str> = line
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let month_and_year = words.iter().any(|w| month(w).is_some())
        && words.iter().all(|w| {
            month(w).is_some()
                || w.chars().all(|c| c.is_ascii_digit())
                || matches!(fold_lower(w).as_str(), "de" | "del" | "of" | "the")
        });
    month_and_year
        || candidates(line)
            .iter()
            .any(|found| found.len * 10 >= line.len() * 7)
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

/// The document's own date, or `None` when no candidate scores above zero, with
/// the score that chose it: how much evidence says this is the document's own
/// date (a signature line, "emitido em", a dateline). A signature stamp, the
/// fallback, scores 0.
pub fn scored_date(text: &str) -> Option<(String, i32)> {
    // A document's own date heads it or closes it; past this many candidates
    // only the first and last halves are scored, which bounds the work on a
    // text that is nothing but dates.
    const SCORED: usize = 1000;
    let mut found_all = candidates(text);
    found_all.sort_by_key(|found| found.at);
    if found_all.len() > SCORED {
        found_all.drain(SCORED / 2..found_all.len() - SCORED / 2);
    }
    // Where the third non-empty line starts: a cited act opening a line up to
    // there is the document's own title.
    let mut title_end = usize::MAX;
    let mut offset = 0;
    let mut filled = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim().is_empty() {
            filled += 1;
            if filled == 3 {
                title_end = offset;
                break;
            }
        }
        offset += line.len();
    }
    // The line and the words before each date are tracked while walking the
    // candidates in order, so a long line is scanned once, not once per date.
    // `words` counts the words before the date on its line, up to three.
    let (mut scanned, mut line_start, mut words, mut in_word) = (0, 0, 0, false);
    let mut scope_of_line: Option<(usize, usize)> = None;
    let mut header_line: Option<(usize, bool)> = None;
    let mut best: Option<(i32, f64, String)> = None;
    let mut first_stamp: Option<(usize, String)> = None;
    for found in found_all {
        for (i, c) in text[scanned..found.at].char_indices() {
            if c == '\n' {
                (line_start, words, in_word) = (scanned + i + 1, 0, false);
            } else if c.is_whitespace() {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words = (words + 1).min(3);
            }
        }
        scanned = found.at;
        let before_raw = &text[..found.at];
        // The words that give the date its role sit on its line ("Data: …",
        // "Vencimento …") or, when the date opens its line, on the line above…
        // …or when only a word or two precede it ("nascido" / "em 13 de março").
        let scope = if words <= 2 {
            match scope_of_line {
                Some((line, scope)) if line == line_start => scope,
                _ => {
                    let scope = before_raw[..line_start]
                        .trim_end()
                        .rfind('\n')
                        .map_or(0, |i| i + 1);
                    scope_of_line = Some((line_start, scope));
                    scope
                }
            }
        } else {
            line_start
        };
        let near_text = norm(last_chars(&text[scope..found.at], 35));
        let near = near_text.as_str();
        // Position as a share of the text, in bytes: counting characters from the
        // start for every candidate would be quadratic on a long document.
        #[expect(
            clippy::cast_precision_loss,
            reason = "document lengths are far below 2^52 bytes"
        )]
        let place = found.at as f64 / text.len().max(1) as f64;
        let window = last_chars(before_raw, 160);
        // The stamp's own date follows it with no other date in between: the
        // issue header after "…by guest on 29 September 2026" is not the copy's.
        let copy_stamp = COPY_STAMP
            .find_iter(window)
            .last()
            .is_some_and(|m| candidates(&window[m.end()..]).is_empty());
        let print_header = match header_line {
            Some((line, header)) if line == line_start => header,
            _ => {
                let line_end = text[found.at..]
                    .find('\n')
                    .map_or(text.len(), |i| found.at + i);
                let header = PRINT_HEADER.is_match(&text[line_start..line_end]);
                header_line = Some((line_start, header));
                header
            }
        };
        if copy_stamp || print_header {
            continue;
        }
        if STAMP.is_match(&norm(window)) {
            if first_stamp.as_ref().is_none_or(|(at, _)| found.at < *at) {
                first_stamp = Some((found.at, found.iso));
            }
            continue;
        }
        let mut score = 0;
        if AGAINST.is_match(near)
            || IDENTITY.is_match(&norm(last_chars(&text[scope..found.at], 200)))
        {
            score -= 3;
        }
        if CITED_ACT.is_match(&norm(last_chars(before_raw, 120))) {
            // Opening one of the first lines, it is the document's own title
            // ("DECRETO Nº 010, DE 02 DE ABRIL DE 2020"); "(Lei nº 7.115, de …)"
            // under a title is the law the document rests on.
            let line = &text[line_start..found.at];
            let own_title = line.len() <= 300
                && line_start <= title_end
                && line.trim_start().starts_with(char::is_alphabetic)
                && CITED_ACT.find(&norm(line)).is_some_and(|m| m.start() == 0);
            score += if own_title { 3 } else { -3 };
        }
        if FOR.is_match(near) && !HISTORY.is_match(near) {
            score += 2;
        }
        if signature_line(&text[line_start..found.at]) || found.formal {
            score += 3;
        }
        // A date may run across a line break ("26 de\nmaio de 2026"): what
        // follows it on its line starts where the date ends.
        let date_end = found.at + found.len;
        if words == 0
            && text[date_end..]
                .chars()
                .find(|&c| c == '\n' || !(c.is_whitespace() || c == '.'))
                .is_none_or(|c| c == '\n')
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
        .map(|(score, _, iso)| (iso, score))
        .or(first_stamp.map(|(_, iso)| (iso, 0)))
}

/// Tuple order: score, then position, then the ISO string.
fn greater(a: &(i32, f64, String), b: &(i32, f64, String)) -> bool {
    match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => match a.1.partial_cmp(&b.1) {
            Some(std::cmp::Ordering::Equal) | None => a.2 > b.2,
            Some(order) => order == std::cmp::Ordering::Less,
        },
        order => order == std::cmp::Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document_date(text: &str) -> Option<String> {
        scored_date(text).map(|(iso, _)| iso)
    }

    /// `month` binary-searches the generated table.
    #[test]
    fn month_table_is_sorted_without_repeats() {
        assert!(MONTHS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn written_dates_in_shipped_languages() {
        for (text, iso) in [
            ("Porto Alegre, 22 de abril de 2026.", "2026-04-22"),
            ("Goiânia, GO, 06 de janeiro de 2026.", "2026-01-06"),
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

    #[test]
    fn formal_dates_in_digits_and_words() {
        for (text, iso) in [
            (
                "Ata da reunião.\nAos onze dias do mês de maio do ano de dois mil e dezoito, com início às oito horas, reuniram-se os membros.",
                "2018-05-11",
            ),
            (
                "Publique-se. Trindade, aos 25 dias de fevereiro de 2025.",
                "2025-02-25",
            ),
            (
                "En la ciudad de Lima, a los veinticinco días del mes de febrero de dos mil veinticinco, se reúnen las partes.",
                "2025-02-25",
            ),
            (
                "IN WITNESS WHEREOF, the parties sign this 12th day of March, 2026.",
                "2026-03-12",
            ),
        ] {
            assert_eq!(document_date(text).as_deref(), Some(iso), "{text}");
        }
    }

    /// A law's date dates the law, unless the document is that law.
    #[test]
    fn cited_acts_date_only_their_own_title() {
        let cites = "Política de privacidade da escola. Coletamos nome, endereço e telefone dos responsáveis para matrícula e comunicação com as famílias durante o ano letivo.\nBase legal: Lei nº 13.709, de 14 de agosto de 2018.";
        assert_eq!(document_date(cites), None);
        let own = "DECRETO Nº 010, DE 02 DE ABRIL DE 2020\nDispõe sobre medidas conforme o Decreto nº 006, de 17 de março de 2020.";
        assert_eq!(document_date(own).as_deref(), Some("2020-04-02"));
    }

    /// A date broken across two lines used to slice the text backwards.
    #[test]
    fn a_date_across_a_line_break() {
        let text = "Relatório anual\n26 de\nmaio de 2026\nTexto do relatório.";
        assert_eq!(document_date(text).as_deref(), Some("2026-05-26"));
    }

    #[test]
    fn impossible_dates_are_refused() {
        for text in [
            "Porto Alegre, 31 de abril de 2026.",
            "Porto Alegre, 29 de fevereiro de 2026.",
            "Emitido em 31/04/2026.",
            "Emitido em 01/01/0000.",
            "Publique-se. Trindade, aos 30 dias de fevereiro de 2025.",
        ] {
            assert_eq!(document_date(text), None, "{text}");
        }
        let leap = "Porto Alegre, 29 de fevereiro de 2024.";
        assert_eq!(document_date(leap).as_deref(), Some("2024-02-29"));
    }

    #[test]
    fn endless_year_words_stay_bounded() {
        let text = format!(
            "Aos onze dias do mês de maio de {}mil.",
            "novecentos ".repeat(5000)
        );
        assert_eq!(document_date(&text), None);
    }

    #[test]
    fn one_line_of_dates_is_linear() {
        let text = "01/01/2026 ".repeat(20_000);
        let started = std::time::Instant::now();
        assert_eq!(document_date(&text).as_deref(), Some("2026-01-01"));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    /// Stamps repeat on every page and for every signer; they date a document
    /// only when nothing else does, and then the first one does.
    #[test]
    fn signature_stamps_are_a_fallback() {
        let stamps = "Encaminho o relatório.\nDocumento assinado eletronicamente por Ana Souza, Diretora, em 08/03/2024, às 10:00.\nAnexo\nDocumento assinado eletronicamente por Rui Lima, Chefe, em 07/03/2024, às 09:00.";
        assert_eq!(document_date(stamps).as_deref(), Some("2024-03-08"));
        let meeting = "Ata da sessão.\nReunião realizada em 23/02/2024.\nDocumento assinado eletronicamente por Ana Souza, Secretária, em 02/04/2024, às 10:00.";
        assert_eq!(document_date(meeting).as_deref(), Some("2024-02-23"));
    }
}
