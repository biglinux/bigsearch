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
/// Lines read for a heading, the bare ones between them included.
const OPENING_LINES: usize = 30;
/// How much of the text must bear out a declared title: its first page or so.
const DECLARED_TITLE_CHARS: usize = 8_000;
/// "CPF:179…", "Local de pagamento: …" — up to three words and a colon open a
/// field, not a title ("Compressão zstd em Btrfs: impacto…" is a title).
/// "…ao todo. Alguém lembrou…", "…disseram na época." — a sentence ends inside or
/// at the end of the line: running text ("S.A." at the end is still a name).
static TWO_SENTENCES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[.!?] [A-ZÀ-Ý]|[a-zà-ÿ]\.$").expect("valid"));
/// A reference to a journal issue: "2021; 66:e025", "2018 mar;39(2):134",
/// "Haemophilia (2017), 1–6".
static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r";\s*\d+\s*[:(]|\(\d{4}\)\s*,?\s*\d").expect("valid"));
/// An article's history: "Received 14 June 2024", "Recebido em…", "Aceito em…".
static HISTORY_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:received|recebido|recibido|accepted|aceito|aceptado|submitted|submetido|revised|revisado|published|publicado)")
        .expect("valid")
});
/// A copyright or licence notice: "© 2016 … published by John Wiley & Sons Ltd".
static COPYRIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)©|\bcopyright\b|published by|all rights reserved|todos os direitos")
        .expect("valid")
});
/// A lettered section: "A - QUALIFICAÇÃO DAS PARTES", "B1 - Origem", "IV - DO OBJETO".
/// ("2.1 O que são diretórios" heads a chapter file and stays a title.)
static SECTION_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[A-Z]\d?|[IVX]+)\s*[-–.)]\s").expect("valid"));
/// An author's affiliation: "From the Department of Laboratory Medicine…".
static AFFILIATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:from the )?(?:department|division) of\b|^(?:departamento|divis[aã]o) de\b")
        .expect("valid")
});
/// A filing or billing code ("6730–02–P", "1–13–25"): number, dash, number, dash.
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+[-–]\d+[-–]\w").expect("valid"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\S+\s+){0,2}\S+:\s*\S").expect("valid"));
/// The type of article a journal prints above its title, in the original or
/// translated: "Original Article", "Relatório especial", "Carta al editor".
/// Alone on its line, or glued to the next word by the extraction
/// ("revisãoAcesso aberto").
static ARTICLE_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^(?:(?:original|regular|review|research|special|accepted|supplement) article",
        r"|(?:brief|special|case) report|research letter|letters? to the editor|correspondence",
        r"|editorial|commentary|perspective|viewpoint|(?:systematic|narrative|mini|illustrated) review",
        r"|clinical trials?(?: and observations)?|abstract|open access|type \w+|original paper",
        r"|clinical guidelines|check for updates|contents lists available.*|articles? in press",
        r"|original (?:research|investigation)|research paper|methodological article|state of the art",
        r"|isth ssc communication|add your logos here",
        r"|(?:\w+ )?investiga(?:tion|[cç][aã]o|ci[oó]n)(?: \w+)?",
        r"|pesquisa original|artigo metodol[oó]gico|estado da arte|estado do cora[cç][aã]o",
        // the first section of a paper, when its title was not found above it
        r"|(?:introduction|introdu[cç][aã]o|introducci[oó]n|background)(?: and \w+)?",
        r"|papel original|diretrizes cl[ií]nicas|verifique se h[aá] atualiza[cç][oõ]es|listas de conte[uú]do.*",
        r"|artigos? na imprensa|art[ií]culos? en prensa",
        r"|artigo (?:original|regular|de revis[aã]o|de pesquisa|especial|suplementar)",
        r"|relat[oó]rio (?:especial|breve)|relato (?:de caso|breve)|carta (?:de pesquisa|ao editor|para o editor)",
        r"|ponto de vista|perspectiva|coment[aá]rio|revis[aã]o(?: sistem[aá]tica| narrativa)?|resumo",
        r"|manuscrito original|acesso aberto",
        r"|art[ií]culo (?:original|de revisi[oó]n|especial)|carta al editor|caso cl[ií]nico|reporte de caso",
        r"|revisi[oó]n sistem[aá]tica|resumen|acceso abierto|punto de vista)(?:$|(?-i:\p{Lu}))"
    ))
    .expect("valid")
});
/// Whether `line` is the type of article a journal prints above its title.
pub(crate) fn is_article_type(line: &str) -> bool {
    ARTICLE_TYPE.is_match(line)
}

/// A journal's own name above an article: "The Joint Commission Journal on…",
/// "Revista de Dermatologia…", "O Novo Jornal de Medicina da Inglaterra".
static MASTHEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:journal|jornal|revista|peri[oó]dico|gazette|gaceta|federal register|di[aá]rio oficial|bolet[ií]n oficial)\b").expect("valid")
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

/// The title a file declares in its metadata, when the opening of its text
/// bears it out: most of its words are there. That leaves out what tools write
/// in that field ("Microsoft Word - doc1.docx", "untitled", a file name) and the
/// original title of a translated copy.
pub fn declared_title(declared: &str, text: &str) -> Option<String> {
    let declared = declared.trim();
    // A page saved from the web: "Recommendations for Women | NBDF",
    // "Shader Debugger Setup — Nsight Graphics"; a file
    // name kept by the tool that made the PDF: "Microsoft Word - Ata.docx".
    let declared = [" | ", " — ", " – "]
        .iter()
        .find_map(|bar| declared.rsplit_once(bar))
        .filter(|(_, site)| word_count(site) <= 4)
        .map_or(declared, |(title, _)| title.trim());
    let declared = declared
        .strip_prefix("Microsoft Word - ")
        .unwrap_or(declared);
    let declared = declared
        .rsplit_once('.')
        .filter(|(_, ext)| {
            [
                "doc", "docx", "odt", "xls", "xlsx", "ods", "ppt", "pptx", "pdf", "txt",
            ]
            .contains(&ext.to_ascii_lowercase().as_str())
        })
        .map_or(declared, |(stem, _)| stem.trim());
    // One token ("doi:10.1182/blood-2021-153527", "-PO_48_WFH2024") is an identifier;
    // "Clinical Guidelines" is the section, not the article.
    if word_count(declared) < 2 || ARTICLE_TYPE.is_match(declared) {
        return None;
    }
    let words: Vec<String> = norm(declared)
        .split(' ')
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_owned)
        .collect();
    if words.len() < 2 || declared.chars().count() > 250 {
        return None;
    }
    let head = norm(first_chars(text, DECLARED_TITLE_CHARS));
    let head: std::collections::HashSet<&str> = head.split(' ').collect();
    let found = words.iter().filter(|w| head.contains(w.as_str())).count();
    (found * 5 >= words.len() * 4).then(|| declared.to_owned())
}

/// The title of `text`; `prominent` are the lines its file sets in large type.
pub fn title(text: &str, prominent: &[String]) -> Option<String> {
    let opening: Vec<&str> = content_lines(first_chars(text, HEAD_CHARS))
        .into_iter()
        .take(OPENING_LINES)
        .collect();
    let lines = &opening[..opening.len().min(TITLE_LINES)];
    for line in lines {
        if let Some(c) = SUBJECT.captures(line) {
            let subject = norm(&c[1]);
            return (!subject.is_empty()
                && !["sem assunto", "sin asunto", "no subject"].contains(&subject.as_str()))
            .then(|| c[1].trim().to_owned());
        }
    }
    let kind_line = (0..lines.len()).find(|&at| {
        let line = lines[at];
        line.chars().count() <= 140
            && !line.ends_with([':', ','])
            && !continues_sentence(lines, at)
            && !ARTICLE_TYPE.is_match(line)
            && names_a_kind(&norm(line))
    });
    // A kind named only in passing leaves the title to the heading above it.
    if let Some(at) = kind_line.filter(|&at| !in_passing(lines, at)) {
        return Some(lines[at].to_owned());
    }
    // The largest words on the page are the title a reader sees — when they
    // read as one. A decorative initial ("C") is not words; past the largest,
    // smaller type is section headings, and position decides instead.
    if let Some(line) = prominent
        .iter()
        .find(|line| line.chars().filter(|c| c.is_alphabetic()).count() >= 2)
        // a section label set as large as the title, on the same line
        .map(|line| without_article_type(line))
        .filter(|line| {
            // words, not a figure ("94 KB")
            (line
                .split_whitespace()
                .filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3)
                .count()
                >= 2
                || line.chars().any(is_cjk))
                && looks_like_title(line, true)
                && !is_above_title(line)
                && !is_date_line(line)
                && !has_glued_words(line)
        })
    {
        return Some(line.to_owned());
    }
    let at = heading(&opening)?;
    Some(match continuation(&opening, at) {
        Some(next) => format!("{} {next}", opening[at]),
        None => opening[at].to_owned(),
    })
}

/// Whether line `at` carries on a sentence from the line above: "…em risco
/// após o" / "de TEV recorrente… para orientar…" is prose, whatever it matches.
fn continues_sentence(lines: &[&str], at: usize) -> bool {
    at > 0
        && lines[at - 1].ends_with(|c: char| c.is_lowercase() || c == ',')
        && lines[at].starts_with(char::is_lowercase)
}

/// The next line when line `at` is a title wrapped mid-phrase ("…gene therapy,
/// and standard" / "care in sickle cell disease: a cost-effectiveness analysis").
fn continuation<'a>(lines: &[&'a str], at: usize) -> Option<&'a str> {
    let next = lines.get(at + 1)?;
    let line = lines[at];
    // Lower case on both sides of the break, or a line that ends on a joining
    // word ("The Urgent Need to" / "Implement…", "IMPLEMENTAÇÃO DO" / …).
    let mid_phrase = (line.ends_with(|c: char| c.is_lowercase() || c == ':')
        && next.starts_with(char::is_lowercase))
        || (line
            .rsplit(' ')
            .next()
            .is_some_and(|w| JOINING.contains(&fold_lower(w).as_str()))
            && next.starts_with(char::is_alphabetic));
    (word_count(line) <= 10 && mid_phrase && !is_letter_spaced(line) && word_count(next) <= 15)
        .then_some(*next)
}

/// The sections of a paper or poster, in the languages read here.
const SECTION_WORDS: &[&str] = &[
    "introduction",
    "background",
    "aim",
    "aims",
    "objective",
    "objectives",
    "methods",
    "results",
    "discussion",
    "conclusion",
    "conclusions",
    "references",
    "and",
    "e",
    "y",
    "introducao",
    "objetivo",
    "objetivos",
    "metodos",
    "metodologia",
    "resultados",
    "discussao",
    "conclusao",
    "conclusoes",
    "referencias",
    "introduccion",
    "discusion",
    "conclusiones",
];

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
/// ("TRANSPLANTATION"), are never the title. Only lines of words count toward
/// the opening: "-", "2.12.008" and a bare link do not push the title out of it.
fn heading(lines: &[&str]) -> Option<usize> {
    let mut seen = 0;
    for (at, &line) in lines.iter().enumerate() {
        if word_count(line) < 2 && !line.chars().any(is_cjk) {
            continue;
        }
        if seen == FALLBACK_TITLE_LINES {
            break;
        }
        let at_top = seen < 3;
        seen += 1;
        if continues_sentence(lines, at) {
            continue;
        }
        // A title wrapped at its colon is judged whole ("…for sickle cell
        // disease:" / "transfusion support").
        let titled = looks_like_title(line, at_top)
            || continuation(lines, at)
                .is_some_and(|next| looks_like_title(&format!("{line} {next}"), true));
        // A label alone is no title: the file keeps the name it has.
        if titled && !is_date_line(line) && !has_glued_words(line) && !is_above_title(line) {
            return Some(at);
        }
    }
    None
}

/// `line` without a section label it opens with: "Relatório especial Avançando
/// em direção…" → "Avançando em direção…".
fn without_article_type(line: &str) -> &str {
    // A label set in spaced letters first: "Edi t or i a l Advanced therapy…" —
    // fragments of a few letters, three or more of them single letters.
    let (mut end, mut fragments, mut single) = (0, 0, 0);
    for word in line.split_whitespace() {
        if word.chars().count() > 3 {
            break;
        }
        fragments += 1;
        single += usize::from(word.chars().count() == 1);
        end = line[end..]
            .find(word)
            .map_or(end, |at| end + at + word.len());
    }
    let line = if fragments >= 4 && single >= 3 {
        line[end..].trim_start()
    } else {
        line
    };
    line.char_indices()
        .filter(|&(_, c)| c == ' ')
        .take(4)
        .find(|&(at, _)| {
            ARTICLE_TYPE
                .find(&line[..at])
                .is_some_and(|m| m.end() == at)
        })
        .map_or(line, |(at, _)| line[at..].trim_start())
}

/// What a journal prints above a title — its section, its name — a lone
/// capitalized word ("TRANSPLANTATION"), and a poster's section headings set
/// side by side ("Introduction Results").
fn is_above_title(line: &str) -> bool {
    line.split_whitespace().all(|word| {
        SECTION_WORDS
            .contains(&fold_lower(word.trim_matches(|c: char| !c.is_alphanumeric())).as_str())
    }) || ARTICLE_TYPE.is_match(line)
        || MASTHEAD.is_match(line)
        || (word_count(line) == 1
            && line.chars().any(char::is_uppercase)
            && !line.chars().any(char::is_lowercase))
        || is_letter_spaced(line)
}

/// Authors with their affiliation numbers: "Manoj Verma1Swati Pandey2, Bina Gidwani3".
fn is_author_line(line: &str) -> bool {
    line.split_whitespace()
        .filter(|word| {
            let mut chars = word.chars();
            chars.next().is_some_and(char::is_uppercase) && {
                let rest: String = chars.collect();
                rest.find(|c: char| c.is_ascii_digit())
                    .is_some_and(|at| at > 0 && rest[..at].chars().all(char::is_lowercase))
            }
        })
        .count()
        >= 2
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
/// At the very top of a text a title may run long (`at_top`): up to 25 words.
fn looks_like_title(line: &str, at_top: bool) -> bool {
    let visible: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    let letters = visible.iter().filter(|c| c.is_alphabetic()).count();
    // Chinese and Japanese put no spaces between words: there the length is
    // counted in characters, and an ideographic full stop marks a sentence.
    let length_ok = if visible.iter().any(|&c| is_cjk(c)) {
        (2..=40).contains(&visible.len()) && !line.contains('。')
    } else {
        (2..=if at_top { 25 } else { 15 }).contains(&word_count(line))
    };
    length_ok
        && line.chars().count() <= if at_top { 200 } else { 120 }
        && letters * 2 >= visible.len()
        && !line.ends_with([',', ';', ':'])
        && !line.starts_with(['[', '(', '{', '<', '.', '…', '•', '▪', '◦', '-', '–', '*'])
        && !line.contains(" = ")
        && !COPYRIGHT.is_match(line)
        && !is_author_line(line)
        // a long line has to read as a title from its first letter
        && (word_count(line) <= 15 || line.starts_with(char::is_uppercase))
        && !FIELD.is_match(line)
        && !TWO_SENTENCES.is_match(line)
        && !CODE.is_match(line)
        && !CITATION.is_match(line)
        && !SECTION_NUMBER.is_match(line)
        && !AFFILIATION.is_match(line)
        && !HISTORY_LINE.is_match(line)
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

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = "Special Report\nMoving toward disease modification in polycythemia vera\nJ. P. Bewersdorf, J. How\nPolycythemia vera belongs to the myeloproliferative neoplasms.";

    #[test]
    fn a_declared_title_the_text_bears_out_is_kept() {
        let declared = "Moving toward disease modification in polycythemia vera";
        assert_eq!(declared_title(declared, ARTICLE).as_deref(), Some(declared));
        let web = "Moving toward disease modification in polycythemia vera | NBDF";
        assert_eq!(declared_title(web, ARTICLE).as_deref(), Some(declared));
    }

    #[test]
    fn what_tools_write_in_the_title_field_is_not_a_title() {
        let translated =
            "Relatório especial\nAvançando em direção à modificação da doença na policitemia vera";
        let original = "Moving toward disease modification in polycythemia vera";
        assert_eq!(declared_title(original, translated), None);
        for junk in [
            "doi:10.1182/blood-2021-153527",
            "-PO_48_WFH2024",
            "Special Report",
            "Microsoft Word - vera.docx",
        ] {
            assert_eq!(declared_title(junk, ARTICLE), None, "{junk}");
        }
    }
}
