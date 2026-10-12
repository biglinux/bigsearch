// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! What a document is, read from its text alone, and a file name built from that.
//!
//! Given the text a desktop extractor gets out of a file (`pdftotext`, the XML of
//! an OpenDocument), [`analyze`] finds the document's kind (contract, invoice,
//! minutes…), the date that identifies it, who it belongs to, its title and a few
//! sentences worth showing, then builds a file name such as
//! `contrato-locacao-residencial-2026-04-22.pdf`.
//!
//! ## No model
//!
//! Everything here is rules and counting. Small language models were measured on
//! the same task and corpus: on an old dual-core laptop they took a minute or
//! more per file, and they turned safe "don't know" answers into wrong dates and
//! wrong kinds. The rules answer in milliseconds and leave a field empty when the
//! text gives no evidence.
//!
//! ## Empty beats wrong
//!
//! Every field is an `Option`. A wrong date in a file name is worse than no date,
//! so each finder abstains unless its evidence wins clearly; the file name is
//! assembled here from whatever survived, never written by guesswork.
//!
//! ## Reference implementation
//!
//! `tests/parity.rs` checks every field against recorded answers, first taken
//! from a Python reference measured on an evaluation corpus and re-recorded when
//! a measured change improves them.

mod date;
mod entity;
mod highlights;
mod kind;
mod months;
mod name;
mod text;

/// What [`analyze`] found. `None` means the text gave no clear evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocInfo {
    /// Document kind as a stable slug (`contrato`, `nota_fiscal`, `ata`…).
    pub kind: Option<&'static str>,
    /// The date that identifies the document (issue, signature, meeting…), ISO 8601.
    pub date: Option<String>,
    /// Who the document belongs to or is about: issuer, party, sender, holder.
    pub entity: Option<String>,
    /// The title as written in the document.
    pub title: Option<String>,
    /// A few sentences of the document itself, in reading order; `None` for
    /// documents made of tables and fields, where sentences say little.
    pub highlights: Option<String>,
    /// File name suggestion: kind, subject and date, with `ext` kept as given.
    pub suggested_name: String,
}

/// What a file says about itself besides its text. Both are hints: the title
/// the metadata declares counts only when the text bears it out, and a
/// prominent line only when it reads as a title.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hints<'a> {
    /// The title the file's metadata declares.
    pub declared_title: Option<&'a str>,
    /// Lines set in large type at the opening, largest first.
    pub prominent: &'a [String],
}

/// The score from which a date goes into the file name: on the evaluation
/// corpus it keeps 80 of 95 right dates and drops the weak ones (a signature
/// stamp, the day an article was received).
const STRONG_DATE: i32 = 3;

/// Reads `text` (the extracted content of a file whose extension is `ext`).
#[must_use]
pub fn analyze(text: &str, ext: &str, hints: &Hints<'_>) -> DocInfo {
    let found = kind::by_rules(text);
    let kind = found.map(|(kind, _)| kind);
    let scored = date::scored_date(text);
    // A title shows as written, without the invisible hyphenation points
    // ("rec\u{AD}om\u{AD}men\u{AD}da\u{AD}tion") typeset books leave in their text.
    let title = hints
        .declared_title
        .and_then(|declared| entity::declared_title(declared, text))
        .or_else(|| entity::title(text, hints.prominent))
        .map(|title| title.replace('\u{AD}', ""));
    // The name says the kind only where the title does not ("Nota fiscal
    // Padaria Pão Quente", not "Nota fiscal Nota fiscal de serviços").
    let label = found
        .and_then(|(kind, lang)| kind::label(kind, lang))
        .filter(|label| {
            !title
                .as_deref()
                .is_some_and(|t| text::fold_lower(t).contains(&text::fold_lower(label)))
        });
    // And the date only on strong evidence: a signature line, "emitido em", a
    // dateline. A weaker one is still shown in the details.
    let name_date = scored
        .as_ref()
        .filter(|(_, score)| *score >= STRONG_DATE)
        .map(|(iso, _)| iso.as_str());
    let suggested_name = name::suggested_name(label, title.as_deref(), name_date, ext);
    let date = scored.map(|(iso, _)| iso);
    DocInfo {
        kind,
        entity: entity::main_entity(text, kind),
        highlights: highlights::extract(text),
        suggested_name,
        date,
        title,
    }
}
