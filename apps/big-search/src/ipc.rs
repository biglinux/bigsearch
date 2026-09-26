//! Server side of the big-indexd protocol: accept connections, admit same-UID
//! peers, parse newline-delimited versioned requests, and reply. The wire types
//! and the client live in the shared `big-indexd-client` crate — this module
//! only serves.
use crate::watch::{WatchControlCommand, WatchMessage};
use crate::{config, index, origin, query, settings};
use anyhow::{Context, Result};
use big_indexd_client::{PROTOCOL_VERSION, Reply, Request, RequestBody};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use tantivy::IndexReader;

/// Handle the IPC thread uses to drive the watcher: deliver lifecycle commands and
/// read the live `paused` flag for `status`.
pub struct Control {
    pub commands: Sender<WatchMessage>,
    pub paused: Arc<AtomicBool>,
    pub activity: Arc<AtomicU8>,
    /// Files whose content extraction is deferred by the edit cooldown; exposed
    /// on query/status replies so UIs can show a "content still indexing" hint.
    pub pending_content: Arc<AtomicU64>,
}

/// Upper bound on a single request line. A query is a few bytes; this only refuses a
/// client that streams an endless line (local DoS — the socket is 0600, same-user).
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Connections served at once. Each costs a thread; the clients are a CLI and a
/// couple of desktop integrations, so this is generous and still bounds a client
/// that leaks connections.
const MAX_LIVE_CONNECTIONS: usize = 16;

/// How long a connection may sit silent before we hang up. Clients are allowed to
/// keep a connection open between a user's keystrokes, so this is minutes rather
/// than seconds; it only reclaims threads from clients that died without closing.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How long a reply may take to drain. Bounds a client that connects, asks, then
/// stops reading — without it that client owns its thread forever.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Decrements the live-connection count however the thread ends, including a panic.
struct ConnectionSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn err(message: impl Into<String>) -> Reply {
    Reply::Error {
        v: PROTOCOL_VERSION,
        message: message.into(),
    }
}

/// Serve requests on `sock` until killed. Holds the index open; clients may send
/// multiple newline-delimited requests per connection. Only same-UID peers are
/// admitted (defence in depth over 0600).
pub fn serve(
    index: IndexReader,
    content_index: IndexReader,
    sock: &Path,
    control: Control,
    origin_db: std::path::PathBuf,
    history: crate::history::SharedHistory,
) -> Result<()> {
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // Must be ours, a real directory, and private. Without XDG_RUNTIME_DIR
        // this falls back to a predictable path under /tmp, where the sticky bit
        // lets another local account create it first; binding our socket inside
        // it would let them replace the socket, and clients do not authenticate
        // the server.
        //
        // Checked before the chmod, and with `symlink_metadata`: the other order
        // followed a planted symlink and changed the mode of someone else's
        // directory on the way to refusing.
        let entry =
            std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
        if entry.file_type().is_symlink() {
            anyhow::bail!("{} is a symlink; refusing to bind inside it", dir.display());
        }
        // SAFETY: getuid is always safe; it reads the calling process's own uid.
        let us = unsafe { libc::getuid() };
        if entry.uid() != us {
            anyhow::bail!(
                "{} is owned by uid {}, not us ({us})",
                dir.display(),
                entry.uid()
            );
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", dir.display()))?;
    }
    if sock.exists() {
        std::fs::remove_file(sock).ok();
    }
    let listener = UnixListener::bind(sock).with_context(|| format!("bind {}", sock.display()))?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))
        .context("socket perms 0600")?;
    log::info!(
        "listening on {} (protocol v{PROTOCOL_VERSION})",
        sock.display()
    );

    // One thread per connection. Serving inline used to mean a single client that
    // held its connection open — which the client crate explicitly supports —
    // blocked every other client, so one idle desktop integration made `status`
    // and every search hang until it disconnected. It also put each handler on
    // the main thread, where a panic took the whole daemon down with it; a
    // connection thread only takes itself.
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept error: {e}");
                continue;
            }
        };
        if !peer_is_self(&s) {
            log::warn!("rejected connection from a different uid");
            continue;
        }
        let _ = s.set_read_timeout(Some(IDLE_TIMEOUT));
        let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
        if live.load(Ordering::Relaxed) >= MAX_LIVE_CONNECTIONS {
            // Say so instead of closing silently: a bare EOF looks to the client
            // like a crashed daemon, and the caller cannot tell "busy" from "gone".
            log::warn!("refusing connection: {MAX_LIVE_CONNECTIONS} already live");
            let mut s = s;
            let busy = err(format!(
                "daemon busy: {MAX_LIVE_CONNECTIONS} connections already open"
            ));
            let _ = serde_json::to_writer(&mut s, &busy);
            let _ = s.write_all(b"\n");
            continue;
        }

        live.fetch_add(1, Ordering::Relaxed);
        let slot = ConnectionSlot(live.clone());
        let index = index.clone();
        let content_index = content_index.clone();
        let control = Control {
            commands: control.commands.clone(),
            paused: control.paused.clone(),
            activity: control.activity.clone(),
            pending_content: control.pending_content.clone(),
        };
        // One reader per connection: it holds a `RefCell<Connection>`, so it is
        // not shareable, and it opens nothing until first use.
        let origin_reader = origin::OriginReader::new(origin_db.clone());
        // The one versions store, shared with the thread that writes versions.
        // A second store opened here would answer from a fresh state: whether
        // this disk can share blocks at all, and whether space ran out, are
        // things only the thread that has tried to write a version knows.
        let history = std::sync::Arc::clone(&history);
        let spawned = std::thread::Builder::new()
            .name("ipc-conn".into())
            .spawn(move || {
                let _slot = slot;
                if let Err(e) = handle(
                    &index,
                    &content_index,
                    s,
                    &control,
                    &origin_reader,
                    &history,
                ) {
                    log::warn!("connection error: {e:#}");
                }
            });
        if let Err(e) = spawned {
            // The closure (and with it the slot guard) is dropped on failure, so
            // the count is already corrected — decrementing here would underflow.
            log::warn!("could not spawn connection thread: {e}");
        }
    }
    Ok(())
}

/// Whether the connected peer runs as the same UID as this daemon (SO_PEERCRED).
fn peer_is_self(stream: &UnixStream) -> bool {
    // SAFETY: getsockopt on a valid connected Unix socket fd; ucred is POD.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    rc == 0 && cred.uid == unsafe { libc::getuid() }
}

fn handle(
    index: &IndexReader,
    content_index: &IndexReader,
    stream: UnixStream,
    control: &Control,
    origin_reader: &origin::OriginReader,
    history: &crate::history::SharedHistory,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    // Buffered on purpose: `serde_json::to_writer` emits every brace, key, comma
    // and value as its own `write`, so an unbuffered socket turned one reply into
    // thousands of syscalls. Measured at 1328 hits: 38 ms to write ~190 KB, and
    // unlike the search and the stat loop it never got cheaper on repeat — it was
    // the largest and most stubborn part of every query, in all three modes.
    let mut out = std::io::BufWriter::new(stream);
    let mut line = Vec::new();
    loop {
        let bytes_read = read_capped_line(&mut reader, &mut line)?;
        if bytes_read == 0 {
            break;
        }
        let request_text = String::from_utf8_lossy(&line);
        let request_text = request_text.trim();
        if request_text.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(request_text) {
            Ok(request) if request.v != PROTOCOL_VERSION => err(format!(
                "unsupported protocol v{}; daemon speaks v{PROTOCOL_VERSION}",
                request.v
            )),
            Ok(request) => dispatch(
                index,
                content_index,
                request.body,
                control,
                origin_reader,
                history,
            ),
            Err(e) => err(format!("bad request: {e}")),
        };

        serde_json::to_writer(&mut out, &reply)?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
    Ok(())
}

fn read_capped_line(reader: &mut BufReader<UnixStream>, line: &mut Vec<u8>) -> io::Result<usize> {
    line.clear();
    let cap = MAX_REQUEST_BYTES as usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(line.len());
        }
        if let Some(newline) = available.iter().position(|&byte| byte == b'\n') {
            let take = newline + 1;
            if line.len().saturating_add(take) > cap {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request line exceeds maximum size",
                ));
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(line.len());
        }
        if line.len().saturating_add(available.len()) > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request line exceeds maximum size",
            ));
        }
        let take = available.len();
        line.extend_from_slice(available);
        reader.consume(take);
    }
}

fn dispatch(
    index: &IndexReader,
    content_index: &IndexReader,
    body: RequestBody,
    control: &Control,
    origin_reader: &origin::OriginReader,
    history: &crate::history::SharedHistory,
) -> Reply {
    match body {
        RequestBody::Query(query_request) => {
            let wants_content = matches!(query_request.mode.as_str(), "content" | "both");
            if wants_content {
                // A content search is the strongest signal that deferred bodies
                // are wanted now: nudge the watcher to extract them, cooldown-free.
                let _ = control.commands.send(WatchMessage::ControlCommand(
                    WatchControlCommand::WarmContent,
                ));
            }
            match query::run_query(
                query::QueryIndexes {
                    name: index,
                    content: Some(content_index),
                },
                &query_request.q,
                &query_request.mode,
                &query_request.filter,
                &query_request.page,
                query_request.include_origin,
                Some(origin_reader),
            ) {
                Ok(mut query_outcome) => {
                    if wants_content {
                        query_outcome.content_pending =
                            control.pending_content.load(Ordering::Relaxed);
                    }
                    Reply::Query {
                        v: PROTOCOL_VERSION,
                        outcome: query_outcome,
                    }
                }
                Err(e) => err(format!("query failed: {e}")),
            }
        }
        RequestBody::Origin(origin_request) => Reply::Origin {
            v: PROTOCOL_VERSION,
            origin: origin_reader.origin(Path::new(&origin_request.path)),
        },
        RequestBody::RegisterCopy(register_copy_request) => register_origin(|writer| {
            writer.register_copy(
                Path::new(&register_copy_request.src),
                Path::new(&register_copy_request.dst),
                register_copy_request.app_id.as_deref(),
            )
        }),
        RequestBody::RegisterMove(register_move_request) => register_origin(|writer| {
            writer.register_move(
                Path::new(&register_move_request.src),
                Path::new(&register_move_request.dst),
                register_move_request.app_id.as_deref(),
            )
        }),
        RequestBody::RegisterCreated(register_created_request) => register_origin(|writer| {
            writer.register_created(
                Path::new(&register_created_request.path),
                register_created_request.app_id.as_deref(),
            )
        }),
        RequestBody::RegisterDerived(register_derived_request) => register_origin(|writer| {
            writer.register_derived(
                Path::new(&register_derived_request.src),
                Path::new(&register_derived_request.dst),
                register_derived_request.app_id.as_deref(),
                register_derived_request.action.as_deref(),
            )
        }),
        RequestBody::Status => {
            let paused = control.paused.load(Ordering::Relaxed);
            Reply::Status {
                v: PROTOCOL_VERSION,
                indexed: index::document_count(index).unwrap_or(0),
                paused,
                activity: crate::watch::activity_label(
                    paused,
                    control.activity.load(Ordering::Relaxed),
                )
                .to_string(),
                capabilities: daemon_capabilities(),
                pending_content: control.pending_content.load(Ordering::Relaxed),
                content_docs: index::document_count(content_index).unwrap_or(0),
                name_index_bytes: flat_directory_file_bytes(&config::index_dir()),
                content_index_bytes: flat_directory_file_bytes(&config::content_index_dir()),
            }
        }
        // Hand the command to the watcher thread over the shared channel; it applies
        // it between batches. Accepted asynchronously (the work may be long).
        RequestBody::Pause => send_control_command(control, WatchControlCommand::Pause),
        RequestBody::Resume => send_control_command(control, WatchControlCommand::Resume),
        RequestBody::Rebuild => send_control_command(control, WatchControlCommand::Rebuild),
        RequestBody::Reindex(request) => send_control_command(
            control,
            WatchControlCommand::ReindexPath(std::path::PathBuf::from(request.path)),
        ),
        RequestBody::ClearOrigin => send_control_command(control, WatchControlCommand::ClearOrigin),
        RequestBody::Versions(request) => {
            let store = history.lock().expect("versions store lock");
            let versions = store
                .versions(std::path::Path::new(&request.path))
                .unwrap_or_default()
                .into_iter()
                .map(|version| big_indexd_client::FileVersion {
                    id: version.id,
                    saved_at: version.saved_at,
                    size: version.size,
                    reason: version.reason.as_str().to_string(),
                    object: version.object.to_string_lossy().into_owned(),
                    was_named: version.was_named,
                })
                .collect();
            Reply::Versions {
                v: PROTOCOL_VERSION,
                versions,
                unavailable: match store.support() {
                    crate::history::Support::Unavailable(reason) => reason.as_str().to_string(),
                    _ => String::new(),
                },
                paused_for_space: store.is_paused_for_space(),
            }
        }
        // Done here rather than handed to the worker: a file manager asks for
        // this immediately before it overwrites the document, and an answer that
        // arrives after the overwrite is no answer at all.
        RequestBody::SaveVersion(request) => history_reply(history, |store| {
            store
                .save_version(
                    std::path::Path::new(&request.path),
                    crate::history::Reason::BeforeRestore,
                )
                .map(|_| ())
        }),
        RequestBody::ForgetVersions(request) => history_reply(history, |store| {
            store.forget(std::path::Path::new(&request.path))
        }),
        RequestBody::ClearHistory => {
            history_reply(history, crate::history::History::forget_everything)
        }
        RequestBody::Sources => Reply::Sources {
            v: PROTOCOL_VERSION,
            roots: indexed_roots(),
        },
    }
}

/// The catalogued directories, as a caller standing in a folder needs them.
///
/// `content` is per source rather than global: `names_only` turns extraction off
/// everywhere, and a source may also have it off on its own, and either way a
/// search for words inside a document cannot be answered from here.
fn indexed_roots() -> Vec<big_indexd_client::IndexedRoot> {
    let names_only = settings::names_only();
    settings::sources()
        .all()
        .iter()
        .map(|source| big_indexd_client::IndexedRoot {
            path: source.path.to_string_lossy().into_owned(),
            name: source.name.clone(),
            content: !names_only && source.content(),
            available: settings::is_available(source),
        })
        .collect()
}

fn flat_directory_file_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries {
        let Ok(entry) = entry else {
            return 0;
        };
        let Ok(metadata) = entry.metadata() else {
            return 0;
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

fn daemon_capabilities() -> Vec<String> {
    let mut capabilities = origin::capabilities();
    if settings::content_index_enabled() {
        capabilities.push(format!(
            "content-index-mode:{}",
            settings::resolved_content_index_mode().as_str()
        ));
        capabilities.push(format!("extract-cap:{}", settings::extract_max_mb_label()));
    } else {
        capabilities.push("names-only".to_string());
    }
    // Named so a client can tell a daemon that will answer `sources` from one
    // that refuses it, without spending a round trip to find out.
    capabilities.push("sources".to_string());
    // The `list` mode narrows by `under` and `ext` inside the index, so it
    // lists every match rather than filtering a sample of the index. An older
    // daemon answers the same request with an empty page.
    capabilities.push("list-filtered".to_string());
    // `filter.mime` (a type or `major/*`) is answered, and hits carry `mime`.
    capabilities.push("mime-filter".to_string());
    capabilities
}

fn register_origin(f: impl FnOnce(&origin::OriginWriter) -> Result<()>) -> Reply {
    match origin::OriginWriter::open(config::origin_db_path()).and_then(|writer| f(&writer)) {
        Ok(()) => Reply::Ok {
            v: PROTOCOL_VERSION,
        },
        Err(e) => err(format!("origin registration failed: {e:#}")),
    }
}

fn send_control_command(control: &Control, control_command: WatchControlCommand) -> Reply {
    match control
        .commands
        .send(WatchMessage::ControlCommand(control_command))
    {
        Ok(()) => Reply::Ok {
            v: PROTOCOL_VERSION,
        },
        Err(_) => err("watcher thread is gone; daemon is shutting down"),
    }
}

/// Run one change against the versions store and answer plainly.
fn history_reply(
    history: &crate::history::SharedHistory,
    change: impl FnOnce(&mut crate::history::History) -> anyhow::Result<()>,
) -> Reply {
    let mut store = history.lock().expect("versions store lock");
    match change(&mut store) {
        Ok(()) => Reply::Ok {
            v: PROTOCOL_VERSION,
        },
        Err(error) => err(error.to_string()),
    }
}
