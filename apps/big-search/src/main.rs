//! big-search: lean unprivileged search: filename (P1) + incremental (P2) + content (P3).
use big_search::{
    config, content, extract, index, ipc, origin, query, scan, settings, state, throttle, watch,
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
    let usage = cyan("Uso");
    let commands = cyan("Comandos principais");
    let search = cyan("Busca");
    let config = cyan("Configuração");
    let examples = cyan("Exemplos");
    let color = cyan("Cores");
    format!(
        "{title} — busca local rápida por nomes e conteúdo\n\n\
{usage}:\n  big-search [--color auto|always|never] <consulta>\n  big-search content <consulta>        busca no conteúdo indexado\n  big-search both <consulta>           une nome + conteúdo, sem duplicar\n  big-search config show               mostra perfil efetivo\n  big-search config preset low-memory  aplica perfil para máquina fraca\n  big-search reindex [RAIZ...]         reconstrói o índice\n\n\
{commands}:\n  count        conta entradas catalogadas; aceita --under DIR\n  list         lista caminhos; aceita --under DIR e -n N\n  daemon       servidor local + watcher incremental\n  status       estado do daemon\n  pause/resume pausa ou retoma indexação de fundo\n  rebuild      pede rebuild ao daemon\n  origin       mostra proveniência registrada de um caminho\n  versions     lista as versões anteriores guardadas de um arquivo\n  forget-versions esquece as versões guardadas de um arquivo\n\n\
{search}:\n  -n, --limit N      limita resultados\n  -a, --all          até 100000 resultados\n  --under DIR        restringe a uma árvore\n  -e, --ext EXT      restringe extensão; pode repetir\n  -p, --pages        tenta mostrar páginas/slides dos hits visíveis\n  --origin           inclui resumo de origem/proveniência\n  --                 encerra flags; o resto vira consulta literal\n\n\
{config}:\n  config show\n  config path\n  config set names-only true|false\n  config set content-index-mode auto|basic|freqs\n  config set extract-max-mb 0|4|8|16|32\n  config preset names-only|low-memory|balanced|complete\n\n\
{color}:\n  --color auto|always|never, --no-color. Também respeita NO_COLOR, CLICOLOR_FORCE e CLICOLOR.\n\n\
{examples}:\n  big-search contrato --ext pdf --under ~/Documentos\n  big-search both goncalves relatorio -n 20 --origin\n  big-search config preset names-only && big-search reindex\n\n\
{}",
        dim(
            "Observação: arquivos ocultos e entradas ignoradas por .gitignore continuam fora do índice por desenho."
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
        Some("rebuild") => cmd_lifecycle("rebuild"),
        Some("content") => cmd_search(&args[1..], "content"),
        Some("both") => cmd_search(&args[1..], "both"),
        _ => cmd_search(&args, "name"),
    }
}

/// Parse flags out of the tokens: `-n N`/`--limit N`, `-a`/`--all`,
/// `-p`/`--pages` (show PDF/PPTX page numbers), `--ext EXT` (repeatable; restrict
/// to those extensions, server-side). The rest is the query.
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
        println!("{path}");
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
                "{} {} documentos · {} {} (nomes {} + conteúdo {})",
                bold("conteúdo:"),
                s.content_docs,
                dim("índices:"),
                format_bytes(total_index_bytes),
                format_bytes(s.name_index_bytes),
                format_bytes(s.content_index_bytes)
            );
            if s.pending_content > 0 {
                println!(
                    "{}",
                    yellow(&format!(
                        "conteúdo pendente: {} arquivo(s) editados recentemente",
                        s.pending_content
                    ))
                );
            }
            if s.unwatched_dirs > 0 {
                println!(
                    "{}",
                    yellow(&format!(
                        "{} pasta(s) sem acompanhamento de mudanças (provavelmente o limite fs.inotify.max_user_watches)",
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
        println!(
            "{}",
            dim("pouco espaço livre: novas versões estão pausadas")
        );
    }
    if answer.versions.is_empty() {
        println!("{path}: {}", dim("nenhuma versão guardada"));
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
    println!("{} {path}", green("esquecido"));
    Ok(())
}

/// Why this machine keeps no versions, in the words the interface uses.
fn explain_unavailable(reason: big_indexd_client::Unavailable) -> &'static str {
    use big_indexd_client::Unavailable;

    // Exhaustive on purpose: a reason added to the store has to be given a
    // sentence here too, not swallowed by a catch-all that says nothing.
    match reason {
        Unavailable::Disabled => "o histórico de versões está desligado",
        Unavailable::NoReflink => "este disco não guarda versões anteriores",
        Unavailable::OldKernel => "o núcleo deste sistema é antigo demais para guardar versões",
        Unavailable::LiveSession => "na sessão de teste nada é guardado",
        Unavailable::Unknown => "este serviço não guarda versões aqui",
    }
}

/// How long ago, in words.
///
/// Relative rather than a calendar date: this binary has no timezone database
/// and no locale, and "há 2 horas" is what somebody helping over the telephone
/// needs anyway. The window shows the real date.
fn how_long_ago(saved_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let seconds = (now - saved_at).max(0);
    match seconds {
        0..=90 => "agora há pouco".to_string(),
        91..=5399 => format!("há {} min", seconds / 60),
        5400..=86_399 => format!("há {} h", seconds / 3600),
        _ => format!("há {} dias", seconds / 86_400),
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
    if !(2..=4).contains(&tokens.len()) {
        print_usage();
        anyhow::bail!("usage: register-derived SRC DST [APP_ID] [ACTION]");
    }
    let app_id = tokens.get(2).map(String::as_str);
    let action = tokens.get(3).map(String::as_str);
    register_with_daemon(|client| client.register_derived(&tokens[0], &tokens[1], app_id, action))
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
    // other programs' databases: control characters would reach the terminal
    // as escape sequences.
    let shown = |text: &str| -> String {
        text.chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect()
    };
    let Some(origin) = origin else {
        println!("{}: no origin", shown(path));
        return;
    };
    println!("{}", shown(path));
    println!("kind: {}", shown(&origin.kind));
    println!("confidence: {}", origin.confidence);
    println!("label: {}", shown(&origin.label));
    println!("first_seen_at: {}", origin.first_seen_at);
    for (field, value) in [
        ("source_app", &origin.source_app),
        ("source_path", &origin.source_path),
        ("source_device_id", &origin.source_device_id),
        ("source_device_label", &origin.source_device_label),
        ("source_domain", &origin.source_domain),
    ] {
        if let Some(value) = value {
            println!("{field}: {}", shown(value));
        }
    }
}

/// Send a lifecycle command (`pause`/`resume`/`rebuild`) to the running daemon.
/// These need a live daemon: without one there is nothing to control. The daemon
/// accepts them asynchronously (it replies at once; the work runs in the watcher).
fn cmd_lifecycle(action: &str) -> Result<()> {
    let client = daemon_client()?;
    let result = match action {
        "pause" => client.pause(),
        "resume" => client.resume(),
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
    // Server-side filter + single page; the daemon resolves everything (the CLI is
    // just another thin client of the shared crate). `--under` is a server filter.
    let filter = query::Filter {
        under: s.under.clone(),
        ext: s.ext.clone(),
        ..Default::default()
    };
    let page = query::Page {
        offset: 0,
        limit: s.limit,
    };

    // Prefer the warm daemon (via the shared client); fall back to opening the index
    // read-only when no daemon is listening.
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
                "⏳ conteúdo de {} arquivo(s) editados há pouco ainda está sendo indexado — repita a busca em instantes",
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
    query::run_query(
        query::QueryIndexes {
            name: &name_reader,
            content: content_reader.as_ref(),
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
                println!("{}  (p. {})", h.path, list.join(", "));
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
    if notes.is_empty() {
        hit.path.clone()
    } else {
        format!("{}  {}", hit.path, dim(&format!("[{}]", notes.join("; "))))
    }
}
