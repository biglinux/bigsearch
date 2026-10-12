//! The daemon is single-instance: a second `daemon` on the same runtime dir must
//! refuse (exit 0) without stealing the socket or colliding on the Tantivy writer
//! lock. Regression guard for the "two daemons" bug.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test spawns the built big-search binary under test"
)]

mod scratch;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Kills the child when dropped so a failed assert never leaks the daemon.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon(home: &Path, xdg_storage_home: &Path, run: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_big-search"))
        .arg("daemon")
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", run)
        .spawn()
        .expect("spawn daemon")
}

fn wait_socket(sock: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if sock.exists() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

fn seed_index(home: &Path, xdg_storage_home: &Path, run: &Path) {
    let reindex = Command::new(env!("CARGO_BIN_EXE_big-search"))
        .arg("reindex")
        .arg(home)
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", run)
        .status()
        .unwrap();
    assert!(reindex.success());
}

/// The exit status of a daemon that has to give up on its own, within 8 s.
fn wait_for_refusal(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("second daemon did not exit — it should refuse the singleton lock");
        }
        sleep(Duration::from_millis(50));
    }
}

/// The raw reply line to a name query for "conf".
fn query_conf(sock: &Path) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect d1");
    stream
        .write_all(b"{\"v\":2,\"cmd\":\"query\",\"q\":\"conf\",\"mode\":\"name\",\"page\":{\"limit\":5}}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    line
}

#[test]
fn second_daemon_leaves_first_intact() {
    let base = scratch::scratch_dir("bs-singleton");
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(home.join("docs/conf.txt"), "config alpha").unwrap();

    // Seed the index so the daemon has something to serve immediately.
    seed_index(&home, &xdg_storage_home, &run);

    let sock = run.join("biglinux/indexd.sock");
    let d1 = KillOnDrop(daemon(&home, &xdg_storage_home, &run));
    assert!(
        wait_socket(&sock, Duration::from_secs(10)),
        "d1 never bound socket"
    );
    let inode_before = std::fs::metadata(&sock).unwrap().ino();

    // Second daemon must exit on its own (refused) within a few seconds.
    let mut d2 = daemon(&home, &xdg_storage_home, &run);
    let status = wait_for_refusal(&mut d2);
    assert!(
        status.success(),
        "refused daemon should exit 0, got {status:?}"
    );

    // The first daemon's socket must be untouched (not stolen/rebound).
    assert_eq!(
        inode_before,
        std::fs::metadata(&sock).unwrap().ino(),
        "second daemon stole the socket"
    );

    // The first daemon still answers a query (versioned protocol v2).
    let line = query_conf(&sock);
    assert!(
        line.contains("\"kind\":\"query\""),
        "d1 response not a query reply: {line}"
    );
    assert!(
        line.contains("conf.txt"),
        "d1 did not return the seeded file: {line}"
    );

    drop(d1); // kills the first daemon
    std::fs::remove_dir_all(&base).ok();
}
