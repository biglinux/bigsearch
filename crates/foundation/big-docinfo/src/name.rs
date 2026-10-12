// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The suggested file name: `[date] [kind] title.ext`, built from fields, never
//! guessed.
//!
//! The title reads as the document writes it (capitals, spaces, accents and
//! all), so the name is the one a person would type. Parts with no evidence are
//! left out; the extension is the one the file already has. The characters that
//! break a path or a FAT/exFAT pen drive (`/ \ : * ? " < > |`) are replaced, a
//! title in capitals is set in sentence case (acronyms kept), and a long title
//! is cut between words.

use crate::text::fold_lower;

/// Longest name before the extension, in characters.
const MAX_NAME: usize = 120;
/// Longest file name a Linux file system takes, in bytes (`NAME_MAX`).
const NAME_MAX_BYTES: usize = 255;
/// Words a cut title does not end on.
const DANGLING: &[&str] = &[
    "a", "as", "o", "os", "e", "de", "da", "do", "das", "dos", "em", "na", "no", "um", "se", "ao",
    "aos", "nas", "nos", "para", "por", "com", "of", "the", "and", "for", "to", "in", "on", "at",
    "by", "an", "or", "is", "it", "with", "del", "la", "el", "lo", "al", "y", "en", "un", "su",
    "con",
];

/// Builds `[date] [kind] title.ext`. `kind` is the word for the kind, given only
/// when the title does not already say it; `date` only when the evidence for it
/// is strong. With neither kind nor title the stem is `documento`.
pub fn suggested_name(
    kind: Option<&str>,
    title: Option<&str>,
    date: Option<&str>,
    ext: &str,
) -> String {
    let dotted_ext = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    let mut lead = String::new();
    for part in [date, kind].into_iter().flatten() {
        lead.push_str(part);
        lead.push(' ');
    }
    let room_chars = MAX_NAME.saturating_sub(lead.chars().count());
    let room_bytes = NAME_MAX_BYTES.saturating_sub(lead.len() + dotted_ext.len());
    let title = title.map(readable).unwrap_or_default();
    let title = tidy_end(&cut(&title, room_chars, room_bytes));
    // A title that was nothing but punctuation and joining words ("; o") is none.
    if title.is_empty() && kind.is_none() {
        return format!("documento{dotted_ext}");
    }
    format!("{}{dotted_ext}", format!("{lead}{title}").trim())
}

/// `title` as a file name part: characters a path or a FAT drive refuses
/// replaced, spaces collapsed, no leading dot (a hidden file), capitals tamed.
fn readable(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut chars = title.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // "Gene therapy: Practical aspects" → "Gene therapy - Practical aspects"
            ':' if chars.peek().is_some_and(|n| n.is_whitespace()) => out.push_str(" -"),
            '/' | '\\' | ':' | '|' => out.push('-'),
            '*' | '?' | '"' | '<' | '>' => {}
            c if c.is_control() || c == '\u{AD}' => out.push(' '),
            c => out.push(c),
        }
    }
    // "aguardando ...... orçamento": a run of dots is a gap, not part of the name.
    let out = out
        .split_whitespace()
        .filter(|w| !(w.len() >= 3 && w.chars().all(|c| c == '.')))
        .collect::<Vec<_>>()
        .join(" ");
    sentence_case(out.trim_start_matches(['.', '-', ' ']))
}

/// A title written in capitals, tamed: a short one (up to six words, usually a
/// name) in title case ("KAUAN RODRIGUES DE LIMA" → "Kauan Rodrigues de Lima"),
/// a longer one in sentence case ("CONTRATO DE LOCAÇÃO DA SALA" → "Contrato de
/// locação da sala"). Words with digits (TF-8-SECEC/2025), parts without vowels
/// (PBM, CNPJ, the "NFS" of "NFS-e") and abbreviations (S.A.) stay as written. A
/// title mostly in lower case is left alone.
fn sentence_case(title: &str) -> String {
    // "º" counts as lower case but has no capital: it does not make "Nº" mixed.
    let cased_lower = |c: &char| c.is_lowercase() && c.to_uppercase().next() != Some(*c);
    let lower = title.chars().filter(cased_lower).count();
    let upper = title.chars().filter(|c| c.is_uppercase()).count();
    if upper == 0 || lower * 5 > upper {
        return title.to_owned();
    }
    let short = title.split_whitespace().count() <= 6;
    let vowel = |c: char| "aeiouyáéíóúâêôãõàäëïöüåæøœºª".contains(c);
    let capital = |part: &str| {
        let mut chars = part.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_uppercase().chain(chars).collect()
        })
    };
    let mut out = Vec::new();
    for (i, word) in title.split(' ').enumerate() {
        if word.chars().any(|c| c.is_ascii_digit()) {
            out.push(word.to_owned());
            continue;
        }
        // Each part of "PEMBERTON-SHAW" or "SEI/GDF" on its own.
        let mut tamed = String::new();
        let mut part = String::new();
        let flush = |part: &mut String, tamed: &mut String| {
            let folded = part.to_lowercase();
            let core = folded.trim_matches(|c: char| !c.is_alphabetic());
            // "S.A.", "P.C.", "UK" and the "PE" of "CRM-PE" keep their capitals too.
            let tail = core.chars().count() <= 2 && !DANGLING.contains(&core);
            tamed.push_str(&if !core.chars().any(vowel) || core.contains('.') || tail {
                std::mem::take(part)
            } else if (short && !DANGLING.contains(&core)) || (i == 0 && tamed.is_empty()) {
                capital(&folded)
            } else {
                folded
            });
            part.clear();
        };
        for c in word.chars() {
            if c == '-' || c == '/' {
                flush(&mut part, &mut tamed);
                tamed.push(c);
            } else {
                part.push(c);
            }
        }
        flush(&mut part, &mut tamed);
        out.push(tamed);
    }
    out.join(" ")
}

/// `title` cut to `chars` characters and `bytes` bytes, between words where it
/// has spaces.
fn cut(title: &str, chars: usize, bytes: usize) -> String {
    if title.chars().count() <= chars && title.len() <= bytes {
        return title.to_owned();
    }
    let mut end = 0;
    for (count, (at, c)) in title.char_indices().enumerate() {
        if count >= chars || at + c.len_utf8() > bytes {
            break;
        }
        end = at + c.len_utf8();
    }
    let mut kept = &title[..end];
    // Between words, when the title has them (Chinese and Japanese do not).
    if let Some(space) = kept.rfind(' ')
        && title[end..].starts_with(|c: char| !c.is_whitespace())
    {
        kept = &kept[..space];
    }
    kept.to_owned()
}

/// No dangling "de" or "of", trailing punctuation or final dot ("S.A." before
/// ".pdf" would read "S.A..pdf") at the end of a title, cut or not.
fn tidy_end(title: &str) -> String {
    let mut words: Vec<&str> = title.split(' ').collect();
    while words.len() > 1
        && words
            .last()
            .is_some_and(|w| DANGLING.contains(&fold_lower(w).as_str()))
    {
        words.pop();
    }
    words
        .join(" ")
        .trim_end_matches(|c: char| c.is_whitespace() || ",;.-–—(".contains(c))
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_as_the_title_is_written() {
        let name = |kind, title, date| suggested_name(kind, title, date, "pdf");
        assert_eq!(
            name(
                None,
                Some("COVID-19 Thrombotic Complications and Therapeutic Strategies"),
                Some("2022-09-21")
            ),
            "2022-09-21 COVID-19 Thrombotic Complications and Therapeutic Strategies.pdf"
        );
        assert_eq!(
            name(
                None,
                Some("Gene therapy: Practical aspects of implementation"),
                None
            ),
            "Gene therapy - Practical aspects of implementation.pdf"
        );
        assert_eq!(
            name(
                None,
                Some("TERMO DE FOMENTO Nº TF-8-SECEC/2025 DA SECRETARIA PBM"),
                None
            ),
            "Termo de fomento Nº TF-8-SECEC-2025 da secretaria PBM.pdf"
        );
        assert_eq!(
            name(
                Some("Nota fiscal"),
                Some("Padaria Pão Quente"),
                Some("2026-03-14")
            ),
            "2026-03-14 Nota fiscal Padaria Pão Quente.pdf"
        );
        assert_eq!(
            name(None, Some("KAUAN RODRIGUES DE LIMA"), None),
            "Kauan Rodrigues de Lima.pdf"
        );
        assert_eq!(
            name(None, Some("VINOS Y ACEITES DEL MAULE S.A."), None),
            "Vinos y Aceites del Maule S.A.pdf"
        );
        assert_eq!(name(None, Some(".hidden?"), None), "hidden.pdf");
        assert_eq!(name(None, None, Some("2026-03-14")), "documento.pdf");
        assert_eq!(name(None, Some("; o"), None), "documento.pdf");
    }

    #[test]
    fn long_titles_cut_between_words() {
        let long = "Development and validation of a clinical prediction model for ninety day venous thromboembolism risk after hospital discharge in adults";
        let name = suggested_name(None, Some(long), Some("2023-06-12"), "pdf");
        let stem = name.trim_end_matches(".pdf");
        assert!(stem.chars().count() <= MAX_NAME, "{name}");
        assert!(
            long.contains(stem.trim_start_matches("2023-06-12 ")),
            "{name}"
        );
        assert!(!stem.ends_with(" in") && !stem.ends_with(" for"), "{name}");
        let cjk = "房屋租赁合同".repeat(40);
        let name = suggested_name(None, Some(&cjk), None, "pdf");
        assert!(
            name.len() <= NAME_MAX_BYTES && name.ends_with(".pdf"),
            "{name}"
        );
    }
}
