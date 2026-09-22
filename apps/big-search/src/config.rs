//! XDG paths and default scan roots.
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Data directory (`$XDG_DATA_HOME/big-search`): holds the index, cache, etc.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    base.join("big-search")
}

/// Create the data directory owner-only, and tighten it if an earlier version
/// left it group/world-readable.
///
/// Everything under it describes the user's private files: `state.sqlite` is a
/// complete list of every path in their home, and the content index holds the
/// extracted *text* of their documents. SQLite and the walker create their files
/// through the process umask, which on this system is `0022` — so `state.sqlite`
/// and `content-signatures.json` were mode 0644 inside a 0755 directory, and any
/// other local account could read them. The Tantivy `index/` directory was
/// already 0700, which is why only it escaped.
///
/// Restricting the directory covers every file inside it, present and future,
/// without having to remember a mode at each creation site.
pub fn ensure_private_data_dir() -> std::io::Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let mode = std::fs::metadata(&dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        log::info!(
            "tightened {} from {mode:03o} to 700 (it lists your private files)",
            dir.display()
        );
    }
    Ok(())
}

/// On-disk Tantivy index directory.
pub fn index_dir() -> PathBuf {
    data_dir().join("index")
}

/// On-disk Tantivy content index directory.
pub fn content_index_dir() -> PathBuf {
    data_dir().join("content-index")
}

/// Dedup/reconcile cache file. New installs store it in `state.sqlite` under the
/// `name` namespace; the JSON path remains the migration source for older caches.
pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

/// Cache of files whose content/metadata extraction was already attempted. New
/// installs store it in `state.sqlite` under the `content` namespace.
pub fn content_state_path() -> PathBuf {
    data_dir().join("content-state.json")
}

/// Cache of content fingerprints already extracted or intentionally skipped.
pub fn content_signature_path() -> PathBuf {
    data_dir().join("content-signatures.json")
}

/// Origin/provenance store. This is intentionally separate from Tantivy and
/// `state.json` so normal search does not depend on it.
pub fn origin_db_path() -> PathBuf {
    data_dir().join("origin.sqlite")
}

/// Earlier versions of the person's documents. Separate from everything else
/// for the same reason as the origin store, and one more: this is durable user
/// data, not a cache the service can rebuild.
pub fn history_db_path() -> PathBuf {
    data_dir().join("history.sqlite")
}

/// Where the cloned versions themselves live.
pub fn history_dir() -> PathBuf {
    data_dir().join("history")
}

/// The effective ignore file (built-in defaults + user overrides), generated.
pub fn effective_ignore() -> PathBuf {
    data_dir().join("effective.ignore")
}

/// User-editable ignore file (`$XDG_CONFIG_HOME/big-search/ignore`, gitignore syntax).
pub fn user_ignore() -> PathBuf {
    config_file().with_file_name("ignore")
}

/// Structured config (`$XDG_CONFIG_HOME/big-search/config.toml`): sources + policy.
///
/// The crate that owns the file's shape owns where it lives too. This file used
/// to resolve the path itself and the two disagreed: with `XDG_CONFIG_HOME` set
/// but empty, one looked at a relative `big-search/config.toml` and the other at
/// `~/.config/big-search/config.toml`.
pub fn config_file() -> PathBuf {
    big_search_config::path()
}

/// Query socket. Lives under the shared `biglinux/` runtime subdir as `indexd.sock`
/// to honour the `big-indexd` microdaemon contract (clients find it by that path).
pub fn socket_path() -> Option<PathBuf> {
    Some(runtime_subdir()?.join("indexd.sock"))
}

/// Single-instance daemon lock (`…/biglinux/indexd.lock`). Held for the daemon's
/// lifetime so a second `daemon` invocation refuses instead of stealing the socket
/// and colliding on the Tantivy writer lock.
pub fn lock_path() -> Option<PathBuf> {
    Some(runtime_subdir()?.join("indexd.lock"))
}

/// The `biglinux/` runtime subdir holding the socket + lock.
pub fn runtime_subdir() -> Option<PathBuf> {
    Some(runtime_dir()?.join("biglinux"))
}

/// The session's private runtime directory, or nothing.
///
/// No `/tmp` fallback. The socket used to land in `/tmp/biglinux` without
/// `XDG_RUNTIME_DIR`, and `big_indexd_client::default_socket` has no fallback at
/// all — so the service was listening where no program looks, in a directory any
/// local account can create first. Refusing is the honest failure.
fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
}
