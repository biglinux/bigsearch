// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The command as the file manager's menu uses it.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the tests run the built binary; argv is fixed and the output is small"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONTRACT: &str = "CONTRATO DE LOCAÇÃO RESIDENCIAL\n\nLOCADORA: Marta Ribeiro Saldanha, aposentada.\n\nPorto Alegre, 22 de abril de 2026.\n";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("big-docinfo-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_big-docinfo"))
        .args(args)
        .output()
        .expect("spawn big-docinfo")
}

fn arg(p: &Path) -> &str {
    p.to_str().expect("utf-8 test path")
}

#[test]
fn suggests_a_free_name_with_the_extension_kept() {
    let dir = scratch("suggest");
    let doc = dir.join("scan0001.txt");
    fs::write(&doc, CONTRACT).expect("write");
    fs::write(
        dir.join("contrato-locacao-residencial-2026-04-22.txt"),
        "taken",
    )
    .expect("write");

    let out = run(&["--json", arg(&doc)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["kind"], "contrato");
    assert_eq!(json["date"], "2026-04-22");
    assert_eq!(
        json["suggested_name"],
        "contrato-locacao-residencial-2026-04-22.txt"
    );
    assert_eq!(
        json["free_name"],
        "contrato-locacao-residencial-2026-04-22-2.txt"
    );
    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn rename_never_replaces_an_existing_file() {
    let dir = scratch("noreplace");
    let doc = dir.join("a.txt");
    let other = dir.join("b.txt");
    fs::write(&doc, "mine").expect("write");
    fs::write(&other, "theirs").expect("write");

    let out = run(&["rename", arg(&doc), "b.txt"]);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&doc).expect("a kept"), "mine");
    assert_eq!(fs::read_to_string(&other).expect("b kept"), "theirs");
    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn rename_keeps_the_extension_and_rejects_paths() {
    let dir = scratch("rename");
    let doc = dir.join("a.pdf");
    fs::write(&doc, "x").expect("write");

    for bad in ["", ".", "..", "../escape.pdf", "sub/dir.pdf"] {
        assert!(
            !run(&["rename", arg(&doc), bad]).status.success(),
            "{bad:?} accepted"
        );
    }
    assert!(doc.exists());

    let out = run(&["rename", arg(&doc), "contrato-2026"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!doc.exists());
    assert_eq!(
        fs::read_to_string(dir.join("contrato-2026.pdf")).expect("renamed"),
        "x"
    );
    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn a_file_without_text_is_an_error_not_a_guess() {
    let dir = scratch("empty");
    let doc = dir.join("image.png");
    fs::write(&doc, [0x89, b'P', b'N', b'G', 0, 0, 0, 0]).expect("write");
    let out = run(&["--json", arg(&doc)]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn a_document_with_nothing_to_go_on_keeps_its_name() {
    let dir = scratch("kept");
    let doc = dir.join("Planilha de horas.txt");
    fs::write(&doc, "12 345 678\n9 10 11\n").expect("write");
    let out = run(&["--json", arg(&doc)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["kept_name"], true);
    assert_eq!(json["free_name"], "Planilha de horas.txt");
    fs::remove_dir_all(dir).expect("cleanup");
}
