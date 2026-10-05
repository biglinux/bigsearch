// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The suggested file name: `kind-subject-date.ext`, built from fields, never guessed.
//!
//! Parts with no evidence are left out; the extension is the one the file already
//! has. Words keep their letters as written — `relatório`, `für`, `договор`,
//! `房屋租赁合同` — lowercased and joined with hyphens, so the name carries no
//! character a file system or a shell would trip on.

use crate::text::{fold_lower, is_word};

/// Longest name before the extension is added, in characters.
const MAX_NAME: usize = 70;
/// Words dropped right after a kind word the title repeats ("Ata *da* reunião").
const LINKING: &[&str] = &["de", "da", "do", "das", "dos", "del", "of"];

/// Lowercased words of `s`, everything that is not a letter or a digit dropped.
/// Invisible hyphenation points (U+00AD, "rec­om­men­da­tion") join, not split.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .replace(['\u{AD}', '\u{200B}', '\u{2060}'], "")
        .split(|c: char| !is_word(c))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds `kind-subject-date.ext`; `kind` is the word for the kind in the
/// document's language. With nothing known the stem is `documento`.
pub fn suggested_name(
    kind: Option<&str>,
    subject: Option<&str>,
    date: Option<&str>,
    ext: &str,
) -> String {
    let kind_words = words(kind.unwrap_or(""));
    let mut topic = words(subject.unwrap_or(""));
    // "relatório" + "Relatório trimestral" must not become "relatório-relatório-trimestral".
    let folded = |w: &[String]| w.iter().map(|x| fold_lower(x)).collect::<Vec<_>>();
    if !kind_words.is_empty() && folded(&topic).starts_with(&folded(&kind_words)) {
        topic.drain(..kind_words.len());
        if topic
            .first()
            .is_some_and(|w| LINKING.contains(&fold_lower(w).as_str()))
        {
            topic.remove(0);
        }
    }
    let stem = kind_words
        .into_iter()
        .chain(topic)
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
