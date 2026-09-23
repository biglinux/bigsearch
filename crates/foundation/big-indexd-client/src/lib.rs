//! Client and wire types for the **big-indexd** (big-search) Unix-socket protocol.
//!
//! This crate is deliberately tiny — `serde` + `std` only, no Tantivy — so GUI
//! consumers (big-shell, big-filemanager) link a few KB and never pull the index
//! engine. The daemon (`big-search`) depends on these same types, so the wire
//! format cannot drift between server and client.
//!
//! ```rust,no_run
//! # use std::io::{BufRead, BufReader, Write};
//! # use std::os::unix::net::UnixListener;
//! # use std::thread;
//! use big_indexd_client::{Client, Filter, Page};
//! # let socket_path = std::env::temp_dir().join(format!("big-indexd-doc-{}.sock", std::process::id()));
//! # let _ = std::fs::remove_file(&socket_path);
//! # let listener = UnixListener::bind(&socket_path)?;
//! # let server_thread = thread::spawn(move || -> std::io::Result<()> {
//! #     let (mut stream, _) = listener.accept()?;
//! #     let mut request_line = String::new();
//! #     BufReader::new(stream.try_clone()?).read_line(&mut request_line)?;
//! #     stream.write_all(br#"{"kind":"query","v":2,"hits":[{"path":"/example/relatorio.pdf","name":"relatorio.pdf","score":1.0,"size":2048,"mtime":1,"ext":"pdf","source":"Example","available":true}],"total":1,"next_offset":null}"#)?;
//! #     stream.write_all(b"\n")?;
//! #     Ok(())
//! # });
//! let client = Client::new(socket_path.clone());
//! let query_page = client.query("relatorio", "both", Filter { ext: vec!["pdf".into()], ..Default::default() }, Page { offset: 0, limit: 50 })?;
//! for hit in &query_page.hits {
//!     println!("{} ({} bytes)", hit.path, hit.size);
//! }
//! # server_thread.join().unwrap()?;
//! # std::fs::remove_file(socket_path).ok();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Wire protocol version understood by this client.
pub const PROTOCOL_VERSION: u32 = 2;

/// How many candidates the daemon considers before filtering and paging.
///
/// Part of the wire contract rather than an implementation detail of the daemon,
/// because [`QueryOutcome::total`] is **capped at it** — so a caller cannot tell
/// an exact count from a floor without knowing this number. A total below it is
/// how many matched; a total equal to it means "this many, and possibly far
/// more", and a surface that prints it as an exact figure is stating something
/// nobody established.
pub const CANDIDATE_CEILING: usize = 5_000;

/// Short origin/provenance summary attached to visible hits only when requested.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
pub struct OriginSummary {
    pub kind: String,
    pub confidence: u8,
    pub label: String,
    pub first_seen_at: i64,
}

/// Full origin/provenance detail for a single path.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
pub struct OriginProvenance {
    pub kind: String,
    pub confidence: u8,
    pub label: String,
    pub first_seen_at: i64,
    #[serde(default)]
    pub source_app: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub source_device_id: Option<String>,
    #[serde(default)]
    pub source_device_label: Option<String>,
    #[serde(default)]
    pub source_domain: Option<String>,
}

impl From<OriginProvenance> for OriginSummary {
    fn from(origin_provenance: OriginProvenance) -> Self {
        Self {
            kind: origin_provenance.kind,
            confidence: origin_provenance.confidence,
            label: origin_provenance.label,
            first_seen_at: origin_provenance.first_seen_at,
        }
    }
}

/// Which index a hit was found through, so a GUI can explain a result whose
/// filename does not contain the query at all.
///
/// The daemon knows this while it merges the two searches in `search_both` and
/// used to throw it away before the client saw it. Preserving it costs no larger
/// index, no token positions, no stored bodies and no extra query — it is a
/// single tag on a hit that already exists.
///
/// `Body` is deliberately not called "found in the document text": the content
/// index also holds extracted metadata and tags, so the honest claim is that the
/// match came from inside the file rather than from its name.
#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The file name contains the query. The default, so a hit written by an
    /// older daemon — and every `name`-mode hit — reads as what it is.
    #[default]
    Name,
    /// Only the content/metadata index matched.
    Body,
    /// Both the name and the content matched.
    Both,
}

/// One search result, with metadata the daemon resolved so the GUI never stats.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Hit {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub score: f32,
    /// Which index this hit came through. Additive: a hit from an older daemon
    /// has no such field and defaults to [`MatchKind::Name`].
    #[serde(default)]
    pub match_kind: MatchKind,
    /// Size in bytes (0 = unknown).
    #[serde(default)]
    pub size: u64,
    /// Modification time, epoch seconds (0 = unknown).
    #[serde(default)]
    pub mtime: i64,
    /// Lowercase extension without the dot; empty for none.
    #[serde(default)]
    pub ext: String,
    /// Display name of the source/device the hit lives on (empty if none).
    #[serde(default)]
    pub source: String,
    /// Whether the file is reachable right now. `false` ⇒ it lives on an offline
    /// removable/network source (`source` says which); `size`/`mtime` are then 0.
    #[serde(default)]
    pub available: bool,
    /// Optional historical origin/provenance summary, omitted unless requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OriginSummary>,
}

/// Server-side result filter. `under`/`ext` are path-only; `mtime`/`size` are
/// inclusive `[min, max]` ranges.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Filter {
    #[serde(default)]
    pub under: Option<String>,
    #[serde(default)]
    pub ext: Vec<String>,
    #[serde(default)]
    pub mtime: Option<[i64; 2]>,
    #[serde(default)]
    pub size: Option<[u64; 2]>,
}

/// Page window for virtual scrolling.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Page {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_page_limit")]
    pub limit: usize,
}

fn default_page_limit() -> usize {
    50
}

impl Default for Page {
    fn default() -> Self {
        Page {
            offset: 0,
            limit: default_page_limit(),
        }
    }
}

/// One page of results plus scrollbar context.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QueryOutcome {
    pub hits: Vec<Hit>,
    /// Matches after filtering, capped at [`CANDIDATE_CEILING`]. Equal to that
    /// ceiling means the real number is unknown and at least this large.
    pub total: usize,
    /// Offset to request next, or `None` at the end.
    pub next_offset: Option<usize>,
    /// Approximate number of recently-changed files whose content is not yet
    /// (re-)extracted. `> 0` on a content/both query means results may be
    /// incomplete — the daemon has already been nudged to extract them, so a
    /// retry shortly will see them ("loading" hint for UIs).
    #[serde(default)]
    pub content_pending: u64,
}

/// A versioned, command-tagged request.
#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum RequestBody {
    Query(QueryRequest),
    Origin(OriginRequest),
    RegisterCopy(RegisterCopyRequest),
    RegisterMove(RegisterMoveRequest),
    RegisterCreated(RegisterCreatedRequest),
    RegisterDerived(RegisterDerivedRequest),
    Status,
    Pause,
    Resume,
    Rebuild,
    Reindex(ReindexRequest),
    /// Which directories this daemon catalogues. See [`IndexedRoot`].
    Sources,
    /// Forget every recorded file origin.
    ///
    /// Done by the daemon rather than by deleting the file: the daemon holds an
    /// open write-ahead connection to that database, and a client that unlinked
    /// the file would leave it writing into an inode nobody can read.
    ClearOrigin,
    /// The earlier versions kept for one document.
    Versions(PathRequest),
    /// Keep the state a document is in right now.
    ///
    /// What a file manager asks for just before putting an older version back,
    /// so changing one's mind about that is possible too.
    SaveVersion(PathRequest),
    /// Forget the versions of one document.
    ForgetVersions(PathRequest),
    /// Forget every version of everything.
    ClearHistory,
}

/// A request that names one file.
#[derive(Serialize, Deserialize, Debug)]
pub struct PathRequest {
    pub path: String,
}

/// One earlier version of a document.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileVersion {
    pub id: i64,
    /// When it was kept, in seconds since 1970.
    pub saved_at: i64,
    pub size: u64,
    /// `baseline`, `saved` or `before_restore`.
    pub reason: String,
    /// The file this version can be read from. Readable, never writable.
    pub object: String,
    /// The name the document had then, when it is not the name it has now.
    #[serde(default)]
    pub was_named: Option<String>,
}

/// Index one path again although nothing about its size or modification time
/// changed — the shape of a tag write, which lives in an extended attribute.
#[derive(Serialize, Deserialize, Debug)]
pub struct ReindexRequest {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct QueryRequest {
    pub q: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub page: Page,
    #[serde(default)]
    pub include_origin: bool,
}

fn default_mode() -> String {
    "name".to_string()
}

#[derive(Serialize, Deserialize, Debug)]
pub struct OriginRequest {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RegisterCopyRequest {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub app_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RegisterMoveRequest {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub app_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RegisterCreatedRequest {
    pub path: String,
    #[serde(default)]
    pub app_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RegisterDerivedRequest {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

/// A versioned reply; `kind` discriminates.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Query {
        v: u32,
        #[serde(flatten)]
        outcome: QueryOutcome,
    },
    Status {
        v: u32,
        indexed: u64,
        paused: bool,
        #[serde(default)]
        activity: String,
        #[serde(default)]
        capabilities: Vec<String>,
        #[serde(default)]
        pending_content: u64,
        #[serde(default)]
        content_docs: u64,
        #[serde(default)]
        name_index_bytes: u64,
        #[serde(default)]
        content_index_bytes: u64,
    },
    Origin {
        v: u32,
        origin: Option<OriginProvenance>,
    },
    Ok {
        v: u32,
    },
    Error {
        v: u32,
        message: String,
    },
    Sources {
        v: u32,
        roots: Vec<IndexedRoot>,
    },
    Versions {
        v: u32,
        versions: Vec<FileVersion>,
        /// Why this machine keeps no versions, when it keeps none: `disabled`,
        /// `no_reflink`, `old_kernel`, `live_session`. Empty when it does.
        #[serde(default)]
        unavailable: String,
        /// The disk got too full, so new versions are not being kept. A
        /// different sentence from "there are none".
        #[serde(default)]
        paused_for_space: bool,
    },
}

/// Why a machine keeps no earlier versions of documents, when it keeps none.
///
/// The type lives here, in the crate both sides already depend on, because each
/// value is a different sentence on three screens: the file manager's history
/// tab, the search settings, and the command line. As a bare string those three
/// all ended in a catch-all arm, so a reason added later showed up as "not kept
/// on this computer" everywhere and nobody noticed. A variant added here is a
/// compile error in all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The person turned it off.
    Disabled,
    /// The filesystem cannot share blocks: ext4, FAT, a network folder.
    NoReflink,
    /// The kernel refuses to share blocks across mount points, which is exactly
    /// what the service has to do — it sees the home folder and its own data
    /// directory as two mounts. Linux allows it from 5.18.
    OldKernel,
    /// A live session: nothing here survives the reboot anyway.
    LiveSession,
    /// A reason this build has never heard of, from a newer daemon. Not a
    /// catch-all for the screens to hide behind: it is the honest answer when an
    /// old program is talking to a new service.
    Unknown,
}

impl Unavailable {
    /// The word that travels on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoReflink => "no_reflink",
            Self::OldKernel => "old_kernel",
            Self::LiveSession => "live_session",
            Self::Unknown => "unknown",
        }
    }

    /// Read the word the daemon sent. An empty string means versions *are* kept.
    #[must_use]
    pub fn from_wire(word: &str) -> Option<Self> {
        match word {
            "" => None,
            "disabled" => Some(Self::Disabled),
            "no_reflink" => Some(Self::NoReflink),
            "old_kernel" => Some(Self::OldKernel),
            "live_session" => Some(Self::LiveSession),
            _ => Some(Self::Unknown),
        }
    }
}

/// What the daemon knows about one document's earlier versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionsAnswer {
    pub versions: Vec<FileVersion>,
    /// Why nothing is kept on this machine. `None` when versions are kept.
    pub unavailable: Option<Unavailable>,
    pub paused_for_space: bool,
}

/// One directory the daemon catalogues, as a caller standing in a folder needs
/// to hear it.
///
/// This exists so a file manager can tell "the index has nothing matching here"
/// apart from "the index has never looked here" — two states that produce the
/// same empty reply and mean opposite things to the person standing in the
/// folder. Without it a search surface either stays silent about which search it
/// ran, or guesses.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IndexedRoot {
    /// Absolute path of the catalogued directory. Everything below it is
    /// covered, minus whatever the daemon's ignore rules skip.
    pub path: String,
    /// Display name for the source/device, for a surface that names it.
    #[serde(default)]
    pub name: String,
    /// Whether the text inside files under this root is indexed too. `false`
    /// means only names are, so a search for words inside a document has to
    /// read the files instead of asking here.
    #[serde(default)]
    pub content: bool,
    /// Whether the directory is mounted and readable right now. A retained
    /// catalogue for an unplugged disk answers queries but the files cannot be
    /// opened.
    #[serde(default)]
    pub available: bool,
}

/// Index daemon status: catalog size plus current background activity.
#[derive(Clone, Debug)]
pub struct Status {
    pub indexed: u64,
    pub paused: bool,
    pub activity: String,
    pub capabilities: Vec<String>,
    /// Recently-changed files whose content extraction is still deferred.
    pub pending_content: u64,
    /// Live document count in the content index.
    pub content_docs: u64,
    /// On-disk size of the name index directory, in bytes.
    pub name_index_bytes: u64,
    /// On-disk size of the content index directory, in bytes.
    pub content_index_bytes: u64,
}

/// Errors talking to the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon listening / socket I/O failure.
    Io(io::Error),
    /// Malformed JSON on the wire.
    Decode(serde_json::Error),
    /// The daemon returned an error reply.
    Daemon(String),
    /// The daemon answered with an unexpected reply kind, or a version mismatch.
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io: {e}"),
            ClientError::Decode(e) => write!(f, "decode: {e}"),
            ClientError::Daemon(m) => write!(f, "daemon error: {m}"),
            ClientError::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}
impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Decode(e)
    }
}

/// How long a request may take before the caller gets an error instead of a
/// thread that never returns.
///
/// There was no deadline at all: a daemon that accepted the connection and then
/// stopped answering — a wedged commit, a stopped process still holding the
/// socket — blocked the calling thread forever, and `PersistentClient`'s single
/// retry only fires on an I/O error, which a silent hang never produces.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// The largest reply this client will assemble.
///
/// `read_line` grew a `String` with no ceiling, so a socket that streams bytes
/// and never sends a newline could take the process out on memory alone. The
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
/// `None` when there is no `XDG_RUNTIME_DIR` — a session started by cron, over
/// ssh, or by a misconfigured display manager. There used to be a `/tmp`
/// fallback, and `/tmp` is world-writable: the first local account to create
/// `/tmp/biglinux` owns it and can bind its own socket there. This client does
/// not check who owns the socket it connects to, so the search window would
/// have rendered another account's replies as trusted results — including the
/// paths the person then clicks and opens.
///
/// Refusing costs a session with no runtime directory its search. That session
/// has no private directory to put a socket in either, so there is nothing to
/// refuse it *for*.
#[must_use]
pub fn default_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|runtime| runtime.join("biglinux").join("indexd.sock"))
}

/// A thin handle to the index daemon. Each call is one short-lived connection — the
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

    /// Keep the state a document is in right now.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the daemon is absent or refuses.
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

    /// Forget every version of everything.
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

    /// Request a full rebuild of the index from source files.
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
    fn request_serializes_to_the_wire_shape() {
        let request = Request {
            v: PROTOCOL_VERSION,
            body: RequestBody::Query(QueryRequest {
                q: "rel".into(),
                mode: "name".into(),
                filter: Filter {
                    ext: vec!["pdf".into()],
                    ..Default::default()
                },
                page: Page {
                    offset: 0,
                    limit: 10,
                },
                include_origin: false,
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"v\":2"));
        assert!(json.contains("\"cmd\":\"query\""));
        assert!(json.contains("\"ext\":[\"pdf\"]"));
    }

    #[test]
    fn reply_roundtrips() {
        let wire = r#"{"kind":"query","v":2,"hits":[{"path":"/a/b.txt","name":"b.txt","score":1.0,"size":5,"mtime":100,"ext":"txt"}],"total":1,"next_offset":null}"#;
        match serde_json::from_str::<Reply>(wire).unwrap() {
            Reply::Query { outcome, .. } => {
                assert_eq!(outcome.total, 1);
                assert_eq!(outcome.hits[0].size, 5);
                assert_eq!(outcome.hits[0].ext, "txt");
                assert_eq!(outcome.content_pending, 0); // additive field defaults
            }
            other => panic!("wrong kind: {other:?}"),
        }

        let status = r#"{"kind":"status","v":2,"indexed":42,"paused":true}"#;
        match serde_json::from_str::<Reply>(status).unwrap() {
            Reply::Status {
                indexed,
                paused,
                activity,
                content_docs,
                name_index_bytes,
                content_index_bytes,
                pending_content,
                ..
            } => {
                assert_eq!(indexed, 42);
                assert!(paused);
                assert!(activity.is_empty());
                assert_eq!(pending_content, 0);
                assert_eq!(content_docs, 0);
                assert_eq!(name_index_bytes, 0);
                assert_eq!(content_index_bytes, 0);
            }
            other => panic!("wrong kind: {other:?}"),
        }

        let status = r#"{"kind":"status","v":2,"indexed":42,"paused":false,"activity":"idle"}"#;
        match serde_json::from_str::<Reply>(status).unwrap() {
            Reply::Status { activity, .. } => assert_eq!(activity, "idle"),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    /// The `sources` command and its reply keep the spelling both ends use.
    ///
    /// A root written by a daemon that only knows the path still reads, and it
    /// reads as "not content-indexed, not available" — the cautious pair. The
    /// opposite defaults would have a file manager promise a search it cannot
    /// run and offer files it cannot open.
    #[test]
    fn sources_request_and_reply_keep_their_wire_spelling() {
        let request = Request {
            v: PROTOCOL_VERSION,
            body: RequestBody::Sources,
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":2,"cmd":"sources"}"#
        );

        let wire = r#"{"kind":"sources","v":2,"roots":[{"path":"/home/p","name":"Home","content":true,"available":true},{"path":"/mnt/backup"}]}"#;
        match serde_json::from_str::<Reply>(wire).unwrap() {
            Reply::Sources { roots, .. } => {
                assert_eq!(
                    roots,
                    [
                        IndexedRoot {
                            path: "/home/p".into(),
                            name: "Home".into(),
                            content: true,
                            available: true,
                        },
                        IndexedRoot {
                            path: "/mnt/backup".into(),
                            name: String::new(),
                            content: false,
                            available: false,
                        },
                    ]
                );
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    /// A hit written before `match_kind` existed still reads, and it reads as a
    /// name match rather than as an unexplained one.
    ///
    /// This is what makes the field free to add: the GUI shows an explanatory
    /// marker only for `Body`, so an old daemon's hits carry no marker at all —
    /// which is exactly right, because a `name`-mode query is all they can be.
    #[test]
    fn match_kind_is_additive_and_defaults_to_name() {
        let old_hit =
            r#"{"path":"/a/b.txt","name":"b.txt","score":1.0,"size":5,"mtime":100,"ext":"txt"}"#;
        assert_eq!(
            serde_json::from_str::<Hit>(old_hit).unwrap().match_kind,
            MatchKind::Name
        );

        for (wire, expected) in [
            ("name", MatchKind::Name),
            ("body", MatchKind::Body),
            ("both", MatchKind::Both),
        ] {
            let hit = format!(r#"{{"path":"/a/b.txt","name":"b.txt","match_kind":"{wire}"}}"#);
            assert_eq!(
                serde_json::from_str::<Hit>(&hit).unwrap().match_kind,
                expected
            );
        }

        // And it goes back out in the same words, so the daemon and the GUI
        // cannot disagree about the spelling.
        let hit = Hit {
            match_kind: MatchKind::Both,
            ..Hit::default()
        };
        assert!(
            serde_json::to_string(&hit)
                .unwrap()
                .contains(r#""match_kind":"both""#)
        );
    }

    #[test]
    fn origin_fields_are_additive() {
        let old_hit =
            r#"{"path":"/a/b.txt","name":"b.txt","score":1.0,"size":5,"mtime":100,"ext":"txt"}"#;
        let hit: Hit = serde_json::from_str(old_hit).unwrap();
        assert!(hit.origin.is_none());

        let new_hit = r#"{"path":"/a/b.txt","name":"b.txt","origin":{"kind":"app_created","confidence":3,"label":"Criado pelo Editor de Textos","first_seen_at":100}}"#;
        let hit: Hit = serde_json::from_str(new_hit).unwrap();
        assert_eq!(hit.origin.unwrap().kind, "app_created");

        let request: QueryRequest = serde_json::from_str(r#"{"q":"b"}"#).unwrap();
        assert!(!request.include_origin);

        let status = r#"{"kind":"status","v":2,"indexed":42,"paused":false}"#;
        match serde_json::from_str::<Reply>(status).unwrap() {
            Reply::Status {
                capabilities,
                content_docs,
                name_index_bytes,
                content_index_bytes,
                ..
            } => {
                assert!(capabilities.is_empty());
                assert_eq!(content_docs, 0);
                assert_eq!(name_index_bytes, 0);
                assert_eq!(content_index_bytes, 0);
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn origin_commands_serialize_to_wire_shape() {
        let request = Request {
            v: PROTOCOL_VERSION,
            body: RequestBody::RegisterCopy(RegisterCopyRequest {
                src: "/from".into(),
                dst: "/to".into(),
                app_id: Some("big-filemanager".into()),
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"v\":2"));
        assert!(json.contains("\"cmd\":\"register_copy\""));
        assert!(json.contains("\"app_id\":\"big-filemanager\""));
    }

    #[test]
    fn persistent_client_reuses_one_connection_for_two_requests() {
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

        let server_thread = thread::spawn(move || -> io::Result<()> {
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
