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

use crate::date::is_date_line;
use crate::kind::{names_a_kind, opens_with_kind};
use crate::text::{content_lines, first_chars, fold_lower, norm, word_count};

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
const FALLBACK_TITLE_LINES: usize = 8;
/// "CPF:179…", "Local de pagamento: …" — up to three words and a colon open a
/// field, not a title ("Compressão zstd em Btrfs: impacto…" is a title).
/// "…ao todo. Alguém lembrou…", "…disseram na época." — a sentence ends inside or
/// at the end of the line: running text ("S.A." at the end is still a name).
static TWO_SENTENCES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[.!?] [A-ZÀ-Ý]|[a-zà-ÿ]\.$").expect("valid"));
/// A filing or billing code ("6730–02–P", "1–13–25"): number, dash, number, dash.
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+[-–]\d+[-–]\w").expect("valid"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\S+\s+){0,2}\S+:\s*\S").expect("valid"));
/// What a journal prints above an article's title: its type ("Original Article",
/// "Artigo de revisão", "Perspectiva") or its own name ("The Joint Commission
/// Journal on…", "Revista de Dermatologia…").
static ABOVE_ARTICLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^(?:(?:original|regular|review|research|special|accepted) article|(?:artigo|art[ií]culo) (?:original|regular|de revis[aã]o|de revisi[oó]n|de pesquisa)",
        r"|perspective|perspectiva|editorial|commentary|coment[aá]rio|case report|relato de caso|brief report|clinical trial",
        // alone on its line, or glued to the next word by the extraction ("revisãoAcesso")
        r"|abstract|resumo|resumen|open access|acesso aberto|acceso abierto)(?:$|(?-i:\p{Lu}))",
        r"|\b(?:journal|jornal|revista|peri[oó]dico|gazette|gaceta|bulletin|boletim)\b"
    ))
    .expect("valid")
});
/// The banner a translation service stamps on top of every page it returns.
static TRANSLATION_BANNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)translated by|traduzido (do|por|pelo)|traducido (del|por)|www\.")
        .expect("valid")
});

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
    let kind_line = lines.iter().position(|line| {
        line.chars().count() <= 140 && !line.ends_with([':', ',']) && names_a_kind(&norm(line))
    });
    // A kind named only in passing leaves the title to the heading above it.
    if let Some(at) = kind_line.filter(|&at| !in_passing(&lines, at)) {
        return Some(lines[at].to_owned());
    }
    let at = heading(&lines)?;
    Some(match continuation(&lines, at) {
        Some(next) => format!("{} {next}", lines[at]),
        None => lines[at].to_owned(),
    })
}

/// The next line when line `at` is a title wrapped mid-phrase ("…gene therapy,
/// and standard" / "care in sickle cell disease: a cost-effectiveness analysis").
fn continuation<'a>(lines: &[&'a str], at: usize) -> Option<&'a str> {
    let next = lines.get(at + 1)?;
    let line = lines[at];
    // Lower case on both sides of the break, or a line that ends on a joining
    // word ("The Urgent Need to" / "Implement…", "IMPLEMENTAÇÃO DO" / …).
    let mid_phrase = (line.ends_with(char::is_lowercase) && next.starts_with(char::is_lowercase))
        || (line
            .rsplit(' ')
            .next()
            .is_some_and(|w| JOINING.contains(&fold_lower(w).as_str()))
            && next.starts_with(char::is_alphabetic));
    (word_count(line) <= 10 && mid_phrase && !is_letter_spaced(line) && word_count(next) <= 15)
        .then_some(*next)
}

/// Words a title does not end on: a line ending on one continues below.
const JOINING: &[&str] = &[
    "a", "an", "and", "as", "at", "by", "com", "con", "da", "das", "de", "del", "der", "des",
    "die", "do", "dos", "du", "e", "el", "em", "en", "et", "for", "in", "la", "las", "le", "les",
    "los", "na", "nas", "no", "nos", "o", "of", "on", "or", "os", "ou", "para", "por", "the", "to",
    "und", "von", "y", "zu",
];

/// A word set in spaced capitals, "EDI T OR I A L": words of a letter or two.
fn is_letter_spaced(line: &str) -> bool {
    let words = word_count(line);
    words >= 3 && line.chars().filter(|c| c.is_alphabetic()).count() < words * 2
}

/// The heading among the opening lines: a text that is not a known kind of
/// document usually opens with its title (an article, a chapter, a review).
/// What a journal prints above the title, and a lone capitalized word
/// ("TRANSPLANTATION"), give way to a heading below them.
fn heading(lines: &[&str]) -> Option<usize> {
    let headings: Vec<usize> = (0..lines.len().min(FALLBACK_TITLE_LINES))
        .filter(|&at| {
            !(at > 0 && continuation(lines, at - 1).is_some())
                && looks_like_title(lines[at])
                && !is_date_line(lines[at])
                && !has_glued_words(lines[at])
        })
        .collect();
    let above_title = |at: usize| {
        let line = lines[at];
        ABOVE_ARTICLE.is_match(line)
            || (word_count(line) == 1 && !line.chars().any(char::is_lowercase))
            || is_letter_spaced(line)
    };
    headings
        .iter()
        .copied()
        .find(|&at| !above_title(at))
        .or(headings.first().copied())
}

/// Words run together by a broken extraction: "INGLATERRAdeMEDICAMENTO", capitals,
/// then lower case, then capitals again inside one word.
fn has_glued_words(line: &str) -> bool {
    line.split_whitespace().any(|word| {
        let mut runs = Vec::new();
        for c in word.chars().filter(|c| c.is_alphabetic()) {
            let upper = c.is_uppercase();
            match runs.last_mut() {
                Some((case, len)) if *case == upper => *len += 1,
                _ => runs.push((upper, 1)),
            }
        }
        runs.windows(3)
            .any(|w| w[0].0 && w[0].1 >= 2 && !w[1].0 && w[1].1 >= 2 && w[2].0 && w[2].1 >= 2)
    })
}

/// Whether line `at` of a document's opening `lines` names a kind only in
/// passing: running prose below a heading ("…compiled from the annual Network
/// Threat Report and the"), not a line that opens with the kind as wrapped
/// titles do ("Real Decreto 105/2025, de 18 de febrero, por el que se").
pub(crate) fn in_passing(lines: &[&str], at: usize) -> bool {
    is_wrapped_prose(lines[at])
        && !opens_with_kind(&norm(lines[at]))
        && heading(lines).is_some_and(|h| h < at)
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
        && !CODE.is_match(line)
        // a fragment of running prose, cut on both ends
        && !(line.starts_with(char::is_lowercase) && line.ends_with(char::is_lowercase))
        && !line.contains('@')
        && !line.contains("http")
        && !TRANSLATION_BANNER.is_match(line)
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
