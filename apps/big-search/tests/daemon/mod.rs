//! What the daemon tests share: starting the built daemon in a private home,
//! talking to it over its socket and reading its state database.
#![allow(
    dead_code,
    reason = "every test binary compiles this module; each uses part of it"
)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::scratch;

pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn env_cmd(args: &[&str], home: &Path, xdg_storage_home: &Path, run: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_big-search"));
    c.args(args)
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", run);
    c
}

/// Send one request line, return the reply line. A daemon that stops
/// answering fails the test instead of hanging it.
pub fn send_daemon_request(sock: &Path, json: &str) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect daemon");
    let limit = Some(Duration::from_secs(30));
    stream.set_read_timeout(limit).unwrap();
    stream.set_write_timeout(limit).unwrap();
    stream.write_all(json.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .unwrap_or_else(|error| panic!("no reply to {json}: {error}"));
    line
}

pub fn wait_socket(sock: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if sock.exists() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

pub fn await_content_result(
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
pub fn command_accepted(sock: &Path, cmd: &str) -> bool {
    send_daemon_request(sock, &format!("{{\"v\":2,\"cmd\":\"{cmd}\"}}")).contains("\"kind\":\"ok\"")
}

/// The `indexed` count a status reply reports.
pub fn indexed_count(sock: &Path) -> Option<u64> {
    let reply = send_daemon_request(sock, "{\"v\":2,\"cmd\":\"status\"}");
    let rest = &reply[reply.find("\"indexed\":")? + "\"indexed\":".len()..];
    rest[..rest.find(|c: char| !c.is_ascii_digit())?]
        .parse()
        .ok()
}

/// Poll `status` until `indexed` reaches `want`. A rebuild walks the disk in
/// the background, as slowly as the backfill on a busy machine.
pub fn await_indexed(sock: &Path, want: u64) -> bool {
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
pub fn await_paused(sock: &Path, want: bool) -> bool {
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

/// One `query` over the socket: the reply's `total` and the hits' paths.
///
/// `total` is counted before any hit is `stat`ed, so it still counts what an
/// index holds for a file that has disappeared; the hits do not.
pub fn query_paths(sock: &Path, mode: &str, q: &str, under: &Path) -> (u64, Vec<String>) {
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
pub fn catalogue_rows(xdg_storage_home: &Path, namespace: &str, dir: &Path) -> i64 {
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

/// Polls `check` for up to 20 s.
pub fn settles(check: impl Fn() -> bool) -> bool {
    settles_within(Duration::from_secs(20), check)
}

/// Whether `check` holds within `wait`, polled every 100 ms.
pub fn settles_within(wait: Duration, check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

/// A home with a `docs` folder holding `first.txt` ("alpha"), and the
/// daemon's data and runtime folders, for tests that stop and start it.
pub struct Sandbox {
    pub base: scratch::ScratchDir,
    pub home: PathBuf,
    pub docs: PathBuf,
    pub data: PathBuf,
    pub run: PathBuf,
    pub sock: PathBuf,
}

impl Sandbox {
    pub fn new(name: &str) -> Self {
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

    pub fn command(&self, args: &[&str]) -> Command {
        env_cmd(args, &self.home, &self.data, &self.run)
    }

    pub fn start(&self) -> KillOnDrop {
        let daemon = KillOnDrop(self.command(&["daemon"]).spawn().unwrap());
        assert!(wait_socket(&self.sock), "daemon never bound socket");
        daemon
    }

    /// Stop the daemon once its run is recorded, as a restart finds it.
    pub fn stop(&self, daemon: KillOnDrop) {
        assert!(
            settles(|| self.run_recorded()),
            "the run never recorded its folders"
        );
        drop(daemon);
        let _ = std::fs::remove_file(&self.sock);
    }

    pub fn state_db(&self) -> rusqlite::Result<rusqlite::Connection> {
        rusqlite::Connection::open_with_flags(
            self.data.join("big-search/state.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
    }

    /// Whether a run recorded its watched folders for the next start.
    pub fn run_recorded(&self) -> bool {
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
    pub fn catalogued_size(&self, path: &Path) -> Option<i64> {
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
    pub fn await_idle(&self) -> bool {
        settles(|| {
            send_daemon_request(&self.sock, "{\"v\":2,\"cmd\":\"status\"}")
                .contains("\"activity\":\"idle\"")
        })
    }

    pub fn name_hits(&self, q: &str) -> u64 {
        query_paths(&self.sock, "name", q, &self.home).0
    }

    /// Wait until a file written now in `docs` is found: the watches are in
    /// place and every earlier event was handled.
    pub fn await_docs_watched(&self) {
        assert!(self.await_idle(), "the start never finished watching");
        std::fs::write(self.docs.join("online.txt"), "gamma").unwrap();
        assert!(
            settles(|| self.name_hits("online") == 1),
            "docs was not watched after the restart"
        );
    }

    /// Folder times are compared in whole seconds with a second of margin:
    /// wait until the folders made so far are older than the next start.
    pub fn age_folders(&self) {
        sleep(Duration::from_millis(2100));
    }
}
