//! big-search — lean unprivileged search: filename (P1) + incremental (P2) + content (P3).
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
    // glibc's default per-thread arenas fragment those bursts across ~8 pools:
    // measured 104–125 MB peak reindex RSS vs 63 MB with a single arena, with
    // no query-latency cost (5–6 ms under churn either way) and equal wall
    // time. Also benchmarked against jemalloc (70–74 MB), mimalloc (76–78 MB)
    // and tcmalloc (85 MB) — single-arena glibc wins on the metric that matters
    // for the 2 GB target machines, with zero extra dependencies.
    // SAFETY: mallopt before any threads are spawned.
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }

    // SQLite temp files (large-scan spill B-trees, statement journals) must land
    // somewhere writable. Under the hardened systemd unit (`ProtectSystem=strict`
    // + `ProtectHome=read-only`) every default candidate — /tmp, /var/tmp, cwd —
    // is read-only and SQLite fails with SQLITE_IOERR_GETTEMPPATH, aborting the
    // reconcile scan. Point it at our own (always writable) data dir.
    if std::env::var_os("SQLITE_TMPDIR").is_none() {
        let data_dir = config::data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        // SAFETY: set_var before any threads are spawned (top of main).
        unsafe {
            std::env::set_var("SQLITE_TMPDIR", &data_dir);
        }
    }

    // Our own logs at info; mute the noisy tantivy mmap/reader debug+info.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,tantivy=warn"),
    )
    .init();

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

fn cmd_reindex(roots: &[String]) -> Result<()> {
    throttle::lower_priority();
    let mut index = index::open_or_create(&config::index_dir())?;
    let mut content_index = index::open_content_or_create(&config::content_index_dir())?;
    scan::write_effective_ignore()?;

    let (scan_roots, mut state, rebuild_content_from_state) = if roots.is_empty() {
        recover_inflated_index(&mut index)?;
        // Config-driven: reconcile the available sources, keeping offline
        // removable/network sources catalogued (a reindex while a device is
        // unplugged must not drop it). A fresh/recreated index starts empty.
        let sources = settings::sources();
        let available: Vec<PathBuf> = sources
            .all()
            .iter()
            .filter(|s| settings::is_available(s))
            .map(|s| s.path.clone())
            .collect();
        log::info!(
            "scan sources: {:?} (of {} configured)",
            available,
            sources.all().len()
        );
        let index_empty = index.reader()?.searcher().num_docs() == 0;
        let state = if index_empty {
            content_index = index::recreate_content_on_disk(&config::content_index_dir())?;
            content::clear_content_state()?;
            state::State::empty(config::state_path())
        } else {
            ensure_content_state_matches_index(&content_index)?;
            state::State::load(config::state_path())
        };
        (
            available,
            state,
            !index::is_content_state_synced(&config::content_index_dir()),
        )
    } else {
        // Explicit roots: a clean full rebuild of exactly those paths.
        let explicit: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        log::info!("scan roots (explicit): {explicit:?}");
        let mut writer = index::bulk_writer(&index)?;
        writer.delete_all_documents()?;
        writer.commit()?;
        content_index = index::recreate_content_on_disk(&config::content_index_dir())?;
        content::clear_content_state()?;
        (explicit, state::State::empty(config::state_path()), false)
    };

    let t0 = Instant::now();
    let sync_outcome = scan::sync(&index, &scan_roots, &mut state)?;
    let names = index.reader()?.searcher().num_docs();
    println!(
        "indexed {names} names in {:.2}s",
        t0.elapsed().as_secs_f64()
    );
    log::info!(
        "sync touched {} files that may need content/metadata",
        sync_outcome.content_candidates
    );
    let c = if settings::content_index_enabled() {
        if rebuild_content_from_state {
            content::clear_content_state()?;
        }
        let indexed = content::index_pending_catalog(&content_index)?;
        index::mark_content_state_synced(&config::content_index_dir())?;
        indexed
    } else {
        clear_content_sidecar(&content_index)?;
        0
    };
    state.persist()?;
    if settings::content_index_enabled() {
        println!(
            "{} content for {c} files ({:.2}s total)",
            green("extracted"),
            t0.elapsed().as_secs_f64()
        );
    } else {
        println!(
            "{} ({:.2}s total)",
            yellow("content disabled; indexed filenames only"),
            t0.elapsed().as_secs_f64()
        );
    }
    // Machine-readable line for the metadata on/off benchmark. RUSAGE_SELF
    // aggregates the worker threads; VmHWM is the whole-process peak.
    if std::env::var_os("BIG_SEARCH_BENCH").is_some() {
        println!(
            "BENCH names={names} content={c} wall_ms={:.1} cpu_ms={:.1} peak_rss_kb={}",
            t0.elapsed().as_secs_f64() * 1000.0,
            self_cpu_ms(),
            peak_rss_kb()
        );
    }
    Ok(())
}

fn clear_content_sidecar(content_index: &tantivy::Index) -> Result<()> {
    content::clear_content_state()?;
    let mut writer = index::content_writer(content_index).context("content writer")?;
    writer.delete_all_documents()?;
    writer.commit().context("clear content index")?;
    index::mark_content_state_synced(&config::content_index_dir())
}

fn ensure_content_state_matches_index(content_index: &tantivy::Index) -> Result<()> {
    if index::is_content_state_synced(&config::content_index_dir()) {
        return Ok(());
    }
    let cached_content_paths = state::State::load(config::content_state_path()).len();
    if cached_content_paths == 0 {
        return Ok(());
    }
    // Only an empty index proves the rows lie. Fewer documents than rows is the
    // normal state: policy-skipped files and files with no extractable body
    // keep a row without a document. Treating that as divergence wiped every
    // extraction done so far at each restart before the first pass completed.
    let indexed_content_documents = index::document_count(&content_index.reader()?)?;
    if indexed_content_documents == 0 {
        log::warn!(
            "content index/state divergence detected: {indexed_content_documents} content docs for {cached_content_paths} cached paths; rebuilding content cache"
        );
        content::clear_content_state()?;
    }
    Ok(())
}

/// Peak resident set of this process (KB), from `/proc/self/status` `VmHWM`.
fn peak_rss_kb() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|kb| kb.parse().ok())
            })
        })
        .unwrap_or(0)
}

/// Total CPU time of this process (all threads), milliseconds, via `getrusage`.
fn self_cpu_ms() -> f64 {
    // SAFETY: getrusage on a zeroed rusage with RUSAGE_SELF is always valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }
    let ms = |tv: libc::timeval| tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
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

/// Take an exclusive, non-blocking advisory lock so exactly one daemon runs per
/// user. Returns the held file on success (keep it alive for the daemon's life;
/// the kernel releases the lock when the fd closes, including on crash), `None` if
/// another daemon already holds it, `Err` only on an actual I/O failure.
fn acquire_daemon_lock(path: &Path) -> Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // lock target only; never written, so don't clobber it
        .open(path)
        .with_context(|| format!("open daemon lock {}", path.display()))?;
    // SAFETY: flock on a freshly-opened valid fd; lock lifetime is tied to `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None), // another daemon holds the lock
        _ => Err(anyhow::Error::new(err).context("flock daemon lock")),
    }
}

fn cmd_daemon() -> Result<()> {
    // Before anything is written: the data directory must not be readable by
    // other local accounts (it maps the user's whole home).
    config::ensure_private_data_dir().context("secure data directory")?;
    // Single-instance guard: a second daemon must refuse, not steal the socket and
    // deadlock the watcher on the Tantivy writer lock. Held for the whole run.
    // No runtime directory means no private place for the socket and the lock.
    // Serving from `/tmp` instead would put the index behind a socket any local
    // account can replace, so the service refuses to start at all.
    let lock_path = config::lock_path()
        .context("this session has no XDG_RUNTIME_DIR; refusing to serve from /tmp")?;
    let socket_path = config::socket_path().expect("runtime directory checked above");
    let _daemon_lock = match acquire_daemon_lock(&lock_path)? {
        Some(lock) => lock,
        None => {
            log::info!("another big-search daemon is already running — exiting");
            return Ok(());
        }
    };
    let mut index = index::open_or_create(&config::index_dir())?;
    let content_index = index::open_content_or_create(&config::content_index_dir())?;
    if !settings::content_index_enabled() {
        clear_content_sidecar(&content_index)?;
    }
    recover_inflated_index(&mut index)?;

    // `paused` and `activity` mirror indexing state for `status`; the channel
    // carries both fs events and lifecycle commands so the watcher reacts without
    // a polling timer. `pending_content` counts cooldown-deferred extractions.
    let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let activity = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(watch::ACTIVITY_INDEXING));
    let pending_content = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (commands, rx) = std::sync::mpsc::channel::<watch::WatchMessage>();
    // One versions store for the whole daemon: the thread that writes versions
    // and the connections that answer questions about them are looking at the
    // same thing, so what one learns the other can say.
    let history: big_search::history::SharedHistory = std::sync::Arc::new(std::sync::Mutex::new(
        big_search::history::History::open(&big_search_config::read().history),
    ));
    let watch_history = std::sync::Arc::clone(&history);
    let watch_commands = commands.clone();
    let watch_paused = paused.clone();
    let watch_activity = activity.clone();
    let watch_pending_content = pending_content.clone();

    // All indexing runs in the watcher thread at background priority; the main
    // thread stays at normal priority so queries are never slowed.
    let watch_index = index.clone();
    let watch_content_index = content_index.clone();
    std::thread::Builder::new()
        .name("watcher".into())
        .spawn(move || {
            let _watcher_exit_guard = WatcherExitGuard;
            throttle::lower_priority();
            let sources = settings::sources();
            let roots: Vec<PathBuf> = sources
                .all()
                .iter()
                .filter(|s| settings::is_available(s))
                .map(|s| s.path.clone())
                .collect();
            log::debug!("scan sources: {roots:?}");
            if let Err(e) = scan::write_effective_ignore() {
                log::warn!("ignore file: {e:#}");
            }
            // Reconcile against the cache: builds a fresh index, or applies only the
            // deltas accumulated while the daemon was down (crash/offline recovery).
            let empty = watch_index
                .reader()
                .map(|r| r.searcher().num_docs() == 0)
                .unwrap_or(true);
            let mut state = if empty {
                if let Err(e) = content::clear_content_state() {
                    log::warn!("clear content state for empty index: {e:#}");
                }
                state::State::empty(config::state_path())
            } else {
                if let Err(e) = ensure_content_state_matches_index(&watch_content_index) {
                    log::warn!("check content index state: {e:#}");
                }
                state::State::load(config::state_path())
            };
            let initial_watch_dirs = match sync_with_retry(&watch_index, &roots, &mut state) {
                Ok(sync_outcome) => {
                    log::debug!(
                        "sync: {} changed files may need content",
                        sync_outcome.content_candidates
                    );
                    let _ = state.persist();
                    // The sync just deleted every catalogue row whose file is
                    // gone. Their content documents would otherwise keep
                    // answering searches, so clear them before serving — and
                    // uncapped, because a first start after an upgrade can face
                    // millions of them and the per-pass cap would take days.
                    match content::prune_orphaned_content(&watch_content_index, usize::MAX) {
                        Ok((0, _)) => {}
                        Ok((removed, _)) => {
                            log::info!("content prune: dropped {removed} row(s) for deleted files")
                        }
                        Err(e) => log::warn!("content prune at start failed: {e:#}"),
                    }
                    // Documents no state row references at all — left behind by
                    // every past `clear_content_state`. Only a walk of the index
                    // itself finds them.
                    match content::prune_unreferenced_content_docs(&watch_content_index) {
                        Ok(0) => {}
                        Ok(removed) => log::info!(
                            "content prune: dropped {removed} unreferenced document(s)"
                        ),
                        Err(e) => log::warn!("unreferenced content prune failed: {e:#}"),
                    }
                    let content_limit = content::daemon_batch_limit();
                    match content::pending_catalog_batch(content_limit, settings::content_cooldown())
                    {
                        Ok(pending_batch) => {
                            if let Err(e) =
                                content::record_skipped_candidates(&pending_batch.non_candidates)
                            {
                                log::warn!("record policy-skipped entries: {e:#}");
                            }
                            log::debug!(
                                "content backfill: {} files this start, {} deferred by cooldown (limit {content_limit} files / {} MiB)",
                                pending_batch.files.len(),
                                pending_batch.deferred,
                                content::daemon_batch_byte_limit() / (1 << 20),
                            );
                            watch_pending_content.store(
                                pending_batch.deferred as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            watch_activity.store(
                                watch::ACTIVITY_BACKFILLING,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if let Err(e) = content::index_content_limited(
                                &watch_content_index,
                                pending_batch.files,
                                Some(content_limit),
                            ) {
                                log::error!("content pass failed: {e:#}");
                            }
                            watch_activity.store(
                                watch::ACTIVITY_IDLE,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if !pending_batch.has_more
                                && pending_batch.deferred == 0
                                && let Err(e) =
                                    index::mark_content_state_synced(&config::content_index_dir())
                            {
                                log::warn!("mark content index synced: {e:#}");
                            }
                        }
                        Err(e) => log::error!("content backfill selection failed: {e:#}"),
                    }
                    let _ = state.persist();
                    sync_outcome.visible_dirs
                }
                Err(e) => {
                    // A failed startup sync means the index may be missing most
                    // of the catalogue (e.g. a sandbox/filesystem fault). Serving
                    // a near-empty index silently is worse than dying: exit so
                    // systemd restarts the unit (visible failure, fresh attempt).
                    log::error!("sync failed after retries — exiting for restart: {e:#}");
                    std::process::exit(1);
                }
            };
            // Startup sync + first backfill batch allocate bulk writer arenas;
            // return the freed pages before settling into the resident loop.
            throttle::release_idle_memory();
            if let Err(e) = watch::run(
                watch_index,
                watch_content_index,
                watch::WatchScope::new(&roots, initial_watch_dirs),
                state,
                watch::WatchRuntime::new(watch_paused, watch_activity, watch_pending_content),
                watch_commands,
                rx,
                watch_history,
            ) {
                log::error!("watcher stopped: {e:#}");
            }
        })?;

    // Main thread serves queries + lifecycle commands; the reader reloads on commit.
    // Built once and held for the daemon's life; `ReloadPolicy::OnCommitWithDelay`
    // (tantivy's default) refreshes them from the meta.json watch after a commit.
    let name_reader = index.reader().context("open name index reader")?;
    let content_reader = content_index
        .reader()
        .context("open content index reader")?;
    ipc::serve(
        name_reader,
        content_reader,
        &socket_path,
        ipc::Control {
            commands,
            paused,
            activity,
            pending_content,
        },
        config::origin_db_path(),
        history,
    )
}

struct WatcherExitGuard;

impl Drop for WatcherExitGuard {
    fn drop(&mut self) {
        log::error!("watcher thread exited; terminating daemon");
        std::process::exit(1);
    }
}

/// Startup reconcile with bounded retries: a transient failure (slow mount,
/// I/O hiccup) gets two more chances before the daemon gives up and exits.
fn sync_with_retry(
    index: &tantivy::Index,
    roots: &[PathBuf],
    state: &mut state::State,
) -> Result<scan::SyncOutcome> {
    const ATTEMPTS: u32 = 3;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match scan::sync(index, roots, state) {
            Ok(count) => return Ok(count),
            Err(e) => {
                log::warn!("sync attempt {attempt}/{ATTEMPTS} failed: {e:#}");
                last_error = Some(e);
                if attempt < ATTEMPTS {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.expect("at least one sync attempt ran"))
}

fn recover_inflated_index(index: &mut tantivy::Index) -> Result<()> {
    let mut cached_state = state::State::load(config::state_path());
    let cached_paths = cached_state.len() as u64;
    let indexed_documents = index::document_count(&index.reader()?)?;
    if !index::is_divergent_document_count(indexed_documents, cached_paths) {
        return Ok(());
    }

    if index::is_deflated_document_count(indexed_documents, cached_paths) {
        let deficit = cached_paths.saturating_sub(indexed_documents);
        if deficit <= index::max_repairable_name_deficit(cached_paths) {
            log::warn!(
                "index/cache deficit detected: {deficit} missing name docs; repairing index"
            );
            index::repair_deflated_against_state(index, &mut cached_state)?;
            return Ok(());
        }
    }

    log::warn!(
        "index/cache divergence detected: {indexed_documents} indexed docs for {} cached paths; rebuilding index",
        cached_paths
    );
    *index = index::recreate_on_disk(&config::index_dir())?;
    cached_state.clear();
    cached_state.persist()?;
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
/// exists — and the way somebody helping over the telephone can find out whether
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
    let Some(origin) = origin else {
        println!("{path}: no origin");
        return;
    };
    println!("{path}");
    println!("kind: {}", origin.kind);
    println!("confidence: {}", origin.confidence);
    println!("label: {}", origin.label);
    println!("first_seen_at: {}", origin.first_seen_at);
    if let Some(source_app) = &origin.source_app {
        println!("source_app: {source_app}");
    }
    if let Some(source_path) = &origin.source_path {
        println!("source_path: {source_path}");
    }
    if let Some(source_device_id) = &origin.source_device_id {
        println!("source_device_id: {source_device_id}");
    }
    if let Some(source_device_label) = &origin.source_device_label {
        println!("source_device_label: {source_device_label}");
    }
    if let Some(source_domain) = &origin.source_domain {
        println!("source_domain: {source_domain}");
    }
}

/// Send a lifecycle command (`pause`/`resume`/`rebuild`) to the running daemon.
/// These need a live daemon — without one there is nothing to control. The daemon
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
            let index = index::open_read_only(&config::index_dir())?;
            let content_index = matches!(mode, "content" | "both")
                .then(|| index::open_read_only(&config::content_index_dir()))
                .transpose()?;
            let origin_reader = s
                .include_origin
                .then(|| origin::OriginReader::new(config::origin_db_path()));
            let name_reader = index.reader()?;
            let content_reader = content_index.as_ref().map(|i| i.reader()).transpose()?;
            query::run_query(
                query::QueryIndexes {
                    name: &name_reader,
                    content: content_reader.as_ref(),
                },
                &s.query,
                mode,
                &filter,
                &page,
                s.include_origin,
                origin_reader.as_ref(),
            )?
        }
        Err(e) => return Err(anyhow::anyhow!(e.to_string())),
    };

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
