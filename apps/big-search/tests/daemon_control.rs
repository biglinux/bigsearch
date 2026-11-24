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

/// With content indexing off, a content search says so instead of an empty
/// answer; a running daemon's profile decides over the command's own.
#[test]
fn content_off_named() {
    let base = scratch::scratch_dir("bs-content-off");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(home.join("docs/conf.txt"), "alpha").unwrap();
    let reindex = env_cmd(
        &["reindex", home.to_str().unwrap()],
        &home,
        &xdg_storage_home,
        &run,
    )
    .env("BIG_SEARCH_NAMES_ONLY", "1")
    .status()
    .unwrap();
    assert!(reindex.success());
    let content_search = |names_only: &str| {
        let output = env_cmd(&["content", "alpha"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_NAMES_ONLY", names_only)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stderr).contains("content search is off")
    };
    assert!(content_search("1"), "no daemon, names only");
    assert!(!content_search("0"), "no daemon, content on");

    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_NAMES_ONLY", "1")
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    assert!(content_search("0"), "names-only daemon");
    let _ = std::fs::remove_dir_all(&base);
}

/// The daemon's reply to `cmd` naming `path`.
fn path_request(sock: &Path, cmd: &str, path: &Path) -> serde_json::Value {
    let request = serde_json::json!({"v": 2, "cmd": cmd, "path": path});
    serde_json::from_str(&send_daemon_request(sock, &request.to_string())).unwrap()
}

/// Guards: security refusal: a client cannot make the daemon read a folder or
/// copy a file outside the searched folders through a symlink inside one.
#[test]
fn client_symlink_out_refused() {
    let base = scratch::scratch_dir("bs-client-link");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let outside = base.join("outside");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(outside.join("secret.txt"), "s").unwrap();
    std::os::unix::fs::symlink(&outside, home.join("docs/link")).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let docs = home.join("docs");
    let reply = |cmd: &str, path: String| path_request(&sock, cmd, Path::new(&path));
    let link = docs.join("link").display().to_string();
    let docs_text = docs.display().to_string();
    // The message tells the path refusal from a later failure, such as a disk
    // that keeps no versions.
    let not_searched = "not in a searched folder";
    let no_source = "outside the configured sources";
    for (cmd, path, refusal) in [
        ("reindex", format!("{link}/"), not_searched),
        ("reindex", format!("{link}/."), not_searched),
        ("save_version", format!("{link}/secret.txt"), not_searched),
        ("versions", format!("{docs_text}/../x.txt"), "holds .."),
    ] {
        let answer = reply(cmd, path.clone());
        assert_eq!(answer["kind"], "error", "{cmd} {path}: {answer}");
        assert!(
            answer["message"].as_str().unwrap().ends_with(refusal),
            "{cmd} {path}: {answer}"
        );
    }
    assert_eq!(reply("reindex", format!("{docs_text}/./"))["kind"], "ok");
    let _ = std::fs::remove_dir_all(&base);
}

/// The ids of the versions the daemon lists for `path`.
fn version_ids(sock: &Path, path: &Path) -> Vec<i64> {
    let reply = path_request(sock, "versions", path);
    reply["versions"]
        .as_array()
        .unwrap_or_else(|| panic!("versions {path:?}: {reply}"))
        .iter()
        .map(|version| version["id"].as_i64().unwrap())
        .collect()
}

/// Guards: privacy action: every request naming a path answers for the
/// document that path names, whatever symlinks, removals or replacements
/// happened on the way: forgetting through one spelling never erases another
/// document's versions or leaves its own, and nothing outside a source is read.
#[test]
fn client_path_matrix() {
    // A disk that can share blocks: `/tmp` is usually tmpfs, which keeps no
    // versions. The socket stays there, under the length limit of its path.
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("client-path-matrix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let outside = base.join("outside");
    let run = scratch::scratch_dir("bs-matrix-run");
    let docs = home.join("docs");
    for folder in ["real", "old", "other", "gone", "proj"] {
        std::fs::create_dir_all(docs.join(folder)).unwrap();
        std::fs::write(docs.join(folder).join("a.txt"), folder).unwrap();
    }
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(docs.join("real"), docs.join("alias")).unwrap();
    std::os::unix::fs::symlink(&outside, docs.join("out")).unwrap();
    // The scratch home lies in the build folder, which the default rules
    // exclude.
    let rules = xdg_storage_home.join("cfg/big-search/ignore");
    std::fs::create_dir_all(rules.parent().unwrap()).unwrap();
    std::fs::write(&rules, "!target/\n!**/target/**\n").unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _d = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let reply = |cmd: &str, path: &Path| path_request(&sock, cmd, path);
    let ids = |path: &Path| version_ids(&sock, path);
    let file = |folder: &str| docs.join(folder).join("a.txt");

    let saved = reply("save_version", &file("alias"));
    if saved["kind"] == "error" {
        let unavailable = reply("versions", &file("alias"))["unavailable"].clone();
        assert_ne!(unavailable, "", "{saved}");
        eprintln!("this disk keeps no versions ({unavailable}); test declared unsupported");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    // One version listed per document: the state on disk is never listed, and
    // keeping it again leaves any automatic version of it nothing new to keep.
    let keep_one = |spelled: &Path, edit: &str| {
        assert_eq!(reply("save_version", spelled)["kind"], "ok", "{spelled:?}");
        std::fs::write(spelled, edit).unwrap();
        assert_eq!(reply("save_version", spelled)["kind"], "ok", "{spelled:?}");
    };
    keep_one(&file("alias"), "real edited");
    for folder in ["old", "other", "gone", "proj"] {
        keep_one(&file(folder), &format!("{folder} edited"));
    }
    std::fs::remove_dir_all(docs.join("old")).unwrap();
    std::os::unix::fs::symlink(docs.join("other"), docs.join("old")).unwrap();
    std::fs::remove_dir_all(docs.join("gone")).unwrap();
    std::fs::remove_dir_all(docs.join("proj")).unwrap();
    std::fs::write(docs.join("proj"), "a file now").unwrap();

    // Alias inside a source: one document under either spelling.
    let real = ids(&file("real"));
    assert_eq!(real.len(), 1, "{real:?}");
    assert_eq!(ids(&file("alias")), real);
    // Folder replaced by a link inside the source: the versions kept under the
    // old folder stay its own, apart from those of the folder linked in. Both
    // of its states are listed, since neither is on disk any more.
    let old = ids(&file("old"));
    let other = ids(&file("other"));
    assert_eq!((old.len(), other.len()), (2, 1), "{old:?} {other:?}");
    assert!(!old.contains(&other[0]), "{old:?} {other:?}");
    // A path vanished, and a folder turned into a file: rows keep their key.
    assert_eq!(ids(&file("gone")).len(), 2);
    assert_eq!(ids(&file("proj")).len(), 2);
    // A link to outside: nothing was kept there and nothing is read.
    assert_eq!(ids(&file("out")), Vec::<i64>::new());

    assert_eq!(reply("forget_versions", &file("old"))["kind"], "ok");
    assert_eq!(ids(&file("other")), other, "forgetting old erased other");
    // With the old folder's rows gone, the spelling leads where it resolves.
    assert_eq!(ids(&file("old")), other);
    assert_eq!(reply("forget_versions", &file("alias"))["kind"], "ok");
    assert_eq!(ids(&file("real")), Vec::<i64>::new());
    for folder in ["gone", "proj", "out"] {
        assert_eq!(reply("forget_versions", &file(folder))["kind"], "ok");
        assert_eq!(ids(&file(folder)), Vec::<i64>::new(), "{folder}");
    }

    // Requests that read the disk follow the link and stay inside the source.
    let not_searched = "not in a searched folder";
    let no_source = "outside the configured sources";
    for (cmd, path, refusal) in [
        ("reindex", docs.join("alias"), None),
        ("reindex", docs.join("old"), None),
        ("reindex", docs.join("gone"), None),
        ("reindex", docs.join("out/"), Some(not_searched)),
        ("save_version", file("out"), Some(not_searched)),
        ("register_created", file("alias"), None),
        ("register_created", file("out"), Some(no_source)),
    ] {
        let answer = reply(cmd, &path);
        match refusal {
            None => assert_eq!(answer["kind"], "ok", "{cmd} {path:?}: {answer}"),
            Some(refusal) => assert!(
                answer["message"].as_str().unwrap().contains(refusal),
                "{cmd} {path:?}: {answer}"
            ),
        }
    }
    let origin = reply("origin", &file("real")).to_string();
    assert!(origin.contains("app_created"), "{origin}");
    keep_one(&file("alias"), "real again");
    assert_eq!(ids(&file("real")).len(), 1);

    // Source root replaced by a link while the daemon runs: it leads nowhere
    // new, and the rows kept under it are still found by their spelling.
    std::fs::rename(&home, base.join("home.old")).unwrap();
    std::os::unix::fs::symlink(&outside, &home).unwrap();
    std::fs::create_dir_all(outside.join("docs/real")).unwrap();
    std::fs::write(outside.join("docs/real/a.txt"), "outside").unwrap();
    for (cmd, path) in [
        ("reindex", docs.join("real")),
        ("save_version", file("real")),
        ("register_created", file("real")),
    ] {
        assert_eq!(reply(cmd, &path)["kind"], "error", "{cmd} {path:?}");
    }
    // Its state on disk is out of reach now, so it is listed too.
    assert_eq!(ids(&file("other")).len(), 2);
    assert!(ids(&file("other")).contains(&other[0]));
    assert_eq!(reply("forget_versions", &file("other"))["kind"], "ok");
    assert_eq!(ids(&file("other")), Vec::<i64>::new());
    let _ = std::fs::remove_dir_all(&base);
}
