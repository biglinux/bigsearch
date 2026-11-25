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

mod daemon;
mod scratch;

use std::fs;
use std::path::Path;
use std::process::Command;

/// The built binary with an isolated HOME/XDG env, so it never touches the real
/// index.
fn big_search(home: &Path, xdg_storage_home: &Path, args: &[&str]) -> Command {
    daemon::env_cmd(args, home, xdg_storage_home, &xdg_storage_home.join("run"))
}

/// Run [`big_search`]. Returns trimmed stdout; panics on a non-zero exit.
fn run(home: &Path, xdg_storage_home: &Path, args: &[&str]) -> String {
    let output = big_search(home, xdg_storage_home, args)
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

    let _ = fs::remove_dir_all(&base);
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

        let _ = fs::remove_dir_all(&base);
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
    let _ = fs::remove_dir_all(&base);
}

/// `config show` reports the posting detail the content index holds, which
/// `auto` keeps, not what this shell's memory would choose for a new one.
#[test]
fn config_show_reads_stored_detail() {
    let base = scratch::scratch_dir("bs-cli-detail");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("a.txt"), "alpha").unwrap();
    let shown = || run(&home, &xdg_storage_home, &["config", "show"]);
    let before = shown();
    assert!(before.contains("for a new content index"), "{before}");
    let reindexed = big_search(&home, &xdg_storage_home, &["reindex"])
        .env("BIG_SEARCH_CONTENT_INDEX_MODE", "basic")
        .status()
        .unwrap();
    assert!(reindexed.success());
    let after = shown();
    assert!(
        after.contains("requested=auto, effective=basic, from the content index"),
        "{after}"
    );
    let _ = fs::remove_dir_all(&base);
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
    let _ = fs::remove_dir_all(&base);
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
        big_search(&home, &xdg_storage_home, args)
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(status(&["nothing-matches-this"]), Some(0));
    assert_eq!(status(&["config", "set", "no-such-key", "1"]), Some(1));
    let _ = fs::remove_dir_all(&base);
}

/// `reindex ROOT` stores absolute paths whatever the working directory, and a
/// missing ROOT is refused before the index it would replace is touched.
#[test]
fn reindex_root_absolute_and_checked() {
    let base = scratch::scratch_dir("bs-cli-root");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(home.join("docs")).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("docs/a.txt"), "alpha").unwrap();
    let in_home = |args: &[&str]| {
        big_search(&home, &xdg_storage_home, args)
            .current_dir(&home)
            .output()
            .unwrap()
    };
    assert!(in_home(&["reindex", "docs"]).status.success());
    let listed = run(&home, &xdg_storage_home, &["list"]);
    let file = fs::canonicalize(home.join("docs/a.txt")).unwrap();
    assert!(
        listed.lines().any(|line| Path::new(line) == file),
        "{listed}"
    );
    let missing = in_home(&["reindex", "no-such-folder"]);
    assert_eq!(missing.status.code(), Some(1));
    // Only docs/a.txt: the entries strictly under the root.
    assert_eq!(run(&home, &xdg_storage_home, &["count"]), "1");
    let _ = fs::remove_dir_all(&base);
}

/// A reindex whose state database will not open is refused before it touches
/// the name index, since the catalogue it built would be kept nowhere.
#[test]
fn reindex_refuses_unopenable_state() {
    let base = scratch::scratch_dir("bs-cli-state");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("alpha.txt"), "a").unwrap();
    let home_str = home.to_str().unwrap();
    run(&home, &xdg_storage_home, &["reindex", home_str]);
    let storage = xdg_storage_home.join("big-search");
    for name in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let _ = fs::remove_file(storage.join(name));
    }
    fs::create_dir(storage.join("state.sqlite")).unwrap();
    let index_meta = || fs::read(storage.join("index/meta.json")).unwrap();
    let before = index_meta();
    for args in [&["reindex"][..], &["reindex", home_str][..]] {
        let output = big_search(&home, &xdg_storage_home, args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(index_meta() == before, "{args:?} rewrote the name index");
    }
    let _ = fs::remove_dir_all(&base);
}

/// `--under` names a folder the way the shell does, relative to where the
/// command runs, `..` included.
#[test]
fn under_relative_to_working_directory() {
    let base = scratch::scratch_dir("bs-cli-under");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(home.join("docs")).unwrap();
    fs::create_dir_all(&xdg_storage_home).unwrap();
    fs::write(home.join("docs/report.txt"), "x").unwrap();
    fs::write(home.join("report.txt"), "x").unwrap();
    run(
        &home,
        &xdg_storage_home,
        &["reindex", home.to_str().unwrap()],
    );
    let file = fs::canonicalize(home.join("docs/report.txt")).unwrap();
    for (folder, working) in [("docs", home.clone()), ("../docs", home.join("docs"))] {
        let output = big_search(&home, &xdg_storage_home, &["report", "--under", folder])
            .current_dir(&working)
            .output()
            .unwrap();
        let hits = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            hits.lines().map(Path::new).collect::<Vec<_>>(),
            [file.as_path()],
            "{folder}"
        );
    }
    let _ = fs::remove_dir_all(&base);
}

/// The commands that need the service say so when it is not running, instead
/// of a bare connection error that reads as the named file missing.
#[test]
fn versions_without_service_named() {
    let base = scratch::scratch_dir("bs-cli-nodaemon");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(xdg_storage_home.join("run")).unwrap();
    for command in ["versions", "forget-versions"] {
        let output = big_search(&home, &xdg_storage_home, &[command, "/etc/hosts"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{command}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("daemon"), "{command}: {stderr}");
    }
    let _ = fs::remove_dir_all(&base);
}
