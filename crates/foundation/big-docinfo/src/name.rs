// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The suggested file name: `kind-subject-date.ext`, built from fields, never guessed.
//!
//! Parts with no evidence are left out; the extension is the one the file already
//! has. Words are folded to lowercase ASCII where the language allows and joined
//! with hyphens, so the name carries no character a file system or a shell would
//! trip on.

use crate::text::norm;

/// Longest name before the extension is added, in characters.
const MAX_NAME: usize = 70;

/// Word used in the name for kinds whose slug is not a good word on its own.
fn kind_word(kind: &str) -> &str {
    match kind {
        "nota_fiscal" => "nota-fiscal",
        "fatura_boleto" => "fatura",
        "ato_normativo" => "norma",
        "oficio_carta" => "oficio",
        "declaracao_certidao" => "declaracao",
        "anotacoes" => "notas",
        other => other,
    }
}

fn slug(s: &str) -> String {
    norm(s).replace(' ', "-")
}

/// Builds `kind-subject-date.ext`. With nothing known the stem is `documento`.
#[must_use]
pub fn suggested_name(
    kind: Option<&str>,
    subject: Option<&str>,
    date: Option<&str>,
    ext: &str,
) -> String {
    let word = slug(kind.map_or("", kind_word));
    let mut topic = slug(subject.unwrap_or(""));
    // "ata" + "ata-da-reuniao-..." must not become "ata-ata-da-reuniao-...".
    let mut prefixes = vec![word.clone(), slug(kind.unwrap_or("")).replace('_', "-")];
    prefixes.retain(|p| !p.is_empty());
    prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    prefixes.dedup();
    for prefix in &prefixes {
        if topic == *prefix || topic.starts_with(&format!("{prefix}-")) {
            let rest = topic[prefix.len()..].trim_start_matches('-');
            let rest = ["de-", "da-", "do-", "das-", "dos-"]
                .iter()
                .find_map(|article| rest.strip_prefix(article))
                .unwrap_or(rest);
            topic = rest.to_owned();
            break;
        }
    }
    let stem = [word.as_str(), topic.as_str()]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("-");
    let date = date.unwrap_or("");
    let room = MAX_NAME
        .saturating_sub(ext.chars().count() + 1)
        .saturating_sub(if date.is_empty() {
            0
        } else {
            date.chars().count() + 1
        });
    let cut: String = stem.chars().take(room).collect();
    let mut stem = cut.trim_end_matches('-').to_owned();
    if stem.is_empty() {
        stem = "documento".to_owned();
    }
    let dotted_ext = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    if date.is_empty() {
        format!("{stem}{dotted_ext}")
    } else {
        format!("{stem}-{date}{dotted_ext}")
    }
}
