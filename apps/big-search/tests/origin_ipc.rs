//! Origin/provenance IPC contract: apps can register a file origin and later ask
//! for it directly or as an opt-in query enrichment.
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_big-search"));
    command
        .args(args)
        .env("RUST_LOG", "off")
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_storage_home)
        .env("XDG_CONFIG_HOME", xdg_storage_home.join("cfg"))
        .env("XDG_RUNTIME_DIR", run)
        .env("BIG_SEARCH_ORIGIN_WATCH", "0");
    command
}

fn send_daemon_request(sock: &Path, json: &str) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect daemon");
    stream.write_all(json.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    line
}

fn wait_socket(sock: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if sock.exists() {
            return;
        }
        sleep(Duration::from_millis(50));
    }
    panic!("daemon never bound socket");
}

fn run_stdout(args: &[&str], home: &Path, xdg_storage_home: &Path, run: &Path) -> String {
    let output = env_cmd(args, home, xdg_storage_home, run)
        .output()
        .expect("spawn cli");
    assert!(
        output.status.success(),
        "args {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn register_created_then_query_origin() {
    let base = std::env::temp_dir().join(format!("bs-origin-ipc-{}", std::process::id()));
    std::fs::remove_dir_all(&base).ok();
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let note = home.join("docs/note.txt");
    std::fs::write(&note, "remember this").unwrap();
    let note_path = note.to_string_lossy();

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
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    wait_socket(&sock);

    let register = format!(
        r#"{{"v":2,"cmd":"register_created","path":"{}","app_id":"big-text-editor"}}"#,
        note_path
    );
    assert!(send_daemon_request(&sock, &register).contains(r#""kind":"ok""#));

    let origin = format!(r#"{{"v":2,"cmd":"origin","path":"{}"}}"#, note_path);
    let origin_reply = send_daemon_request(&sock, &origin);
    assert!(
        origin_reply.contains(r#""kind":"origin""#),
        "{origin_reply}"
    );
    assert!(
        origin_reply.contains(r#""kind":"app_created""#),
        "{origin_reply}"
    );
    assert!(origin_reply.contains("Editor de Textos"), "{origin_reply}");

    let query = r#"{"v":2,"cmd":"query","q":"note","mode":"name","include_origin":true,"page":{"limit":5}}"#;
    let query_reply = send_daemon_request(&sock, query);
    assert!(query_reply.contains(r#""kind":"query""#), "{query_reply}");
    assert!(query_reply.contains(r#""origin""#), "{query_reply}");
    assert!(query_reply.contains(r#""app_created""#), "{query_reply}");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn cli_exposes_origin_commands() {
    let base = std::env::temp_dir().join(format!("bs-origin-cli-{}", std::process::id()));
    std::fs::remove_dir_all(&base).ok();
    let home = base.join("home");
    let xdg_storage_home = base.join("data");
    let run = base.join("run");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let note = home.join("docs/note.txt");
    std::fs::write(&note, "remember this").unwrap();
    let note_path = note.to_str().unwrap();

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
    let _daemon = KillOnDrop(
        env_cmd(&["daemon"], &home, &xdg_storage_home, &run)
            .spawn()
            .unwrap(),
    );
    wait_socket(&sock);

    let created = run_stdout(
        &["register-created", note_path, "big-text-editor"],
        &home,
        &xdg_storage_home,
        &run,
    );
    assert_eq!(created.trim(), "ok");

    let origin = run_stdout(&["origin", note_path], &home, &xdg_storage_home, &run);
    assert!(origin.contains("kind: app_created"), "{origin}");
    assert!(
        origin.contains("label: Criado por Editor de Textos"),
        "{origin}"
    );
    assert!(origin.contains("source_app: big-text-editor"), "{origin}");

    let query = run_stdout(&["--origin", "note"], &home, &xdg_storage_home, &run);
    assert!(query.contains("note.txt"), "{query}");
    assert!(
        query.contains("origin: Criado por Editor de Textos"),
        "{query}"
    );

    let capabilities = run_stdout(&["capabilities"], &home, &xdg_storage_home, &run);
    assert!(capabilities.contains("file_origin"), "{capabilities}");
    assert!(
        capabilities.contains("file_origin_register"),
        "{capabilities}"
    );

    std::fs::remove_dir_all(&base).ok();
}
