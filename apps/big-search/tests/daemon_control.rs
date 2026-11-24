//! Lifecycle commands over the v2 protocol: pause flips the reported state, resume
//! clears it, rebuild walks the disk again.
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

use daemon::*;

use std::path::Path;
use std::time::Duration;

#[test]
fn pause_resume_rebuild_cycle() {
    let base = scratch::scratch_dir("bs-ctl");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(home.join("docs/conf.txt"), "alpha").unwrap();

    assert!(
        env_cmd(
            &["reindex", home.to_str().unwrap()],
            &home,
            &xdg_storage_home,
            &run
        )
        .status()
        .unwrap()
        .success()
    );

    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");

    // Starts unpaused.
    assert!(
        send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"status\"}").contains("\"paused\":false")
    );

    let before = indexed_count(&sock).expect("status reports indexed");

    // Pause → status reports paused, and at once: a rescan sent right after
    // it is refused.
    assert!(command_accepted(&sock, "pause"));
    assert!(
        !command_accepted(&sock, "rescan"),
        "rescan ran on a paused daemon"
    );
    assert!(await_paused(&sock, true), "daemon did not report paused");
    let rescan = env_cmd(&["rescan"], &home, &xdg_storage_home, &run)
        .output()
        .unwrap();
    assert!(!rescan.status.success(), "rescan ran on a paused daemon");

    // A file written while paused produces no indexing; only the rebuild's
    // walk can catalogue it.
    std::fs::write(home.join("docs/added.txt"), "beta").unwrap();
    assert!(command_accepted(&sock, "rebuild"));
    assert!(
        await_indexed(&sock, before + 1),
        "rebuild did not catalogue the file written while paused"
    );

    // Resume → status reports running again.
    assert!(command_accepted(&sock, "resume"));
    assert!(await_paused(&sock, false), "daemon did not report resumed");

    let _ = std::fs::remove_dir_all(&base);
}

/// Debug builds only: a release binary ignores the batch size this test sets.
#[cfg(debug_assertions)]
#[test]
fn content_backfill_continues_past_batch() {
    let base = scratch::scratch_dir("bs-content-backfill");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let first = home.join("docs/first.md");
    let second = home.join("docs/second.md");
    std::fs::write(&first, "alpha-first-unique").unwrap();
    std::fs::write(&second, "beta-second-unique").unwrap();

    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_DAEMON_CONTENT_BATCH", "1")
            .env("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "100")
            .env("BIG_SEARCH_META", "0")
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");

    assert!(
        await_content_result(
            &home,
            &xdg_storage_home,
            &run,
            "alpha-first-unique",
            "first.md"
        ),
        "first content file was not indexed"
    );
    assert!(
        await_content_result(
            &home,
            &xdg_storage_home,
            &run,
            "beta-second-unique",
            "second.md"
        ),
        "second content file was not indexed by an idle follow-up batch"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A folder deleted or moved while the daemon runs leaves nothing of its tree
/// behind: not in the name index, the content index or either catalogue.
#[test]
fn departed_tree_leaves_every_index() {
    let base = scratch::scratch_dir("bs-tree-gone");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let outside = base.join("outside");
    let docs = home.join("docs");
    for tree in ["deleted", "away", "moving"] {
        plant_tree(&docs.join(tree), tree);
    }
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&run).unwrap();

    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "100")
            .env("BIG_SEARCH_CONTENT_COOLDOWN_SECS", "0")
            .env("BIG_SEARCH_META", "0")
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    for tree in ["deleted", "away", "moving"] {
        let word = format!("{tree}worddeep");
        let file = format!("{tree}-deep.txt");
        assert!(
            await_content_result(&home, &xdg_storage_home, &run, &word, &file),
            "{file} was not indexed"
        );
    }

    // A folder moved away is one event naming the folder; none name what it
    // held.
    std::fs::remove_dir_all(docs.join("deleted")).unwrap();
    std::fs::rename(docs.join("away"), outside.join("away")).unwrap();
    std::fs::rename(docs.join("moving"), docs.join("moved")).unwrap();

    for tree in ["deleted", "away", "moving"] {
        assert_tree_gone(&sock, &xdg_storage_home, &docs.join(tree), tree);
    }
    let settled = |check: &dyn Fn() -> bool| settles_within(SETTLE, check);
    let moved = docs.join("moved");
    let deep = moved.join("deep/moving-deep.txt").display().to_string();
    assert!(
        settled(&|| query_paths(&sock, "list", "", &moved).1.contains(&deep)),
        "the moved tree is not catalogued under its new path"
    );
    assert!(
        settled(&|| query_paths(&sock, "content", "movingworddeep", &home).1 == [deep.clone()]),
        "the moved tree is not searchable by content under its new path"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// How long a tree's departure may take to reach every index: the watcher
/// debounces, and the content side follows in the next backfill pass, which a
/// busy machine delays (see `await_content_result`).
const SETTLE: Duration = Duration::from_secs(45);

/// A tree `tree` at `dir`: a file on top and one a folder down, each with a
/// word of its own.
fn plant_tree(dir: &Path, tree: &str) {
    std::fs::create_dir_all(dir.join("deep")).unwrap();
    std::fs::write(
        dir.join(format!("{tree}-top.txt")),
        format!("{tree}wordtop"),
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("deep/{tree}-deep.txt")),
        format!("{tree}worddeep"),
    )
    .unwrap();
}

/// Nothing of the tree that was at `old` is left in any index or catalogue.
fn assert_tree_gone(sock: &Path, data: &Path, old: &Path, tree: &str) {
    assert!(
        settles_within(SETTLE, || query_paths(sock, "list", "", old).0 == 0),
        "{tree}: the catalogue still lists {:?}",
        query_paths(sock, "list", "", old)
    );
    assert!(
        settles_within(SETTLE, || query_paths(sock, "name", tree, old).0 == 0),
        "{tree}: the name index still holds {:?}",
        query_paths(sock, "name", tree, old)
    );
    let word = format!("{tree}worddeep");
    assert!(
        settles_within(SETTLE, || query_paths(sock, "content", &word, old).0 == 0),
        "{tree}: the content index still answers for the old path"
    );
    assert!(
        settles_within(SETTLE, || catalogue_rows(data, "content", old) == 0),
        "{tree}: the content catalogue still holds rows under the old path"
    );
    assert_eq!(catalogue_rows(data, "name", old), 0);
}

/// A file that appears in a repository while the daemon runs is left out when
/// the global git ignore file excludes it, as the walk leaves it out.
#[test]
fn global_git_ignore_covers_events() {
    let base = scratch::scratch_dir("bs-git-events");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(xdg_storage_home.join("cfg/git")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(xdg_storage_home.join("cfg/git/ignore"), "*.secret\n").unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");

    std::fs::write(repo.join("b.secret"), "alpha").unwrap();
    std::fs::write(repo.join("visible.txt"), "beta").unwrap();
    assert!(
        settles(|| query_paths(&sock, "name", "visible", &home).0 == 1),
        "the visible file was not indexed"
    );
    assert_eq!(catalogue_rows(&xdg_storage_home, "name", &repo), 1);
    let _ = std::fs::remove_dir_all(&base);
}

/// A source inside a dot-folder indexes the files that appear in it: only what
/// is hidden below the source is out of scope.
#[test]
fn hidden_source_indexes_new_files() {
    let base = scratch::scratch_dir("bs-hidden-events");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let notes = home.join(".stash/notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("first.txt"), "alpha").unwrap();
    std::fs::create_dir_all(xdg_storage_home.join("cfg/big-search")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        xdg_storage_home.join("cfg/big-search/config.toml"),
        format!("[[source]]\npath = \"{}\"\n", notes.display()),
    )
    .unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");

    std::fs::write(notes.join("appeared.txt"), "beta").unwrap();
    assert!(
        settles(|| query_paths(&sock, "name", "appeared", &notes).0 == 1),
        "the new file in the hidden source was not indexed"
    );
    let _ = std::fs::remove_dir_all(&base);
}
