//! Socket transport: one connection per call, or one reused session.

use crate::{
    ClientError, Filter, IndexedRoot, OriginProvenance, OriginRequest, PROTOCOL_VERSION, Page,
    PathRequest, QueryOutcome, QueryRequest, RegisterCopyRequest, RegisterCreatedRequest,
    RegisterDerivedRequest, RegisterMoveRequest, Reply, Request, RequestBody, Status, Unavailable,
    VersionsAnswer,
};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a request may take before the caller gets an error instead of a
/// thread that never returns.
///
/// Without a deadline, a daemon that accepted the connection and then stopped
/// answering (a wedged commit, a stopped process still holding the socket)
/// blocks the calling thread forever, and `PersistentClient`'s single retry
/// only fires on a closed connection, which a silent hang never produces.
/// Short under test, where a deadline that expires is the case being checked.
const IO_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(15)
};

/// The largest reply this client will assemble.
///
/// `read_line` grows its `String` without limit, so without this ceiling a
/// socket that streams bytes and never sends a newline could take the process
/// out on memory alone. The
/// daemon's own pages are far below this.
const MAX_REPLY_BYTES: u64 = 8 * 1024 * 1024;

/// Read one newline-terminated reply, refusing anything past the ceiling.
fn read_reply_line(reader: &mut impl BufRead, line: &mut String) -> Result<(), ClientError> {
    let read = reader.take(MAX_REPLY_BYTES).read_line(line)?;
    if read == 0 {
        return Err(ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the index daemon closed the connection without answering",
        )));
    }
    if !line.ends_with('\n') {
        return Err(ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the index daemon's reply passed the size limit without ending",
        )));
    }
    Ok(())
}

/// Apply the request deadline to both directions of `stream`.
fn apply_deadlines(stream: &UnixStream) -> Result<(), ClientError> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

/// The default socket path: `$XDG_RUNTIME_DIR/biglinux/indexd.sock`.
///
/// `None` when there is no `XDG_RUNTIME_DIR` (a session started by cron, over
/// ssh, or by a misconfigured display manager). There is no `/tmp` fallback:
/// `/tmp` is world-writable, so the first local account to create
/// `/tmp/biglinux` owns it and can bind its own socket there. This client does
/// not check who owns the socket it connects to, so the search window would
/// render another account's replies as trusted results, including the paths
/// the person then clicks and opens.
///
/// Refusing costs a session with no runtime directory its search. That session
/// has no private directory to put a socket in either, so there is nothing to
/// refuse it for.
#[must_use]
pub fn default_socket() -> Option<PathBuf> {
    // An empty or relative value is unset (XDG Base Directory spec), as the
    // daemon reads it; joining it would name a socket under the current folder.
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|runtime| runtime.is_absolute())
        .map(|runtime| runtime.join("biglinux").join("indexd.sock"))
}

/// A thin handle to the index daemon. Each call is one short-lived connection: the
/// client holds no result state, so a GUI can't leak the index through it.
pub struct Client {
    sock: PathBuf,
}

struct PersistentConnection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

/// A reusable daemon session over one Unix-socket connection.
pub struct PersistentClient {
    sock: PathBuf,
    connection: Option<PersistentConnection>,
}

impl Client {
    /// Talk to the daemon at `sock`.
    pub fn new(sock: impl Into<PathBuf>) -> Self {
        Client { sock: sock.into() }
    }

    /// Talk to the daemon at the default socket path.
    ///
    /// `None` when this session has no private runtime directory to hold a
    /// socket; see [`default_socket`].
    #[must_use]
    pub fn connect_default() -> Option<Self> {
        default_socket().map(Client::new)
    }

    /// Whether a daemon appears to be listening (a cheap connect probe).
    #[must_use]
    pub fn is_available(&self) -> bool {
        UnixStream::connect(&self.sock).is_ok()
    }

    fn roundtrip(&self, body: RequestBody) -> Result<Reply, ClientError> {
        exchange(&mut connect(&self.sock)?, &request_line(body)?)
    }

    /// Run a query; the daemon filters, pages, and attaches metadata server-side.
    pub fn query(
        &self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        self.run_query(q, mode, filter, page, false)
    }

    /// Run a query and request origin summaries for the visible page.
    pub fn query_with_origin(
        &self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        self.run_query(q, mode, filter, page, true)
    }

    fn run_query(
        &self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
        include_origin: bool,
    ) -> Result<QueryOutcome, ClientError> {
        match self.roundtrip(RequestBody::Query(QueryRequest {
            q: q.to_string(),
            mode: mode.to_string(),
            filter,
            page,
            include_origin,
        }))? {
            Reply::Query { outcome, .. } => Ok(outcome),
            other => Err(ClientError::Protocol(format!(
                "expected query reply, got {other:?}"
            ))),
        }
    }

    /// Fetch full origin/provenance details for one path.
    pub fn origin(&self, path: &str) -> Result<Option<OriginProvenance>, ClientError> {
        match self.roundtrip(RequestBody::Origin(OriginRequest {
            path: path.to_string(),
        }))? {
            Reply::Origin { origin, .. } => Ok(origin),
            other => Err(ClientError::Protocol(format!(
                "expected origin reply, got {other:?}"
            ))),
        }
    }

    /// Register a successful file copy.
    pub fn register_copy(
        &self,
        src: &str,
        dst: &str,
        app_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::RegisterCopy(RegisterCopyRequest {
            src: src.to_string(),
            dst: dst.to_string(),
            app_id: app_id.map(str::to_string),
        }))
    }

    /// Register a successful move or rename.
    pub fn register_move(
        &self,
        src: &str,
        dst: &str,
        app_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::RegisterMove(RegisterMoveRequest {
            src: src.to_string(),
            dst: dst.to_string(),
            app_id: app_id.map(str::to_string),
        }))
    }

    /// Register a file created by an application.
    pub fn register_created(&self, path: &str, app_id: Option<&str>) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::RegisterCreated(RegisterCreatedRequest {
            path: path.to_string(),
            app_id: app_id.map(str::to_string),
        }))
    }

    /// Register a file derived from another file.
    pub fn register_derived(
        &self,
        src: &str,
        dst: &str,
        app_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::RegisterDerived(RegisterDerivedRequest {
            src: src.to_string(),
            dst: dst.to_string(),
            app_id: app_id.map(str::to_string),
        }))
    }

    /// Cataloged count + paused flag.
    pub fn status(&self) -> Result<Status, ClientError> {
        match self.roundtrip(RequestBody::Status)? {
            Reply::Status {
                indexed,
                paused,
                activity,
                capabilities,
                pending_content,
                content_docs,
                name_index_bytes,
                content_index_bytes,
                unwatched_dirs,
                ..
            } => Ok(Status {
                indexed,
                paused,
                activity: if activity.is_empty() {
                    if paused { "paused" } else { "indexing" }.to_string()
                } else {
                    activity
                },
                capabilities,
                pending_content,
                content_docs,
                name_index_bytes,
                content_index_bytes,
                unwatched_dirs,
            }),
            other => Err(ClientError::Protocol(format!(
                "expected status reply, got {other:?}"
            ))),
        }
    }

    /// Which directories this daemon catalogues.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] when no daemon is listening, and
    /// [`ClientError::Daemon`] from one too old to know the command. Neither
    /// means "nothing is indexed": a caller that cannot get an answer knows
    /// only that it does not know, and must not tell the person otherwise.
    pub fn sources(&self) -> Result<Vec<IndexedRoot>, ClientError> {
        match self.roundtrip(RequestBody::Sources)? {
            Reply::Sources { roots, .. } => Ok(roots),
            other => Err(ClientError::Protocol(format!(
                "expected sources reply, got {other:?}"
            ))),
        }
    }

    /// Forget every recorded file origin.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn clear_origin(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::ClearOrigin)
    }

    /// The earlier versions kept for one document, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or answers oddly.
    pub fn versions(&self, path: &str) -> Result<VersionsAnswer, ClientError> {
        match self.roundtrip(RequestBody::Versions(PathRequest {
            path: path.to_string(),
        }))? {
            Reply::Versions {
                versions,
                unavailable,
                paused_for_space,
                ..
            } => Ok(VersionsAnswer {
                versions,
                unavailable: Unavailable::from_wire(&unavailable),
                paused_for_space,
            }),
            other => Err(ClientError::Protocol(format!(
                "expected versions reply, got {other:?}"
            ))),
        }
    }

    /// Keep the state a document is in right now, before a version is restored
    /// over it.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses, including
    /// when the document exists but its current state could not be kept. Do not
    /// restore over the document after an error.
    pub fn save_version(&self, path: &str) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::SaveVersion(PathRequest {
            path: path.to_string(),
        }))
    }

    /// Forget the versions of one document.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn forget_versions(&self, path: &str) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::ForgetVersions(PathRequest {
            path: path.to_string(),
        }))
    }

    /// Forget every version of everything, also when versions are turned off.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn clear_history(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::ClearHistory)
    }

    /// Pause indexing (queries keep working against the current index).
    pub fn pause(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Pause)
    }

    /// Resume indexing and reconcile changes missed while paused.
    pub fn resume(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Resume)
    }

    /// Reconcile the sources with the catalogue, for changes made while the
    /// service was stopped. The daemon refuses it while indexing is paused.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn rescan(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Rescan)
    }

    /// Read every catalogued folder from the start. Queries keep answering
    /// meanwhile, from what is catalogued so far.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn rebuild(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Rebuild)
    }

    fn expect_ok(&self, body: RequestBody) -> Result<(), ClientError> {
        ok_from(self.roundtrip(body)?)
    }
}

impl PersistentClient {
    /// Talk to the daemon at `sock` with a reusable connection.
    pub fn new(sock: impl Into<PathBuf>) -> Self {
        Self {
            sock: sock.into(),
            connection: None,
        }
    }

    /// Talk to the daemon at the default socket path with a reusable connection.
    ///
    /// `None` when this session has no private runtime directory; see
    /// [`default_socket`].
    #[must_use]
    pub fn connect_default() -> Option<Self> {
        default_socket().map(Self::new)
    }

    fn connection(&mut self) -> Result<&mut PersistentConnection, ClientError> {
        let connection = match self.connection.take() {
            Some(connection) => connection,
            None => connect(&self.sock)?,
        };
        Ok(self.connection.insert(connection))
    }

    /// Sent again only when a reused connection turns out closed: the daemon
    /// restarted or dropped it while idle. A timeout is not retried, so a
    /// stuck daemon costs one deadline. After any failure but the daemon's own
    /// error reply the connection is dropped, so a late answer is never read
    /// as the next one.
    fn roundtrip(&mut self, body: RequestBody) -> Result<Reply, ClientError> {
        let request_bytes = request_line(body)?;
        let reused = self.connection.is_some();
        let result = match exchange(self.connection()?, &request_bytes) {
            Err(ClientError::Io(error))
                if reused
                    && matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
            {
                self.connection = None;
                exchange(self.connection()?, &request_bytes)
            }
            result => result,
        };
        if result
            .as_ref()
            .is_err_and(|error| !matches!(error, ClientError::Daemon(_)))
        {
            self.connection = None;
        }
        result
    }

    /// Forget the versions kept for one document.
    ///
    /// Also here because forgetting comes in runs (emptying the Trash), where a
    /// connection per item costs a daemon thread each.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or answers oddly.
    pub fn forget_versions(&mut self, path: &str) -> Result<(), ClientError> {
        ok_from(self.roundtrip(RequestBody::ForgetVersions(PathRequest {
            path: path.to_string(),
        }))?)
    }
}

fn ok_from(reply: Reply) -> Result<(), ClientError> {
    match reply {
        Reply::Ok { .. } => Ok(()),
        other => Err(ClientError::Protocol(format!(
            "expected ok reply, got {other:?}"
        ))),
    }
}

fn connect(sock: &Path) -> Result<PersistentConnection, ClientError> {
    let stream = UnixStream::connect(sock)?;
    apply_deadlines(&stream)?;
    let reader = BufReader::new(stream.try_clone()?);
    Ok(PersistentConnection { stream, reader })
}

/// The request as one newline-terminated line, so it goes out in one write.
fn request_line(body: RequestBody) -> Result<Vec<u8>, ClientError> {
    let mut line = serde_json::to_vec(&Request {
        v: PROTOCOL_VERSION,
        body,
    })?;
    line.push(b'\n');
    Ok(line)
}

fn exchange(
    connection: &mut PersistentConnection,
    request_bytes: &[u8],
) -> Result<Reply, ClientError> {
    connection.stream.write_all(request_bytes)?;
    connection.stream.flush()?;

    let mut line = String::new();
    read_reply_line(&mut connection.reader, &mut line)?;
    let reply: Reply = serde_json::from_str(line.trim())?;
    if let Reply::Error { message, .. } = &reply {
        return Err(ClientError::Daemon(message.clone()));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_client_reuses_its_connection() {
        use std::os::unix::net::UnixListener;
        use std::thread;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // In `/tmp`: a socket path must fit `sun_path` (108 bytes), and `TMPDIR` can be long.
        let socket_path = std::path::Path::new("/tmp").join(format!(
            "big-indexd-persistent-{}-{unique}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server_thread = thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            for _ in 0..2 {
                let mut request_line = String::new();
                reader.read_line(&mut request_line)?;
                assert!(request_line.contains("\"cmd\":\"forget_versions\""));
                writeln!(stream, "{{\"kind\":\"ok\",\"v\":2}}")?;
                stream.flush()?;
            }
            Ok(())
        });

        let mut client = PersistentClient::new(socket_path.clone());
        client.forget_versions("/a").unwrap();
        client.forget_versions("/b").unwrap();

        server_thread.join().unwrap().unwrap();
        std::fs::remove_file(socket_path).ok();
    }

    #[test]
    fn silent_daemon_costs_one_deadline() {
        use std::os::unix::net::UnixListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Instant;

        // In `/tmp`: a socket path must fit `sun_path` (108 bytes), and `TMPDIR` can be long.
        let socket_path = std::path::Path::new("/tmp")
            .join(format!("big-indexd-silent-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        // Takes the request and never answers; a second connection would be a
        // resend.
        let server = std::thread::spawn({
            let done = done.clone();
            move || {
                let mut held = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    if let Ok((stream, _)) = listener.accept() {
                        held.push(stream);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                held.len()
            }
        });

        let mut client = PersistentClient::new(socket_path.clone());
        let started = Instant::now();
        let outcome = client.forget_versions("/home/p/a");
        let took = started.elapsed();
        done.store(true, Ordering::Relaxed);
        std::fs::remove_file(&socket_path).ok();
        assert!(matches!(outcome, Err(ClientError::Io(_))), "{outcome:?}");
        assert!(took < IO_TIMEOUT + IO_TIMEOUT / 2, "{took:?}");
        assert_eq!(server.join().unwrap(), 1);
    }
}
