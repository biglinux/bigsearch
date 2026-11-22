//! End-to-end CLI contract for the `count` and `list` subcommands: drive the real
//! binary against a throwaway index so the thin `cmd_*` wrappers (which only wire
//! stdout to the query layer) are exercised, not just the query functions beneath.
#![cfg(not(miri))]
#![expect(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] functions panic on setup failure"
)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test spawns the built big-search binary under test"
)]

mod scratch;

use std::fs;
use std::path::Path;
use std::process::Command;

/// Run the built binary with an isolated HOME/XDG env so it never touches the real
/// index. Returns trimmed stdout; panics on a non-zero exit.
fn run(home: &Path, xdg_storage_home: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_big-search"))
        .args(args)
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", xdg_storage_home.join("run"))
        .output()
        .expect("spawn big-search");
    assert!(
        output.status.success(),
        "args {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn count_and_list_subcommands_report_catalog() {
    let base = scratch::scratch_dir("bs-cli");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(home.join("sub")).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("a.txt"), "alpha one").unwrap();
    fs::write(home.join("sub/b.md"), "beta two").unwrap();
    // Visible entries strictly under home: a.txt, sub/ (dir), sub/b.md → 3 cataloged.
    let home_str = home.to_str().unwrap();
    let sub_str = home.join("sub").to_str().unwrap().to_string();

    run(&home, &xdg_storage_home, &["reindex", home_str]);

    assert_eq!(run(&home, &xdg_storage_home, &["count"]), "3");
    // count --under DIR: scoped tally (sub/ + sub/b.md).
    assert_eq!(
        run(&home, &xdg_storage_home, &["count", "--under", &sub_str]),
        "2"
    );

    let listed: Vec<String> = run(&home, &xdg_storage_home, &["list"])
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(listed.len(), 3);
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "list output must be sorted");
    assert!(listed.iter().any(|p| p.ends_with("/a.txt")));
    assert!(listed.iter().any(|p| p.ends_with("/sub/b.md")));

    // list --under DIR: scoped to the subtree.
    let scoped = run(&home, &xdg_storage_home, &["list", "--under", &sub_str]);
    assert_eq!(scoped.lines().count(), 2);
    assert!(scoped.lines().all(|p| p.starts_with(&sub_str)));

    // -n N caps the output.
    assert_eq!(
        run(&home, &xdg_storage_home, &["list", "-n", "1"])
            .lines()
            .count(),
        1
    );

    fs::remove_dir_all(&base).ok();
}

/// A reindex after an index directory was deleted must re-extract content even
/// though the content state still records every file as done.
#[test]
fn reindex_rebuilds_either_missing_index() {
    for lost in ["index", "content-index"] {
        let base = scratch::scratch_dir(&format!("bs-cli-lost-{lost}"));
        let home = base.join("home");
        let xdg_storage_home = base.join("data");
        fs::create_dir_all(home.join("docs")).unwrap();
        fs::create_dir_all(&xdg_storage_home).unwrap();
        fs::write(home.join("docs/release-runbook.md"), "cassia nutricao").unwrap();

        run(&home, &xdg_storage_home, &["reindex"]);
        assert!(
            run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md")
        );

        fs::remove_dir_all(xdg_storage_home.join("big-search").join(lost)).unwrap();
        run(&home, &xdg_storage_home, &["reindex"]);
        assert!(
            run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md"),
            "content lost after deleting {lost}"
        );

        fs::remove_dir_all(&base).ok();
    }
}

/// The service reads config.toml once, when it starts; a change says so.
#[test]
fn config_change_names_the_restart() {
    let base = scratch::scratch_dir("bs-cli-config");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    for args in [
        &["config", "set", "names-only", "true"][..],
        &["config", "preset", "balanced"][..],
    ] {
        let out = run(&home, &xdg_storage_home, args);
        assert!(
            out.contains("systemctl --user restart big-search"),
            "{args:?}: {out}"
        );
    }
    fs::remove_dir_all(&base).ok();
}

/// A file name is outside text: its control characters reach the terminal
/// escaped, never as an escape sequence.
#[test]
fn file_names_print_without_control_characters() {
    let base = scratch::scratch_dir("bs-cli-escape");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("rel\u{1b}]0;title\u{7}.txt"), "corpo").unwrap();
    fs::write(home.join("relfdp.\u{202e}txt.exe"), "corpo").unwrap();
    run(
        &home,
        &xdg_storage_home,
        &["reindex", home.to_str().unwrap()],
    );
    for args in [&["rel"][..], &["list"][..], &["both", "rel"][..]] {
        let out = run(&home, &xdg_storage_home, args);
        assert!(
            out.contains("rel\\u{1b}]0;title\\u{7}.txt"),
            "{args:?}: {out:?}"
        );
        assert!(out.contains("relfdp.\\u{202e}txt.exe"), "{args:?}: {out:?}");
        assert!(
            !out.contains(['\u{1b}', '\u{7}', '\u{202e}']),
            "{args:?}: {out:?}"
        );
    }
    fs::remove_dir_all(&base).ok();
}

/// An error exits 1; a search with no hits is no error.
#[test]
fn exit_status_tells_error_from_empty() {
    let base = scratch::scratch_dir("bs-cli-exit");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    run(
        &home,
        &xdg_storage_home,
        &["reindex", home.to_str().unwrap()],
    );
    let status = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_big-search"))
            .args(args)
            .env("RUST_LOG", "off")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &xdg_storage_home)
            .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
            .env("XDG_RUNTIME_DIR", xdg_storage_home.join("run"))
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(status(&["nothing-matches-this"]), Some(0));
    assert_eq!(status(&["config", "set", "no-such-key", "1"]), Some(1));
    fs::remove_dir_all(&base).ok();
}
