// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Every field of every fixture document, against a recorded answer.
//!
//! `fixtures/expected.jsonl` holds the recorded answers. A behaviour change is
//! measured on the evaluation corpus, and the answers are re-recorded with
//! `BIG_DOCINFO_BLESS=1 cargo test -p big-docinfo`.
//! The public documents of the corpus carry real names and stay out of this
//! repository; point `BIG_DOCINFO_CORPUS` at the corpus `docs/` directory and
//! `BIG_DOCINFO_EXPECTED` at a recorded file to check them (`cargo test -- --ignored`).

use std::fmt::Write as _;
use std::path::Path;

use big_docinfo::{Hints, analyze};
use serde_json::Value;

fn read_doc(path: &Path) -> String {
    // The reference reads text files with universal newlines.
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Re-records `expected` with the current answers for the same documents.
fn bless(docs: &Path, expected: &Path) {
    let lines = std::fs::read_to_string(expected).expect("expected file");
    let mut out = String::new();
    for line in lines.lines() {
        let id = serde_json::from_str::<Value>(line).expect("JSON")["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let got = analyze(
            &read_doc(&docs.join(format!("{id}.txt"))),
            "pdf",
            &Hints::default(),
        );
        let row = serde_json::json!({
            "id": id, "kind": got.kind, "date": got.date, "entity": got.entity,
            "title": got.title, "highlights": got.highlights, "name": got.suggested_name,
        });
        let _ = writeln!(out, "{row}");
    }
    std::fs::write(expected, out).expect("write expected");
}

/// Every field of every document that differs from the recording, one per line.
fn mismatches(docs: &Path, expected: &Path) -> (usize, String) {
    if std::env::var_os("BIG_DOCINFO_BLESS").is_some() {
        bless(docs, expected);
    }
    let lines =
        std::fs::read_to_string(expected).unwrap_or_else(|e| panic!("{}: {e}", expected.display()));
    let mut report = String::new();
    let mut count = 0;
    for line in lines.lines() {
        let want: Value = serde_json::from_str(line).expect("reference lines are JSON");
        let id = want["id"].as_str().expect("id is a string");
        let got = analyze(
            &read_doc(&docs.join(format!("{id}.txt"))),
            "pdf",
            &Hints::default(),
        );
        let fields = [
            ("kind", got.kind.map(str::to_owned)),
            ("date", got.date),
            ("entity", got.entity),
            ("title", got.title),
            ("highlights", got.highlights),
            ("name", Some(got.suggested_name)),
        ];
        for (field, value) in fields {
            let expected_value = want[field].as_str();
            if value.as_deref() != expected_value {
                count += 1;
                let _ = writeln!(
                    report,
                    "{id} {field}: got {value:?}, recorded {expected_value:?}"
                );
            }
        }
    }
    (count, report)
}

#[test]
fn fixture_documents_match_the_recording() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let (count, report) = mismatches(&dir.join("docs"), &dir.join("expected.jsonl"));
    assert_eq!(
        count, 0,
        "{count} fields differ from the recording:\n{report}"
    );
}

#[test]
#[ignore = "needs the evaluation corpus: BIG_DOCINFO_CORPUS and BIG_DOCINFO_EXPECTED"]
fn whole_corpus_matches_the_recording() {
    let docs = std::env::var("BIG_DOCINFO_CORPUS").expect("BIG_DOCINFO_CORPUS");
    let expected = std::env::var("BIG_DOCINFO_EXPECTED").expect("BIG_DOCINFO_EXPECTED");
    let (count, report) = mismatches(Path::new(&docs), Path::new(&expected));
    assert_eq!(
        count, 0,
        "{count} fields differ from the recording:\n{report}"
    );
}
