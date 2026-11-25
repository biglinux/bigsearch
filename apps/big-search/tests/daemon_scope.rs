//! What the daemon leaves out of the index, and how each pass agrees on it:
//! the start's walk, a rescan, the watcher and a forced reindex apply the same
//! ignore rules, anchored at the same source, and evict what leaves them.
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

/// The global git ignore file is anchored at the root of each work tree, as
/// git reads it (gitignore(5)): its `/rootnote.md` leaves out the file at the
/// root of the repository and not one in a folder below it.
#[test]
fn global_ignore_anchors_at_work_tree() {
    let base = scratch::scratch_dir("bs-git-anchor");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("rootnote.md"), "a").unwrap();
    std::fs::write(repo.join("sub/rootnote.md"), "b").unwrap();
    std::fs::write(repo.join("marker.txt"), "c").unwrap();
    std::fs::create_dir_all(xdg_storage_home.join("cfg/git")).unwrap();
    std::fs::write(xdg_storage_home.join("cfg/git/ignore"), "/rootnote.md\n").unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    assert!(
        settles(|| query_paths(&sock, "name", "marker", &home).0 == 1),
        "the start walk did not index the repository"
    );
    let (found, paths) = query_paths(&sock, "name", "rootnote", &home);
    assert_eq!(found, 1, "{paths:?}");
    assert!(paths[0].ends_with("/repo/sub/rootnote.md"), "{paths:?}");
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

/// The ignore rules apply below the source: a home inside a folder named
/// `build` is indexed by the scan and the watcher, and a `build` folder inside
/// it is still left out.
#[test]
fn source_under_excluded_name_indexed() {
    let base = scratch::scratch_dir("bs-excluded-ancestor");
    let home = base.join("out/build/home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let docs = home.join("docs");
    std::fs::create_dir_all(docs.join("build")).unwrap();
    std::fs::write(docs.join("before.txt"), "alpha").unwrap();
    std::fs::write(docs.join("build/artifact.txt"), "beta").unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");

    assert!(
        settles(|| query_paths(&sock, "name", "before", &home).0 == 1),
        "the scan left out a file of the source"
    );
    std::fs::write(docs.join("appeared.txt"), "gamma").unwrap();
    assert!(
        settles(|| query_paths(&sock, "name", "appeared", &home).0 == 1),
        "the watcher left out a new file of the source"
    );
    assert_eq!(query_paths(&sock, "name", "artifact", &home).0, 0);
    let _ = std::fs::remove_dir_all(&base);
}

/// An ignore file above a source speaks only for what lies below it, as the
/// walk reads it: a home kept as a dotfiles repository that ignores all but
/// `.bashrc` leaves a source inside it whole, in the walk, the watcher and a
/// forced reindex alike.
#[test]
fn ignore_file_above_source_spares_it() {
    let base = scratch::scratch_dir("bs-rules-above");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let docs = home.join("Documents");
    std::fs::create_dir_all(home.join(".git")).unwrap();
    std::fs::write(home.join(".gitignore"), "/*\n!/.bashrc\n").unwrap();
    std::fs::create_dir_all(docs.join("sub")).unwrap();
    std::fs::write(docs.join("first.txt"), "a").unwrap();
    std::fs::write(docs.join("sub/second.txt"), "b").unwrap();
    std::fs::create_dir_all(xdg_storage_home.join("cfg/big-search")).unwrap();
    std::fs::write(
        xdg_storage_home.join("cfg/big-search/config.toml"),
        format!("[[source]]\npath = \"{}\"\n", docs.display()),
    )
    .unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let found = |name: &str| query_paths(&sock, "name", name, &docs).0;
    assert!(
        settles(|| found("first") == 1 && found("second") == 1),
        "the start walk left out the source"
    );

    std::fs::write(docs.join("third.txt"), "c").unwrap();
    assert!(
        settles(|| found("third") == 1),
        "the watcher left out a new file of the source"
    );
    assert_answers(
        &sock,
        &[
            (path_cmd("reindex", &docs.join("first.txt")), None),
            (path_cmd("reindex", &docs.join("sub")), None),
        ],
    );
    // The watcher takes the reindexes and the events in order.
    std::fs::write(docs.join("marker.txt"), "d").unwrap();
    assert!(settles(|| found("marker") == 1), "the watcher stopped");
    assert_eq!(found("first"), 1, "the reindex evicted the file");
    assert_eq!(found("second"), 1, "the reindex evicted the folder");
    let _ = std::fs::remove_dir_all(&base);
}

/// A forced reindex judges the path as new: a catalogued folder its repository
/// came to ignore leaves with what it holds, as a new one would never enter.
#[test]
fn reindex_evicts_ignored_folder() {
    let base = scratch::scratch_dir("bs-reindex-ignored");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    let generated = repo.join("generated");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&generated).unwrap();
    std::fs::write(generated.join("inner.txt"), "alpha").unwrap();
    for sub in 0..10 {
        std::fs::create_dir_all(generated.join(format!("sub{sub}"))).unwrap();
    }
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    assert!(
        settles(|| query_paths(&sock, "name", "inner", &home).0 == 1),
        "the first walk did not index the folder"
    );
    assert!(
        settles(|| catalogue_rows(&xdg_storage_home, "watch", &generated) == 10),
        "the first walk did not list the subfolders"
    );
    let watches = inotify_watches(daemon.0.id());

    std::fs::write(repo.join(".gitignore"), "generated/\n").unwrap();
    assert_answers(&sock, &[(path_cmd("reindex", &generated), None)]);
    assert!(
        settles(|| query_paths(&sock, "name", "generated", &home).0 == 0),
        "the ignored folder is still found"
    );
    assert_eq!(query_paths(&sock, "name", "inner", &home).0, 0);
    assert_eq!(catalogue_rows(&xdg_storage_home, "name", &generated), 0);
    // The folder and its ten subfolders, still on disk, lose their watches.
    assert!(
        settles(|| inotify_watches(daemon.0.id()) + 11 <= watches),
        "the ignored folders are still watched"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A forced reindex of a folder compares its whole tree with the catalogue: a
/// subfolder its repository came to ignore leaves every index and the folder
/// list.
#[test]
fn reindex_of_parent_evicts_ignored_subtree() {
    let base = scratch::scratch_dir("bs-reindex-parent");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    let generated = repo.join("generated");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(generated.join("deep")).unwrap();
    std::fs::write(generated.join("deep/inner.txt"), "innerworddeep").unwrap();
    for sub in 0..10 {
        std::fs::create_dir_all(generated.join(format!("deep/sub{sub}"))).unwrap();
    }
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "100")
            .env("BIG_SEARCH_CONTENT_COOLDOWN_SECS", "0")
            .env("BIG_SEARCH_META", "0")
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    assert!(
        await_content_result(&home, &xdg_storage_home, &run, "innerworddeep", "inner.txt"),
        "the first walk did not index the subfolder"
    );
    assert!(
        settles(|| catalogue_rows(&xdg_storage_home, "watch", &generated) == 11),
        "the first walk did not list the subfolders"
    );
    let watches = inotify_watches(daemon.0.id());

    std::fs::write(repo.join(".gitignore"), "generated/\n").unwrap();
    assert_answers(&sock, &[(path_cmd("reindex", &repo), None)]);
    assert_tree_gone(&sock, &xdg_storage_home, &generated, "inner");
    assert_eq!(query_paths(&sock, "name", "generated", &home).0, 0);
    assert_eq!(catalogue_rows(&xdg_storage_home, "watch", &generated), 0);
    // The twelve folders the reindex leaves out, still on disk, lose their
    // watches.
    assert!(
        settles(|| inotify_watches(daemon.0.id()) + 12 <= watches),
        "the ignored folders are still watched"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A forced reindex of a file its repository came to ignore takes its text out
/// of the content index too, not only its name.
#[test]
fn reindex_evicts_ignored_file_content() {
    let base = scratch::scratch_dir("bs-reindex-content");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    let notes = repo.join("notes.txt");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(&notes, "noteswordevicted").unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .env("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "100")
            .env("BIG_SEARCH_CONTENT_COOLDOWN_SECS", "0")
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
            "noteswordevicted",
            "notes.txt"
        ),
        "the first walk did not index the file's text"
    );

    std::fs::write(repo.join(".gitignore"), "notes.txt\n").unwrap();
    assert_answers(&sock, &[(path_cmd("reindex", &notes), None)]);
    assert!(
        settles(|| query_paths(&sock, "name", "notes", &home).0 == 0),
        "the ignored file is still found by name"
    );
    assert!(
        settles_within(SETTLE, || {
            query_paths(&sock, "content", "noteswordevicted", &home).0 == 0
        }),
        "the ignored file is still found by its text"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A source inside another applies the ignore rules below itself, in the
/// walk as in the watcher: an anchored rule leaves out its own folder of that
/// name and not the outer source's, whichever pass looks.
#[test]
fn nested_source_anchors_its_rules() {
    let base = scratch::scratch_dir("bs-nested-anchor");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let sync = home.join("sync");
    std::fs::create_dir_all(sync.join("Archive")).unwrap();
    std::fs::create_dir_all(home.join("docs/Archive")).unwrap();
    std::fs::write(sync.join("Archive/old.txt"), "a").unwrap();
    std::fs::write(sync.join("keep.txt"), "b").unwrap();
    std::fs::write(home.join("docs/Archive/deeper.txt"), "c").unwrap();
    std::fs::create_dir_all(xdg_storage_home.join("cfg/big-search")).unwrap();
    std::fs::write(
        xdg_storage_home.join("cfg/big-search/ignore"),
        "/Archive/\n",
    )
    .unwrap();
    std::fs::write(
        xdg_storage_home.join("cfg/big-search/config.toml"),
        format!(
            "[[source]]\npath = \"{}\"\n\n[[source]]\npath = \"{}\"\n",
            home.display(),
            sync.display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let found = |name: &str| query_paths(&sock, "name", name, &home).0;
    assert!(
        settles(|| found("keep") == 1),
        "the start left out keep.txt"
    );
    assert!(
        settles(|| found("deeper") == 1),
        "the start left out deeper.txt"
    );
    assert_eq!(found("old"), 0, "the start indexed the excluded folder");

    assert!(command_accepted(&sock, "rescan"));
    std::fs::write(sync.join("after.txt"), "d").unwrap();
    assert!(settles(|| found("after") == 1), "the watcher stopped");
    assert_eq!(found("old"), 0, "the rescan indexed the excluded folder");
    let _ = std::fs::remove_dir_all(&base);
}

/// A `.ignore` file leaves its folders out of a forced reindex and of the
/// watcher, as it leaves them out of the walk.
#[test]
fn dot_ignore_applies_to_every_pass() {
    let base = scratch::scratch_dir("bs-dot-ignore");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let docs = home.join("docs");
    std::fs::create_dir_all(docs.join("skipme")).unwrap();
    std::fs::write(docs.join(".ignore"), "skipme/\n").unwrap();
    std::fs::write(docs.join("skipme/walked.txt"), "a").unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let found = |name: &str| query_paths(&sock, "name", name, &home).0;
    std::fs::write(docs.join("marker1.txt"), "b").unwrap();
    assert!(
        settles(|| found("marker1") == 1),
        "the watcher is not running"
    );
    assert_eq!(found("walked"), 0, "the walk entered the ignored folder");

    assert_answers(&sock, &[(path_cmd("reindex", &docs.join("skipme")), None)]);
    std::fs::create_dir_all(docs.join("later/skipme")).unwrap();
    std::fs::write(docs.join("later/skipme/created.txt"), "c").unwrap();
    std::fs::write(docs.join("marker2.txt"), "d").unwrap();
    assert!(
        settles(|| found("marker2") == 1),
        "the watcher is not running"
    );
    assert_eq!(found("walked"), 0, "the reindex entered the ignored folder");
    assert_eq!(found("created"), 0, "the watcher entered an ignored folder");
    let _ = std::fs::remove_dir_all(&base);
}

/// The effective ignore rules come before a repository's own: a `.gitignore`
/// that brings back a folder the built-in rules leave out does not put it in
/// the walk, as it does not in the watcher.
#[test]
fn effective_rules_precede_gitignore() {
    let base = scratch::scratch_dir("bs-rules-precede");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    let repo = home.join("docs/repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("build")).unwrap();
    std::fs::write(repo.join(".gitignore"), "!build/\n").unwrap();
    std::fs::write(repo.join("build/artifact.txt"), "a").unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join("biglinux/indexd.sock");
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sock), "daemon never bound socket");
    let found = |name: &str| query_paths(&sock, "name", name, &home).0;
    assert!(command_accepted(&sock, "rescan"));
    std::fs::write(repo.join("marker.txt"), "b").unwrap();
    assert!(
        settles(|| found("marker") == 1),
        "the watcher is not running"
    );
    assert_eq!(found("build"), 0, "the walk took in a built-in exclusion");
    assert_eq!(
        found("artifact"),
        0,
        "the walk entered a built-in exclusion"
    );
    let _ = std::fs::remove_dir_all(&base);
}
