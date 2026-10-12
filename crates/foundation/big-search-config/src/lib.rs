// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The big-search configuration file: what it says, and how to change it.
//!
//! One file, `$XDG_CONFIG_HOME/big-search/config.toml`, read by the search
//! service and written by whatever the person is using to change it: the
//! `big-search config` command and the desktop's own settings window. This
//! crate is the single description of that file, so the two sides cannot drift.
//!
//! ## The service never writes it
//!
//! `big-search.service` runs with `ProtectHome=read-only`. The daemon reads this
//! file and nothing else; every write here comes from a tool the person
//! started. That is also why writing is careful: the file belongs to them, it
//! has their comments in it, and it is read by a service that may be running at
//! the same moment.
//!
//! * Comments and layout survive a change. A setting is rewritten in place, and
//!   only a setting that was not there yet is appended.
//! * The write is atomic (a temporary file beside the real one, then a rename
//!   over it), so a reader sees the old file or the new one, never half of
//!   each. A symbolic link is written through to its target, and the file keeps
//!   its permissions.
//! * A file that exists but cannot be read is an error, never an empty file to
//!   write over.
//!
//! ## The empty file means "my home folder"
//!
//! With no `[[source]]` block at all, the service indexes the home folder on its
//! own. So the moment anything writes the first source, the home folder has to
//! be written with it; otherwise adding an external disk silently removes the
//! person's own files from the search. [`add_source`] does that; nothing else
//! needs to remember it.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What the file says. Missing pieces read as their defaults; a file that
/// cannot be parsed is read by [`read_text`], which keeps the search running
/// with privacy features off.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub defaults: Defaults,
    pub origin: Origin,
    pub history: FileHistory,
    #[serde(rename = "source")]
    pub sources: Vec<SourceEntry>,
}

/// Settings that apply to every catalogued folder unless the folder says
/// otherwise.
///
/// The field names are the keys in the file, and they are what the service has
/// always read; renaming one here silently changes what a person's existing file
/// means.
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
    pub content_index_mode: ContentIndexMode,
    /// Most text kept per file, in MiB. 0 chooses by the size of the machine.
    pub extract_max_mb: u64,
    /// Largest plain-text file read, in MiB. 0 reads any size.
    pub text_max_mb: u64,
    /// Largest office document read, in MiB. 0 chooses by the machine.
    pub office_max_mb: u64,
    /// Largest PDF read, in MiB. 0 reads any size.
    pub pdf_max_mb: u64,
    pub pdf_timeout_secs: u64,
    /// How long a changed file is left alone before its text is read again, so
    /// a document being edited is not re-read on every save.
    pub content_cooldown_secs: u64,
    /// Also index the folders a symbolic link in the home folder points at.
    pub follow_symlinks: bool,
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
/// evidence.
///
/// Its own block rather than two more `[defaults]` keys: this is the one part of
/// the configuration that is about privacy rather than about how much of the
/// disk to read, and the settings window shows it as its own section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Origin {
    /// Remember where files came from at all.
    pub enabled: bool,
    /// Also read the download list the web browsers keep. Off unless the person
    /// turns it on: that list is the browser's, and nobody expects a file
    /// manager to have read it.
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
    /// Keep earlier versions at all. On where the disk can do it; the service
    /// finds out for itself and stays quiet where it cannot.
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
    /// Decide by the size of the machine.
    #[default]
    Auto,
    /// Which files hold the word, and nothing else. Smallest index.
    Basic,
    /// How often each word appears too, which ranks results better.
    Freqs,
}

impl ContentIndexMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Basic => "basic",
            Self::Freqs => "freqs",
        }
    }

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
    #[default]
    PersonalLocal,
    LocalExact,
}

/// One `[[source]]` block: a folder to catalogue, and what to read in it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SourceEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub content: Option<bool>,
    pub metadata: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub symlink_policy: Option<SymlinkPolicy>,
    /// A disk that comes and goes. Its catalogue is kept while it is unplugged.
    pub removable: bool,
    /// A folder on another machine. Same keeping, different reason.
    pub network: bool,
}

impl Default for SourceEntry {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            name: None,
            content: None,
            metadata: None,
            follow_symlinks: None,
            symlink_policy: None,
            removable: false,
            network: false,
        }
    }
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

fn config_home() -> PathBuf {
    // The XDG spec says a relative value is invalid and must be ignored; an
    // empty one is relative too.
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

/// The same, but saying what is wrong with the file.
///
/// A hand-edited `config.toml` with a typo silently becomes the defaults
/// everywhere else; the service is the one place that can tell somebody, so it
/// needs the error rather than the fallback.
///
/// # Errors
///
/// Returns the parse error, with the line the file went wrong on.
pub fn parse(text: &str) -> Result<SearchConfig, toml::de::Error> {
    toml::from_str(text)
}

/// The named amounts of work the settings window offers.
///
/// Each is a whole set of values rather than one knob, because the knobs only
/// make sense together: an index that keeps word frequencies and a four-megabyte
/// extraction cap is a contradiction nobody would choose on purpose.
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
        apply_to(&mut candidate, *preset);
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
/// file would.
///
/// This is how a settings window offers the presets without keeping a second
/// copy of their values: it applies one here and writes out the fields.
pub fn apply_preset_in_memory(defaults: &mut Defaults, preset: Preset) {
    apply_to(defaults, preset);
}

fn apply_to(defaults: &mut Defaults, preset: Preset) {
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

/// Write one preset's whole set of values.
///
/// # Errors
///
/// When the file cannot be written.
pub fn apply_preset(preset: Preset) -> io::Result<()> {
    let values = preset.values();
    let pairs: Vec<(&str, &str)> = values
        .iter()
        .map(|(key, literal)| (*key, literal.as_str()))
        .collect();
    set_defaults(&pairs)
}

/// Set keys in `[defaults]`, leaving the rest of the file as it was.
///
/// `literal` is written as it stands, so a string value has to arrive quoted;
/// [`string_literal`] does that.
///
/// # Errors
///
/// When the file cannot be written.
pub fn set_defaults(values: &[(&str, &str)]) -> io::Result<()> {
    edit(|lines| {
        set_in_table(lines, "[defaults]", values);
        Ok(())
    })
}

/// Set keys in `[origin]`, the block that says what may be remembered about
/// where files came from.
///
/// # Errors
///
/// When the file cannot be written.
pub fn set_origin(values: &[(&str, &str)]) -> io::Result<()> {
    edit(|lines| {
        set_in_table(lines, "[origin]", values);
        Ok(())
    })
}

/// Set keys in `[history]`, the block that says whether earlier versions of
/// documents are kept.
///
/// # Errors
///
/// When the file cannot be written.
pub fn set_history(values: &[(&str, &str)]) -> io::Result<()> {
    edit(|lines| {
        set_in_table(lines, "[history]", values);
        Ok(())
    })
}

/// Add a folder to the catalogue.
///
/// Adding the first one also writes the home folder, because a file with no
/// folders in it means "my home folder", and adding an external disk must not
/// take the person's own files out of the search.
///
/// # Errors
///
/// When the file cannot be written.
pub fn add_source(source: &SourceEntry) -> io::Result<()> {
    let existing = read();
    if existing.sources.iter().any(|kept| kept.path == source.path) {
        return Ok(());
    }
    edit(|lines| {
        // No `[[source]]` means the home folder alone; adding a folder must keep
        // it, unless the folder is home itself.
        if existing.sources.is_empty() && source.path != home_dir() {
            append_source(lines, &SourceEntry::new(home_dir()));
        }
        append_source(lines, source);
        Ok(())
    })
}

/// Take a folder out of the catalogue.
///
/// # Errors
///
/// [`io::ErrorKind::NotFound`] when the file has no `[[source]]` for `path`;
/// otherwise when the file cannot be read or written.
pub fn remove_source(path: &Path) -> io::Result<()> {
    edit(|lines| {
        let block = find_source(lines, path).ok_or_else(|| missing_source(path))?;
        lines.drain(block);
        Ok(())
    })
}

/// Turn reading the text inside a folder's files on or off.
///
/// A file with no `[[source]]` catalogues the home folder alone; setting it for
/// home writes that folder's block.
///
/// # Errors
///
/// [`io::ErrorKind::NotFound`] when the file has no `[[source]]` for `path`;
/// otherwise when the file cannot be read or written.
pub fn set_source_content(path: &Path, content: bool) -> io::Result<()> {
    let literal = if content { "true" } else { "false" };
    let implicit_home = read().sources.is_empty() && path == home_dir();
    edit(|lines| {
        if implicit_home {
            append_source(
                lines,
                &SourceEntry {
                    content: Some(content),
                    ..SourceEntry::new(home_dir())
                },
            );
            return Ok(());
        }
        let block = find_source(lines, path).ok_or_else(|| missing_source(path))?;
        upsert_line(lines, block.start + 1, block.end, "content", literal);
        Ok(())
    })
}

fn missing_source(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no [[source]] for {} in {}",
            path.display(),
            self::path().display()
        ),
    )
}

/// A string as TOML writes it.
#[must_use]
pub fn string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Read the file, hand its lines to `change`, and put it back atomically.
///
/// Only a missing file reads as empty: any other read error (permissions, bytes
/// that are not UTF-8) is returned before anything is written, so the person's
/// file is never replaced by one holding only the new setting.
fn edit(change: impl FnOnce(&mut Vec<String>) -> io::Result<()>) -> io::Result<()> {
    edit_file(&path(), change)
}

fn edit_file(
    path: &Path,
    change: impl FnOnce(&mut Vec<String>) -> io::Result<()>,
) -> io::Result<()> {
    // A symbolic link (a dotfiles checkout, say) is edited where it points, so
    // the rename replaces the target and the link stays.
    let path = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (text, permissions) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, Some(std::fs::metadata(&path)?.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), None),
        Err(error) => return Err(error),
    };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    change(&mut lines)?;
    write_atomically(&path, &format!("{}\n", lines.join("\n")), permissions)
}

/// Write beside the file and rename over it, keeping `permissions` when the
/// file had some.
///
/// A rename is the one filesystem operation that either happened or did not, so
/// a reader either sees the old configuration or the new one and never half of
/// each, including the reader that is a running service. There is no fsync:
/// this is program state, and losing the last change to a power cut costs
/// repeating it.
fn write_atomically(
    path: &Path,
    text: &str,
    permissions: Option<std::fs::Permissions>,
) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".{}.new",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    ));
    let written = std::fs::write(&temporary, text)
        .and_then(|()| match permissions {
            Some(permissions) => std::fs::set_permissions(&temporary, permissions),
            None => Ok(()),
        })
        .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Set keys inside `table`, adding the table itself when the file has none.
fn set_in_table(lines: &mut Vec<String>, table: &str, values: &[(&str, &str)]) {
    let start = match lines.iter().position(|line| header(line) == Some(table)) {
        Some(index) => index,
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(table.to_owned());
            lines.len() - 1
        }
    };
    let end = next_table(lines, start);
    for (key, literal) in values {
        upsert_line(lines, start + 1, end, key, literal);
    }
}

/// Where the table starting at `start` ends: the next `[section]` line, or the
/// end of the file.
fn next_table(lines: &[String], start: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| header(line).map(|_| index))
        .unwrap_or(lines.len())
}

/// The `[table]` or `[[array]]` header on this line, without its trailing
/// comment, or `None` when the line is not a header.
fn header(line: &str) -> Option<&str> {
    let code = without_comment(line).trim();
    (code.starts_with('[') && code.ends_with(']')).then_some(code)
}

/// The line up to a `#` comment that is not inside a string.
fn without_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        match quote {
            // Only basic ("...") strings have escapes; literal ('...') ones
            // end at the next quote.
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return &line[..index],
            None => {}
        }
    }
    line
}

/// Rewrite `key` where it already is, keeping its indentation and its trailing
/// comment; append it after the last setting otherwise.
fn upsert_line(lines: &mut Vec<String>, start: usize, end: usize, key: &str, literal: &str) {
    let mut insert_at = end;
    for (index, line) in lines.iter_mut().enumerate().take(end).skip(start) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(key)
            && rest.trim_start().starts_with('=')
        {
            *line = rewrite_assignment(line, key, literal);
            return;
        }
        insert_at = index + 1;
    }
    lines.insert(insert_at, format!("{key} = {literal}"));
}

fn rewrite_assignment(line: &str, key: &str, literal: &str) -> String {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    match line.get(without_comment(line).len()..).map(str::trim_end) {
        Some(comment) if !comment.is_empty() => {
            format!("{indent}{key} = {literal}      {comment}")
        }
        _ => format!("{indent}{key} = {literal}"),
    }
}

/// The lines of the `[[source]]` block for `path`, if the file has one.
///
/// The block is parsed as TOML, so a path written as a literal string, with
/// escapes or with a trailing comment is found like any other.
fn find_source(lines: &[String], path: &Path) -> Option<std::ops::Range<usize>> {
    let wanted = path.to_string_lossy();
    let mut index = 0;
    while index < lines.len() {
        if header(&lines[index]) == Some("[[source]]") {
            let end = next_table(lines, index);
            let block = lines[index + 1..end].join("\n").parse::<toml::Table>();
            let is_wanted = block
                .ok()
                .and_then(|block| block.get("path")?.as_str().map(|found| found == wanted))
                .unwrap_or(false);
            if is_wanted {
                return Some(index..end);
            }
            index = end;
        } else {
            index += 1;
        }
    }
    None
}

/// Write a whole `[[source]]` block at the end of the file.
fn append_source(lines: &mut Vec<String>, source: &SourceEntry) {
    if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.push(String::new());
    }
    lines.push("[[source]]".to_owned());
    lines.push(format!(
        "path = {}",
        string_literal(&source.path.to_string_lossy())
    ));
    if let Some(name) = &source.name {
        lines.push(format!("name = {}", string_literal(name)));
    }
    if let Some(content) = source.content {
        lines.push(format!("content = {content}"));
    }
    if let Some(metadata) = source.metadata {
        lines.push(format!("metadata = {metadata}"));
    }
    if source.removable {
        lines.push("removable = true".to_owned());
    }
    if source.network {
        lines.push("network = true".to_owned());
    }
}

#[cfg(test)]
mod tests;
