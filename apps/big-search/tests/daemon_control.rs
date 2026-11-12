//! Lifecycle commands over the v2 protocol: pause flips the reported state, resume
//! clears it, rebuild walks the disk again.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test spawns the built big-search binary under test"
)]

mod scratch;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn env_cmd(args: &[&str], home: &Path, xdg_storage_home: &Path, run: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_big-search"));
    c.args(args)
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", run);
    c
}

/// Send one request line, return the reply line.
fn send_daemon_request(sock: &Path, json: &str) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect daemon");
    stream.write_all(json.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    line
}

fn wait_socket(sock: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if sock.exists() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

fn await_content_result(
    home: &Path,
    xdg_storage_home: &Path,
    run: &Path,
    query: &str,
    needle: &str,
) -> bool {
    // The daemon paces its backfill by the machine's load and rests 30 s between
    // batches when the machine is busy (`throttle::pause_after`, `Load::Heavy`);
    // a parallel build is enough. The wait covers one such rest; a calm machine
    // answers in about a second.
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        let output = env_cmd(&["content", "--", query], home, xdg_storage_home, run)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "content query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if String::from_utf8_lossy(&output.stdout).contains(needle) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

/// Send one lifecycle command; whether the daemon accepted it.
fn command_accepted(sock: &Path, cmd: &str) -> bool {
    send_daemon_request(sock, &format!("{{\"v\":2,\"cmd\":\"{cmd}\"}}")).contains("\"kind\":\"ok\"")
}

/// The `indexed` count a status reply reports.
fn indexed_count(sock: &Path) -> Option<u64> {
    let reply = send_daemon_request(sock, "{\"v\":2,\"cmd\":\"status\"}");
    let rest = &reply[reply.find("\"indexed\":")? + "\"indexed\":".len()..];
    rest[..rest.find(|c: char| !c.is_ascii_digit())?]
        .parse()
        .ok()
}

/// Poll `status` until `indexed` reaches `want`. A rebuild walks the disk in
/// the background, as slowly as the backfill on a busy machine.
fn await_indexed(sock: &Path, want: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if indexed_count(sock) == Some(want) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// Poll `status` until `paused` reads the wanted value (commands apply async).
fn await_paused(sock: &Path, want: bool) -> bool {
    let needle = format!("\"paused\":{want}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if send_daemon_request(sock, "{\"v\":2,\"cmd\":\"status\"}").contains(&needle) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

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

    std::fs::remove_dir_all(&base).ok();
}

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

    std::fs::remove_dir_all(&base).ok();
}

/// One `query` over the socket: the reply's `total` and the hits' paths.
///
/// `total` is counted before any hit is `stat`ed, so it still counts what an
/// index holds for a file that has disappeared; the hits do not.
fn query_paths(sock: &Path, mode: &str, q: &str, under: &Path) -> (u64, Vec<String>) {
    let request = serde_json::json!({
        "v": 2, "cmd": "query", "q": q, "mode": mode,
        "filter": { "under": under }, "page": { "limit": 100 },
    });
    let reply: serde_json::Value =
        serde_json::from_str(&send_daemon_request(sock, &request.to_string())).unwrap();
    let paths = reply["hits"]
        .as_array()
        .map(|hits| {
            hits.iter()
                .filter_map(|hit| hit["path"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (reply["total"].as_u64().unwrap_or(u64::MAX), paths)
}

/// Rows the catalogue keeps under `dir` in `namespace`.
fn catalogue_rows(xdg_storage_home: &Path, namespace: &str, dir: &Path) -> i64 {
    let db = rusqlite::Connection::open_with_flags(
        xdg_storage_home.join("big-search/state.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let dir = dir.to_str().unwrap();
    db.query_row(
        "SELECT COUNT(*) FROM file_state WHERE namespace = ?1 AND path >= ?2 AND path < ?3",
        rusqlite::params![namespace, format!("{dir}/"), format!("{dir}0")],
        |row| row.get(0),
    )
    .unwrap()
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
        std::fs::create_dir_all(docs.join(tree).join("deep")).unwrap();
        std::fs::write(
            docs.join(tree).join(format!("{tree}-top.txt")),
            format!("{tree}wordtop"),
        )
        .unwrap();
        std::fs::write(
            docs.join(tree).join(format!("deep/{tree}-deep.txt")),
            format!("{tree}worddeep"),
        )
        .unwrap();
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

    // Each check polls: the watcher debounces, and the content side follows
    // in the next backfill pass, which a busy machine delays (see
    // `await_content_result`).
    let deadline = Instant::now() + Duration::from_secs(45);
    let settled = |check: &dyn Fn() -> bool| {
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            sleep(Duration::from_millis(100));
        }
        false
    };
    for tree in ["deleted", "away", "moving"] {
        let old = docs.join(tree);
        assert!(
            settled(&|| query_paths(&sock, "list", "", &old).0 == 0),
            "{tree}: the catalogue still lists {:?}",
            query_paths(&sock, "list", "", &old)
        );
        assert!(
            settled(&|| query_paths(&sock, "name", tree, &old).0 == 0),
            "{tree}: the name index still holds {:?}",
            query_paths(&sock, "name", tree, &old)
        );
        let word = format!("{tree}worddeep");
        assert!(
            settled(&|| query_paths(&sock, "content", &word, &old).0 == 0),
            "{tree}: the content index still answers for the old path"
        );
        assert!(
            settled(&|| catalogue_rows(&xdg_storage_home, "content", &old) == 0),
            "{tree}: the content catalogue still holds rows under the old path"
        );
        assert_eq!(catalogue_rows(&xdg_storage_home, "name", &old), 0);
    }
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

    std::fs::remove_dir_all(&base).ok();
}

/// Polls `check` for up to 20 s.
fn settles(check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
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
    std::fs::remove_dir_all(&base).ok();
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
    std::fs::remove_dir_all(&base).ok();
}

/// A home with a `docs` folder holding `first.txt` ("alpha"), and the
/// daemon's data and runtime folders, for tests that stop and start it.
struct Sandbox {
    base: PathBuf,
    home: PathBuf,
    docs: PathBuf,
    data: PathBuf,
    run: PathBuf,
    sock: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let base = scratch::scratch_dir(name);
        let home = base.join("home");
        let docs = home.join("docs");
        let run = base.join("run");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(docs.join("first.txt"), "alpha").unwrap();
        Self {
            data: base.join("data"),
            sock: run.join("biglinux/indexd.sock"),
            base,
            home,
            docs,
            run,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        env_cmd(args, &self.home, &self.data, &self.run)
    }

    fn start(&self) -> KillOnDrop {
        let daemon = KillOnDrop(self.command(&["daemon"]).spawn().unwrap());
        assert!(wait_socket(&self.sock), "daemon never bound socket");
        daemon
    }

    /// Stop the daemon once its run is recorded, as a restart finds it.
    fn stop(&self, daemon: KillOnDrop) {
        assert!(
            settles(|| self.run_recorded()),
            "the run never recorded its folders"
        );
        drop(daemon);
        std::fs::remove_file(&self.sock).ok();
    }

    fn state_db(&self) -> rusqlite::Result<rusqlite::Connection> {
        rusqlite::Connection::open_with_flags(
            self.data.join("big-search/state.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
    }

    /// Whether a run recorded its watched folders for the next start.
    fn run_recorded(&self) -> bool {
        self.state_db()
            .and_then(|db| {
                db.query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = 'watch_fingerprint'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .is_ok_and(|rows| rows == 1)
    }

    /// The size the name catalogue records for `path`.
    fn catalogued_size(&self, path: &Path) -> Option<i64> {
        self.state_db()
            .and_then(|db| {
                db.query_row(
                    "SELECT size FROM file_state WHERE namespace = 'name' AND path = ?1",
                    [path.to_str().unwrap()],
                    |row| row.get(0),
                )
            })
            .ok()
    }

    /// Poll `status` until the daemon reports idle: a start that watches the
    /// saved folders reports indexing until every one is watched.
    fn await_idle(&self) -> bool {
        settles(|| {
            send_daemon_request(&self.sock, "{\"v\":2,\"cmd\":\"status\"}")
                .contains("\"activity\":\"idle\"")
        })
    }

    fn name_hits(&self, q: &str) -> u64 {
        query_paths(&self.sock, "name", q, &self.home).0
    }

    /// Wait until a file written now in `docs` is found: the watches are in
    /// place and every earlier event was handled.
    fn await_docs_watched(&self) {
        assert!(self.await_idle(), "the start never finished watching");
        std::fs::write(self.docs.join("online.txt"), "gamma").unwrap();
        assert!(
            settles(|| self.name_hits("online") == 1),
            "docs was not watched after the restart"
        );
    }

    /// Folder times are compared in whole seconds with a second of margin:
    /// wait until the folders made so far are older than the next start.
    fn age_folders(&self) {
        sleep(Duration::from_millis(2100));
    }
}

/// A restart watches the saved folders without walking: a file rewritten in
/// place while the daemon was stopped changes no folder and waits for
/// `rescan`.
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
}

/// A saved folder replaced by a symlink is not watched through it, nor are the
/// saved folders below it, and what the catalogue held under it leaves.
#[test]
fn symlinked_folder_not_followed() {
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
    std::fs::remove_dir_all(&sandbox.base).ok();
}

/// A folder this user cannot read does not keep the next start walking, and
/// once made readable it is indexed by the next start.
#[test]
fn unreadable_folder_keeps_the_record() {
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
    std::fs::remove_file(&sandbox.sock).ok();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(recorded, "the run did not record its folders");

    let _daemon = sandbox.start();
    sandbox.await_docs_watched();
    assert_eq!(sandbox.catalogued_size(&inside), Some(5));
    std::fs::remove_dir_all(&sandbox.base).ok();
}

/// A folder made readable while the daemon runs is watched and indexed.
#[test]
fn folder_made_readable_is_indexed() {
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_file(&sandbox.sock).ok();

    let _daemon = sandbox.start();
    assert!(
        settles(|| sandbox.catalogued_size(&first) == Some(8)),
        "the start after a paused rebuild did not walk"
    );
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
}

/// A change to the global git ignore file makes the next start walk, and the
/// start's catch-up honours it as the walk does.
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
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
    std::fs::remove_dir_all(&sandbox.base).ok();
}

/// A folder listed but not catalogued, as a kill between the two saves leaves
/// it, is walked whole by the next start's catch-up.
#[test]
fn listed_uncatalogued_folder_is_walked() {
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
    std::fs::remove_dir_all(&sandbox.base).ok();
}
