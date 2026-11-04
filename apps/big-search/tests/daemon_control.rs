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
use std::path::Path;
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

    // Pause → status reports paused.
    assert!(command_accepted(&sock, "pause"));
    assert!(await_paused(&sock, true), "daemon did not report paused");

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
