//! The big-search configuration file: what it says, and how to change it.
//!
//! One file, `$XDG_CONFIG_HOME/big-search/config.toml`, read by the search
//! service and written by whatever the person is using to change it: the
//! `big-search config` command and the desktop's own settings window. This
//! crate is the single description of that file, so the two sides cannot drift.
//!
//! ## The service never writes it
//!
//! `big-search.service` runs with `ProtectHome=read-only`; every write comes
//! from a tool the person started. The file is theirs and a running service
//! may read it at any moment, so:
//!
//! * Comments, layout and line endings survive a change. A setting is
//!   rewritten in place, and only a setting that was not there yet is appended.
//! * A file that does not parse is refused, never edited: rewriting it would
//!   drop the person's folders and settings, and editing lines the parser
//!   rejects could write a second block for one folder.
//! * Writers take a lock beside the file, so the settings window and
//!   `big-search config` writing at once keep both changes.
//! * The write is atomic (a temporary file beside the real one, then a rename
//!   over it), so a reader sees the old file or the new one, never half of
//!   each. A symbolic link is written through to its target, and the file keeps
//!   its permissions.
//! * A file that exists but cannot be read is an error, never an empty file to
//!   write over.
//! * A folder edit is made by line and kept only when the file then reads, as
//!   TOML, the way the request asked: a line the editor misreads (a header in a
//!   string, an inline array) leaves the file as it was.
//!
//! ## The empty file means "my home folder"
//!
//! With no `[[source]]` block at all, the service indexes the home folder on its
//! own. So the moment anything writes the first source, the home folder has to
//! be written with it; otherwise adding an external disk silently removes the
//! person's own files from the search. [`add_source`] does that; nothing else
//! needs to remember it. For the same reason [`remove_source`] refuses to take
//! out the last folder listed.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod edit;
pub use edit::{
    add_source, apply_preset, remove_source, set_defaults, set_history, set_origin,
    set_source_content, string_literal,
};

/// What the file says. Missing pieces read as their defaults; a file that
/// cannot be parsed is read by [`read_text`], which keeps the search running
/// with privacy features off.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// The `[defaults]` block.
    pub defaults: Defaults,
    /// The `[origin]` block.
    pub origin: Origin,
    /// The `[history]` block.
    pub history: FileHistory,
    /// The `[[source]]` blocks, in file order. None means the home folder.
    #[serde(rename = "source")]
    pub sources: Vec<SourceEntry>,
}

/// Settings that apply to every catalogued folder unless the folder says
/// otherwise.
///
/// Field names are the file's keys: renaming one changes what existing files
/// mean.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Defaults {
    /// Read the text inside files, not only their names.
    pub content: bool,
    /// Read the details a file carries: kind, how long a video runs, how large
    /// a picture is.
    pub metadata: bool,
    /// Names only, whatever else is set. The one switch that turns off all
    /// reading of file contents.
    pub names_only: bool,
    /// How much detail the content index keeps per word.
    pub content_index_mode: ContentIndexMode,
    /// Most text kept per file, in MiB. 0 chooses by the size of the machine.
    pub extract_max_mb: u64,
    /// Largest plain-text file read, in MiB. 0 reads any size.
    pub text_max_mb: u64,
    /// Largest office document read, in MiB. 0 chooses by the machine.
    pub office_max_mb: u64,
    /// Largest PDF read, in MiB. 0 reads any size.
    pub pdf_max_mb: u64,
    /// Longest one PDF may take to read, in seconds, before it is skipped.
    pub pdf_timeout_secs: u64,
    /// How long a changed file is left alone before its text is read again, so
    /// a document being edited is not re-read on every save.
    pub content_cooldown_secs: u64,
    /// Also index the folders a symbolic link in the home folder points at.
    pub follow_symlinks: bool,
    /// Which link targets `follow_symlinks` follows.
    pub symlink_policy: SymlinkPolicy,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            content: true,
            metadata: false,
            names_only: false,
            content_index_mode: ContentIndexMode::Auto,
            extract_max_mb: 0,
            text_max_mb: 0,
            office_max_mb: 0,
            pdf_max_mb: 0,
            pdf_timeout_secs: 5,
            content_cooldown_secs: 900,
            follow_symlinks: false,
            symlink_policy: SymlinkPolicy::PersonalLocal,
        }
    }
}

/// Whether the service remembers where files came from, and from which
/// evidence. Its own `[origin]` block because it is a privacy setting.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Origin {
    /// Remember where files came from at all.
    pub enabled: bool,
    /// Also read the download list the web browsers keep. Off by default: that
    /// list is the browser's.
    pub browser_history: bool,
}

impl Default for Origin {
    fn default() -> Self {
        Self {
            enabled: true,
            browser_history: false,
        }
    }
}

/// Whether the service keeps earlier versions of the person's documents, and
/// how many.
///
/// Only the values somebody could reasonably want to change live here. The
/// rest (which files qualify, how long the service waits before saving a
/// version, when it stops for lack of space) are constants: one known case
/// each, and no reader for a knob.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct FileHistory {
    /// Keep earlier versions at all, where the filesystem supports it.
    pub enabled: bool,
    /// Most versions kept per document. A ceiling, not a target: the thinning
    /// by age below usually keeps fewer.
    pub keep_versions: u32,
    /// Oldest version kept, in days.
    pub keep_days: u32,
    /// Most disk the whole history may hold, in GiB. `0` means "decide by the
    /// size of the disk": the smaller of a twentieth of it and 10 GiB.
    pub max_total_gib: u64,
    /// Keep nothing inside a folder tree that holds a `.git`: git already keeps
    /// every committed state there, and a branch switch rewrites thousands of
    /// files that would each become a version.
    pub skip_git_repositories: bool,
    /// When git repositories are not skipped, still skip what their
    /// `.gitignore` files exclude, such as build output and generated files.
    pub respect_gitignore: bool,
}

impl Default for FileHistory {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_versions: 10,
            keep_days: 30,
            max_total_gib: 0,
            skip_git_repositories: true,
            respect_gitignore: true,
        }
    }
}

/// How much disk the history may hold, resolved against the disk it lives on.
///
/// A fixed number cannot fit both a 120 GB netbook and a 2 TB desktop, so `0`
/// means "a twentieth of the disk, and never more than 10 GiB".
#[must_use]
pub fn history_budget_bytes(configured_gib: u64, disk_bytes: u64) -> u64 {
    const GIB: u64 = 1 << 30;
    if configured_gib > 0 {
        return configured_gib.saturating_mul(GIB);
    }
    (disk_bytes / 20).min(10 * GIB)
}

/// How much detail the content index keeps per word.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ContentIndexMode {
    /// Which files hold the word, and nothing else. Smallest index.
    Basic,
    /// How often each word appears too, which ranks results better.
    Freqs,
    /// Decide by the size of the machine.
    ///
    /// Also what a value this build does not know reads as, so a choice another
    /// build added does not cost the rest of `[defaults]`; serde wants that
    /// variant last.
    #[default]
    #[serde(other)]
    Auto,
}

impl ContentIndexMode {
    /// The value written in the file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Basic => "basic",
            Self::Freqs => "freqs",
        }
    }

    /// Reads a value from the file or the command line, ignoring case and
    /// accepting the aliases. `None` for an unknown one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "basic" | "docids" | "doc-ids" => Some(Self::Basic),
            "freqs" | "with-freqs" | "frequency" | "frequencies" => Some(Self::Freqs),
            _ => None,
        }
    }
}

/// Which symbolic links inside a catalogued folder are followed.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SymlinkPolicy {
    /// Follow a link to any local folder. Network and virtual file systems are
    /// still skipped.
    LocalExact,
    /// Follow a link only to a local folder that looks like the person's own
    /// data, such as the documents of another system's user.
    ///
    /// Also what a value this build does not know reads as; serde wants that
    /// variant last.
    #[default]
    #[serde(other)]
    PersonalLocal,
}

impl SymlinkPolicy {
    /// The value written in the file.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalExact => "local-exact",
            Self::PersonalLocal => "personal-local",
        }
    }
}

/// One `[[source]]` block: a folder to catalogue, and what to read in it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SourceEntry {
    /// The folder.
    pub path: PathBuf,
    /// The name shown for the folder. Without one, the disk label or the
    /// folder's own name.
    pub name: Option<String>,
    /// Overrides [`Defaults::content`] for this folder.
    pub content: Option<bool>,
    /// Overrides [`Defaults::metadata`] for this folder.
    pub metadata: Option<bool>,
    /// Overrides [`Defaults::follow_symlinks`] for this folder.
    pub follow_symlinks: Option<bool>,
    /// Overrides [`Defaults::symlink_policy`] for this folder.
    pub symlink_policy: Option<SymlinkPolicy>,
    /// A disk that comes and goes. Its catalogue is kept while it is unplugged.
    pub removable: bool,
    /// A folder on another machine. Same keeping, different reason.
    pub network: bool,
}

impl SourceEntry {
    /// A folder catalogued with the settings everything else uses.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// Where the file lives.
#[must_use]
pub fn path() -> PathBuf {
    config_home().join("big-search").join("config.toml")
}

/// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset or not absolute: the XDG
/// spec says a relative value is invalid and must be ignored, and an empty one
/// is relative too.
#[must_use]
pub fn config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|value| Path::new(value).is_absolute()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join(".config"),
    }
}

/// The person's home folder, which is also the folder the service catalogues
/// when the file names none.
#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// What the file says now.
///
/// A missing file reads as the defaults. A file that exists but cannot be read
/// is treated like a malformed one (see [`read_text`]). This never creates the
/// file: writing is the caller's decision, and the service that reads it is
/// not allowed to write anyway.
#[must_use]
pub fn read() -> SearchConfig {
    match std::fs::read_to_string(path()) {
        Ok(text) => read_text(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => SearchConfig::default(),
        Err(_) => unreadable(),
    }
}

/// Each choice in `text` this build does not know, as `key = "value"`: it
/// reads as the default (`auto`, `personal-local`), so a typo would otherwise
/// pass unseen.
#[must_use]
pub fn unknown_choices(text: &str) -> Vec<String> {
    // Each key, its values, and whether a folder may set it for itself.
    const CHOICES: [(&str, &[&str], bool); 2] = [
        ("content_index_mode", &["auto", "basic", "freqs"], false),
        ("symlink_policy", &["local-exact", "personal-local"], true),
    ];
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let defaults = table.get("defaults").and_then(toml::Value::as_table);
    let sources = table
        .get("source")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table);
    let blocks = defaults
        .into_iter()
        .map(|block| (block, false))
        .chain(sources.map(|block| (block, true)));
    let mut unknown = Vec::new();
    for (block, in_folder) in blocks {
        for (key, known, per_folder) in CHOICES {
            if in_folder && !per_folder {
                continue;
            }
            if let Some(value) = block.get(key).and_then(toml::Value::as_str)
                && !known.contains(&value)
            {
                unknown.push(format!("{key} = {}", string_literal(value)));
            }
        }
    }
    unknown
}

/// What `text` says, keeping the search running when it is malformed.
///
/// A malformed file must not take the search down, and it must not turn on what
/// the person turned off either. So when the whole file does not parse, each
/// table that still parses on its own (`[defaults]`, `[origin]`, `[history]`,
/// every `[[source]]`) is kept, and everything else is replaced by the defaults
/// with remembering where files came from (`[origin]`) and keeping earlier
/// versions (`[history]`) off. When no `[[source]]` survives, the service
/// falls back to the home folder, as for an empty file.
#[must_use]
pub fn read_text(text: &str) -> SearchConfig {
    parse(text).unwrap_or_else(|_| salvage(text))
}

/// The defaults with every feature that records something about the person
/// off: what a file stands for when what it says cannot be known.
fn unreadable() -> SearchConfig {
    SearchConfig {
        origin: Origin {
            enabled: false,
            browser_history: false,
        },
        history: FileHistory {
            enabled: false,
            ..FileHistory::default()
        },
        ..SearchConfig::default()
    }
}

/// The tables of a malformed file that still parse on their own.
fn salvage(text: &str) -> SearchConfig {
    let mut config = unreadable();
    let Ok(table) = text.parse::<toml::Table>() else {
        return config;
    };
    if let Some(Ok(defaults)) = table.get("defaults").map(|value| value.clone().try_into()) {
        config.defaults = defaults;
    }
    if let Some(Ok(origin)) = table.get("origin").map(|value| value.clone().try_into()) {
        config.origin = origin;
    }
    if let Some(Ok(history)) = table.get("history").map(|value| value.clone().try_into()) {
        config.history = history;
    }
    if let Some(toml::Value::Array(sources)) = table.get("source") {
        config.sources = sources
            .iter()
            .filter_map(|source| source.clone().try_into().ok())
            .collect();
    }
    config
}

/// Parse the file's text, reporting what is wrong with it.
///
/// For the service, the one place that can tell somebody their hand-edited
/// file has a typo; everything else uses [`read`] and its defaults.
///
/// # Errors
///
/// Returns the parse error, with the line the file went wrong on.
pub fn parse(text: &str) -> Result<SearchConfig, toml::de::Error> {
    toml::from_str(text)
}

/// The named amounts of work the settings window offers. Each sets several
/// keys together, because they only make sense as a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Names of files and folders only.
    NamesOnly,
    /// Contents too, kept small, for a machine with little memory.
    Light,
    /// Contents, with the machine deciding the detail. The recommended one.
    Balanced,
    /// Everything, with more detail kept.
    Complete,
}

impl Preset {
    /// The name the `big-search config preset` command uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NamesOnly => "names-only",
            Self::Light => "low-memory",
            Self::Balanced => "balanced",
            Self::Complete => "complete",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "names-only" | "name-only" => Some(Self::NamesOnly),
            "low-memory" | "lowmem" => Some(Self::Light),
            "balanced" | "auto" => Some(Self::Balanced),
            "complete" | "full" => Some(Self::Complete),
            _ => None,
        }
    }

    /// Every preset, in the order a person reads them: least work first.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::NamesOnly, Self::Light, Self::Balanced, Self::Complete]
    }

    /// The values this preset writes.
    #[must_use]
    fn values(self) -> Vec<(&'static str, String)> {
        let text = |value: &str| string_literal(value);
        match self {
            Self::NamesOnly => vec![
                ("names_only", "true".to_owned()),
                ("content", "false".to_owned()),
                ("metadata", "false".to_owned()),
                ("content_index_mode", text("basic")),
                ("extract_max_mb", "0".to_owned()),
            ],
            Self::Light => vec![
                ("names_only", "false".to_owned()),
                ("content", "true".to_owned()),
                ("metadata", "true".to_owned()),
                ("content_index_mode", text("basic")),
                ("extract_max_mb", "4".to_owned()),
                ("text_max_mb", "0".to_owned()),
                ("office_max_mb", "64".to_owned()),
                ("pdf_max_mb", "0".to_owned()),
                ("pdf_timeout_secs", "5".to_owned()),
            ],
            Self::Balanced => vec![
                ("names_only", "false".to_owned()),
                ("content", "true".to_owned()),
                ("metadata", "true".to_owned()),
                ("content_index_mode", text("auto")),
                ("extract_max_mb", "0".to_owned()),
                ("text_max_mb", "0".to_owned()),
                ("office_max_mb", "0".to_owned()),
                ("pdf_max_mb", "0".to_owned()),
                ("pdf_timeout_secs", "5".to_owned()),
            ],
            Self::Complete => vec![
                ("names_only", "false".to_owned()),
                ("content", "true".to_owned()),
                ("metadata", "true".to_owned()),
                ("content_index_mode", text("freqs")),
                ("extract_max_mb", "32".to_owned()),
                ("text_max_mb", "0".to_owned()),
                ("office_max_mb", "512".to_owned()),
                ("pdf_max_mb", "0".to_owned()),
                ("pdf_timeout_secs", "10".to_owned()),
            ],
        }
    }
}

/// Which preset these settings are, or `None` when they are somebody's own
/// mixture.
///
/// Only the keys that tell the presets apart are compared. Comparing every
/// key would answer `None` for a machine nobody has ever configured: the file's
/// own defaults have `metadata = false` while three of the four presets turn it
/// on, so a brand-new installation would be reported as a hand-made mixture and
/// the settings window would show no choice selected.
#[must_use]
pub fn preset_of(defaults: &Defaults) -> Option<Preset> {
    Preset::all().into_iter().find(|preset| {
        let mut candidate = defaults.clone();
        apply_preset_in_memory(&mut candidate, *preset);
        distinguishing(&candidate) == distinguishing(defaults)
    })
}

/// The values that separate one preset from another.
fn distinguishing(defaults: &Defaults) -> (bool, ContentIndexMode, u64, u64, u64) {
    (
        defaults.names_only,
        defaults.content_index_mode,
        defaults.extract_max_mb,
        defaults.office_max_mb,
        defaults.pdf_timeout_secs,
    )
}

/// Apply a preset to a settings value in memory, the same way writing it to the
/// file would, so a settings window needs no second copy of the values.
pub fn apply_preset_in_memory(defaults: &mut Defaults, preset: Preset) {
    for (key, literal) in preset.values() {
        match key {
            "names_only" => defaults.names_only = literal == "true",
            "content" => defaults.content = literal == "true",
            "metadata" => defaults.metadata = literal == "true",
            "content_index_mode" => {
                defaults.content_index_mode =
                    ContentIndexMode::parse(literal.trim_matches('"')).unwrap_or_default();
            }
            "extract_max_mb" => defaults.extract_max_mb = literal.parse().unwrap_or_default(),
            "text_max_mb" => defaults.text_max_mb = literal.parse().unwrap_or_default(),
            "office_max_mb" => defaults.office_max_mb = literal.parse().unwrap_or_default(),
            "pdf_max_mb" => defaults.pdf_max_mb = literal.parse().unwrap_or_default(),
            "pdf_timeout_secs" => defaults.pdf_timeout_secs = literal.parse().unwrap_or_default(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
