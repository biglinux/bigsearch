// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! A few sentences of the document itself, as a preview ("trechos principais").
//!
//! Nothing is rewritten, so nothing can be invented: sentences are scored by how
//! close they are to the document as a whole (TF-IDF, cosine to the centroid),
//! picked greedily against redundancy (MMR) up to about 90 words, and shown in
//! reading order. Documents made of short lines (bills, forms, lists) get no
//! preview: their fields say more than any sentence.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::text::{first_chars, lines, norm, word_count};

const MAX_WORDS: usize = 90;
/// The preview reads the first characters and sentences only.
const HEAD_CHARS: usize = 100_000;
const MAX_SENTENCES: usize = 400;
const DIVERSITY: f64 = 0.4;
/// List items and "Label: value" lines are fields, not sentences.
static LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([-•*]|\d+[.)]\s|[A-ZÀ-Ý][\wÀ-ÿ /]*:\s)").expect("valid"));

/// A list item, or a "Label: value" field whose label has at most 26 characters
/// (bounded here: a counted repeat of `\w` compiles into that many copies of it).
fn is_label(line: &str) -> bool {
    LABEL.find(line).is_some_and(|m| {
        let found = m.as_str();
        !found.ends_with(char::is_whitespace) || found.chars().count() <= 28
    })
}

pub fn extract(text: &str) -> Option<String> {
    // A preview comes from the opening; reading a whole book for it would cost
    // time and memory that grow with the file.
    let text = first_chars(text, HEAD_CHARS);
    let mut sents = sentences(text);
    sents.truncate(MAX_SENTENCES);
    if looks_structured(text) || sents.len() < 2 {
        return None;
    }
    let (vecs, terms) = tfidf(&sents);
    let mut centroid = vec![0.0; terms];
    for v in &vecs {
        for &(j, x) in v {
            centroid[j] += x;
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a document has far fewer than 2^52 sentences"
    )]
    let n = vecs.len() as f64;
    for c in &mut centroid {
        *c /= n;
    }
    let length = centroid.iter().map(|c| c * c).sum::<f64>().sqrt();
    let length = if length == 0.0 { 1.0 } else { length };
    for c in &mut centroid {
        *c /= length;
    }
    Some(pick(&vecs, &centroid, &sents))
}

/// Candidate sentences: wrapped lines rejoined, headers and label lines dropped.
fn sentences(text: &str) -> Vec<String> {
    let mut paras: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in lines(text) {
        let line = line.trim();
        let open_end = !cur.is_empty() && !cur.ends_with(['.', '!', '?', ':', ';']);
        if line.is_empty() {
            paras.push(std::mem::take(&mut cur));
        } else if open_end
            && !is_label(line)
            && (line.chars().next().is_some_and(char::is_lowercase) || word_count(&cur) >= 8)
        {
            cur.push(' ');
            cur.push_str(line);
        } else {
            paras.push(std::mem::replace(&mut cur, line.to_owned()));
        }
    }
    paras.push(cur);
    let mut out = Vec::new();
    for para in &paras {
        for sent in split_sentences(para) {
            let letters: Vec<char> = sent.chars().filter(|c| c.is_alphabetic()).collect();
            let upper = letters.iter().filter(|c| c.is_uppercase()).count();
            if word_count(sent) >= 6
                && !letters.is_empty()
                && upper * 2 < letters.len()
                && !is_label(sent)
            {
                out.push(sent.trim().to_owned());
            }
        }
    }
    out
}

/// Splits after `.`, `!` or `?` when whitespace follows and the next sentence
/// starts with a capital, a digit, a quote or a parenthesis.
fn split_sentences(para: &str) -> Vec<&str> {
    let starts_sentence =
        |c: char| c.is_ascii_digit() || matches!(c, 'A'..='Z' | 'À'..='Ý' | '"' | '“' | '(');
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut chars = para.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_whitespace() && prev.is_some_and(|p| matches!(p, '.' | '!' | '?')) {
            let mut end = i + c.len_utf8();
            while let Some(&(j, w)) = chars.peek().filter(|(_, w)| w.is_whitespace()) {
                end = j + w.len_utf8();
                chars.next();
            }
            if chars.peek().is_some_and(|&(_, n)| starts_sentence(n)) {
                out.push(&para[start..i]);
                start = end;
            }
            prev = None;
            continue;
        }
        prev = Some(c);
    }
    out.push(&para[start..]);
    out
}

fn looks_structured(text: &str) -> bool {
    let content: Vec<&str> = lines(text)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let short = content.iter().filter(|l| word_count(l) <= 6).count();
    content.is_empty() || short * 5 > content.len() * 3
}

/// A sentence as (term, weight) pairs sorted by term: a few dozen entries, where
/// a dense row would hold the whole vocabulary.
type Sparse = Vec<(usize, f64)>;

/// Rows of L2-normalized TF-IDF: sublinear term frequency, smoothed inverse
/// document frequency over the sentences, words of two characters or more.
fn tfidf(sents: &[String]) -> (Vec<Sparse>, usize) {
    let docs: Vec<HashMap<String, usize>> = sents
        .iter()
        .map(|s| {
            let mut counts = HashMap::new();
            for w in norm(s).split(' ').filter(|w| w.chars().count() >= 2) {
                *counts.entry(w.to_owned()).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let mut vocab: Vec<&String> = docs.iter().flat_map(HashMap::keys).collect();
    vocab.sort();
    vocab.dedup();
    let index: HashMap<&String, usize> = vocab.iter().enumerate().map(|(i, w)| (*w, i)).collect();
    let mut df = vec![0usize; vocab.len()];
    for d in &docs {
        for w in d.keys() {
            df[index[w]] += 1;
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "counts are far below 2^52")]
    let n = docs.len() as f64;
    #[expect(clippy::cast_precision_loss, reason = "counts are far below 2^52")]
    let idf: Vec<f64> = df
        .iter()
        .map(|&d| ((1.0 + n) / (1.0 + d as f64)).ln() + 1.0)
        .collect();
    let rows = docs
        .iter()
        .map(|d| {
            let mut v: Sparse = d
                .iter()
                .map(|(w, &count)| {
                    let j = index[w];
                    #[expect(clippy::cast_precision_loss, reason = "counts are far below 2^52")]
                    let tf = 1.0 + (count as f64).ln();
                    (j, tf * idf[j])
                })
                .collect();
            v.sort_unstable_by_key(|&(j, _)| j);
            let length = v.iter().map(|(_, x)| x * x).sum::<f64>().sqrt();
            if length > 0.0 {
                for (_, x) in &mut v {
                    *x /= length;
                }
            }
            // The reference divided once more by (length + 1e-9); kept for stable output.
            let length = v.iter().map(|(_, x)| x * x).sum::<f64>().sqrt() + 1e-9;
            for (_, x) in &mut v {
                *x /= length;
            }
            v
        })
        .collect();
    (rows, vocab.len())
}

/// Greedy MMR: relevance to the centroid minus similarity to what is already chosen.
fn pick(vecs: &[Sparse], centroid: &[f64], sents: &[String]) -> String {
    let rel: Vec<f64> = vecs
        .iter()
        .map(|v| v.iter().map(|&(j, x)| x * centroid[j]).sum())
        .collect();
    let mut chosen: Vec<usize> = Vec::new();
    let mut words = 0;
    while chosen.len() < sents.len() {
        let mut best: Option<(f64, usize)> = None;
        for i in (0..sents.len()).filter(|i| !chosen.contains(i)) {
            let redundancy = chosen
                .iter()
                .map(|&j| dot(&vecs[i], &vecs[j]))
                .fold(0.0, f64::max);
            // Rounded so exact ties go to the earlier sentence whatever the summation order.
            let score = (((1.0 - DIVERSITY) * rel[i] - DIVERSITY * redundancy) * 1e9).round();
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, i));
            }
        }
        let Some((_, i)) = best else { break };
        let n = word_count(&sents[i]);
        if words > 0 && words + n > MAX_WORDS {
            break;
        }
        chosen.push(i);
        words += n;
    }
    chosen.sort_unstable();
    chosen
        .iter()
        .map(|&i| sents[i].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Dot product of two term-sorted sparse rows.
fn dot(a: &Sparse, b: &Sparse) -> f64 {
    let (mut i, mut j, mut sum) = (0, 0, 0.0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                sum += a[i].1 * b[j].1;
                i += 1;
                j += 1;
            }
        }
    }
    sum
}
