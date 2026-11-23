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
        // Refused on purpose: an unknown command, and a version copy on the
        // capture's file system, which cannot clone a file.
        let refused_on_purpose = ["no_such_command", "save_version"];
        assert_eq!(
            answer["kind"] == "error",
            refused_on_purpose.contains(&command.as_str()),
            "{command}: {answer}"
        );
        let reply: Reply = serde_json::from_value(answer.clone())
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        let kind = serde_json::to_value(&reply).expect("reply serializes")["kind"].clone();
        assert_eq!(kind, answer["kind"], "{command}");
        commands.push(command);
    }
    // Every command in `RequestBody`: serde names the unknown one and each it
    // knows between backticks when refusing one.
    let refused =
        serde_json::from_value::<RequestBody>(serde_json::json!({"cmd": "no_such_command"}))
            .expect_err("an unknown command")
            .to_string();
    let mut known: Vec<&str> = refused.split('`').skip(1).step_by(2).collect();
    assert!(known.len() > 2, "serde named no commands: {refused}");
    known.sort_unstable();
    known.dedup();
    commands.sort_unstable();
    commands.dedup();
    assert_eq!(commands, known);
}
