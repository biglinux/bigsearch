// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Document kind from the words at its top.
//!
//! A document usually names itself in its first lines ("CONTRATO DE LOCAÇÃO",
//! "ATA DA REUNIÃO", "DANFE"). Each kind has a pattern over the normalized first
//! 600 characters; the kind whose pattern hits earliest wins. No hit, no kind.

use std::sync::LazyLock;

use regex::Regex;

use crate::text::{first_chars, norm};

const HEAD_CHARS: usize = 600;

/// (kind, pattern). Order breaks ties at the same position: "extrato de
/// contrato" is a contract, not a bank statement, so `contrato` comes first.
const RULES: &[(&str, &str)] = &[
    (
        "contrato",
        r"\bextrato (do|de) (contrato|termo|aditivo)|\btermo aditivo\b|\bcontrato\b",
    ),
    (
        "nota_fiscal",
        r"\bnota fiscal\b|\bnfs? e\b|\bdanfe\b|\bcupom fiscal\b",
    ),
    ("recibo", r"\brecibo\b"),
    (
        "fatura_boleto",
        r"\bfatura\b|\bboleto\b|\blinha digitavel\b|\bconta de (energia|agua|luz|gas)\b",
    ),
    (
        "extrato",
        r"\bextrato (bancario|de conta|mensal)\b|\bsaldo anterior\b",
    ),
    ("ata", r"^ata\b|\bata d[aeo]s?\b"),
    ("anotacoes", r"^(anotacoes|notas|lista|lembretes?)\b"),
    ("edital", r"\bedital\b|\bpregao\b|\bchamamento publico\b"),
    (
        "ato_normativo",
        r"\b(portaria|decreto|resolucao|instrucao normativa|lei)( [a-z]+)? n[o ]|\bpolitica de\b",
    ),
    ("parecer", r"\bparecer\b|\bnota tecnica\b|\blaudo tecnico\b"),
    (
        "saude",
        r"\breceita\b|\bhemograma\b|\batestado medico\b|\bcrm\b|\bexame\b",
    ),
    (
        "declaracao_certidao",
        r"\bdeclar(o|acao)\b|\bcertidao\b|\batestado\b",
    ),
    ("relatorio", r"\brelatorio\b"),
    (
        "oficio_carta",
        r"\boficio\b|\brequerimento\b|\bmemorando\b|^prezad|^carta\b",
    ),
    (
        "email",
        r"^de \S+.*\b(para|assunto)\b|\bassunto\b.*\benviado\b",
    ),
    ("curriculo", r"\bcurriculo\b|\bexperiencia profissional\b"),
    ("manual", r"\bmanual\b|\binstrucoes de uso\b"),
    ("orcamento", r"\borcamento\b|\bproposta comercial\b"),
    ("artigo", r"\bresumo\b.*\bmetodologia\b|\babstract\b"),
];

static COMPILED: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    RULES
        .iter()
        .map(|&(kind, pattern)| (kind, Regex::new(pattern).expect("kind patterns are valid")))
        .collect()
});

/// The kind named earliest in the document's head, if any.
pub fn by_rules(text: &str) -> Option<&'static str> {
    let head = norm(first_chars(text, HEAD_CHARS));
    let mut best: Option<(usize, &'static str)> = None;
    for (kind, re) in COMPILED.iter() {
        if let Some(m) = re.find(&head)
            && best.is_none_or(|(start, _)| m.start() < start)
        {
            best = Some((m.start(), kind));
        }
    }
    best.map(|(_, kind)| kind)
}

/// Whether a normalized line names some kind ("Ofício nº 027/2026" does).
pub fn names_a_kind(normalized: &str) -> bool {
    COMPILED.iter().any(|(_, re)| re.is_match(normalized))
}
