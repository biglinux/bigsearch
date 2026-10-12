//! Content search: words of the query matched against the `body` field.
use super::{Hit, MatchKind, count_capped};
use crate::index::{BODY_QUERY, content_fields};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TokenStream;
use tantivy::{IndexReader, TantivyDocument, Term};

/// Full-text content search over the `body` field (word match, ranked by BM25).
pub fn search_content(reader: &IndexReader, q: &str, limit: usize) -> Result<Vec<Hit>> {
    let searcher = reader.searcher();
    let index = searcher.index();
    if !crate::settings::content_index_enabled() {
        return Ok(Vec::new());
    }
    let f = content_fields(index)?;
    let Some(query) = content_query(&searcher, q)? else {
        return Ok(Vec::new());
    };

    let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;

    let mut hits = Vec::with_capacity(top.len());
    let mut seen_paths = HashSet::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        if let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) {
            if !seen_paths.insert(path.to_string()) {
                continue;
            }
            let name = Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            hits.push(Hit {
                path: path.to_string(),
                name,
                score,
                source: crate::settings::source_name(Path::new(path)),
                // Found inside the file, not by its name. Said here rather than
                // only in `search_both`, so a plain `content` query carries it
                // too and the GUI can explain a result whose filename has
                // nothing to do with what was typed.
                match_kind: MatchKind::Body,
                ..Default::default()
            });
        }
    }
    Ok(hits)
}

/// The query for documents holding every word of `q`, the last one as typed
/// so far (see [`word_query`]); `None` for a query with no word.
fn content_query(searcher: &tantivy::Searcher, q: &str) -> Result<Option<BooleanQuery>> {
    let index = searcher.index();
    let f = content_fields(index)?;
    // Reader policy follows the index it opened, not the current writer's
    // memory profile/config. Resolve once, rather than rereading configuration
    // for every term; requesting frequencies from Basic cannot invent them.
    let schema = index.schema();
    let indexing = match schema.get_field_entry(f.body).field_type() {
        tantivy::schema::FieldType::Str(options) => options.get_indexing_options(),
        _ => None,
    }
    .context("body must be an indexed text field")?;
    let record = if indexing.index_option().has_freq() {
        IndexRecordOption::WithFreqs
    } else {
        IndexRecordOption::Basic
    };
    let mut analyzer = index
        .tokenizers()
        .get(BODY_QUERY)
        .context("body query tokenizer")?;
    let mut words: Vec<String> = Vec::new();
    let mut stream = analyzer.token_stream(q);
    while stream.advance() {
        words.push(stream.token().text.clone());
    }
    let Some(last) = words.pop() else {
        return Ok(None);
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(words.len() + 1);
    for word in &words {
        clauses.push((
            Occur::Must,
            word_query(searcher, f.body, word, record, false)?,
        ));
    }
    clauses.push((
        Occur::Must,
        word_query(searcher, f.body, &last, record, true)?,
    ));
    Ok(Some(BooleanQuery::new(clauses)))
}

/// Shortest last word read as a prefix: shorter ones begin too many words.
const MIN_PREFIX_CHARS: usize = 3;
/// Most indexed words a prefix stands for.
const MAX_PREFIX_TERMS: usize = 32;
/// Most documents, summed over the words a prefix stands for, read for it:
/// the bound that matters on a cold disk, where reads follow the postings.
const PREFIX_DOC_BUDGET: u64 = 50_000;
/// Indexed words looked at per segment for a prefix, against a very short
/// one in a large vocabulary.
const MAX_SCANNED_TERMS: usize = 20_000;

/// One word of a content query: the word itself, ranked first, or its plural
/// or singular (see [`inflections`]): "relatorios" finds "relatorio", "acao"
/// finds "acoes". The last word is also read as typed so far: it finds any
/// indexed word it begins (as-you-type in a launcher), the most frequent
/// first (see [`words_beginning`]). A Russian or Ukrainian word also matches
/// its other case endings (see [`slavic_stem`]). Every form keeps its BM25
/// score. A CJK bigram or character is matched exactly, and a last word
/// shorter than [`MIN_PREFIX_CHARS`] begins nothing.
fn word_query(
    searcher: &tantivy::Searcher,
    field: tantivy::schema::Field,
    word: &str,
    record: IndexRecordOption,
    last: bool,
) -> Result<Box<dyn Query>> {
    if word.chars().any(crate::cjk_tokenizer::is_unspaced) {
        return Ok(with_forms(word, Vec::new(), field, record));
    }
    // Only forms the index holds: a query term for a word no document has
    // would cost a lookup for nothing on every segment.
    let mut forms: Vec<String> = inflections(word)
        .into_iter()
        .filter(|form| {
            searcher.segment_readers().iter().any(|segment| {
                segment.inverted_index(field).is_ok_and(|inverted| {
                    inverted
                        .terms()
                        .get(form.as_bytes())
                        .is_ok_and(|info| info.is_some())
                })
            })
        })
        .collect();
    if let Some(stem) = slavic_stem(word) {
        forms.extend(words_beginning(searcher, field, stem, |rest| {
            rest.is_empty() || SLAVIC_ENDINGS.contains(&rest)
        })?);
    }
    if last && word.chars().count() >= MIN_PREFIX_CHARS {
        forms.extend(words_beginning(searcher, field, word, |rest| {
            !rest.is_empty()
        })?);
        // Indexed Arabic words lost their article (`index::ScriptFold`), which
        // a word still being typed keeps: "الات" is on its way to "الاتصال",
        // indexed as "اتصال".
        if let Some(rest) = crate::index::ARABIC_ARTICLES
            .iter()
            .find_map(|article| word.strip_prefix(article))
            .filter(|rest| rest.chars().count() >= 2)
        {
            forms.extend(words_beginning(searcher, field, rest, |_| true)?);
        }
    }
    Ok(with_forms(word, forms, field, record))
}

/// `word`, ranked first, or any of `forms` other than it.
fn with_forms(
    word: &str,
    mut forms: Vec<String>,
    field: tantivy::schema::Field,
    record: IndexRecordOption,
) -> Box<dyn Query> {
    let exact = TermQuery::new(Term::from_field_text(field, word), record);
    forms.retain(|form| form != word);
    if forms.is_empty() {
        return Box::new(exact);
    }
    forms.sort_unstable();
    forms.dedup();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(
        Occur::Should,
        Box::new(BoostQuery::new(Box::new(exact), EXACT_WORD_BOOST)),
    )];
    for form in forms {
        let term = Term::from_field_text(field, &form);
        clauses.push((Occur::Should, Box::new(TermQuery::new(term, record))));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// The indexed words that begin with `prefix` and go on with a rest `keep`
/// accepts, the most frequent first, within [`MAX_PREFIX_TERMS`] and
/// [`PREFIX_DOC_BUDGET`].
fn words_beginning(
    searcher: &tantivy::Searcher,
    field: tantivy::schema::Field,
    prefix: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let upper = prefix_end(prefix.as_bytes());
    let mut doc_freqs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(field)?;
        let mut range = inverted.terms().range().ge(prefix.as_bytes());
        if let Some(upper) = &upper {
            range = range.lt(upper);
        }
        let mut terms = range.into_stream()?;
        let mut scanned = 0;
        while scanned < MAX_SCANNED_TERMS && terms.advance() {
            scanned += 1;
            if let Ok(text) = std::str::from_utf8(terms.key())
                && text.strip_prefix(prefix).is_some_and(&keep)
            {
                *doc_freqs.entry(text.to_owned()).or_default() += u64::from(terms.value().doc_freq);
            }
        }
    }
    let mut expansions: Vec<(String, u64)> = doc_freqs.into_iter().collect();
    expansions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut forms = Vec::new();
    let mut read = 0u64;
    for (text, doc_freq) in expansions.into_iter().take(MAX_PREFIX_TERMS) {
        if read > 0 && read + doc_freq > PREFIX_DOC_BUDGET {
            break;
        }
        read += doc_freq;
        forms.push(text);
    }
    Ok(forms)
}

/// The plural and singular forms of a folded Portuguese, Spanish or English
/// word, by suffix: "acao" ↔ "acoes", "responsavel" ↔ "responsaveis",
/// "imagem" ↔ "imagens", "flor" ↔ "flores", "relatorio" ↔ "relatorios".
///
/// Snowball stemming does not fit folded words: it joins none of the
/// -ões/-ais/-eis/-ns plurals to their singulars and merges "casa" with
/// "caso", "casado" and "case". These rules join such pairs in both
/// directions. A form counts only if it has four
/// letters or more (against "tres" → "tre", "pais" → "pai"), and it ranks below
/// the word as typed.
fn inflections(word: &str) -> Vec<String> {
    const PLURAL_TO_SINGULAR: &[(&str, &[&str])] = &[
        ("oes", &["ao"]),
        ("aes", &["ao"]),
        ("aos", &["ao"]),
        ("ais", &["al"]),
        ("eis", &["el", "il"]),
        ("ois", &["ol"]),
        ("uis", &["ul"]),
        ("ns", &["m"]),
        ("res", &["r"]),
        ("zes", &["z"]),
        ("ses", &["s"]),
        ("xes", &["x"]),
        ("ches", &["ch"]),
        ("shes", &["sh"]),
        ("s", &[""]),
    ];
    const SINGULAR_TO_PLURAL: &[(&str, &[&str])] = &[
        ("ao", &["oes", "aes", "aos"]),
        ("al", &["ais"]),
        ("el", &["eis"]),
        ("il", &["eis"]),
        ("ol", &["ois"]),
        ("ul", &["uis"]),
        ("m", &["ns"]),
        ("r", &["res"]),
        ("z", &["zes"]),
        ("s", &["ses"]),
        ("x", &["xes"]),
        ("ch", &["ches"]),
        ("sh", &["shes"]),
        ("", &["s"]),
    ];
    let mut forms: Vec<String> = Vec::new();
    if !word.is_ascii() || word.len() < 4 {
        return forms;
    }
    for (from, tos) in PLURAL_TO_SINGULAR.iter().chain(SINGULAR_TO_PLURAL) {
        let Some(base) = word.strip_suffix(from) else {
            continue;
        };
        if base.len() < 2 {
            continue;
        }
        for to in *tos {
            let form = format!("{base}{to}");
            if form.len() >= 4 && !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    forms
}

/// Russian and Ukrainian noun, adjective and participle endings, folded ("ё"
/// is "е"): "квартал", "квартала", "квартале", "кварталами" are one word.
const SLAVIC_ENDINGS: &[&str] = &[
    "а", "я", "о", "е", "ы", "и", "у", "ю", "ь", "і", "ї", "є", "ом", "ем", "ой", "ей", "ою", "ею",
    "ам", "ям", "ах", "ях", "ов", "ев", "ів", "їв", "ий", "ій", "ия", "ие", "ии", "ию", "ью", "ья",
    "ье", "ьи", "ый", "ая", "яя", "ое", "ее", "ые", "ым", "им", "ім", "ую", "юю", "ых", "их", "іх",
    "ої", "ові", "еві", "ами", "ями", "ого", "его", "ього", "ому", "ему", "ьому", "ыми", "ими",
    "іми", "ием", "иям", "иях", "ьям", "ьях", "ьев", "иями", "ьями",
];

/// The stem of a folded Russian or Ukrainian word: the word without the
/// longest of [`SLAVIC_ENDINGS`] that leaves four letters or more, or the word
/// itself ("квартал"). A shorter stem joins unrelated words: "сто" (a
/// hundred) would find "стой" (stop) and "стою" (I stand).
fn slavic_stem(word: &str) -> Option<&str> {
    if !word
        .chars()
        .all(|c| matches!(c, 'а'..='я' | 'і' | 'ї' | 'є' | 'ґ'))
    {
        return None;
    }
    let stem = SLAVIC_ENDINGS
        .iter()
        .filter_map(|ending| word.strip_suffix(ending))
        .filter(|stem| stem.chars().count() >= 4)
        .min_by_key(|stem| stem.len())
        .unwrap_or(word);
    (stem.chars().count() >= 4).then_some(stem)
}

/// The word as typed weighs this much more than the words it begins.
const EXACT_WORD_BOOST: f32 = 2.0;

/// The first byte string after every string starting with `prefix`; `None`
/// when there is none (all 0xFF).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// How many documents hold `q`'s words, up to `cap`, from the postings alone.
pub(super) fn count_content(reader: &IndexReader, q: &str, cap: usize) -> Result<usize> {
    let searcher = reader.searcher();
    match content_query(&searcher, q)? {
        Some(query) => count_capped(&searcher, &query, cap),
        None => Ok(0),
    }
}

#[cfg(all(test, not(miri)))]
mod tests;
