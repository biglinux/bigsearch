// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The big-search daemon's own answers, captured by
//! `scripts/capture-search-wire.py`: every request this client can send goes
//! through its types unchanged, and every answer decodes.

use big_indexd_client::{Reply, Request, RequestBody};
use serde_json::Value;

#[test]
fn daemon_exchanges_round_trip() {
    let exchanges = include_str!("wire.jsonl");
    let mut commands = Vec::new();
    for line in exchanges.lines() {
        let exchange: Value = serde_json::from_str(line).expect("fixture line");
        let sent = &exchange["request"];
        let command = sent["cmd"].as_str().expect("cmd").to_owned();
        match serde_json::from_value::<Request>(sent.clone()) {
            Ok(request) => {
                let resent = serde_json::to_value(&request).expect("request serializes");
                assert_eq!(&resent, sent, "{command} changes on the way through");
            }
            // The one request the daemon is shown to refuse.
            Err(_) => assert_eq!(command, "no_such_command"),
        }
        let answer = &exchange["reply"];
        let reply: Reply = serde_json::from_value(answer.clone())
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        let kind = serde_json::to_value(&reply).expect("reply serializes")["kind"].clone();
        assert_eq!(kind, answer["kind"], "{command}");
        commands.push(command);
    }
    // Every command in `RequestBody`, as serde lists them when refusing one.
    let refused =
        serde_json::from_value::<RequestBody>(serde_json::json!({"cmd": "no_such_command"}))
            .expect_err("an unknown command")
            .to_string();
    let (_, expected) = refused
        .split_once("expected one of ")
        .expect("serde lists the commands");
    let mut known: Vec<&str> = expected
        .split(", ")
        .map(|name| name.trim_matches('`'))
        .collect();
    known.push("no_such_command");
    known.sort_unstable();
    commands.sort_unstable();
    commands.dedup();
    assert_eq!(commands, known);
}
