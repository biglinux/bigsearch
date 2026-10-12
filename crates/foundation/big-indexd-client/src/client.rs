//! Socket transport: one connection per call, or one reused session.

use crate::{
    ClientError, Filter, IndexedRoot, OriginProvenance, OriginRequest, PROTOCOL_VERSION, Page,
    PathRequest, QueryOutcome, QueryRequest, RegisterCopyRequest, RegisterCreatedRequest,
    RegisterDerivedRequest, RegisterMoveRequest, ReindexRequest, Reply, Request, RequestBody,
    Status, Unavailable, VersionsAnswer,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// How long a request may take before the caller gets an error instead of a
/// thread that never returns.
///
/// Without a deadline, a daemon that accepted the connection and then stopped
/// answering (a wedged commit, a stopped process still holding the socket)
/// blocks the calling thread forever, and `PersistentClient`'s single retry
/// only fires on an I/O error, which a silent hang never produces.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

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
    pub fn is_available(&self) -> bool {
        UnixStream::connect(&self.sock).is_ok()
    }

    /// Open a reusable session. Existing one-shot methods remain available for
    /// compatibility; use this when issuing several requests in a row.
    pub fn session(&self) -> PersistentClient {
        PersistentClient::new(self.sock.clone())
    }

    fn roundtrip(&self, body: RequestBody) -> Result<Reply, ClientError> {
        let stream = UnixStream::connect(&self.sock)?;
        apply_deadlines(&stream)?;
        let request = Request {
            v: PROTOCOL_VERSION,
            body,
        };
        let mut w = stream.try_clone()?;
        serde_json::to_writer(&mut w, &request)?;
        w.write_all(b"\n")?;
        w.flush()?;

        let mut line = String::new();
        read_reply_line(&mut BufReader::new(stream), &mut line)?;
        let reply: Reply = serde_json::from_str(line.trim())?;
        if let Reply::Error { message, .. } = &reply {
            return Err(ClientError::Daemon(message.clone()));
        }
        Ok(reply)
    }

    /// Run a query; the daemon filters, pages, and attaches metadata server-side.
    pub fn query(
        &self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        match self.roundtrip(RequestBody::Query(QueryRequest {
            q: q.to_string(),
            mode: mode.to_string(),
            filter,
            page,
            include_origin: false,
        }))? {
            Reply::Query { outcome, .. } => Ok(outcome),
            other => Err(ClientError::Protocol(format!(
                "expected query reply, got {other:?}"
            ))),
        }
    }

    /// Run a query and request origin summaries for the visible page.
    pub fn query_with_origin(
        &self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        match self.roundtrip(RequestBody::Query(QueryRequest {
            q: q.to_string(),
            mode: mode.to_string(),
            filter,
            page,
            include_origin: true,
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
        action: Option<&str>,
    ) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::RegisterDerived(RegisterDerivedRequest {
            src: src.to_string(),
            dst: dst.to_string(),
            app_id: app_id.map(str::to_string),
            action: action.map(str::to_string),
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

    /// Ask the daemon to read `path` again.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn reindex(&self, path: &std::path::Path) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Reindex(ReindexRequest {
            path: path.to_string_lossy().into_owned(),
        }))
    }

    /// Request a full rebuild of the index from source files.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
    pub fn rebuild(&self) -> Result<(), ClientError> {
        self.expect_ok(RequestBody::Rebuild)
    }

    fn expect_ok(&self, body: RequestBody) -> Result<(), ClientError> {
        match self.roundtrip(body)? {
            Reply::Ok { .. } => Ok(()),
            other => Err(ClientError::Protocol(format!(
                "expected ok reply, got {other:?}"
            ))),
        }
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

    fn connect(&self) -> Result<PersistentConnection, ClientError> {
        let stream = UnixStream::connect(&self.sock)?;
        apply_deadlines(&stream)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(PersistentConnection { stream, reader })
    }

    fn connection(&mut self) -> Result<&mut PersistentConnection, ClientError> {
        if self.connection.is_none() {
            self.connection = Some(self.connect()?);
        }
        Ok(self
            .connection
            .as_mut()
            .expect("connection was just opened"))
    }

    fn roundtrip(&mut self, body: RequestBody) -> Result<Reply, ClientError> {
        let request = Request {
            v: PROTOCOL_VERSION,
            body,
        };
        let mut request_bytes = serde_json::to_vec(&request)?;
        request_bytes.push(b'\n');

        for attempt in 0..2 {
            let result = {
                let connection = self.connection()?;
                persistent_roundtrip(connection, &request_bytes)
            };
            match result {
                Ok(reply) => return Ok(reply),
                Err(ClientError::Io(_)) if attempt == 0 => {
                    self.connection = None;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("persistent retry loop always returns");
    }

    /// Run a query; the daemon filters, pages, and attaches metadata server-side.
    pub fn query(
        &mut self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        match self.roundtrip(RequestBody::Query(QueryRequest {
            q: q.to_string(),
            mode: mode.to_string(),
            filter,
            page,
            include_origin: false,
        }))? {
            Reply::Query { outcome, .. } => Ok(outcome),
            other => Err(ClientError::Protocol(format!(
                "expected query reply, got {other:?}"
            ))),
        }
    }

    /// Run a query and request origin summaries for the visible page.
    pub fn query_with_origin(
        &mut self,
        q: &str,
        mode: &str,
        filter: Filter,
        page: Page,
    ) -> Result<QueryOutcome, ClientError> {
        match self.roundtrip(RequestBody::Query(QueryRequest {
            q: q.to_string(),
            mode: mode.to_string(),
            filter,
            page,
            include_origin: true,
        }))? {
            Reply::Query { outcome, .. } => Ok(outcome),
            other => Err(ClientError::Protocol(format!(
                "expected query reply, got {other:?}"
            ))),
        }
    }

    /// Fetch full origin/provenance details for one path.
    pub fn origin(&mut self, path: &str) -> Result<Option<OriginProvenance>, ClientError> {
        match self.roundtrip(RequestBody::Origin(OriginRequest {
            path: path.to_string(),
        }))? {
            Reply::Origin { origin, .. } => Ok(origin),
            other => Err(ClientError::Protocol(format!(
                "expected origin reply, got {other:?}"
            ))),
        }
    }

    /// Forget the versions kept for one document.
    ///
    /// Here as well as on [`Client`] because forgetting comes in runs: emptying
    /// the Trash asks about every item in it, and a fresh connection per item is
    /// a connection, a thread in the daemon and a teardown for each.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or answers oddly.
    pub fn forget_versions(&mut self, path: &str) -> Result<(), ClientError> {
        match self.roundtrip(RequestBody::ForgetVersions(PathRequest {
            path: path.to_string(),
        }))? {
            Reply::Ok { .. } => Ok(()),
            other => Err(ClientError::Protocol(format!(
                "expected ok reply, got {other:?}"
            ))),
        }
    }

    /// Cataloged count + paused flag.
    pub fn status(&mut self) -> Result<Status, ClientError> {
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
}

fn persistent_roundtrip(
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
        let socket_path = std::env::temp_dir().join(format!(
            "big-indexd-persistent-{}-{unique}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server_thread = thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            for indexed in [1, 2] {
                let mut request_line = String::new();
                reader.read_line(&mut request_line)?;
                assert!(request_line.contains("\"cmd\":\"status\""));
                writeln!(
                    stream,
                    "{{\"kind\":\"status\",\"v\":2,\"indexed\":{indexed},\"paused\":false,\"activity\":\"idle\"}}"
                )?;
                stream.flush()?;
            }
            Ok(())
        });

        let mut client = Client::new(socket_path.clone()).session();
        assert_eq!(client.status().unwrap().indexed, 1);
        assert_eq!(client.status().unwrap().indexed, 2);

        server_thread.join().unwrap().unwrap();
        std::fs::remove_file(socket_path).ok();
    }
}
