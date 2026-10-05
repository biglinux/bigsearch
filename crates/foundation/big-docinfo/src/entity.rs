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

use crate::kind::names_a_kind;
use crate::text::{content_lines, norm, word_count};

static COMPANY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^raz[aã]o social:\s*(.+)").expect("valid"));
static PARTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:paciente|titular|cliente|contribuinte|requerente|declarante|contratante|locador[a]?|vendedor[a]?|benefici[aá]rio|candidat[oa])\s*:\s*([A-ZÀ-Ý][\wÀ-ÿ'.& -]{3,80})",
    )
    .expect("valid")
});
static SENDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^de:\s*([^<@]+?)\s*<").expect("valid"));
static FIRST_PERSON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bEu,\s+([A-ZÀ-Ý][\wÀ-ÿ' ]{5,60}?),").expect("valid"));
static PERSON_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-ZÀ-Ý][A-Za-zÀ-ÿ]+( [A-ZÀ-Ý][A-Za-zÀ-ÿ]+){1,4}$").expect("valid")
});
static INSTITUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(prefeitura|camara|secretaria|ministerio|universidade|instituto|associacao|condominio|fundacao|tribunal|governo|companhia|cooperativa|banco|hospital|escola|agencia|conselho|assembleia|ltda|s a|eireli|me)\b",
    )
    .expect("valid")
});
static SUBJECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^assunto:\s*(.+)").expect("valid"));

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

pub fn main_entity(text: &str, kind: Option<&str>) -> Option<String> {
    let lines = content_lines(text);
    for line in lines.iter().take(PARTY_LINES) {
        if let Some(c) = COMPANY_NAME.captures(line).or_else(|| PARTY.captures(line)) {
            return Some(c[1].trim_matches([' ', '.', ',', ';']).to_owned());
        }
        if let Some(c) = SENDER.captures(line) {
            return Some(c[1].trim().to_owned());
        }
    }
    if let Some(c) = FIRST_PERSON.captures(text) {
        return Some(c[1].trim().to_owned());
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
    let lines: Vec<&str> = content_lines(text).into_iter().take(TITLE_LINES).collect();
    for line in &lines {
        if let Some(c) = SUBJECT.captures(line) {
            let subject = norm(&c[1]);
            return (!subject.is_empty() && subject != "sem assunto")
                .then(|| c[1].trim().to_owned());
        }
    }
    if let Some(line) = lines
        .iter()
        .find(|line| line.chars().count() <= 140 && names_a_kind(&norm(line)))
    {
        return Some((*line).to_owned());
    }
    // A text that is not a known kind of document usually opens with its title:
    // a book chapter, a report, a review.
    lines
        .into_iter()
        .take(FALLBACK_TITLE_LINES)
        .find(|line| looks_like_title(line))
        .map(str::to_owned)
}

/// A short line of words that is not a field, a greeting, a placeholder or an address.
fn looks_like_title(line: &str) -> bool {
    let words = word_count(line);
    let visible: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    let letters = visible.iter().filter(|c| c.is_alphabetic()).count();
    (2..=15).contains(&words)
        && line.chars().count() <= 120
        && letters * 2 >= visible.len()
        && !line.ends_with([',', ';', ':'])
        && !line.starts_with(['[', '(', '{', '<', '.', '…'])
        && !FIELD.is_match(line)
        && !TWO_SENTENCES.is_match(line)
        && !line.contains('@')
        && !line.contains("http")
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
