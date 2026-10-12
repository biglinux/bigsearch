// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Document kind from the words at its top, in Portuguese, English and Spanish.
//!
//! A document usually names itself in its first lines ("CONTRATO DE LOCAÇÃO",
//! "MINUTES OF THE MEETING", "ACTA DE LA SESIÓN"). Each kind has a pattern per
//! language over the normalized first 600 characters; the earliest hit wins, and
//! the language of that pattern is the language of the word used in the file
//! name. Text in other languages gets no kind and is named by its title.

use std::sync::LazyLock;

use regex::Regex;

use crate::entity::{in_passing, is_article_type};
use crate::text::{content_lines, first_chars, norm};

const HEAD_CHARS: usize = 600;

/// (language, kind, pattern). Within a language, order breaks ties at the same
/// position ("extrato de contrato" is a contract; "certificado médico" is health).
const RULES: &[(&str, &str, &str)] = &[
    (
        "pt",
        "contrato",
        r"\bextrato (do|de) (contrato|termo|aditivo)|\btermo aditivo\b|\bcontrato\b",
    ),
    (
        "pt",
        "nota_fiscal",
        r"\bnota fiscal\b|\bnfs? e\b|\bdanfe\b|\bcupom fiscal\b",
    ),
    ("pt", "recibo", r"\brecibo\b"),
    (
        "pt",
        "fatura_boleto",
        r"\bfatura\b|\bboleto\b|\blinha digitavel\b|\bconta de (energia|agua|luz|gas)\b",
    ),
    (
        "pt",
        "extrato",
        r"\bextrato (bancario|de conta|mensal)\b|\bsaldo anterior\b",
    ),
    ("pt", "ata", r"^ata\b|\bata d[aeo]s?\b"),
    ("pt", "anotacoes", r"^(anotacoes|notas|lista|lembretes?)\b"),
    (
        "pt",
        "edital",
        r"\bedital\b|\bpregao\b|\bchamamento publico\b",
    ),
    (
        "pt",
        "ato_normativo",
        r"\b(portaria|decreto|resolucao|instrucao normativa|lei)( [a-z]+)? n[o ]|\bpolitica de\b",
    ),
    (
        "pt",
        "parecer",
        r"\bparecer\b|\bnota tecnica\b|\blaudo tecnico\b",
    ),
    (
        "pt",
        "saude",
        r"\breceita\b|\bhemograma\b|\batestado medico\b|\bcrm\b|\bexame\b",
    ),
    (
        "pt",
        "declaracao_certidao",
        r"\bdeclar(o|acao)\b|\bcertidao\b|\batestado\b",
    ),
    ("pt", "relatorio", r"\brelatorio\b"),
    (
        "pt",
        "oficio_carta",
        r"\boficio\b|\brequerimento\b|\bmemorando\b|^prezad|^carta\b",
    ),
    (
        "pt",
        "email",
        r"^de \S+.*\b(para|assunto)\b|\bassunto\b.*\benviado\b",
    ),
    (
        "pt",
        "curriculo",
        r"\bcurriculo\b|\bexperiencia profissional\b",
    ),
    ("pt", "manual", r"\bmanual\b|\binstrucoes de uso\b"),
    ("pt", "orcamento", r"\borcamento\b|\bproposta comercial\b"),
    ("pt", "artigo", r"\bresumo\b.*\bmetodologia\b|\babstract\b"),
    (
        "en",
        "contrato",
        r"\b(contract|lease|rental agreement|service agreement|purchase agreement)\b",
    ),
    (
        "en",
        "fatura_boleto",
        r"\b(utility|electricity|water|gas|phone) bill\b|\bamount due\b",
    ),
    ("en", "nota_fiscal", r"\b(tax )?invoice\b"),
    ("en", "recibo", r"\breceipt\b"),
    (
        "en",
        "extrato",
        r"\b(bank|account) statement\b|\bstatement of account\b",
    ),
    (
        "en",
        "ata",
        r"^minutes\b|\bminutes of\b|\bmeeting minutes\b",
    ),
    (
        "en",
        "ato_normativo",
        r"\b(executive order|ordinance|final rule|proposed rule|interim rule|public law)\b|\bregulation (no|number)\b",
    ),
    (
        "en",
        "edital",
        r"\brequest for (proposals|applications|quotations)\b|\binvitation to bid\b|\bcall for (proposals|applications|tenders)\b|\bnotice of funding\b|\bsolicitation\b",
    ),
    (
        "en",
        "parecer",
        r"\b(legal|advisory|expert) opinion\b|\btechnical (note|opinion)\b",
    ),
    (
        "en",
        "saude",
        r"\bprescription\b|\bmedical certificate\b|\blab(oratory)? (results|report)\b|\bblood test\b",
    ),
    (
        "en",
        "declaracao_certidao",
        r"\b(certificate|certification|affidavit|attestation)\b",
    ),
    ("en", "relatorio", r"\breport\b"),
    ("en", "oficio_carta", r"^dear\b|\bmemorandum\b|\bmemo\b"),
    (
        "en",
        "email",
        r"^from \S+.*\b(to|subject)\b|\bsubject\b.*\bsent\b",
    ),
    (
        "en",
        "curriculo",
        r"\bresume\b|\bcurriculum vitae\b|\bwork experience\b",
    ),
    (
        "en",
        "manual",
        r"\buser (manual|guide)\b|\bquick start\b|\binstruction manual\b",
    ),
    ("en", "orcamento", r"\b(quote|quotation|estimate)\b"),
    ("en", "anotacoes", r"^(notes|to do|todo|shopping list)\b"),
    ("es", "contrato", r"\bcontrato\b"),
    (
        "es",
        "fatura_boleto",
        r"\b(recibo|factura) de (luz|agua|gas|telefono)\b",
    ),
    ("es", "nota_fiscal", r"\bfactura\b"),
    ("es", "extrato", r"\bextracto (bancario|de cuenta)\b"),
    ("es", "ata", r"^acta\b|\bacta d[eo]\b"),
    (
        "es",
        "ato_normativo",
        r"\b(real decreto|decreto|resolucion|ley|reglamento|ordenanza)( [a-z]+)? \d|\borden [a-z]{2,5} \d",
    ),
    (
        "es",
        "edital",
        r"\blicitacion\b|\bconvocatoria\b|\bpliego\b",
    ),
    ("es", "parecer", r"\bdictamen\b|\binforme tecnico\b"),
    (
        "es",
        "saude",
        r"\breceta\b|\brecetario\b|\banalisis clinicos\b|\bcertificado medico\b",
    ),
    (
        "es",
        "declaracao_certidao",
        r"\bcertificado\b|\bcertificacion\b|\bdeclaracion jurada\b|\bconstancia\b",
    ),
    ("es", "relatorio", r"\binforme\b|\bmemoria anual\b"),
    (
        "es",
        "oficio_carta",
        r"\boficio\b|\bsolicitud\b|^estimad[oa]s?\b|\bmemorando\b",
    ),
    ("es", "email", r"^de \S+.*\b(para|asunto)\b"),
    (
        "es",
        "curriculo",
        r"\bcurriculum\b|\bhoja de vida\b|\bexperiencia (laboral|profesional)\b",
    ),
    ("es", "manual", r"\bmanual\b|\binstrucciones\b"),
    ("es", "orcamento", r"\bpresupuesto\b|\bcotizacion\b"),
    ("es", "artigo", r"\bresumen\b.*\bmetodologia\b"),
    ("es", "anotacoes", r"^(notas|apuntes)\b"),
];

/// All rules as one alternation, one group per rule: the leftmost match wins and,
/// at the same position, the earlier rule (the tie order of `RULES`). One
/// automaton instead of one per rule keeps start-up time and memory small.
static COMBINED: LazyLock<Regex> = LazyLock::new(|| {
    // Groups inside a rule become non-capturing, so group i + 1 is rule i.
    let alternation: Vec<String> = RULES
        .iter()
        .map(|(_, _, pattern)| format!("({})", pattern.replace('(', "(?:")))
        .collect();
    Regex::new(&alternation.join("|")).expect("kind patterns are valid")
});

/// The kind named earliest in the document's head, with the language that named
/// it. A kind named only in passing, in prose below a heading, is skipped: a
/// checklist that cites "the annual Network Threat Report" is not a report; and
/// a "Case Report" is an article.
pub fn by_rules(text: &str) -> Option<(&'static str, &'static str)> {
    let lines = content_lines(first_chars(text, HEAD_CHARS));
    // The normalized head, line by line: `norm` turns a line break into one
    // space, so joining the normalized lines gives the same string, and the end
    // of each line in it tells which line a match falls on.
    let mut head = String::new();
    let mut ends = Vec::with_capacity(lines.len());
    for line in &lines {
        let normalized = norm(line);
        if !normalized.is_empty() && !head.is_empty() {
            head.push(' ');
        }
        head.push_str(&normalized);
        ends.push(head.len());
    }
    COMBINED.captures_iter(&head).find_map(|captures| {
        let start = captures.get(0)?.start();
        let line = ends.partition_point(|&end| end <= start);
        if in_passing(&lines, line) {
            return None;
        }
        let rule = (1..captures.len()).find(|&i| captures.get(i).is_some())?;
        let &(lang, kind, _) = RULES.get(rule - 1)?;
        // A journal's section ("Case Report", "Relatório especial") above the
        // title says the document is an article, whatever kind the words name.
        let kind = if is_article_type(lines[line]) {
            "artigo"
        } else {
            kind
        };
        Some((kind, lang))
    })
}

/// Whether a normalized line names some kind ("Ofício nº 027/2026" does).
pub fn names_a_kind(normalized: &str) -> bool {
    COMBINED.is_match(normalized)
}

/// Whether a normalized line opens with a kind ("real decreto 105 2025 de…").
pub fn opens_with_kind(normalized: &str) -> bool {
    COMBINED.find(normalized).is_some_and(|m| m.start() == 0)
}

/// The word a file name opens with for `kind`, in the language that named it,
/// only for kinds whose own words are evidence enough (an invoice says
/// "nota fiscal"); an article or a report is named by its title alone.
pub fn label(kind: &str, lang: &str) -> Option<&'static str> {
    let labels: &[(&str, &str)] = match lang {
        "en" => &[
            ("nota_fiscal", "Invoice"),
            ("recibo", "Receipt"),
            ("fatura_boleto", "Bill"),
            ("extrato", "Statement"),
            ("contrato", "Contract"),
        ],
        "es" => &[
            ("nota_fiscal", "Factura"),
            ("recibo", "Recibo"),
            ("fatura_boleto", "Cobro"),
            ("extrato", "Extracto"),
            ("contrato", "Contrato"),
        ],
        _ => &[
            ("nota_fiscal", "Nota fiscal"),
            ("recibo", "Recibo"),
            ("fatura_boleto", "Fatura"),
            ("extrato", "Extrato"),
            ("contrato", "Contrato"),
        ],
    };
    labels.iter().find(|(k, _)| *k == kind).map(|(_, w)| *w)
}
