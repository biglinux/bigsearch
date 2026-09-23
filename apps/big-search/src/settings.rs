//! Structured per-user configuration: the directories ("sources") to index and,
//! per source, whether to extract content / metadata. A source may be a removable
//! or network mount; each carries its filesystem identity (UUID + label) so that
//! when the device is unplugged its catalogue is kept and a hit is still reported
//! as living on that device (offline). Config file:
//! `$XDG_CONFIG_HOME/big-search/config.toml`.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use big_search_config::{ContentIndexMode, SymlinkPolicy};
/// The configuration file's own shape lives in `big-search-config`, which the
/// desktop's settings window writes and this service reads. Named here the way
/// this file has always named them, so the code below reads unchanged.
use big_search_config::{Defaults, SearchConfig as ConfigFile, SourceEntry as SourceConfiguration};

impl From<SymlinkPolicy> for crate::scan_roots::SymlinkPolicy {
    fn from(policy: SymlinkPolicy) -> Self {
        match policy {
            SymlinkPolicy::PersonalLocal => Self::PersonalLocal,
            SymlinkPolicy::LocalExact => Self::LocalExact,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedContentIndexMode {
    Basic,
    Freqs,
}

impl ResolvedContentIndexMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Freqs => "freqs",
        }
    }
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
pub const LOW_MEMORY_THRESHOLD_BYTES: u64 = 6 * GIB;

/// A resolved indexing source: a directory, its extraction policy, and the
/// device identity used to recognise it across (un)mounts.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    /// Stable id: the filesystem UUID when known, else the canonical path.
    pub id: String,
    /// Display / device name (config `name` › fs label › path basename).
    pub name: String,
    /// Four source booleans packed into a single byte. This keeps cloned source
    /// descriptors smaller and makes policy checks a bit-mask read instead of four
    /// separately-addressed bool fields.
    pub flags: SourceFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFlags(u8);

impl SourceFlags {
    const CONTENT: u8 = 1 << 0;
    const METADATA: u8 = 1 << 1;
    const REMOVABLE: u8 = 1 << 2;
    const NETWORK: u8 = 1 << 3;

    fn new(content: bool, metadata: bool, removable: bool, network: bool) -> Self {
        let mut bits = 0u8;
        if content {
            bits |= Self::CONTENT;
        }
        if metadata {
            bits |= Self::METADATA;
        }
        if removable {
            bits |= Self::REMOVABLE;
        }
        if network {
            bits |= Self::NETWORK;
        }
        Self(bits)
    }

    pub fn content(self) -> bool {
        self.0 & Self::CONTENT != 0
    }

    pub fn metadata(self) -> bool {
        self.0 & Self::METADATA != 0
    }

    pub fn removable(self) -> bool {
        self.0 & Self::REMOVABLE != 0
    }

    pub fn network(self) -> bool {
        self.0 & Self::NETWORK != 0
    }

    pub fn retained_when_unavailable(self) -> bool {
        self.removable() || self.network()
    }
}

impl Source {
    pub fn content(&self) -> bool {
        self.flags.content()
    }

    pub fn metadata(&self) -> bool {
        self.flags.metadata()
    }

    pub fn removable(&self) -> bool {
        self.flags.removable()
    }

    pub fn network(&self) -> bool {
        self.flags.network()
    }

    pub fn retained_when_unavailable(&self) -> bool {
        self.flags.retained_when_unavailable()
    }
}

/// The resolved source set, most-specific (longest path) first.
pub struct Sources {
    list: Vec<Source>,
}

impl Sources {
    /// Load config (seeding a documented template on first run); fall back to the
    /// home root — symlink-expanded when enabled — if no sources are configured.
    pub fn load() -> Self {
        seed_config_template();
        let config_file = read_config();
        let default_follow = resolve_follow(&config_file.defaults);
        let raw = if config_file.sources.is_empty() {
            default_sources()
        } else {
            config_file.sources
        };

        let mut list: Vec<Source> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for source in raw
            .into_iter()
            .flat_map(|s| expand_source_configuration(s, &config_file.defaults, default_follow))
            .map(|s| resolve(s, &config_file.defaults))
        {
            if seen.insert(source.path.clone()) {
                list.push(source);
            }
        }
        list.sort_by_key(|s| std::cmp::Reverse(s.path.as_os_str().len()));
        Self { list }
    }

    /// One scan root per source.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.list.iter().map(|s| s.path.clone()).collect()
    }

    pub fn all(&self) -> &[Source] {
        &self.list
    }

    /// The source owning `path` (longest matching prefix).
    pub fn owner(&self, path: &Path) -> Option<&Source> {
        self.list.iter().find(|s| path.starts_with(&s.path))
    }
}

/// A source is available when its directory exists and is non-empty. An unplugged
/// removable / unmounted network share reads as empty or gone → its catalogue is
/// kept but not re-scanned.
pub fn is_available(source: &Source) -> bool {
    std::fs::read_dir(&source.path)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Process-wide resolved sources (loaded once: config + device probe).
pub fn sources() -> &'static Sources {
    static SOURCES: OnceLock<Sources> = OnceLock::new();
    SOURCES.get_or_init(Sources::load)
}

/// Extraction policy `(content, metadata)` for the source owning `path`; both
/// default to on for a path under no configured source.
pub fn policy_for(path: &Path) -> (bool, bool) {
    if names_only() {
        return (false, false);
    }
    let (content, metadata) = sources()
        .owner(path)
        .map(|s| (s.content(), s.metadata()))
        .unwrap_or((true, true));
    (
        env_bool("BIG_SEARCH_CONTENT").unwrap_or(content),
        env_bool("BIG_SEARCH_META").unwrap_or(metadata),
    )
}

/// True when the global profile indexes filenames only and skips the content sidecar.
pub fn names_only() -> bool {
    env_bool("BIG_SEARCH_NAMES_ONLY").unwrap_or_else(|| read_config().defaults.names_only)
}

pub fn content_index_enabled() -> bool {
    !names_only()
}

/// Requested content posting detail. `BIG_SEARCH_CONTENT_INDEX_MODE` overrides config.
pub fn content_index_mode() -> ContentIndexMode {
    std::env::var("BIG_SEARCH_CONTENT_INDEX_MODE")
        .ok()
        .and_then(|value| ContentIndexMode::parse(&value))
        .unwrap_or_else(|| read_config().defaults.content_index_mode)
}

/// Actual posting detail after resolving `auto` against observed memory capacity.
pub fn resolved_content_index_mode() -> ResolvedContentIndexMode {
    match content_index_mode() {
        ContentIndexMode::Basic => ResolvedContentIndexMode::Basic,
        ContentIndexMode::Freqs => ResolvedContentIndexMode::Freqs,
        ContentIndexMode::Auto => {
            if low_memory_machine() {
                ResolvedContentIndexMode::Basic
            } else {
                ResolvedContentIndexMode::Freqs
            }
        }
    }
}

pub fn low_memory_machine() -> bool {
    low_memory_capacity(effective_memory_bytes())
}

fn low_memory_capacity(bytes: Option<u64>) -> bool {
    bytes.is_none_or(|bytes| bytes < LOW_MEMORY_THRESHOLD_BYTES)
}

/// Physical memory for presentation, not a budget for the indexer.
pub fn total_memory_bytes() -> Option<u64> {
    static TOTAL: OnceLock<Option<u64>> = OnceLock::new();
    *TOTAL.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        mem_total_bytes(&text)
    })
}

/// Memory this service may size itself against: physical RAM, or less when
/// its own cgroup or one above it sets `memory.high` or `memory.max`. `None`
/// when the cgroup cannot be read, so callers keep their conservative defaults.
pub fn effective_memory_bytes() -> Option<u64> {
    static CAPACITY: OnceLock<Option<u64>> = OnceLock::new();
    *CAPACITY.get_or_init(|| {
        let mut dir = crate::throttle::own_cgroup_dir()?;
        std::fs::metadata(dir.join("cgroup.controllers")).ok()?;
        let mut limit = total_memory_bytes();
        loop {
            for control in ["memory.high", "memory.max"] {
                let text = std::fs::read_to_string(dir.join(control)).unwrap_or_default();
                if let Some(bytes) = cgroup_limit_bytes(&text) {
                    limit = Some(limit.map_or(bytes, |known| known.min(bytes)));
                }
            }
            if dir == Path::new(crate::throttle::CGROUP_ROOT) || !dir.pop() {
                break;
            }
        }
        limit
    })
}

/// `MemTotal` from `/proc/meminfo`, in bytes. `None` when missing, zero,
/// repeated or not in kB.
fn mem_total_bytes(meminfo: &str) -> Option<u64> {
    let mut values = meminfo
        .lines()
        .filter_map(|line| line.strip_prefix("MemTotal:"));
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let mut fields = value.split_whitespace();
    let kib: u64 = fields.next()?.parse().ok()?;
    (fields.next() == Some("kB") && fields.next().is_none() && kib > 0)
        .then(|| kib.checked_mul(1024))
        .flatten()
}

/// A finite cgroup memory control in bytes; `max` (unlimited) and anything
/// unreadable are `None`.
fn cgroup_limit_bytes(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Adaptive per-file extracted text cap. This replaced the old fixed 1 MiB cap.
pub fn extract_max_bytes() -> usize {
    let configured_mb = env_u64("BIG_SEARCH_EXTRACT_MAX_MB")
        .unwrap_or_else(|| read_config().defaults.extract_max_mb);
    let mb = if configured_mb > 0 {
        configured_mb
    } else {
        adaptive_extract_max_mb()
    };
    mb.clamp(1, 128).saturating_mul(MIB) as usize
}

pub fn extract_max_mb_label() -> String {
    let configured_mb = env_u64("BIG_SEARCH_EXTRACT_MAX_MB")
        .unwrap_or_else(|| read_config().defaults.extract_max_mb);
    if configured_mb > 0 {
        format!("{configured_mb} MiB")
    } else {
        format!("auto ({} MiB)", adaptive_extract_max_mb())
    }
}

fn adaptive_extract_max_mb() -> u64 {
    adaptive_extract_max_mb_for(effective_memory_bytes())
}

fn adaptive_extract_max_mb_for(capacity: Option<u64>) -> u64 {
    match capacity {
        Some(bytes) if bytes < 3 * GIB => 4,
        Some(bytes) if bytes < 6 * GIB => 8,
        Some(bytes) if bytes < 12 * GIB => 16,
        Some(_) => 32,
        None => 8,
    }
}

pub fn text_max_input_bytes() -> u64 {
    env_u64("BIG_SEARCH_TEXT_MAX_MB")
        .unwrap_or_else(|| read_config().defaults.text_max_mb)
        .saturating_mul(MIB)
}

pub fn office_max_input_bytes() -> u64 {
    let configured_mb = env_u64("BIG_SEARCH_CONTENT_MAX_MB")
        .or_else(|| env_u64("BIG_SEARCH_OFFICE_MAX_MB"))
        .unwrap_or_else(|| read_config().defaults.office_max_mb);
    let mb = if configured_mb > 0 {
        configured_mb
    } else if low_memory_machine() {
        64
    } else {
        256
    };
    mb.saturating_mul(MIB)
}

pub fn pdf_max_input_bytes() -> u64 {
    env_u64("BIG_SEARCH_PDF_MAX_MB")
        .unwrap_or_else(|| read_config().defaults.pdf_max_mb)
        .saturating_mul(MIB)
}

pub fn pdf_timeout_secs() -> u64 {
    env_u64("BIG_SEARCH_PDF_TIMEOUT_SECS")
        .unwrap_or_else(|| read_config().defaults.pdf_timeout_secs)
        .max(1)
}

/// Address-space cap for one `pdftotext` run. Poppler on a pathological PDF
/// (huge OCR scans) can balloon to hundreds of MB; under the daemon's cgroup
/// that becomes swap pressure. Past the cap the subprocess fails and the file
/// is indexed by name only.
pub fn pdf_memory_limit_bytes() -> u64 {
    let cap_mib: u64 = if low_memory_machine() { 384 } else { 768 };
    cap_mib.saturating_mul(MIB)
}

/// Quiet period before a changed file's content is (re-)extracted. Bounds the
/// re-read cost of actively-edited documents; a content search bypasses it.
pub fn content_cooldown() -> std::time::Duration {
    let secs = env_u64("BIG_SEARCH_CONTENT_COOLDOWN_SECS")
        .unwrap_or_else(|| read_config().defaults.content_cooldown_secs);
    std::time::Duration::from_secs(secs)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
}

/// Display name of the source owning `path` (empty if none) — stored on the doc
/// so a hit can report which device it lives on, even while offline.
pub fn source_name(path: &Path) -> String {
    sources()
        .owner(path)
        .map(|s| s.name.clone())
        .unwrap_or_default()
}

fn read_config() -> ConfigFile {
    static CONFIG: OnceLock<ConfigFile> = OnceLock::new();
    CONFIG.get_or_init(read_config_uncached).clone()
}

/// Read the file through the crate that owns its shape.
///
/// Not a second parser here: the settings window writes this file through
/// `big_search_config`, and a service resolving the path its own way is how the
/// two ended up disagreeing about where the file even is.
fn read_config_uncached() -> ConfigFile {
    let Ok(text) = std::fs::read_to_string(big_search_config::path()) else {
        return ConfigFile::default();
    };
    big_search_config::parse(&text).unwrap_or_else(|error| {
        // The only place a typo in a hand-edited file is ever mentioned.
        log::warn!("config.toml: {error}; using defaults");
        ConfigFile::default()
    })
}

/// `BIG_SEARCH_FOLLOW_LINKS` env wins if set, else the config default.
fn resolve_follow(defaults: &Defaults) -> bool {
    env_bool("BIG_SEARCH_FOLLOW_LINKS").unwrap_or(defaults.follow_symlinks)
}

fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(name).ok().as_deref() {
        Some("1" | "true" | "yes" | "on") => Some(true),
        Some("0" | "false" | "no" | "off") => Some(false),
        Some(_) | None => None,
    }
}

/// Default source set when config lists none: the home root, symlink-expanded
/// into extra real-path roots when `follow` is on.
fn default_sources() -> Vec<SourceConfiguration> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    vec![SourceConfiguration {
        path: home,
        name: None,
        content: None,
        metadata: None,
        follow_symlinks: None,
        symlink_policy: None,
        removable: false,
        network: false,
    }]
}

fn expand_source_configuration(
    source_configuration: SourceConfiguration,
    defaults: &Defaults,
    default_follow: bool,
) -> Vec<SourceConfiguration> {
    let follow = source_configuration
        .follow_symlinks
        .unwrap_or(default_follow);
    let policy = source_configuration
        .symlink_policy
        .unwrap_or(defaults.symlink_policy);
    crate::scan_roots::resolve_roots_with_policy(
        std::slice::from_ref(&source_configuration.path),
        follow,
        policy.into(),
    )
    .into_iter()
    .map(|path| SourceConfiguration {
        path,
        name: source_configuration.name.clone(),
        content: source_configuration.content,
        metadata: source_configuration.metadata,
        follow_symlinks: Some(false),
        symlink_policy: Some(policy),
        removable: source_configuration.removable,
        network: source_configuration.network,
    })
    .collect()
}

fn resolve(source_configuration: SourceConfiguration, defaults: &Defaults) -> Source {
    let path =
        std::fs::canonicalize(&source_configuration.path).unwrap_or(source_configuration.path);
    let (uuid, label) = device_uuid_label(&path);
    let id = uuid.unwrap_or_else(|| path.to_string_lossy().into_owned());
    let name = source_configuration.name.or(label).unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    });
    Source {
        path,
        id,
        name,
        flags: SourceFlags::new(
            source_configuration.content.unwrap_or(defaults.content),
            source_configuration.metadata.unwrap_or(defaults.metadata),
            source_configuration.removable,
            source_configuration.network,
        ),
    }
}

/// Filesystem UUID + label for the mount backing `path`. `None` for either
/// when unknown.
fn device_uuid_label(path: &Path) -> (Option<String>, Option<String>) {
    crate::mounts::mount_for(path).map_or((None, None), |mount| {
        crate::mounts::uuid_label(&mount.source)
    })
}

/// Documented `config.toml` template, seeded once (never overwrites edits).
const CONFIG_TEMPLATE: &str = "\
# big-search configuration. Re-read on the next reindex / daemon start.
# big-search never overwrites this file.

[defaults]
# Quick profiles:
#   big-search config preset names-only   # filenames only, smallest index
#   big-search config preset low-memory   # content on, doc-id-only postings
#   big-search config preset balanced     # automatic defaults
#   big-search config preset complete     # larger content cap + frequency ranking
content = true          # extract file content (text/PDF/office) by default
metadata = false        # extract metadata (type, audio/video duration, image size)
names_only = false      # true = only filenames; disables content/metadata sidecar
content_index_mode = \"auto\" # auto | basic | freqs. auto: basic below 6 GiB RAM
extract_max_mb = 0      # extracted text per file. 0 = adaptive (4/8/16/32 MiB)
text_max_mb = 0         # plain text input cap. 0 = no input-size skip; output is capped
office_max_mb = 0       # Office input cap. 0 = adaptive (64 MiB low RAM, else 256 MiB)
pdf_max_mb = 0          # PDF input cap. 0 = no input-size skip; timeout/output cap apply
pdf_timeout_secs = 5    # wall-clock cap per PDF extraction
content_cooldown_secs = 900 # wait this long after a change before re-reading a file's
                        # content (files being edited are not re-read per save; a
                        # content search skips the wait). 0 = extract immediately
follow_symlinks = false # also index top-level symlinked dirs of the home root
symlink_policy = \"personal-local\" # personal-local | local-exact

# Sources = the directories to index. With NONE listed, big-search indexes your
# home (plus, when follow_symlinks is on, safe personal-local targets of its
# top-level symlinked dirs).
#
# Each [[source]] can override content/metadata, and be marked removable or
# network: those keep their catalogue when unplugged/unmounted, and a hit on an
# offline source is reported as living on that device.
#
# Examples — uncomment / adapt:
#
# [[source]]
# path = \"/home/me\"
# name = \"Home\"
#
# [[source]]
# path = \"/mnt/OldHome\"
# name = \"Old Home\"
# content = false        # index by name only (still findable), skip content
#
# [[source]]
# path = \"/home/me/WindowsDiskLink\"
# name = \"Windows personal files\"
# follow_symlinks = true
# symlink_policy = \"personal-local\" # indexes Users/* personal dirs, not Windows/Program Files
#
# [[source]]
# path = \"/run/media/me/BACKUP\"
# name = \"Backup USB\"
# removable = true       # kept in the catalogue when unplugged
# content = false
#
# [[source]]
# path = \"/mnt/nas/photos\"
# name = \"NAS Photos\"
# network = true
";

pub fn seed_config_template() {
    let path = crate::config::config_file();
    if path.exists() {
        return;
    }
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::write(&path, CONFIG_TEMPLATE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_capacity_controls_extraction_and_low_memory_defaults() {
        // A large host running a 768 MiB indexer must use the small policy.
        let capacity = [64 * GIB, 768 * MIB, GIB].into_iter().min();
        assert!(low_memory_capacity(capacity));
        assert_eq!(adaptive_extract_max_mb_for(capacity), 4);
        assert_eq!(adaptive_extract_max_mb_for(Some(4 * GIB)), 8);
        assert_eq!(adaptive_extract_max_mb_for(Some(8 * GIB)), 16);
        assert_eq!(adaptive_extract_max_mb_for(Some(64 * GIB)), 32);
    }

    #[test]
    fn memory_readings_reject_what_they_cannot_trust() {
        assert_eq!(
            mem_total_bytes("MemTotal: 1024 kB\nMemFree: 4 kB\n"),
            Some(1_048_576)
        );
        for text in [
            "",
            "MemTotal: 0 kB",
            "MemTotal: bad kB",
            "MemTotal: 50 MB",
            "MemTotal: 18446744073709551615 kB",
            "MemTotal: 1 kB\nMemTotal: 2 kB",
        ] {
            assert_eq!(mem_total_bytes(text), None, "{text}");
        }
        assert_eq!(cgroup_limit_bytes("536870912\n"), Some(512 * MIB));
        assert_eq!(cgroup_limit_bytes("max\n"), None);
        assert_eq!(cgroup_limit_bytes(""), None);
    }

    #[test]
    fn unknown_capacity_preserves_conservative_search_defaults() {
        assert!(super::low_memory_capacity(None));
        assert!(super::low_memory_capacity(Some(0)));
        assert_eq!(super::adaptive_extract_max_mb_for(None), 8);
    }

    #[test]
    fn config_parses_defaults_and_per_source_overrides() {
        let parsed_config_file: ConfigFile = big_search_config::parse(
            r#"
            [defaults]
            content = false
            follow_symlinks = true
            symlink_policy = "local-exact"
            [[source]]
            path = "/a"
            name = "A"
            [[source]]
            path = "/b"
            content = true
            symlink_policy = "personal-local"
            removable = true
        "#,
        )
        .unwrap();
        assert!(!parsed_config_file.defaults.content); // overridden
        assert!(!parsed_config_file.defaults.metadata); // default-off; the unit no longer forces it
        assert!(parsed_config_file.defaults.follow_symlinks);
        assert!(matches!(
            parsed_config_file.defaults.symlink_policy,
            SymlinkPolicy::LocalExact
        ));
        assert_eq!(parsed_config_file.sources.len(), 2);
        assert_eq!(parsed_config_file.sources[0].name.as_deref(), Some("A"));
        assert_eq!(parsed_config_file.sources[0].content, None); // inherits the default (false)
        assert_eq!(parsed_config_file.sources[1].content, Some(true)); // explicit override
        assert!(matches!(
            parsed_config_file.sources[1].symlink_policy,
            Some(SymlinkPolicy::PersonalLocal)
        ));
        assert!(parsed_config_file.sources[1].removable);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let parsed_config_file: ConfigFile = big_search_config::parse("").unwrap();
        assert!(parsed_config_file.defaults.content);
        assert!(!parsed_config_file.defaults.metadata); // metadata is opt-in
        assert!(!parsed_config_file.defaults.follow_symlinks);
        assert!(matches!(
            parsed_config_file.defaults.symlink_policy,
            SymlinkPolicy::PersonalLocal
        ));
        assert!(parsed_config_file.sources.is_empty());
    }

    #[test]
    fn environment_booleans_accept_explicit_on_and_off() {
        let previous = std::env::var_os("BIG_SEARCH_CONTENT");
        unsafe {
            std::env::set_var("BIG_SEARCH_CONTENT", "0");
        }
        assert_eq!(env_bool("BIG_SEARCH_CONTENT"), Some(false));
        unsafe {
            std::env::set_var("BIG_SEARCH_CONTENT", "yes");
        }
        assert_eq!(env_bool("BIG_SEARCH_CONTENT"), Some(true));
        unsafe {
            std::env::set_var("BIG_SEARCH_CONTENT", "maybe");
        }
        assert_eq!(env_bool("BIG_SEARCH_CONTENT"), None);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("BIG_SEARCH_CONTENT", value),
                None => std::env::remove_var("BIG_SEARCH_CONTENT"),
            }
        }
    }

    fn src(path: &str, content: bool) -> Source {
        Source {
            path: PathBuf::from(path),
            id: path.into(),
            name: path.into(),
            flags: SourceFlags::new(content, true, false, false),
        }
    }

    #[test]
    fn owner_matches_longest_prefix() {
        // load() sorts most-specific first; mirror that here.
        let sources = Sources {
            list: vec![src("/home/u/code", false), src("/home/u", true)],
        };
        assert_eq!(
            sources.owner(Path::new("/home/u/code/x.rs")).unwrap().path,
            PathBuf::from("/home/u/code")
        );
        assert_eq!(
            sources.owner(Path::new("/home/u/doc.txt")).unwrap().path,
            PathBuf::from("/home/u")
        );
        assert!(sources.owner(Path::new("/etc/x")).is_none());
        // policy follows the owning source.
        assert_eq!(
            sources
                .owner(Path::new("/home/u/code/x.rs"))
                .map(Source::content),
            Some(false)
        );
    }
}
