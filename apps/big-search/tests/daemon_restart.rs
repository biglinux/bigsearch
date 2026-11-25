//! A daemon that starts again: the saved folder list watched instead of a
//! walk, and what changed while it was stopped caught up.
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
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A restart watches the saved folders without walking: a file rewritten in
/// place while the daemon was stopped changes no folder and waits for the walk
/// a minute after the start, or `rescan`.
#[test]
fn restart_watches_without_walking() {
    let sandbox = Sandbox::new("bs-restart");
    sandbox.age_folders();
    let first = sandbox.docs.join("first.txt");
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(5)),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::write(&first, "alphabet").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&first), Some(5));

    let rescan = sandbox.command(&["rescan"]).output().unwrap();
    assert!(
        rescan.status.success(),
        "rescan failed: {}",
        String::from_utf8_lossy(&rescan.stderr)
    );
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(8)),
        "rescan did not read the file rewritten while stopped"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// The walk after a start from the saved list reads what changed while the
/// daemon was stopped and moved no folder: a file rewritten in place, and a
/// `.gitignore` rewritten in place to leave a folder out.
#[test]
fn saved_start_walks_once_idle() {
    let sandbox = Sandbox::new("bs-saved-check");
    let first = sandbox.docs.join("first.txt");
    let repo = sandbox.docs.join("repo");
    let built = repo.join("generated/out.txt");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(built.parent().unwrap()).unwrap();
    std::fs::write(&built, "beta").unwrap();
    std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&built) == Some(4)),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::write(&first, "alphabet").unwrap();
    std::fs::write(repo.join(".gitignore"), "generated/\n").unwrap();
    let _daemon = KillOnDrop(
        sandbox
            .command(&["daemon"])
            .env("BIG_SEARCH_SAVED_START_CHECK_MS", "200")
            .spawn()
            .unwrap(),
    );
    assert!(wait_socket(&sandbox.sock), "daemon never bound socket");
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(8)),
        "the file rewritten while stopped was not read again"
    );
    assert!(
        settles(|| sandbox.catalogued_size(&built).is_none()),
        "the folder the edited .gitignore leaves out is still catalogued"
    );
    assert_eq!(sandbox.name_hits("out"), 0);
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A folder created while the daemon was stopped is watched after a restart.
#[test]
fn folder_made_while_stopped_is_watched() {
    let sandbox = Sandbox::new("bs-new-folder");
    let daemon = sandbox.start();
    sandbox.stop(daemon);

    std::fs::create_dir(sandbox.docs.join("later")).unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    std::fs::write(sandbox.docs.join("later/inside.txt"), "beta").unwrap();
    assert!(
        settles(|| sandbox.name_hits("inside") == 1),
        "the folder made while stopped is not watched"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A file added while the daemon was stopped is found at the restart even
/// when its folder's modification time was set back, as `cp -a` and
/// `rsync -a` do.
#[test]
fn folder_time_set_back_caught_up() {
    let sandbox = Sandbox::new("bs-time-set-back");
    sandbox.age_folders();
    let daemon = sandbox.start();
    sandbox.stop(daemon);

    std::fs::write(sandbox.docs.join("restored.txt"), "beta").unwrap();
    std::fs::File::open(&sandbox.docs)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.name_hits("restored"), 1);
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A saved folder replaced by a symlink is not watched through it, nor are the
/// saved folders below it, and what the catalogue held under it leaves.
#[test]
fn symlinked_folder_left_alone() {
    let sandbox = Sandbox::new("bs-symlinked-folder");
    let outside = sandbox.base.join("outside");
    std::fs::create_dir_all(outside.join("inner")).unwrap();
    std::fs::create_dir_all(sandbox.docs.join("linked/inner")).unwrap();
    let stale = sandbox.docs.join("linked/inner/stale.txt");
    std::fs::write(&stale, "beta").unwrap();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&stale).is_some()),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::rename(sandbox.docs.join("linked"), sandbox.docs.join("old")).unwrap();
    std::os::unix::fs::symlink(&outside, sandbox.docs.join("linked")).unwrap();
    let _daemon = sandbox.start();
    assert!(sandbox.await_idle(), "the restart never finished watching");
    std::fs::write(outside.join("inner/intruder.txt"), "gamma").unwrap();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.name_hits("intruder"), 0);
    assert_eq!(sandbox.catalogued_size(&stale), None);
    assert_eq!(
        query_paths(&sandbox.sock, "name", "stale", &sandbox.docs.join("linked")).0,
        0
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A folder this user cannot read does not keep the next start walking, and
/// once made readable it is indexed by the next start.
#[test]
fn unreadable_folder_keeps_the_record() {
    if scratch::runs_as_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new("bs-unreadable");
    let locked = sandbox.docs.join("locked");
    std::fs::create_dir(&locked).unwrap();
    let inside = locked.join("inside.txt");
    std::fs::write(&inside, "alpha").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    let recorded = settles(|| sandbox.run_recorded());
    drop(daemon);
    let _ = std::fs::remove_file(&sandbox.sock);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(recorded, "the run did not record its folders");

    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&inside), Some(5));
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A content index left empty at start, as a new analyzer leaves it, is filled
/// again on a start that watches the saved folders.
#[test]
fn emptied_content_index_refilled() {
    let sandbox = Sandbox::new("bs-content-refilled");
    std::fs::write(sandbox.docs.join("note.md"), "epsilon-unique-word").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    let found = |what: &str| {
        await_content_result(
            &sandbox.home,
            &sandbox.data,
            &sandbox.run,
            "epsilon-unique-word",
            what,
        )
    };
    assert!(found("note.md"), "the note was never indexed");
    sandbox.stop(daemon);
    for entry in std::fs::read_dir(sandbox.data.join("big-search/content-index")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }

    let _daemon = sandbox.start();
    assert!(
        found("note.md"),
        "the emptied content index was not filled again"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A folder renamed over a listed one with the same time and size (two empty
/// folders) is watched and listed with its own inode.
#[test]
fn renamed_over_folder_relisted() {
    use std::os::unix::fs::MetadataExt;
    let sandbox = Sandbox::new("bs-renamed-over");
    let project = sandbox.docs.join("proj");
    let replacement = sandbox.docs.join("proj.new");
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for folder in [&project, &replacement] {
        std::fs::create_dir(folder).unwrap();
        std::fs::File::open(folder)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
    }
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    std::fs::rename(&replacement, &project).unwrap();
    let inode = std::fs::symlink_metadata(&project).unwrap().ino();
    let listed = |db: rusqlite::Connection| {
        db.query_row(
            "SELECT size FROM file_state WHERE namespace = 'watch' AND path = ?1",
            [project.to_str().unwrap()],
            |row| row.get::<_, i64>(0),
        )
    };
    assert!(
        settles(|| sandbox
            .state_db()
            .and_then(listed)
            .is_ok_and(|size| u64::try_from(size) == Ok(inode))),
        "the folder renamed into place is listed with the old inode"
    );
    std::fs::write(project.join("plan.txt"), "delta").unwrap();
    assert!(
        settles(|| sandbox.name_hits("plan") == 1),
        "the folder renamed into place is not watched"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A saved folder that cannot be read keeps its entries, at the next start and
/// through a rescan: a failed lookup is no evidence that its files are gone.
#[test]
fn unreadable_saved_folder_keeps_entries() {
    if scratch::runs_as_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new("bs-unreadable-saved");
    let outer = sandbox.docs.join("outer");
    let inner = outer.join("inner");
    let deep = inner.join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    let inside = inner.join("inside.txt");
    std::fs::write(&inside, "alpha").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    let recorded = settles(|| sandbox.run_recorded());
    drop(daemon);
    let _ = std::fs::remove_file(&sandbox.sock);
    assert!(recorded, "the run did not record its folders");

    std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable_again = Readable(&outer);
    let daemon = sandbox.start();
    assert!(sandbox.await_idle(), "the start never finished");
    let recorded_while_unread = sandbox.run_recorded();
    // A rescan cannot read it either: what it held stays, and a folder only
    // this user cannot read does not keep the run from being recorded.
    assert!(command_accepted(&sandbox.sock, "rescan"));
    sandbox.await_docs_watched();
    let kept = sandbox.catalogued_size(&inside);
    let recorded_after_rescan = settles(|| sandbox.run_recorded());
    drop(daemon);
    let _ = std::fs::remove_file(&sandbox.sock);
    drop(readable_again);
    assert!(
        !recorded_while_unread,
        "a run that missed a folder was recorded"
    );
    assert_eq!(
        kept,
        Some(5),
        "the rescan dropped what the unread folder held"
    );
    assert!(recorded_after_rescan, "the rescan did not record its run");

    // Readable again: the folders below it are still watched.
    let _daemon = sandbox.start();
    assert!(sandbox.await_idle(), "the start never finished");
    std::fs::write(deep.join("arrived.txt"), "beta").unwrap();
    assert!(
        settles(|| sandbox.name_hits("arrived") == 1),
        "a folder below the unreadable one lost its watch"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// Gives a folder its mode back however the test ends, so it can be removed.
struct Readable<'a>(&'a std::path::Path);

impl Drop for Readable<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A folder made readable while the daemon runs is watched and indexed.
#[test]
fn folder_made_readable_is_indexed() {
    if scratch::runs_as_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new("bs-made-readable");
    let locked = sandbox.docs.join("locked");
    std::fs::create_dir(&locked).unwrap();
    let inside = locked.join("inside.txt");
    std::fs::write(&inside, "alpha").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        settles(|| sandbox.catalogued_size(&inside) == Some(5)),
        "the folder made readable was not indexed"
    );
    std::fs::write(locked.join("later.txt"), "beta").unwrap();
    assert!(
        settles(|| sandbox.name_hits("later") == 1),
        "the folder made readable is not watched"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A removable source is walked at every start: it may have changed on
/// another machine without moving its folder times.
#[test]
fn removable_source_walks_at_start() {
    let sandbox = Sandbox::new("bs-removable");
    sandbox.age_folders();
    let config = sandbox.data.join("cfg/big-search/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "[[source]]\npath = \"{}\"\nremovable = true\n",
            sandbox.home.display()
        ),
    )
    .unwrap();
    let first = sandbox.docs.join("first.txt");
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(5)),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::write(&first, "alphabet").unwrap();
    let _daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(8)),
        "the removable source was not walked at start"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A source removed from the settings leaves the index and the saved folder
/// list at the next start.
#[test]
fn removed_source_leaves_on_restart() {
    let sandbox = Sandbox::new("bs-removed-source");
    let kept = sandbox.home.join("kept");
    let dropped = sandbox.home.join("dropped");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::create_dir_all(dropped.join("sub")).unwrap();
    std::fs::write(kept.join("kept.txt"), "alpha").unwrap();
    std::fs::write(dropped.join("sub/gone.txt"), "beta").unwrap();
    let config = sandbox.data.join("cfg/big-search/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let source = |dir: &Path| format!("[[source]]\npath = \"{}\"\n", dir.display());
    std::fs::write(&config, source(&kept) + &source(&dropped)).unwrap();

    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.name_hits("gone") == 1),
        "the second source was not indexed"
    );
    sandbox.stop(daemon);
    assert!(catalogue_rows(&sandbox.data, "watch", &dropped) > 0);

    std::fs::write(&config, source(&kept)).unwrap();
    let _daemon = sandbox.start();
    assert!(
        settles(|| sandbox.name_hits("gone") == 0),
        "the removed source is still searchable"
    );
    assert!(
        settles(|| catalogue_rows(&sandbox.data, "watch", &dropped) == 0),
        "the removed source is still in the saved folder list"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// The catch-up compares the direct entries of a changed folder; a watched
/// folder among them is not walked, since it has a pass of its own.
#[test]
fn catch_up_skips_watched_subtrees() {
    let sandbox = Sandbox::new("bs-catch-up-depth");
    let deep = sandbox.docs.join("big/deep");
    std::fs::create_dir_all(&deep).unwrap();
    let inner = deep.join("inner.txt");
    std::fs::write(&inner, "alpha").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&inner) == Some(5)),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    // `docs` and `big` change; `deep` does not, so its file waits for a rescan.
    std::fs::write(sandbox.docs.join("top.txt"), "beta").unwrap();
    std::fs::write(sandbox.docs.join("big/added.txt"), "beta").unwrap();
    std::fs::write(&inner, "alphabet").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.name_hits("added"), 1);
    assert_eq!(sandbox.catalogued_size(&inner), Some(5));
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A rebuild while paused records no run: events are dropped while paused, so
/// the next start walks.
#[test]
fn rebuild_while_paused_records_nothing() {
    let sandbox = Sandbox::new("bs-paused-rebuild");
    // Out of `docs`, whose time the restart's catch-up must find unchanged.
    let side = sandbox.home.join("side");
    std::fs::create_dir(&side).unwrap();
    sandbox.age_folders();
    let first = sandbox.docs.join("first.txt");
    let daemon = sandbox.start();
    assert!(settles(|| sandbox.run_recorded()), "the run never recorded");
    assert!(command_accepted(&sandbox.sock, "pause"));
    let during = side.join("during.txt");
    std::fs::write(&during, "beta").unwrap();
    assert!(command_accepted(&sandbox.sock, "rebuild"));
    assert!(
        settles(|| sandbox.catalogued_size(&during).is_some()),
        "the rebuild did not finish"
    );
    // The rebuild records its run, when it does, right after the catalogue.
    sleep(Duration::from_millis(300));
    std::fs::write(&first, "alphabet").unwrap();
    drop(daemon);
    let _ = std::fs::remove_file(&sandbox.sock);

    let _daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(8)),
        "the start after a paused rebuild did not walk"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A folder another one took the place of while the daemon was stopped leaves
/// no entry of the old one behind.
#[test]
fn swapped_folder_leaves_no_ghosts() {
    let sandbox = Sandbox::new("bs-swapped-folder");
    let proj = sandbox.docs.join("proj");
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::write(proj.join("sub/ghost.txt"), "alpha").unwrap();
    // Older than the first start, like a restored tree: only the renamed
    // folder's own time moves.
    let fresh = sandbox.docs.join("proj.new");
    std::fs::create_dir_all(fresh.join("sub")).unwrap();
    std::fs::write(fresh.join("sub/fresh.txt"), "beta").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.name_hits("ghost") == 1),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::rename(&proj, sandbox.docs.join("proj.old")).unwrap();
    std::fs::rename(&fresh, &proj).unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&proj.join("sub/ghost.txt")), None);
    assert!(
        sandbox
            .catalogued_size(&proj.join("sub/fresh.txt"))
            .is_some()
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A source inside a dot-folder keeps its files across a restart: only what
/// is hidden below the source is out of scope.
#[test]
fn hidden_source_keeps_its_files() {
    let sandbox = Sandbox::new("bs-hidden-source");
    let notes = sandbox.home.join(".stash/notes");
    std::fs::create_dir_all(&notes).unwrap();
    let kept = notes.join("kept.txt");
    std::fs::write(&kept, "alpha").unwrap();
    let config = sandbox.data.join("cfg/big-search/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!("[[source]]\npath = \"{}\"\n", notes.display()),
    )
    .unwrap();
    let daemon = sandbox.start();
    sandbox.stop(daemon);

    std::fs::write(notes.join("later.txt"), "beta").unwrap();
    let _daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&notes.join("later.txt")).is_some()),
        "the file added while stopped was not found"
    );
    assert_eq!(sandbox.catalogued_size(&kept), Some(5));
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A change to the global git ignore file makes the next start walk, and the
/// start's catch-up honors it as the walk does.
#[test]
fn global_git_ignore_change_walks() {
    let sandbox = Sandbox::new("bs-git-excludes");
    let repo = sandbox.docs.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let secret = repo.join("a.secret");
    std::fs::write(&secret, "alpha").unwrap();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&secret).is_some()),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    let excludes = sandbox.data.join("cfg/git/ignore");
    std::fs::create_dir_all(excludes.parent().unwrap()).unwrap();
    std::fs::write(&excludes, "*.secret\n").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(
        sandbox.catalogued_size(&secret),
        None,
        "the newly ignored file is still catalogued"
    );
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A saved folder replaced by a file while the daemon was stopped leaves with
/// what it held, and the file is indexed.
#[test]
fn folder_turned_file_leaves_no_ghosts() {
    let sandbox = Sandbox::new("bs-folder-to-file");
    let sub = sandbox.docs.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let ghost = sub.join("ghostfile.txt");
    std::fs::write(&ghost, "alpha").unwrap();
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&ghost).is_some()),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::remove_dir_all(&sub).unwrap();
    std::fs::write(&sub, "x").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&ghost), None);
    assert_eq!(
        query_paths(&sandbox.sock, "name", "ghostfile", &sandbox.home).0,
        0
    );
    assert_eq!(sandbox.catalogued_size(&sub), Some(1));
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A source folder made anew while the daemon was stopped is walked: nothing
/// of the old one stays, and the root itself is never a catalogue entry.
#[test]
fn recreated_root_leaves_no_ghosts() {
    let sandbox = Sandbox::new("bs-recreated-root");
    let first = sandbox.docs.join("first.txt");
    let daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first).is_some()),
        "first start did not walk"
    );
    sandbox.stop(daemon);

    std::fs::rename(&sandbox.home, sandbox.base.join("home.old")).unwrap();
    std::fs::create_dir_all(&sandbox.docs).unwrap();
    let fresh = sandbox.docs.join("fresh.txt");
    std::fs::write(&fresh, "beta").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&first), None);
    assert!(sandbox.catalogued_size(&fresh).is_some());
    assert_eq!(sandbox.catalogued_size(&sandbox.home), None);
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// A folder listed but not catalogued, as a kill between the two saves leaves
/// it, is walked whole by the next start's catch-up.
#[test]
fn listed_uncataloged_folder_is_walked() {
    let sandbox = Sandbox::new("bs-listed-only");
    let sub = sandbox.docs.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let inner = sub.join("inner.txt");
    std::fs::write(&inner, "alpha").unwrap();
    sandbox.age_folders();
    let daemon = sandbox.start();
    sandbox.stop(daemon);

    let db = rusqlite::Connection::open(sandbox.data.join("big-search/state.sqlite")).unwrap();
    db.execute(
        "DELETE FROM file_state WHERE namespace = 'name' AND (path = ?1 OR path = ?2)",
        [sub.to_str().unwrap(), inner.to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    std::fs::write(sandbox.docs.join("top.txt"), "beta").unwrap();
    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&inner), Some(5));
    let _ = std::fs::remove_dir_all(&sandbox.base);
}

/// Guards: privacy refusal: a start reads the browsers' download lists only
/// once `[origin] browser_history` is on in the configuration file.
#[test]
fn browser_downloads_read_when_allowed() {
    let sandbox = Sandbox::new("bs-browser-history");
    let download = sandbox.docs.join("report.pdf");
    std::fs::write(&download, "pdf").unwrap();
    let profile = sandbox.data.join("cfg/chromium/Default");
    std::fs::create_dir_all(&profile).unwrap();
    // Chromium counts microseconds since 1601.
    let finished = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 11_644_473_600)
        * 1_000_000;
    let history = rusqlite::Connection::open(profile.join("History")).unwrap();
    history
        .execute_batch(
            "CREATE TABLE downloads (id INTEGER PRIMARY KEY, current_path TEXT,
                 target_path TEXT, start_time INTEGER, end_time INTEGER, state INTEGER);
             CREATE TABLE downloads_url_chains (id INTEGER, chain_index INTEGER, url TEXT);
             INSERT INTO downloads_url_chains
                 VALUES (1, 0, 'https://files.example.org/report.pdf?token=secret');",
        )
        .unwrap();
    history
        .execute(
            "INSERT INTO downloads VALUES (1, ?1, ?1, ?2, ?2, 1)",
            rusqlite::params![download.to_str().unwrap(), finished.cast_signed()],
        )
        .unwrap();
    drop(history);
    let config = sandbox.data.join("cfg/big-search/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let origin = || ask(&sandbox.sock, &path_cmd("origin", &download))["origin"].clone();

    std::fs::write(&config, "[origin]\nbrowser_history = false\n").unwrap();
    let daemon = sandbox.start();
    assert!(sandbox.await_idle(), "the first start never finished");
    assert!(origin().is_null(), "read while off: {}", origin());
    sandbox.stop(daemon);

    std::fs::write(&config, "[origin]\nbrowser_history = true\n").unwrap();
    let _daemon = sandbox.start();
    assert!(sandbox.await_idle(), "the second start never finished");
    assert_eq!(origin()["source_domain"], "files.example.org");
    let _ = std::fs::remove_dir_all(&sandbox.base);
}
