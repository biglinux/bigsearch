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

use std::path::{Path, PathBuf};

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

/// A request naming the file another one came from and the file it made.
fn pair_cmd(cmd: &str, src: &Path, dst: &Path) -> serde_json::Value {
    serde_json::json!({"v": 2, "cmd": cmd, "src": src, "dst": dst})
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
    let docs = home.join("docs").display().to_string();
    let spelled = |text: String| PathBuf::from(text);
    // The message tells the path refusal from a later failure, such as a disk
    // that keeps no versions.
    let not_searched = Some("not in a searched folder");
    assert_answers(
        &sock,
        &[
            (
                path_cmd("reindex", &spelled(format!("{docs}/link/"))),
                not_searched,
            ),
            (
                path_cmd("reindex", &spelled(format!("{docs}/link/."))),
                not_searched,
            ),
            (
                path_cmd("save_version", &spelled(format!("{docs}/link/secret.txt"))),
                not_searched,
            ),
            (
                path_cmd("versions", &spelled(format!("{docs}/../x.txt"))),
                Some("holds .."),
            ),
            (path_cmd("reindex", &spelled(format!("{docs}/./"))), None),
        ],
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The ids of the versions the daemon lists for `path`.
fn version_ids(sock: &Path, path: &Path) -> Vec<i64> {
    let reply = ask(sock, &path_cmd("versions", path));
    reply["versions"]
        .as_array()
        .unwrap_or_else(|| panic!("versions {path:?}: {reply}"))
        .iter()
        .map(|version| version["id"].as_i64().unwrap())
        .collect()
}

/// A daemon searching `docs`, which holds the folders `real`, `old`, `other`,
/// `gone`, `proj`, `vacated` and `moved` with an `a.txt` each, `alias` linked
/// to `real` and `out` linked to a folder outside the source.
struct PathMatrix {
    base: PathBuf,
    home: PathBuf,
    docs: PathBuf,
    outside: PathBuf,
    sock: PathBuf,
    /// Stopped before `base` is removed, or it writes its data folder again.
    daemon: Option<KillOnDrop>,
    _run: scratch::ScratchDir,
}

impl PathMatrix {
    fn start() -> Self {
        // A disk that can share blocks: `/tmp` is usually tmpfs, which keeps no
        // versions. The socket stays there, under the length limit of its path.
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("client-path-matrix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let xdg_storage_home = base.join("data");
        let outside = base.join("outside");
        let docs = home.join("docs");
        for folder in ["real", "old", "other", "gone", "proj", "vacated", "moved"] {
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
        let run = scratch::scratch_dir("bs-matrix-run");
        let sock = run.join("biglinux/indexd.sock");
        let daemon = KillOnDrop(
            env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
                .spawn()
                .unwrap(),
        );
        assert!(wait_socket(&sock), "daemon never bound socket");
        // Watching, so the rearrangement is heard.
        assert!(
            settles(
                || send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"status\"}")
                    .contains("\"activity\":\"idle\"")
            ),
            "the daemon never finished its start"
        );
        Self {
            base,
            home,
            docs,
            outside,
            sock,
            daemon: Some(daemon),
            _run: run,
        }
    }

    fn file(&self, folder: &str) -> PathBuf {
        self.docs.join(folder).join("a.txt")
    }

    fn reply(&self, cmd: &str, path: &Path) -> serde_json::Value {
        ask(&self.sock, &path_cmd(cmd, path))
    }

    fn ids(&self, folder: &str) -> Vec<i64> {
        version_ids(&self.sock, &self.file(folder))
    }

    /// Whether this disk keeps versions, said on stderr when it does not.
    fn keeps_versions(&self) -> bool {
        let saved = self.reply("save_version", &self.file("alias"));
        if saved["kind"] == "ok" {
            return true;
        }
        let unavailable = self.reply("versions", &self.file("alias"))["unavailable"].clone();
        assert_ne!(unavailable, "", "{saved}");
        eprintln!("this disk keeps no versions ({unavailable}); test declared unsupported");
        false
    }

    /// One version listed for `folder`'s file: the state on disk is never
    /// listed, and keeping it again leaves any automatic version of it nothing
    /// new to keep.
    fn keep_one(&self, folder: &str, edit: &str) {
        let file = self.file(folder);
        assert_eq!(self.reply("save_version", &file)["kind"], "ok", "{file:?}");
        std::fs::write(&file, edit).unwrap();
        assert_eq!(self.reply("save_version", &file)["kind"], "ok", "{file:?}");
    }

    /// `old` replaced by a link to `other`, `gone` removed, `proj` turned into
    /// a file, `vacated` replaced by a link to an empty folder, `moved` moved
    /// to `moved-to` with a link in its place, and `dangling` linked to a
    /// folder that does not exist yet.
    fn rearrange(&self) {
        let docs = &self.docs;
        std::fs::rename(docs.join("moved"), docs.join("moved-to")).unwrap();
        std::os::unix::fs::symlink(docs.join("moved-to"), docs.join("moved")).unwrap();
        std::fs::remove_dir_all(docs.join("vacated")).unwrap();
        std::fs::create_dir(docs.join("empty")).unwrap();
        std::os::unix::fs::symlink(docs.join("empty"), docs.join("vacated")).unwrap();
        std::fs::remove_dir_all(docs.join("old")).unwrap();
        std::os::unix::fs::symlink(docs.join("other"), docs.join("old")).unwrap();
        std::fs::remove_dir_all(docs.join("gone")).unwrap();
        std::fs::remove_dir_all(docs.join("proj")).unwrap();
        std::fs::write(docs.join("proj"), "a file now").unwrap();
        std::os::unix::fs::symlink(self.outside.join("later"), docs.join("dangling")).unwrap();
    }
}

impl Drop for PathMatrix {
    fn drop(&mut self) {
        drop(self.daemon.take());
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Guards: privacy action: a request naming a path answers for the document
/// it names whatever symlinks, removals or replacements happened on the way,
/// and nothing outside a source is read.
#[test]
fn client_path_matrix() {
    let matrix = PathMatrix::start();
    if !matrix.keeps_versions() {
        return;
    }
    matrix.keep_one("alias", "real edited");
    for folder in ["old", "other", "gone", "proj", "vacated", "moved"] {
        matrix.keep_one(folder, &format!("{folder} edited"));
    }
    matrix.rearrange();
    versions_by_spelling(&matrix);
    moved_folder_linked(&matrix);
    disk_requests(&matrix);
    origin_requests(&matrix);
    let other = forget_by_spelling(&matrix);
    source_root_replaced(&matrix, &other);
}

/// The versions listed through each spelling.
fn versions_by_spelling(matrix: &PathMatrix) {
    // Alias inside a source: one document under either spelling.
    let real = matrix.ids("real");
    assert_eq!(real.len(), 1, "{real:?}");
    assert_eq!(matrix.ids("alias"), real);
    // Folder replaced by a link inside the source: the versions kept under the
    // old folder stay its own, apart from those of the folder linked in. Both
    // of its states are listed, since neither is on disk any more.
    let old = matrix.ids("old");
    let other = matrix.ids("other");
    assert_eq!((old.len(), other.len()), (2, 1), "{old:?} {other:?}");
    assert!(!old.contains(&other[0]), "{old:?} {other:?}");
    // A path vanished, a folder turned into a file, and one replaced by a link
    // to where no such file is: rows keep their key.
    for folder in ["gone", "proj", "vacated"] {
        assert_eq!(matrix.ids(folder).len(), 2, "{folder}");
    }
    // Links to outside and to nothing: nothing was kept there.
    assert_eq!(matrix.ids("out"), Vec::<i64>::new());
    assert_eq!(matrix.ids("dangling"), Vec::<i64>::new());
}

/// A folder moved and a link put in its place while the daemon runs: the
/// rows follow the folder, so the link's spelling lists them where it leads
/// and a restore through it proceeds.
fn moved_folder_linked(matrix: &PathMatrix) {
    let kept = matrix.ids("moved");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(
        settles(|| matrix.ids("moved-to") == kept),
        "the rows did not follow the folder: {:?}",
        matrix.ids("moved-to")
    );
    assert_answers(
        &matrix.sock,
        &[(path_cmd("save_version", &matrix.file("moved")), None)],
    );
    let listed = matrix.ids("moved-to");
    assert!(
        kept.iter().all(|id| listed.contains(id)),
        "{kept:?} {listed:?}"
    );
    assert_eq!(matrix.ids("moved"), listed);
}

/// Requests that read the disk follow a link and stay inside the source. A restore through a link put in place of a folder that
/// has versions would write that folder's file over the one linked in, so it
/// is refused with the file it leads to named.
fn disk_requests(matrix: &PathMatrix) {
    let docs = &matrix.docs;
    let file = |folder: &str| matrix.file(folder);
    let save = |folder: &str| path_cmd("save_version", &file(folder));
    let reindex = |name: &str| path_cmd("reindex", &docs.join(name));
    let created = |folder: &str| path_cmd("register_created", &file(folder));
    let leads_to = file("other").display().to_string();
    let not_searched = Some("not in a searched folder");
    let no_source = Some("outside the configured sources");
    assert_answers(
        &matrix.sock,
        &[
            (save("old"), Some(leads_to.as_str())),
            // No file there: nothing a restore would lose.
            (save("gone"), None),
            (save("proj"), None),
            (save("vacated"), None),
            (
                path_cmd("save_version", &docs.join("proj")),
                Some("not kept: it has no document extension"),
            ),
            (save("out"), not_searched),
            (save("dangling"), not_searched),
            (reindex("alias"), None),
            (reindex("old"), None),
            (reindex("gone"), None),
            (reindex("proj"), None),
            (reindex("proj/a.txt"), None),
            (reindex("out/"), not_searched),
            (reindex("dangling"), not_searched),
            (created("alias"), None),
            (created("old"), None),
            (created("gone"), None),
            (created("proj"), None),
            (created("out"), no_source),
            (created("dangling"), no_source),
        ],
    );
    assert_eq!(
        matrix.ids("vacated").len(),
        2,
        "the restore moved vacated's rows"
    );
    // Recorded where the link leads.
    for folder in ["real", "other"] {
        let origin = matrix.reply("origin", &file(folder)).to_string();
        assert!(origin.contains("app_created"), "{folder}: {origin}");
    }
}

/// Origins recorded by a client: the file made is keyed inside the source as
/// [`disk_requests`] keys it, the one it came from may be anywhere.
fn origin_requests(matrix: &PathMatrix) {
    let file = |folder: &str| matrix.file(folder);
    let no_source = Some("outside the configured sources");
    let pair = |cmd: &str, src: &Path, dst: &str| pair_cmd(cmd, src, &matrix.docs.join(dst));
    assert_answers(
        &matrix.sock,
        &[
            (
                pair(
                    "register_copy",
                    &matrix.outside.join("a.txt"),
                    "alias/c.txt",
                ),
                None,
            ),
            (pair("register_copy", &file("out"), "old/c.txt"), None),
            (
                pair("register_copy", Path::new("a.txt"), "real/c.txt"),
                Some("relative"),
            ),
            (pair("register_copy", &file("real"), "out/c.txt"), no_source),
            (pair("register_move", &file("gone"), "proj/m.txt"), None),
            (
                pair("register_move", &file("real"), "dangling/m.txt"),
                no_source,
            ),
            (pair("register_derived", &file("proj"), "gone/d.txt"), None),
            (
                pair("register_derived", &file("real"), "out/d.txt"),
                no_source,
            ),
        ],
    );
}

/// Forgetting through a spelling erases every version listed through it or
/// where it leads; returns those of `other` kept again.
fn forget_by_spelling(matrix: &PathMatrix) -> Vec<i64> {
    let forget = |folder: &str| matrix.reply("forget_versions", &matrix.file(folder));
    assert_eq!(forget("old")["kind"], "ok");
    assert_eq!(matrix.ids("old"), Vec::<i64>::new());
    assert_eq!(matrix.ids("other"), Vec::<i64>::new());
    assert_eq!(forget("alias")["kind"], "ok");
    assert_eq!(matrix.ids("real"), Vec::<i64>::new());
    for folder in ["gone", "proj", "vacated", "out", "dangling"] {
        assert_eq!(forget(folder)["kind"], "ok", "{folder}");
        assert_eq!(matrix.ids(folder), Vec::<i64>::new(), "{folder}");
    }
    matrix.keep_one("alias", "real again");
    assert_eq!(matrix.ids("real").len(), 1);
    matrix.keep_one("other", "other again");
    let other = matrix.ids("other");
    assert_eq!(other.len(), 1, "{other:?}");
    other
}

/// The source root replaced by a link while the daemon runs leads nowhere
/// new, and the rows kept under it are still found by their spelling.
fn source_root_replaced(matrix: &PathMatrix, other: &[i64]) {
    std::fs::rename(&matrix.home, matrix.base.join("home.old")).unwrap();
    std::os::unix::fs::symlink(&matrix.outside, &matrix.home).unwrap();
    std::fs::create_dir_all(matrix.outside.join("docs/real")).unwrap();
    std::fs::write(matrix.outside.join("docs/real/a.txt"), "outside").unwrap();
    let real = matrix.file("real");
    let not_searched = Some("not in a searched folder");
    assert_answers(
        &matrix.sock,
        &[
            (path_cmd("reindex", &matrix.docs.join("real")), not_searched),
            (path_cmd("save_version", &real), not_searched),
            (
                path_cmd("register_created", &real),
                Some("outside the configured sources"),
            ),
        ],
    );
    // Its state on disk is out of reach now, so it is listed too.
    let listed = matrix.ids("other");
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed.contains(&other[0]), "{listed:?}");
    assert_eq!(
        matrix.reply("forget_versions", &matrix.file("other"))["kind"],
        "ok"
    );
    assert_eq!(matrix.ids("other"), Vec::<i64>::new());
}
