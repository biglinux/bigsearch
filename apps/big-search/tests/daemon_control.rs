//! Lifecycle commands over the v2 protocol: pause flips the reported state, resume
//! clears it, rebuild is accepted and leaves the index populated.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test spawns the built big-search binary under test"
)]

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
    // batches when the machine is busy (`throttle::pause_after`, `Load::Heavy`) —
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
    let base = std::env::temp_dir().join(format!("bs-ctl-{}", std::process::id()));
    std::fs::remove_dir_all(&base).ok();
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

    // Pause → status reports paused.
    assert!(send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"pause\"}").contains("\"kind\":\"ok\""));
    assert!(await_paused(&sock, true), "daemon did not report paused");

    // Resume → status reports running again.
    assert!(send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"resume\"}").contains("\"kind\":\"ok\""));
    assert!(await_paused(&sock, false), "daemon did not report resumed");

    // Rebuild is accepted; the index stays populated afterwards.
    assert!(
        send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"rebuild\"}").contains("\"kind\":\"ok\"")
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rebuilt = false;
    while Instant::now() < deadline {
        let status_reply = send_daemon_request(&sock, "{\"v\":2,\"cmd\":\"status\"}");
        // conf.txt + docs/ dir → at least one indexed entry once rebuild commits.
        if status_reply.contains("\"indexed\":") && !status_reply.contains("\"indexed\":0") {
            rebuilt = true;
            break;
        }
        sleep(Duration::from_millis(50));
    }
    assert!(rebuilt, "index empty after rebuild");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn daemon_content_backfill_continues_after_first_limited_batch() {
    let base = std::env::temp_dir().join(format!("bs-content-backfill-{}", std::process::id()));
    std::fs::remove_dir_all(&base).ok();
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
