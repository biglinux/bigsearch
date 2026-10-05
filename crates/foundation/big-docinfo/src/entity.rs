// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Who a document belongs to, and its title.
//!
//! The entity comes from the first evidence found, in this order: a labelled
//! party ("Razão social:", "Paciente:", "Contratante:"…), an e-mail sender
//! ("De: Nome <…>"), a first-person declaration ("Eu, Fulana de Tal,"), the name
//! heading a résumé, and finally an institution named in the header lines.
//! The title is the e-mail subject, or the first line that names a kind of
//! document ("CONTRATO DE LOCAÇÃO RESIDENCIAL").

use std::sync::LazyLock;

use regex::Regex;

use crate::kind::{names_a_kind, opens_with_kind};
use crate::text::{content_lines, first_chars, norm, word_count};

static COMPANY_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:raz[aã]o social|raz[oó]n social|company name):\s*(.+)").expect("valid")
});
static PARTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        concat!(
            r"^(?i:paciente|titular|cliente|contribuinte|requerente|declarante|contratante|locador[a]?|vendedor[a]?|benefici[aá]rio|candidat[oa]",
            r"|contribuyente|arrendador[a]?|comprador[a]?",
            r"|patient|client|customer|tenant|landlord|lessor|seller|buyer|applicant|candidate|beneficiary|taxpayer)",
            // Unbounded here, bounded in code: a counted repeat of a Unicode class
            // is compiled as that many copies of it (megabytes of automaton).
            r"\s*:\s*([A-ZÀ-Ý][\wÀ-ÿ'.& -]{3,})"
        ),
    )
    .expect("valid")
});
static SENDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:de|from):\s*([^<@]+?)\s*<").expect("valid"));
static FIRST_PERSON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:Eu|Yo),\s+([A-ZÀ-Ý][\wÀ-ÿ' ]{5,}?),",
        // English "I," also opens "items I, III and IV": a name needs a lowercase letter.
        r"|\bI,\s+([A-ZÀ-Ý][a-zà-ÿ][\wÀ-ÿ' ]{4,}?),"
    ))
    .expect("valid")
});
static PERSON_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-ZÀ-Ý][A-Za-zÀ-ÿ]+( [A-ZÀ-Ý][A-Za-zÀ-ÿ]+){1,4}$").expect("valid")
});
static INSTITUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        concat!(
            r"\b(prefeitura|camara|secretaria|ministerio|universidade|instituto|associacao|condominio|fundacao|tribunal|governo|companhia|cooperativa|banco|hospital|escola|agencia|conselho|assembleia|ltda|s a|eireli|me",
            r"|ayuntamiento|municipalidad|universidad|asociacion|gobierno|compania|escuela|consejo|junta|diputacion|s l",
            r"|city of|county|department|ministry|university|institute|association|foundation|court|government|company|bank|school|agency|council|inc|llc|ltd|corp|corporation)\b"
        ),
    )
    .expect("valid")
});
static SUBJECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:assunto|asunto|subject):\s*(.+)").expect("valid"));

/// Header lines searched for labelled parties, and for institutions.
const PARTY_LINES: usize = 40;
const HEADER_LINES: usize = 15;
const TITLE_LINES: usize = 12;
const FALLBACK_TITLE_LINES: usize = 5;
/// "CPF:179…", "Local de pagamento: …" — up to three words and a colon open a
/// field, not a title ("Compressão zstd em Btrfs: impacto…" is a title).
/// "…ao todo. Alguém lembrou…", "…disseram na época." — a sentence ends inside or
/// at the end of the line: running text ("S.A." at the end is still a name).
static TWO_SENTENCES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[.!?] [A-ZÀ-Ý]|[a-zà-ÿ]\.$").expect("valid"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\S+\s+){0,2}\S+:\s*\S").expect("valid"));

/// Only the opening of a document is read for its entity and title.
const HEAD_CHARS: usize = 20_000;

pub fn main_entity(text: &str, kind: Option<&str>) -> Option<String> {
    let text = first_chars(text, HEAD_CHARS);
    let lines = content_lines(text);
    for line in lines.iter().take(PARTY_LINES) {
        if let Some(c) = COMPANY_NAME.captures(line) {
            return Some(c[1].trim_matches([' ', '.', ',', ';']).to_owned());
        }
        if let Some(c) = PARTY.captures(line) {
            let name = first_chars(&c[1], 81); // at most 80 after the capital
            return Some(name.trim_matches([' ', '.', ',', ';']).to_owned());
        }
        if let Some(c) = SENDER.captures(line) {
            return Some(c[1].trim().to_owned());
        }
    }
    if let Some(name) = FIRST_PERSON
        .captures_iter(text)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str())
        .find(|name| name.chars().count() <= 61)
    {
        return Some(name.trim().to_owned());
    }
    if kind == Some("curriculo")
        && let Some(first) = lines.first()
    {
        let name = title_case(first);
        if PERSON_NAME.is_match(&name) {
            return Some(name);
        }
    }
    for line in lines.iter().take(HEADER_LINES) {
        if line.chars().count() <= 80 && INSTITUTION.is_match(&norm(line)) {
            return Some(line.trim_matches([' ', '-', '—', ':']).to_owned());
        }
    }
    None
}

pub fn title(text: &str) -> Option<String> {
    let lines: Vec<&str> = content_lines(first_chars(text, HEAD_CHARS))
        .into_iter()
        .take(TITLE_LINES)
        .collect();
    for line in &lines {
        if let Some(c) = SUBJECT.captures(line) {
            let subject = norm(&c[1]);
            return (!subject.is_empty()
                && !["sem assunto", "sin asunto", "no subject"].contains(&subject.as_str()))
            .then(|| c[1].trim().to_owned());
        }
    }
    // A text that is not a known kind of document usually opens with its title:
    // a book chapter, a report, a review.
    let first_title = lines
        .iter()
        .take(FALLBACK_TITLE_LINES)
        .position(|line| looks_like_title(line));
    if let Some(at) = lines.iter().position(|line| {
        line.chars().count() <= 140 && !line.ends_with([':', ',']) && names_a_kind(&norm(line))
    }) {
        // A kind word inside running prose ("…compiled from our AI Risk Report
        // and the") does not make that line the title of an earlier heading.
        let title = match first_title {
            Some(title)
                if title < at
                    && is_wrapped_prose(lines[at])
                    && !opens_with_kind(&norm(lines[at])) =>
            {
                lines[title]
            }
            _ => lines[at],
        };
        return Some(title.to_owned());
    }
    first_title.map(|at| lines[at].to_owned())
}

/// A short line of words that is not a field, a greeting, a placeholder or an address.
fn looks_like_title(line: &str) -> bool {
    let visible: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    let letters = visible.iter().filter(|c| c.is_alphabetic()).count();
    // Chinese and Japanese put no spaces between words: there the length is
    // counted in characters, and an ideographic full stop marks a sentence.
    let length_ok = if visible.iter().any(|&c| is_cjk(c)) {
        (2..=40).contains(&visible.len()) && !line.contains('。')
    } else {
        (2..=15).contains(&word_count(line))
    };
    length_ok
        && line.chars().count() <= 120
        && letters * 2 >= visible.len()
        && !line.ends_with([',', ';', ':'])
        && !line.starts_with(['[', '(', '{', '<', '.', '…'])
        && !FIELD.is_match(line)
        && !TWO_SENTENCES.is_match(line)
        && !line.contains('@')
        && !line.contains("http")
}

/// A long line cut mid-sentence: many words and no closing punctuation.
fn is_wrapped_prose(line: &str) -> bool {
    word_count(line) >= 10 && line.ends_with(char::is_lowercase)
}

/// "JOANA D'ARC SILVA" → "Joana D'Arc Silva": capital after every non-letter.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut after_letter = false;
    for c in s.chars() {
        let cased = c.is_lowercase() || c.is_uppercase();
        if cased && after_letter {
            out.extend(c.to_lowercase());
        } else if cased {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        after_letter = cased;
    }
    out
}

/// Han ideographs and Japanese kana (Hangul is written with spaces).
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}' | '\u{20000}'..='\u{2FA1F}')
}
