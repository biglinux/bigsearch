//! Minimal big-indexd client, the shape a GUI (big-shell, big-filemanager) uses:
//! ask the daemon to filter + page server-side, render the page, hold no state.
//!
//!     cargo run -p big-indexd-client --example probe -- relatorio
use big_indexd_client::{Client, Filter, Page};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "the".to_string());
    let Some(client) = Client::connect_default() else {
        eprintln!("no XDG_RUNTIME_DIR: this session has no private socket to talk to");
        std::process::exit(1);
    };

    match client.status() {
        Ok(s) => println!(
            "index: {} entries, paused={}, activity={}",
            s.indexed, s.paused, s.activity
        ),
        Err(e) => {
            eprintln!("daemon unavailable: {e}");
            return;
        }
    }

    // First page only — virtual scroll would request the rest via `next_offset`.
    let page = match client.query(
        &query,
        "both",
        Filter::default(),
        Page {
            offset: 0,
            limit: 10,
        },
    ) {
        Ok(page) => page,
        Err(e) => {
            eprintln!("query failed: {e}");
            return;
        }
    };

    println!(
        "{} matches (showing {}{})",
        page.total,
        page.hits.len(),
        page.next_offset
            .map(|o| format!(", next offset {o}"))
            .unwrap_or_default()
    );
    for h in &page.hits {
        println!("  {:>10} B  {}", h.size, h.path);
    }
}
