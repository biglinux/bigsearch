//! Changing the file: settings rewritten in place, folders added, removed or
//! changed by line, each edit checked against what TOML reads afterwards.

use std::io;
use std::path::{Path, PathBuf};

use crate::{Preset, SourceEntry, home_dir, parse, path};

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
/// When the file cannot be read or written.
pub fn add_source(source: &SourceEntry) -> io::Result<()> {
    edit(|lines| add_source_to(lines, source, &home_dir()))
}

/// Decided on the lines read under the write lock, so a folder another writer
/// added a moment ago is not added twice.
pub(crate) fn add_source_to(
    lines: &mut Vec<String>,
    source: &SourceEntry,
    home: &Path,
) -> io::Result<()> {
    // TOML strings are UTF-8: another name would be written as a different path.
    if source.path.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a UTF-8 path", source.path.display()),
        ));
    }
    let before = parsed(lines)?;
    let sources = source_entries(&before).to_vec();
    if sources.iter().any(|entry| is_folder(entry, &source.path)) {
        return Ok(());
    }
    // No `[[source]]` means the home folder alone; adding a folder must keep
    // it, unless the folder is home itself.
    let mut added = Vec::new();
    if sources.is_empty() && source.path != home {
        added.push(SourceEntry::new(home));
    }
    added.push(source.clone());
    let mut appended = Vec::new();
    for entry in &added {
        append_source(&mut appended, entry);
    }
    let expected = with_sources(
        before,
        [sources, source_entries(&parsed(&appended)?).to_vec()].concat(),
    );
    edit_lines_to(lines, &expected, |lines| {
        for entry in &added {
            append_source(lines, entry);
        }
    })
}

/// The file as TOML reads it, every key kept.
pub(crate) fn parsed(lines: &[String]) -> io::Result<toml::Table> {
    // Ended like the file, so a last line's `\r` is part of a line ending.
    format!("{}\n", lines.join("\n"))
        .parse::<toml::Table>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The `[[source]]` entries of a parsed file, in file order.
fn source_entries(table: &toml::Table) -> &[toml::Value] {
    table
        .get("source")
        .and_then(toml::Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn is_folder(entry: &toml::Value, path: &Path) -> bool {
    entry
        .get("path")
        .and_then(toml::Value::as_str)
        .is_some_and(|listed| Path::new(listed) == path)
}

/// `table` with these `[[source]]` entries; none leaves no `source` key.
fn with_sources(mut table: toml::Table, sources: Vec<toml::Value>) -> toml::Table {
    if sources.is_empty() {
        table.remove("source");
    } else {
        table.insert("source".to_owned(), toml::Value::Array(sources));
    }
    table
}

/// Apply an edit by line and keep it only when the file then reads as
/// `expected`, key for key: the line editor sees headers, TOML also sees
/// strings and inline arrays, so an edit that touched anything else is refused.
fn edit_lines_to(
    lines: &mut Vec<String>,
    expected: &toml::Table,
    edit: impl FnOnce(&mut Vec<String>),
) -> io::Result<()> {
    let mut edited = lines.clone();
    edit(&mut edited);
    if !same_table(&parsed(&edited)?, expected) {
        return Err(unchanged_form());
    }
    *lines = edited;
    Ok(())
}

/// Value equality with a NaN equal to itself, so a file holding one can be
/// edited like any other.
fn same_table(left: &toml::Table, right: &toml::Table) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| right.get(key).is_some_and(|other| same_value(value, other)))
}

fn same_value(left: &toml::Value, right: &toml::Value) -> bool {
    match (left, right) {
        (toml::Value::Float(left), toml::Value::Float(right)) => {
            left == right || (left.is_nan() && right.is_nan())
        }
        (toml::Value::Array(left), toml::Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_value(left, right))
        }
        (toml::Value::Table(left), toml::Value::Table(right)) => same_table(left, right),
        _ => left == right,
    }
}

/// Take a folder out of the catalogue. A folder the file does not list is
/// out already, so the request holds and the file is left as it was.
///
/// # Errors
///
/// [`io::ErrorKind::NotFound`] for the home folder of a file that lists no
/// folder, and [`io::ErrorKind::InvalidInput`] for the last folder listed,
/// since a file that lists none catalogues home; otherwise when the file
/// cannot be read or written.
pub fn remove_source(path: &Path) -> io::Result<()> {
    edit(|lines| remove_source_from(lines, path, &home_dir()))
}

pub(crate) fn remove_source_from(
    lines: &mut Vec<String>,
    path: &Path,
    home: &Path,
) -> io::Result<()> {
    let before = parsed(lines)?;
    let sources = source_entries(&before).to_vec();
    if !sources.iter().any(|entry| is_folder(entry, path)) {
        if path == home && sources.is_empty() {
            return Err(missing_source(path));
        }
        return Ok(());
    }
    let kept: Vec<toml::Value> = sources
        .into_iter()
        .filter(|entry| !is_folder(entry, path))
        .collect();
    if kept.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the last folder cannot be taken out: a file that lists none catalogues the home folder",
        ));
    }
    let blocks = source_blocks(lines)?;
    let expected = with_sources(before, kept);
    edit_lines_to(lines, &expected, |lines| {
        for block in blocks.iter().rev().filter(|block| block.is_folder(path)) {
            lines.drain(block.whole.clone());
        }
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
    edit(|lines| {
        if set_content_in(lines, path, &home_dir(), content)? {
            Ok(())
        } else {
            Err(missing_source(path))
        }
    })
}

/// Set `content` in the block for `path`, reporting whether there was one.
/// Decided on the lines read under the write lock, like [`add_source`].
pub(crate) fn set_content_in(
    lines: &mut Vec<String>,
    path: &Path,
    home: &Path,
    content: bool,
) -> io::Result<bool> {
    let before = parsed(lines)?;
    let mut sources = source_entries(&before).to_vec();
    if sources.iter().any(|entry| is_folder(entry, path)) {
        for entry in sources.iter_mut().filter(|entry| is_folder(entry, path)) {
            if let Some(entry) = entry.as_table_mut() {
                entry.insert("content".to_owned(), toml::Value::Boolean(content));
            }
        }
        let blocks = source_blocks(lines)?;
        let literal = if content { "true" } else { "false" };
        let expected = with_sources(before, sources);
        edit_lines_to(lines, &expected, |lines| {
            for block in blocks.iter().rev().filter(|block| block.is_folder(path)) {
                upsert_line(lines, block.header + 1, block.whole.end, "content", literal);
            }
        })?;
        return Ok(true);
    }
    if path == home && sources.is_empty() {
        let entry = SourceEntry {
            content: Some(content),
            ..SourceEntry::new(home)
        };
        let mut appended = Vec::new();
        append_source(&mut appended, &entry);
        let expected = with_sources(before, source_entries(&parsed(&appended)?).to_vec());
        edit_lines_to(lines, &expected, |lines| append_source(lines, &entry))?;
        return Ok(true);
    }
    Ok(false)
}

fn unchanged_form() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the search settings file lists its folders in a form this editor does not change",
    )
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

/// A string as TOML writes it, on one line: control characters are escaped.
#[must_use]
pub fn string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for character in value.chars() {
        match character {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            control if control.is_control() => {
                literal.push_str(&format!("\\u{:04X}", u32::from(control)));
            }
            other => literal.push(other),
        }
    }
    literal.push('"');
    literal
}

/// Read the file, hand its lines to `change`, and put it back atomically.
///
/// Only a missing file reads as empty: any other read error (permissions, bytes
/// that are not UTF-8) and a file that does not parse are returned before
/// anything is written, so the person's file is never replaced by one holding
/// only the new setting.
///
/// The read, the change and the rename happen under a lock on a file beside
/// the config, so two writers at once keep both changes. The lines keep the
/// file's own line ending.
pub(crate) fn edit(change: impl FnOnce(&mut Vec<String>) -> io::Result<()>) -> io::Result<()> {
    edit_file(&path(), change)
}

pub(crate) fn edit_file(
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
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(&path))?;
    lock.lock()?;
    let (text, permissions) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, Some(std::fs::metadata(&path)?.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), None),
        Err(error) => return Err(error),
    };
    if let Err(error) = parse(&text) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, error));
    }
    // The rename would replace a file somebody made read-only, so its mode is
    // the refusal the write itself never meets.
    if permissions
        .as_ref()
        .is_some_and(std::fs::Permissions::readonly)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the search settings file is read-only",
        ));
    }
    // A file with CRLF throughout is written back with CRLF; any other keeps
    // each line's own ending, the `\r` of a CRLF line staying in the line.
    let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
    let (mut lines, ending): (Vec<String>, &str) = if crlf {
        (text.lines().map(String::from).collect(), "\r\n")
    } else if text.is_empty() {
        (Vec::new(), "\n")
    } else {
        let body = text.strip_suffix('\n').unwrap_or(&text);
        (body.split('\n').map(String::from).collect(), "\n")
    };
    let before = lines.clone();
    change(&mut lines)?;
    // Rewriting the same settings would still restart the service that
    // watches the file, and would create a file that was not there.
    if lines == before {
        return Ok(());
    }
    let edited = format!("{}{ending}", lines.join(ending));
    // A table the line editor does not read (an inline table, say) could get
    // a second definition; the file is never left unreadable.
    if let Err(error) = parse(&edited) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, error));
    }
    write_atomically(&path, &edited, permissions)
}

/// The lock beside the file a link points at, so two names for one config
/// share one lock. It is never removed: a writer waiting on a removed lock
/// would hold a lock nobody else sees.
pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(".{name}.lock"))
}

/// Write beside the file and rename over it, keeping `permissions` when the
/// file had some.
///
/// A rename is the one filesystem operation that either happened or did not, so
/// a reader either sees the old configuration or the new one and never half of
/// each, including the reader that is a running service. There is no fsync:
/// this is program state, and losing the last change to a power cut costs
/// repeating it. The temporary name is unique per writer and created new, so
/// no writer ever writes into another's.
pub(crate) fn write_atomically(
    path: &Path,
    text: &str,
    permissions: Option<std::fs::Permissions>,
) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static WRITES: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".{}.{}.{}.new",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml"),
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            match permissions {
                Some(permissions) => file.set_permissions(permissions),
                None => Ok(()),
            }
        })
        .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Set keys inside `table`, adding the table itself when the file has none.
pub(crate) fn set_in_table(lines: &mut Vec<String>, table: &str, values: &[(&str, &str)]) {
    let start = match lines
        .iter()
        .position(|line| header(line).as_deref() == Some(table))
    {
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
pub(crate) fn next_table(lines: &[String], start: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| header(line).map(|_| index))
        .unwrap_or(lines.len())
}

/// The `[table]` or `[[array]]` header on this line, without its trailing
/// comment, the spaces around the name and one pair of quotes around it, so
/// `[[ source ]]` and `[["source"]]` read as `[[source]]`. `None` when the
/// line is not a header.
pub(crate) fn header(line: &str) -> Option<String> {
    let code = without_comment(line).trim_start_matches('\u{feff}').trim();
    let (open, close) = if code.starts_with("[[") && code.ends_with("]]") {
        ("[[", "]]")
    } else if code.starts_with('[') && code.ends_with(']') && code.len() > 1 {
        ("[", "]")
    } else {
        return None;
    };
    let name = code[open.len()..code.len() - close.len()].trim();
    let name = ['"', '\'']
        .into_iter()
        .find_map(|quote| name.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(name);
    Some(format!("{open}{name}{close}"))
}

/// The line up to a `#` comment that is not inside a string.
pub(crate) fn without_comment(line: &str) -> &str {
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
pub(crate) fn upsert_line(
    lines: &mut Vec<String>,
    start: usize,
    end: usize,
    key: &str,
    literal: &str,
) {
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
    // A line of a file that mixes endings keeps its own `\r`.
    let cr = if line.ends_with('\r') { "\r" } else { "" };
    match line.get(without_comment(line).len()..).map(str::trim_end) {
        Some(comment) if !comment.is_empty() => {
            format!("{indent}{key} = {literal}      {comment}{cr}")
        }
        _ => format!("{indent}{key} = {literal}{cr}"),
    }
}

/// One `[[source]]` block by line, paired with the folder TOML reads from the
/// entry in the same place. Where the lines and the parse disagree (an inline
/// array, a header-like line inside a string) the pairing is wrong, and
/// [`edit_lines_to`] refuses the edit built on it.
pub(crate) struct SourceBlock {
    pub(crate) folder: Option<PathBuf>,
    pub(crate) header: usize,
    /// The block's own comments above the header through the line before the
    /// next table's: what leaves with the block.
    pub(crate) whole: std::ops::Range<usize>,
}

impl SourceBlock {
    pub(crate) fn is_folder(&self, path: &Path) -> bool {
        self.folder.as_deref() == Some(path)
    }
}

/// The `[[source]]` blocks in file order.
pub(crate) fn source_blocks(lines: &[String]) -> io::Result<Vec<SourceBlock>> {
    let folders = source_entries(&parsed(lines)?)
        .iter()
        .map(|entry| {
            entry
                .get("path")
                .and_then(toml::Value::as_str)
                .map(PathBuf::from)
        })
        .collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if header(&lines[index]).as_deref() != Some("[[source]]") {
            index += 1;
            continue;
        }
        let next = next_table(lines, index);
        let end = if next < lines.len() {
            owned_comments_start(lines, next)
        } else {
            next
        };
        blocks.push((index, owned_comments_start(lines, index)..end));
        index = next;
    }
    Ok(folders
        .into_iter()
        .zip(blocks)
        .map(|(folder, (header, whole))| SourceBlock {
            folder,
            header,
            whole,
        })
        .collect())
}

/// Where the comments that belong to the table at `header` begin: the comment
/// lines right above it, when a blank line parts them from what comes before.
/// Comments that follow a setting with no blank line are that table's (a
/// setting commented out), and those at the top of the file are the file's.
fn owned_comments_start(lines: &[String], header: usize) -> usize {
    let mut start = header;
    while start > 0 && lines[start - 1].trim_start().starts_with('#') {
        start -= 1;
    }
    if start > 0 && start < header && lines[start - 1].trim().is_empty() {
        start
    } else {
        header
    }
}

/// Write a whole `[[source]]` block at the end of the file.
pub(crate) fn append_source(lines: &mut Vec<String>, source: &SourceEntry) {
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
