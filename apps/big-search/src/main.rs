//! big-search: lean unprivileged search of file names and contents, kept current by watching the disk.
//!
//! `config_cmd` serves `config`; `index_cmd` serves `reindex` and `daemon`.
use big_search::{
    catalogue, config, content, extract, index, ipc, mounts, origin, query, scan, settings,
    terminal_text, throttle, watch,
};

use anyhow::{Context, Result};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

#[derive(Clone, Copy)]
enum ColorChoice {
    Auto = 0,
    Always = 1,
    Never = 2,
}

static COLOR_CHOICE: AtomicU8 = AtomicU8::new(ColorChoice::Auto as u8);

fn set_color_choice(choice: ColorChoice) {
    COLOR_CHOICE.store(choice as u8, Ordering::Relaxed);
}

fn split_global_flags(args: Vec<String>) -> Result<Vec<String>> {
    let mut filtered = Vec::with_capacity(args.len());
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-color" => set_color_choice(ColorChoice::Never),
            "--color" => {
                let choice = it
                    .peek()
                    .and_then(|value| parse_color_choice(value))
                    .unwrap_or(ColorChoice::Always);
                if it
                    .peek()
                    .and_then(|value| parse_color_choice(value))
                    .is_some()
                {
                    let _ = it.next();
                }
                set_color_choice(choice);
            }
            _ if arg.starts_with("--color=") => {
                let value = arg.trim_start_matches("--color=");
                let Some(choice) = parse_color_choice(value) else {
                    anyhow::bail!("invalid --color value: {value} (use auto, always, or never)");
                };
                set_color_choice(choice);
            }
            _ => filtered.push(arg),
        }
    }
    Ok(filtered)
}

fn parse_color_choice(value: &str) -> Option<ColorChoice> {
    match value {
        "auto" => Some(ColorChoice::Auto),
        "always" | "on" | "yes" => Some(ColorChoice::Always),
        "never" | "off" | "no" => Some(ColorChoice::Never),
        _ => None,
    }
}

fn colors_enabled() -> bool {
    match COLOR_CHOICE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => colors_auto_enabled(),
    }
}

fn colors_auto_enabled() -> bool {
    if env_var_nonempty("NO_COLOR") {
        return false;
    }
    if env_var_nonzero("CLICOLOR_FORCE") {
        return true;
    }
    if std::env::var("CLICOLOR").ok().as_deref() == Some("0") {
        return false;
    }
    stdout_is_tty()
}

fn env_var_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn env_var_nonzero(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

fn stdout_is_tty() -> bool {
    // SAFETY: isatty(3) only inspects a descriptor number; an invalid one
    // returns 0.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

fn paint(text: &str, code: &str) -> String {
    if colors_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn bold(text: &str) -> String {
    paint(text, "1")
}

fn dim(text: &str) -> String {
    paint(text, "2")
}

fn cyan(text: &str) -> String {
    paint(text, "36")
}

fn green(text: &str) -> String {
    paint(text, "32")
}

fn yellow(text: &str) -> String {
    paint(text, "33")
}

fn print_usage() {
    println!("{}", colored_usage());
}

fn colored_usage() -> String {
    let title = bold("big-search");
    let usage = cyan("Usage");
    let commands = cyan("Commands");
    let origin = cyan("Recording origin (for other programs, with the service running)");
    let search = cyan("Search");
    let config = cyan("Configuration");
    let examples = cyan("Examples");
    let color = cyan("Color");
    format!(
        "{title}: fast local search of file names and contents\n\n\
{usage}:\n  big-search [--color auto|always|never] <query>\n  big-search content <query>           search inside the indexed files\n  big-search both <query>              names and contents, without duplicates\n  big-search config show               the effective profile\n  big-search config preset low-memory  the profile for a weak machine\n  big-search reindex [ROOT...]         rebuild the index\n\n\
{commands}:\n  count           count catalogued entries; takes --under DIR\n  list            list paths; takes --under DIR and -n N\n  daemon          the service: local server and change watcher\n  status          what the service is doing\n  pause/resume    pause or resume background indexing\n  rescan          look for changes made while the service was stopped\n  rebuild         ask the service to rebuild\n  origin          where a file came from\n  versions        the earlier versions kept of a file\n  forget-versions forget the versions kept of a file\n  capabilities    what the running service offers\n\n\
{origin}:\n  register-created PATH [APP_ID]           created by a program\n  register-copy SOURCE TARGET [APP_ID]     a copy\n  register-move SOURCE TARGET [APP_ID]     moved or renamed\n  register-derived SOURCE TARGET [APP_ID]  made from another file\n\n\
{search}:\n  -n, --limit N      at most N results\n  -a, --all          up to 100000 results\n  --under DIR        only inside DIR\n  -e, --ext EXT      only this extension; may repeat\n  -p, --pages        show matching pages or slides of the visible hits\n  --origin           show where each file came from\n  --                 end of options; the rest is the query, as written\n  -V, --version      print the version\n\n\
{config}:\n  config show\n  config path\n  config set KEY VALUE   `big-search config --help` lists the keys\n  config preset names-only|low-memory|balanced|complete\n\n\
{color}:\n  --color auto|always|never, --no-color. NO_COLOR, CLICOLOR_FORCE and CLICOLOR are honored.\n\n\
{examples}:\n  big-search contract --ext pdf --under ~/Documents\n  big-search both goncalves report -n 20 --origin\n  big-search config preset names-only && big-search reindex\n\n\
{}",
        dim(
            "Hidden files and anything a .gitignore excludes are not indexed. Control and\nbidirectional characters in names print as escapes (\\u{1b})."
        ),
    )
}

fn main() -> Result<()> {
    // Restore default SIGPIPE so `big-search x | head` terminates quietly instead of
    // panicking on a broken stdout pipe (Rust sets SIGPIPE to ignore by default).
    // SAFETY: setting a signal disposition before any threads are spawned.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // One malloc arena. Our few threads allocate in coordinated bursts, and
    // glibc's default per-thread arenas fragment those bursts across several
    // pools and raise peak reindex RSS with no gain in query latency or wall
    // time. Single-arena glibc also beats jemalloc, mimalloc and tcmalloc on
    // peak RSS, the metric that matters on 2 GB machines, without a dependency.
    // SAFETY: mallopt before any threads are spawned.
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }

    // Our own logs at info; mute the noisy tantivy mmap/reader debug+info.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,tantivy=warn"),
    )
    .init();

    // Every command may write the data dir (reindex, the CLI's own index
    // reads), and the shell's umask would leave it readable by other accounts.
    // The daemon refuses to start on this error; elsewhere a warning is enough.
    if let Err(error) = config::ensure_private_data_dir() {
        log::warn!("data directory {}: {error}", config::data_dir().display());
    }

    // SQLite temp files (large-scan spill B-trees, statement journals) must land
    // somewhere writable. Under the hardened systemd unit (`ProtectSystem=strict`
    // + `ProtectHome=read-only`) every default candidate (/tmp, /var/tmp, cwd)
    // is read-only and SQLite fails with SQLITE_IOERR_GETTEMPPATH, aborting the
    // reconcile scan. Point it at our own (always writable) data dir.
    if std::env::var_os("SQLITE_TMPDIR").is_none() {
        // SAFETY: set_var before any threads are spawned (top of main; the
        // logger starts none).
        unsafe {
            std::env::set_var("SQLITE_TMPDIR", config::data_dir());
        }
    }

    let args = split_global_flags(std::env::args().skip(1).collect())?;
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print_usage();
            Ok(())
        }
        Some("reindex") => cmd_reindex(&args[1..]),
        Some("count") => cmd_count(&args[1..]),
        Some("config") => cmd_config(&args[1..]),
        Some("--version") | Some("-V") => {
            println!("big-search {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("list") => cmd_list(&args[1..]),
        Some("origin") => cmd_origin(&args[1..]),
        Some("versions") => cmd_versions(&args[1..]),
        Some("forget-versions") => cmd_forget_versions(&args[1..]),
        Some("register-created") => cmd_register_created(&args[1..]),
        Some("register-copy") => cmd_register_copy(&args[1..]),
        Some("register-move") => cmd_register_move(&args[1..]),
        Some("register-derived") => cmd_register_derived(&args[1..]),
        Some("daemon") => cmd_daemon(),
        Some("status") => cmd_status(),
        Some("capabilities") => cmd_capabilities(),
        Some("pause") => cmd_lifecycle("pause"),
        Some("resume") => cmd_lifecycle("resume"),
        Some("rescan") => cmd_lifecycle("rescan"),
        Some("rebuild") => cmd_lifecycle("rebuild"),
        Some("content") => cmd_search(&args[1..], "content"),
        Some("both") => cmd_search(&args[1..], "both"),
        _ => cmd_search(&args, "name"),
    }
}

/// Search flags parsed by [`parse_query`]; the remaining tokens form the query.
struct Search {
    limit: usize,
    pages: bool,
    include_origin: bool,
    under: Option<String>,
    ext: Vec<String>,
    query: String,
}

fn parse_query(tokens: &[String]) -> Search {
    let mut limit = 100_000usize; // effectively uncapped; -n N to cap
    let mut pages = false;
    let mut include_origin = false;
    let mut under = None;
    let mut ext: Vec<String> = Vec::new();
    let mut parts: Vec<&str> = Vec::new();
    let mut it = tokens.iter();
    while let Some(t) = it.next() {
        match t.as_str() {
            "-n" | "--limit" => {
                if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                    limit = n;
                }
            }
            "-a" | "--all" => limit = 100_000,
            "-p" | "--pages" => pages = true,
            "--origin" => include_origin = true,
            "--under" => under = it.next().cloned(),
            "--ext" | "-e" => {
                if let Some(e) = it.next() {
                    ext.push(e.trim_start_matches('.').to_ascii_lowercase());
                }
            }
            "--" => {
                // Everything after `--` is the literal query (safe for terms
                // that start with `-`); callers pass user input this way.
                parts.extend(it.by_ref().map(String::as_str));
                break;
            }
            other => parts.push(other),
        }
    }
    Search {
        limit,
        pages,
        include_origin,
        under,
        ext,
        query: parts.join(" "),
    }
}

/// `count`: print how many files are cataloged. `--under DIR` scopes the tally.
fn cmd_count(tokens: &[String]) -> Result<()> {
    let s = parse_query(tokens);
    let index = index::open_read_only(&config::index_dir())?;
    let n = match s.under.as_deref() {
        None => query::count(&index.reader()?)?,
        Some(dir) => query::list_paths(&index.reader()?, usize::MAX, Some(dir))?.len() as u64,
    };
    println!("{n}");
    Ok(())
}

#[path = "config_cmd.rs"]
mod config_cmd;
use config_cmd::*;

#[path = "index_cmd.rs"]
mod index_cmd;
use index_cmd::{cmd_daemon, cmd_reindex};

/// `list`: print every cataloged file path (sorted). `--under DIR` scopes,
/// `-n N` caps. Reads the index read-only (works with or without the daemon).
fn cmd_list(tokens: &[String]) -> Result<()> {
    let s = parse_query(tokens);
    let index = index::open_read_only(&config::index_dir())?;
    for path in query::list_paths(&index.reader()?, s.limit, s.under.as_deref())? {
        println!("{}", terminal_text(&path));
    }
    Ok(())
}

/// A client pointed at this session's daemon socket.
///
/// Refuses when the session has no runtime directory rather than falling back to
/// `/tmp`: that directory is world-writable, so another local account could put
/// a socket there first and answer in the daemon's place.
fn daemon_client() -> Result<big_indexd_client::Client> {
    let socket = config::socket_path()
        .context("this session has no XDG_RUNTIME_DIR, so there is no daemon socket")?;
    Ok(big_indexd_client::Client::new(socket))
}

/// `status`: ask the running daemon for its cataloged count and paused state.
fn cmd_status() -> Result<()> {
    let client = daemon_client()?;
    match client.status() {
        Ok(s) => {
            println!("{} {} · {}", bold("indexed"), s.indexed, s.activity);
            let total_index_bytes = s.name_index_bytes.saturating_add(s.content_index_bytes);
            println!(
                "{} {} documents · {} {} (names {} + content {})",
                bold("content:"),
                s.content_docs,
                dim("indexes:"),
                format_bytes(total_index_bytes),
                format_bytes(s.name_index_bytes),
                format_bytes(s.content_index_bytes)
            );
            if s.pending_content > 0 {
                println!(
                    "{}",
                    yellow(&format!(
                        "content pending: {} recently edited file(s)",
                        s.pending_content
                    ))
                );
            }
            if s.unwatched_dirs > 0 {
                println!(
                    "{}",
                    yellow(&format!(
                        "{} folder(s) not watched for changes (probably the fs.inotify.max_user_watches limit)",
                        s.unwatched_dirs
                    ))
                );
            }
            Ok(())
        }
        Err(big_indexd_client::ClientError::Io(_)) => Err(no_daemon()),
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    }
}

fn cmd_capabilities() -> Result<()> {
    let client = daemon_client()?;
    match client.status() {
        Ok(s) => {
            for capability in s.capabilities {
                println!("{capability}");
            }
            Ok(())
        }
        Err(big_indexd_client::ClientError::Io(_)) => Err(no_daemon()),
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    }
}

fn cmd_origin(tokens: &[String]) -> Result<()> {
    let Some(path) = single_arg(tokens, "origin PATH")? else {
        print_usage();
        return Ok(());
    };
    let client = daemon_client()?;
    let origin = match client.origin(path) {
        Ok(origin) => origin,
        Err(big_indexd_client::ClientError::Io(_)) => {
            origin::OriginReader::new(config::origin_db_path()).origin(Path::new(path))
        }
        Err(e) => return Err(anyhow::anyhow!(e.to_string())),
    };
    print_origin(path, origin.as_ref());
    Ok(())
}

/// `versions PATH`: what earlier states of this document are kept.
///
/// The way this whole subsystem is used, tested and supported before any window
/// exists, and the way somebody helping over the telephone can find out whether
/// a machine keeps versions at all.
fn cmd_versions(tokens: &[String]) -> Result<()> {
    let Some(path) = single_arg(tokens, "versions PATH")? else {
        print_usage();
        return Ok(());
    };
    let client = daemon_client()?;
    let answer = client
        .versions(path)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    if let Some(reason) = answer.unavailable {
        println!("{path}: {}", dim(explain_unavailable(reason)));
        return Ok(());
    }
    if answer.paused_for_space {
        println!("{}", dim("low on free space: new versions are paused"));
    }
    if answer.versions.is_empty() {
        println!("{path}: {}", dim("no versions kept"));
        return Ok(());
    }
    println!("{path}");
    for version in answer.versions {
        println!(
            "  {:>16}  {:>10}  {:<14}  {}",
            how_long_ago(version.saved_at),
            crate::config_cmd::format_bytes(version.size),
            version.reason,
            version.object
        );
    }
    Ok(())
}

fn cmd_forget_versions(tokens: &[String]) -> Result<()> {
    let Some(path) = single_arg(tokens, "forget-versions PATH")? else {
        print_usage();
        return Ok(());
    };
    let client = daemon_client()?;
    client
        .forget_versions(path)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    println!("{} {path}", green("forgotten"));
    Ok(())
}

/// Why this machine keeps no versions, in the words the interface uses.
fn explain_unavailable(reason: big_indexd_client::Unavailable) -> &'static str {
    use big_indexd_client::Unavailable;

    // Exhaustive on purpose: a reason added to the store has to be given a
    // sentence here too, not swallowed by a catch-all that says nothing.
    match reason {
        Unavailable::Disabled => "file history is off",
        Unavailable::NoReflink => "this disk keeps no earlier versions",
        Unavailable::OldKernel => "this system's kernel is too old to keep versions",
        Unavailable::LiveSession => "a live session keeps nothing",
        Unavailable::Unknown => "this service keeps no versions here",
    }
}

/// How long ago, in words.
///
/// Relative rather than a calendar date: this binary has no timezone database
/// and no locale, and "2 h ago" is what somebody helping over the telephone
/// needs anyway. The window shows the real date.
fn how_long_ago(saved_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let seconds = (now - saved_at).max(0);
    match seconds {
        0..=90 => "just now".to_string(),
        91..=5399 => format!("{} min ago", seconds / 60),
        5400..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

fn cmd_register_created(tokens: &[String]) -> Result<()> {
    let (path, app_id) = path_and_optional_app(tokens, "register-created PATH [APP_ID]")?;
    register_with_daemon(|client| client.register_created(path, app_id))
}

fn cmd_register_copy(tokens: &[String]) -> Result<()> {
    let (src, dst, app_id) = two_paths_and_optional_app(tokens, "register-copy SRC DST [APP_ID]")?;
    register_with_daemon(|client| client.register_copy(src, dst, app_id))
}

fn cmd_register_move(tokens: &[String]) -> Result<()> {
    let (src, dst, app_id) = two_paths_and_optional_app(tokens, "register-move SRC DST [APP_ID]")?;
    register_with_daemon(|client| client.register_move(src, dst, app_id))
}

fn cmd_register_derived(tokens: &[String]) -> Result<()> {
    let (src, dst, app_id) =
        two_paths_and_optional_app(tokens, "register-derived SRC DST [APP_ID]")?;
    register_with_daemon(|client| client.register_derived(src, dst, app_id))
}

fn register_with_daemon(
    f: impl FnOnce(&big_indexd_client::Client) -> Result<(), big_indexd_client::ClientError>,
) -> Result<()> {
    let client = daemon_client()?;
    match f(&client) {
        Ok(()) => {
            println!("ok");
            Ok(())
        }
        Err(big_indexd_client::ClientError::Io(_)) => Err(no_daemon()),
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    }
}

fn single_arg<'a>(tokens: &'a [String], usage: &str) -> Result<Option<&'a str>> {
    match tokens {
        [] => Ok(None),
        [path] => Ok(Some(path)),
        _ => {
            print_usage();
            anyhow::bail!("usage: {usage}")
        }
    }
}

fn path_and_optional_app<'a>(
    tokens: &'a [String],
    usage: &str,
) -> Result<(&'a str, Option<&'a str>)> {
    match tokens {
        [path] => Ok((path, None)),
        [path, app_id] => Ok((path, Some(app_id))),
        _ => {
            print_usage();
            anyhow::bail!("usage: {usage}")
        }
    }
}

fn two_paths_and_optional_app<'a>(
    tokens: &'a [String],
    usage: &str,
) -> Result<(&'a str, &'a str, Option<&'a str>)> {
    match tokens {
        [src, dst] => Ok((src, dst, None)),
        [src, dst, app_id] => Ok((src, dst, Some(app_id))),
        _ => {
            print_usage();
            anyhow::bail!("usage: {usage}")
        }
    }
}

fn print_origin(path: &str, origin: Option<&big_indexd_client::OriginProvenance>) {
    // Every field but the numbers comes from file names, extended attributes or
    // other programs' databases.
    let Some(origin) = origin else {
        println!("{}: no origin", terminal_text(path));
        return;
    };
    println!("{}", terminal_text(path));
    println!("kind: {}", terminal_text(&origin.kind));
    println!("confidence: {}", origin.confidence);
    println!("label: {}", terminal_text(&origin.label));
    println!("first_seen_at: {}", origin.first_seen_at);
    for (field, value) in [
        ("source_app", &origin.source_app),
        ("source_path", &origin.source_path),
        ("source_device_id", &origin.source_device_id),
        ("source_device_label", &origin.source_device_label),
        ("source_domain", &origin.source_domain),
    ] {
        if let Some(value) = value {
            println!("{field}: {}", terminal_text(value));
        }
    }
}

/// Send a lifecycle command (`pause`/`resume`/`rescan`/`rebuild`) to the running daemon.
/// These need a live daemon: without one there is nothing to control. The daemon
/// accepts them asynchronously (it replies at once; the work runs in the watcher).
fn cmd_lifecycle(action: &str) -> Result<()> {
    let client = daemon_client()?;
    let result = match action {
        "pause" => client.pause(),
        "resume" => client.resume(),
        "rescan" => client.rescan(),
        "rebuild" => client.rebuild(),
        _ => unreachable!("unknown lifecycle action {action}"),
    };
    match result {
        Ok(()) => {
            println!("{action}: ok");
            Ok(())
        }
        Err(big_indexd_client::ClientError::Io(_)) => Err(no_daemon()),
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    }
}

fn no_daemon() -> anyhow::Error {
    anyhow::anyhow!("no big-search daemon running (start it: systemctl --user start big-search)")
}

/// Cap on how many results we re-extract for `-p` page numbers (pdftotext per file).
const PAGES_SCAN_CAP: usize = 200;

fn cmd_search(tokens: &[String], mode: &str) -> Result<()> {
    let s = parse_query(tokens);
    if s.query.is_empty() {
        print_usage();
        return Ok(());
    }
    // Filters and paging are resolved server-side.
    let filter = query::Filter {
        under: s.under.clone(),
        ext: s.ext.clone(),
        ..Default::default()
    };
    let page = query::Page {
        offset: 0,
        limit: s.limit,
    };

    // Prefer the warm daemon; open the index read-only when none is listening.
    let client = daemon_client()?;
    let daemon_query = if s.include_origin {
        client.query_with_origin(&s.query, mode, filter.clone(), page.clone())
    } else {
        client.query(&s.query, mode, filter.clone(), page.clone())
    };
    let outcome = match daemon_query {
        Ok(outcome) => outcome,
        Err(big_indexd_client::ClientError::Io(_)) => {
            query_without_daemon(&s, mode, &filter, &page)?
        }
        Err(e) => return Err(anyhow::anyhow!(e.to_string())),
    };

    print_hits(&s, &outcome);
    // "Loading" hint: recently-edited files are still being extracted (this very
    // search already told the daemon to prioritise them, bypassing the cooldown).
    if outcome.content_pending > 0 {
        eprintln!(
            "{}",
            dim(&format!(
                "the content of {} recently edited file(s) is still being indexed; search again in a moment",
                outcome.content_pending
            ))
        );
    }
    Ok(())
}

/// The same query against the index files, read-only, when no daemon listens.
fn query_without_daemon(
    s: &Search,
    mode: &str,
    filter: &query::Filter,
    page: &query::Page,
) -> Result<big_indexd_client::QueryOutcome> {
    let index = index::open_for_queries(&config::index_dir())?;
    let content_index = matches!(mode, "content" | "both")
        .then(|| index::open_for_queries(&config::content_index_dir()))
        .transpose()?;
    let origin_reader = s
        .include_origin
        .then(|| origin::OriginReader::new(config::origin_db_path()));
    let name_reader = index::query_reader(&index)?;
    let content_reader = content_index
        .as_ref()
        .map(index::query_reader)
        .transpose()?;
    let catalogue = catalogue::CatalogueReader::new(&config::state_db_path());
    query::run_query(
        query::QueryIndexes {
            name: &name_reader,
            content: content_reader.as_ref(),
            catalogue: &catalogue,
        },
        &s.query,
        mode,
        filter,
        page,
        s.include_origin,
        origin_reader.as_ref(),
    )
}

fn print_hits(s: &Search, outcome: &big_indexd_client::QueryOutcome) {
    let terms: Vec<String> = s.query.split_whitespace().map(String::from).collect();
    for (i, h) in outcome.hits.iter().enumerate() {
        // `-p`: annotate paginated formats with the page/slide numbers that match.
        if s.pages && i < PAGES_SCAN_CAP {
            let pp = extract::pages_with(std::path::Path::new(&h.path), &terms);
            if !pp.is_empty() {
                let list: Vec<String> = pp.iter().map(usize::to_string).collect();
                println!("{}  (p. {})", terminal_text(&h.path), list.join(", "));
                continue;
            }
        }
        // A hit on an offline removable/network source: still listed, flagged with
        // the device it lives on.
        if !h.available {
            let location = if h.source.is_empty() {
                "offline".to_string()
            } else {
                format!("offline · on {}", h.source)
            };
            println!("{}", format_hit_path(h, Some(&location), s.include_origin));
        } else {
            println!("{}", format_hit_path(h, None, s.include_origin));
        }
    }
}

fn format_hit_path(
    hit: &big_indexd_client::Hit,
    location: Option<&str>,
    include_origin: bool,
) -> String {
    let mut notes = Vec::new();
    if let Some(location) = location {
        notes.push(location.to_string());
    }
    if include_origin && let Some(origin) = &hit.origin {
        notes.push(format!("origin: {}", origin.label));
    }
    let path = terminal_text(&hit.path);
    if notes.is_empty() {
        path.into_owned()
    } else {
        let notes = terminal_text(&notes.join("; ")).into_owned();
        format!("{path}  {}", dim(&format!("[{notes}]")))
    }
}
