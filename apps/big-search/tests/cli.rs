//! End-to-end CLI contract for the `count` and `list` subcommands: drive the real
//! binary against a throwaway index so the thin `cmd_*` wrappers (which only wire
//! stdout to the query layer) are exercised, not just the query functions beneath.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test spawns the built big-search binary under test"
)]

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
    let base = std::env::temp_dir().join(format!("bs-cli-{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
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

    // count: total cataloged entries.
    assert_eq!(run(&home, &xdg_storage_home, &["count"]), "3");
    // count --under DIR: scoped tally (sub/ + sub/b.md).
    assert_eq!(
        run(&home, &xdg_storage_home, &["count", "--under", &sub_str]),
        "2"
    );

    // list: every path, sorted, one per line.
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

#[test]
fn reindex_empty_index_reextracts_content_despite_stale_content_state() {
    let base = std::env::temp_dir().join(format!("bs-cli-stale-content-{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(home.join("docs")).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("docs/release-runbook.md"), "cassia nutricao").unwrap();

    run(&home, &xdg_storage_home, &["reindex"]);
    assert!(run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md"));

    fs::remove_dir_all(xdg_storage_home.join("big-search/index")).unwrap();
    run(&home, &xdg_storage_home, &["reindex"]);
    assert!(run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md"));

    fs::remove_dir_all(&base).ok();
}

#[test]
fn reindex_empty_content_index_reextracts_content_despite_stale_content_state() {
    let base =
        std::env::temp_dir().join(format!("bs-cli-stale-content-index-{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(home.join("docs")).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("docs/release-runbook.md"), "cassia nutricao").unwrap();

    run(&home, &xdg_storage_home, &["reindex"]);
    assert!(run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md"));

    fs::remove_dir_all(xdg_storage_home.join("big-search/content-index")).unwrap();
    run(&home, &xdg_storage_home, &["reindex"]);
    assert!(run(&home, &xdg_storage_home, &["content", "cassia"]).contains("release-runbook.md"));

    fs::remove_dir_all(&base).ok();
}
