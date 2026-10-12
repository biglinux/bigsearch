//! Client and wire types for the big-indexd (big-search) Unix-socket protocol.
//!
//! This crate is tiny (`serde` and `std` only, no Tantivy), so GUI
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
use std::io;

mod client;

pub use client::{Client, PersistentClient, default_socket};

/// Wire protocol version understood by this client.
pub const PROTOCOL_VERSION: u32 = 2;

/// How many candidates the daemon considers before filtering and paging.
///
/// Part of the wire contract rather than an implementation detail of the daemon,
/// because [`QueryOutcome::total`] is capped at it, so a caller cannot tell
/// an exact count from a floor without knowing this number. A total below it is
/// how many matched, within what [`QueryOutcome::total`] says about paged
/// counts; a total equal to it means "this many, and possibly far more", and a
/// surface that prints it as an exact figure is stating something nobody
/// established.
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
/// The daemon learns this while it merges the two searches in `search_both`.
/// Carrying it costs a single tag on a hit that already exists: no larger
/// index, no token positions, no stored bodies and no extra query.
///
/// `Body` does not mean "found in the document text": the content index also
/// holds extracted metadata and tags, so the claim is that the match came from
/// inside the file rather than from its name.
#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The file name contains the query. The default, so a hit written by an
    /// older daemon, and every `name`-mode hit, reads as what it is.
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
    /// Media type (`video/mp4`), from the name or, when the name cannot tell,
    /// the first bytes; empty when unknown or from an older daemon.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
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

/// Server-side result filter. `under`/`ext` are path-only; `mime` takes exact
/// types or `major/*` groups (daemons with the `mime-filter` capability);
/// `mtime`/`size` are inclusive `[min, max]` ranges.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Filter {
    #[serde(default)]
    pub under: Option<String>,
    #[serde(default)]
    pub ext: Vec<String>,
    #[serde(default)]
    pub mime: Vec<String>,
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
    ///
    /// In the `name` and `content` modes without date or size filters the
    /// daemon reads only the candidates a page needs and counts the rest from
    /// the index, so a total can run over what paging reaches: the last
    /// `next_offset` may return fewer hits than asked for, or none. Files
    /// deleted since they were indexed have the same effect in every mode.
    pub total: usize,
    /// Offset to request next, or `None` at the end.
    pub next_offset: Option<usize>,
    /// Approximate number of recently-changed files whose content is not yet
    /// (re-)extracted. `> 0` on a content/both query means results may be
    /// incomplete; the daemon has already been nudged to extract them, so a
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
/// changed: the shape of a tag write, which lives in an extended attribute.
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
        #[serde(default)]
        unwatched_dirs: u64,
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
    /// what the service has to do: it sees the home folder and its own data
    /// directory as two mounts. Linux allows it from 5.18.
    OldKernel,
    /// A live session: nothing here survives the reboot anyway.
    LiveSession,
    /// A reason this build has never heard of, from a newer daemon. Not a
    /// catch-all for the screens to hide behind: it is the answer when an old
    /// program is talking to a new service.
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
/// apart from "the index has never looked here": two states that produce the
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
    /// Folders the daemon could not watch, usually for the inotify watch
    /// limit: changes in them are seen only by the next full reconcile. Zero
    /// from a daemon that does not report it.
    pub unwatched_dirs: u64,
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
    /// The daemon answered with an unexpected reply kind. A daemon that refuses
    /// the request's protocol version answers with [`ClientError::Daemon`].
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
    /// reads as "not content-indexed, not available", the cautious pair. The
    /// opposite defaults would have a file manager promise a search it cannot
    /// run and offer files it cannot open.
    #[test]
    fn sources_wire_spelling_round_trips() {
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
    /// marker only for `Body`, so an old daemon's hits carry no marker at all,
    /// which is right because a `name`-mode query is all they can be.
    #[test]
    fn match_kind_defaults_to_name() {
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
}
