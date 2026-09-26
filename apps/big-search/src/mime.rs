//! A file's media type, as the desktop's own database has it.
//!
//! The name decides whenever the shared-mime-info globs give it one type: that
//! costs a map lookup and no read, which is the case for nearly every file.
//! Only a name with no type, or with several (`.ts` is a video, TypeScript or a
//! Qt translation), has its first bytes read — `file-format`, pure Rust like
//! every parser here — and among the name's candidates the one the content is,
//! or descends from most closely, wins: text makes a `.ts` TypeScript, a
//! transport stream makes it a video. Directories, empty files and anything not
//! a regular file are never opened (a FIFO would block the scan).
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Bytes read to tell a file's type: past the deepest signature `file-format`
/// looks for (an ISO 9660 volume's, at 32 769).
const HEAD_BYTES: u64 = 40 * 1024;

struct Glob {
    weight: u32,
    mime: String,
}

#[derive(Default)]
struct Database {
    /// `*.suffix` patterns, lowercase suffix → the types it names.
    suffixes: HashMap<String, Vec<Glob>>,
    /// The case-sensitive ones (`cs` flag), matched first: `*.c` is C,
    /// `*.C` C++, where case-folded they are one ambiguous suffix.
    cased_suffixes: HashMap<String, Vec<Glob>>,
    /// Whole names (`Makefile`), matched exactly.
    literals: HashMap<String, Vec<Glob>>,
    parents: HashMap<String, Vec<String>>,
    aliases: HashMap<String, String>,
}

impl Database {
    fn load() -> Self {
        let mut db = Self::default();
        for dir in data_dirs() {
            let dir = dir.join("mime");
            if let Ok(text) = std::fs::read_to_string(dir.join("globs2")) {
                db.add_globs(&text);
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("subclasses")) {
                for (child, parent) in text.lines().filter_map(|line| line.split_once(' ')) {
                    db.parents
                        .entry(child.to_string())
                        .or_default()
                        .push(parent.to_string());
                }
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("aliases")) {
                for (alias, canonical) in text.lines().filter_map(|line| line.split_once(' ')) {
                    db.aliases.insert(alias.to_string(), canonical.to_string());
                }
            }
        }
        db
    }

    /// `weight:type:pattern[:flags]` lines. Only plain suffixes and whole names
    /// are kept: the other shapes are a handful of rare types, and matching
    /// them would cost every file a pattern walk.
    fn add_globs(&mut self, text: &str) {
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let mut fields = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let cased = fields
                .next()
                .is_some_and(|flags| flags.split(',').any(|flag| flag == "cs"));
            let Ok(weight) = weight.parse() else {
                continue;
            };
            if mime == "__NOGLOBS__" {
                continue;
            }
            let glob = Glob {
                weight,
                mime: mime.to_string(),
            };
            let wild = |s: &str| s.contains(['*', '?', '[']);
            if let Some(suffix) = pattern.strip_prefix("*.")
                && !wild(suffix)
            {
                let globs = if cased {
                    self.cased_suffixes.entry(suffix.to_string()).or_default()
                } else {
                    self.suffixes.entry(suffix.to_lowercase()).or_default()
                };
                if !globs.iter().any(|known| known.mime == glob.mime) {
                    globs.push(glob);
                }
            } else if !wild(pattern) {
                self.literals
                    .entry(pattern.to_string())
                    .or_default()
                    .push(glob);
            }
        }
    }

    /// The types `name` may be: a whole-name match, else its longest suffix's,
    /// the case-sensitive patterns first.
    fn candidates(&self, name: &str) -> &[Glob] {
        if let Some(globs) = self.literals.get(name) {
            return globs;
        }
        if let Some(globs) = name
            .match_indices('.')
            .find_map(|(dot, _)| self.cased_suffixes.get(&name[dot + 1..]))
        {
            return globs;
        }
        let lower = name.to_lowercase();
        lower
            .match_indices('.')
            .find_map(|(dot, _)| self.suffixes.get(&lower[dot + 1..]))
            .map_or(&[], Vec::as_slice)
    }

    fn canonical<'a>(&'a self, mime: &'a str) -> &'a str {
        self.aliases.get(mime).map_or(mime, String::as_str)
    }

    /// How many subclass steps lead from `mime` up to `ancestor`, if any do.
    fn distance(&self, mime: &str, ancestor: &str) -> Option<usize> {
        let mut level = vec![self.canonical(mime).to_string()];
        for steps in 0..8 {
            if level.iter().any(|mime| mime == ancestor) {
                return Some(steps);
            }
            level = level
                .iter()
                .filter_map(|mime| self.parents.get(mime))
                .flatten()
                .map(|parent| self.canonical(parent).to_string())
                .collect();
        }
        None
    }

    fn of(&self, path: &Path, known: Option<&std::fs::Metadata>) -> Option<String> {
        // A link is typed as what it points to.
        let followed;
        let known = match known {
            Some(metadata) if metadata.is_symlink() => {
                followed = std::fs::metadata(path).ok()?;
                Some(&followed)
            }
            known => known,
        };
        // `conf.d` is a directory, not D source, and an empty `.mp4` no video.
        if known.is_some_and(std::fs::Metadata::is_dir) {
            return Some("inode/directory".to_string());
        }
        if known.is_some_and(|metadata| metadata.is_file() && metadata.len() == 0) {
            return Some("application/x-zerosize".to_string());
        }
        let name = path.file_name()?.to_str()?;
        let candidates = self.candidates(name);
        if let [only] = candidates {
            return Some(only.mime.clone());
        }
        let stat;
        let metadata = match known {
            Some(metadata) => metadata,
            None => {
                stat = std::fs::metadata(path).ok()?;
                &stat
            }
        };
        if metadata.is_dir() {
            return Some("inode/directory".to_string());
        }
        if !metadata.is_file() {
            return None;
        }
        if metadata.len() == 0 {
            return Some("application/x-zerosize".to_string());
        }
        // Between equal weights, the database's own order (`max_by_key`
        // alone keeps the last).
        let heaviest = || {
            candidates
                .iter()
                .rev()
                .max_by_key(|glob| glob.weight)
                .map(|glob| glob.mime.clone())
        };
        let Some(content) = sniff(path) else {
            return heaviest().or_else(|| Some("application/octet-stream".to_string()));
        };
        let content = self.canonical(&content).to_string();
        // The candidate the content is, or descends from most closely; the
        // name's weight breaks a tie. Content that confirms none is what the
        // file is: a `.png` that holds a JPEG, or HTML from a failed download.
        candidates
            .iter()
            .filter_map(|glob| {
                self.distance(&glob.mime, &content)
                    .map(|steps| (steps, std::cmp::Reverse(glob.weight), &glob.mime))
            })
            .min()
            .map(|(_, _, mime)| mime.clone())
            .or(Some(content))
    }
}

/// What the first bytes say, or `None` when they say nothing (unknown binary,
/// or unreadable).
fn sniff(path: &Path) -> Option<String> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(HEAD_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    let format = file_format::FileFormat::from_bytes(&head);
    let mime = format.media_type();
    // Text is UTF-8 without a NUL; the head may end inside a character.
    let text = !head.contains(&0)
        && std::str::from_utf8(&head).map_or_else(|e| e.error_len().is_none(), |_| true);
    // A media signature inside text is a coincidence — "### freedreno" holds
    // QuickTime's `free` atom at byte 4 — for 40 KiB of real audio, video or a
    // raster image always hold a NUL. Other signatures found in text (PGP
    // armour, HTML, SVG) are what the text is.
    let media = ["video/", "audio/", "image/"]
        .iter()
        .any(|major| mime.starts_with(major))
        && !mime.ends_with("+xml");
    if text && (media || mime == "application/octet-stream") {
        return Some("text/plain".to_string());
    }
    (mime != "application/octet-stream").then(|| mime.to_string())
}

/// Where shared-mime-info lives: the user's data dir, then the system's.
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    home.into_iter()
        .chain(system.split(':').map(PathBuf::from))
        .collect()
}

fn database() -> &'static Database {
    static DATABASE: OnceLock<Database> = OnceLock::new();
    DATABASE.get_or_init(Database::load)
}

/// The media type of `path`, or `None` when it has none to tell (a FIFO, a
/// device, a path that is gone). `metadata`, when the caller has it (the
/// scan's walk does), saves a `stat` and tells a directory with a dotted name
/// from a file.
#[must_use]
pub fn of(path: &Path, metadata: Option<&std::fs::Metadata>) -> Option<String> {
    database().of(path, metadata)
}

/// Whether `mime` is what `wanted` asks for: the same type, or `major/*`.
#[must_use]
pub fn matches(mime: &str, wanted: &str) -> bool {
    match wanted.strip_suffix("/*") {
        Some(major) => mime.split_once('/').is_some_and(|(m, _)| m == major),
        None => mime == wanted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Database {
        let mut db = Database::default();
        db.add_globs(concat!(
            "# comment\n",
            "50:video/mp2t:*.ts\n",
            "50:text/vnd.trolltech.linguist:*.ts\n",
            "40:application/typescript:*.ts\n",
            "50:video/mp4:*.mp4\n",
            "50:application/x-compressed-tar:*.tar.gz\n",
            "50:application/gzip:*.gz\n",
            "50:text/x-makefile:Makefile\n",
            "50:text/x-dsrc:*.d\n",
            "50:text/x-c++src:*.C:cs\n",
            "50:text/x-c++src:*.C\n",
            "50:text/x-csrc:*.c:cs\n",
            "50:text/x-csrc:*.c\n",
            "10:text/x-readme:README*\n",
        ));
        for (child, parent) in [
            ("text/vnd.trolltech.linguist", "application/xml"),
            ("application/xml", "text/plain"),
            ("application/typescript", "text/plain"),
        ] {
            db.parents
                .entry(child.into())
                .or_default()
                .push(parent.into());
        }
        db.aliases
            .insert("text/xml".into(), "application/xml".into());
        db
    }

    #[test]
    fn a_name_with_one_type_is_not_read() {
        let db = database();
        // The path does not exist: a read would fail and give no type.
        let missing = Path::new("/nowhere/Film.MP4");
        assert_eq!(db.of(missing, None).as_deref(), Some("video/mp4"));
        assert_eq!(
            db.of(Path::new("/nowhere/a.tar.gz"), None).as_deref(),
            Some("application/x-compressed-tar")
        );
        assert_eq!(
            db.of(Path::new("/nowhere/Makefile"), None).as_deref(),
            Some("text/x-makefile")
        );
        assert_eq!(db.of(Path::new("/nowhere/unknown"), None), None);
        assert_eq!(
            db.of(Path::new("/nowhere/noop.c"), None).as_deref(),
            Some("text/x-csrc")
        );
        assert_eq!(
            db.of(Path::new("/nowhere/Widget.C"), None).as_deref(),
            Some("text/x-c++src")
        );
    }

    #[test]
    fn an_ambiguous_or_missing_extension_is_told_by_the_content() {
        let db = database();
        let dir = std::env::temp_dir().join(format!("lsearch-mime-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let mut stream = vec![0_u8; 188 * 4];
        stream.iter_mut().step_by(188).for_each(|byte| *byte = 0x47);
        assert_eq!(
            db.of(&write("clip.ts", &stream), None).as_deref(),
            Some("video/mp2t")
        );
        assert_eq!(
            db.of(
                &write("app.ts", b"export const answer: number = 42;\n"),
                None
            )
            .as_deref(),
            Some("application/typescript")
        );
        assert_eq!(
            db.of(
                &write(
                    "pt_BR.ts",
                    b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<TS version=\"2.1\"></TS>\n"
                ),
                None
            )
            .as_deref(),
            Some("text/vnd.trolltech.linguist")
        );
        // No extension at all: the content alone.
        assert_eq!(
            db.of(&write("clip", &stream), None).as_deref(),
            Some("video/mp2t")
        );
        assert_eq!(
            db.of(&write("notes", "olá, mundo\n".as_bytes()), None)
                .as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            db.of(&write("blob", &[0, 1, 2, 3, 0xff]), None).as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            db.of(&write("empty", b""), None).as_deref(),
            Some("application/x-zerosize")
        );
        // QuickTime's `free` atom at byte 4 of a text is not a video.
        assert_eq!(
            db.of(&write("NOTES", b"### freedreno backend\n"), None)
                .as_deref(),
            Some("text/plain")
        );
        // Content that confirms no candidate is what the file is.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend([0_u8; 64]);
        assert_eq!(
            db.of(&write("odd.ts", &png), None).as_deref(),
            Some("image/png")
        );
        assert_eq!(db.of(&dir, None).as_deref(), Some("inode/directory"));
        let dotted = dir.join("conf.d");
        std::fs::create_dir_all(&dotted).unwrap();
        let metadata = std::fs::symlink_metadata(&dotted).unwrap();
        assert_eq!(
            db.of(&dotted, Some(&metadata)).as_deref(),
            Some("inode/directory")
        );
        let hollow = write("hollow.mp4", b"");
        let metadata = std::fs::metadata(&hollow).unwrap();
        assert_eq!(
            db.of(&hollow, Some(&metadata)).as_deref(),
            Some("application/x-zerosize")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_type_matches_itself_or_its_group() {
        assert!(matches("video/mp2t", "video/*"));
        assert!(matches("video/mp2t", "video/mp2t"));
        assert!(!matches("application/typescript", "video/*"));
        assert!(!matches("videos/x", "video/*"));
    }
}
