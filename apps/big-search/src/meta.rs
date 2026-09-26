//! Optional metadata enrichment: content-based file **kind** plus media/image
//! metadata, indexed as searchable `body` text (no schema change, no new wire
//! fields). Every parser is pure safe Rust — `symphonia` is `#![forbid(unsafe)]`,
//! `file-format`/`imagesize` are pure Rust — so a malicious file cannot trigger
//! the C memory-corruption class that forces other indexers to sandbox poppler/
//! gstreamer. Work is still bounded (max file size) and runs only in the
//! throttled background pass. Each feature is individually switchable by env so
//! the daemon can be tuned (and a parser disabled instantly if a CVE lands).
//!
//! Env (all default on; `0`/`false`/`no`/`off` disables):
//!   BIG_SEARCH_META            master switch
//!   BIG_SEARCH_META_SNIFF      file-format content sniff for extension-less files
//!   BIG_SEARCH_META_AUDIO      audio duration + tags (symphonia)
//!   BIG_SEARCH_META_VIDEO      mkv/webm/mp4 duration (symphonia)
//!   BIG_SEARCH_META_IMAGE      image dimensions (imagesize)
//!   BIG_SEARCH_META_MAX_MB     skip metadata parse above this size (default 64)
use std::path::Path;
use std::sync::OnceLock;

/// Per-feature switches + bounds, read once from the environment.
#[derive(Clone, Copy, Debug)]
pub struct MetaConfig {
    pub sniff_type: bool,
    pub audio: bool,
    pub video: bool,
    pub image: bool,
    pub max_bytes: u64,
    /// True if any extraction feature is enabled (fast bail).
    pub any: bool,
}

impl MetaConfig {
    fn from_env() -> Self {
        let on = |key: &str, default: bool| match std::env::var(key) {
            Ok(v) => !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            Err(_) => default,
        };
        let master = on("BIG_SEARCH_META", true);
        let sniff_type = master && on("BIG_SEARCH_META_SNIFF", true);
        let audio = master && on("BIG_SEARCH_META_AUDIO", true);
        let video = master && on("BIG_SEARCH_META_VIDEO", true);
        let image = master && on("BIG_SEARCH_META_IMAGE", true);
        let max_mb: u64 = std::env::var("BIG_SEARCH_META_MAX_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64);
        Self {
            sniff_type,
            audio,
            video,
            image,
            max_bytes: max_mb.saturating_mul(1 << 20),
            any: sniff_type || audio || video || image,
        }
    }
}

/// Process-wide config (env read once).
pub fn config() -> &'static MetaConfig {
    static META_CONFIG: OnceLock<MetaConfig> = OnceLock::new();
    META_CONFIG.get_or_init(MetaConfig::from_env)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Audio,
    Video,
    Image,
    Document,
    Archive,
    Other,
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Audio => "audio",
        Kind::Video => "video",
        Kind::Image => "image",
        Kind::Document => "document",
        Kind::Archive => "archive",
        Kind::Other => "file",
    }
}

fn ext_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

fn kind_from_ext(ext: &str) -> Option<Kind> {
    const VIDEO_EXTENSIONS: &[&str] = &[
        "3gp", "asf", "avi", "divx", "f4v", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg",
        "mpg", "mts", "ogv", "ts", "vob", "webm", "wmv",
    ];
    const AUDIO_EXTENSIONS: &[&str] = &[
        "aac", "ac3", "aif", "aiff", "ape", "dts", "flac", "m4a", "mka", "mp3", "oga", "ogg",
        "opus", "wav", "wma", "wv",
    ];
    const IMAGE_EXTENSIONS: &[&str] = &[
        "avif", "bmp", "dib", "exr", "gif", "heic", "heif", "ico", "jpe", "jpeg", "jpg", "pam",
        "pbm", "pgm", "png", "pnm", "ppm", "qoi", "svg", "tga", "tif", "tiff", "webp",
    ];
    // `alac` is not a container, so it never reached this arm.
    Some(if AUDIO_EXTENSIONS.contains(&ext) {
        Kind::Audio
    } else if VIDEO_EXTENSIONS.contains(&ext) {
        Kind::Video
    } else if IMAGE_EXTENSIONS.contains(&ext) {
        Kind::Image
    } else {
        return None;
    })
}

fn kind_from_mime(mime: &str) -> Kind {
    let major = mime.split('/').next().unwrap_or("");
    match major {
        "audio" => Kind::Audio,
        "video" => Kind::Video,
        "image" => Kind::Image,
        _ if mime == "application/pdf" || mime.starts_with("text/") => Kind::Document,
        _ if mime.contains("zip") || mime.contains("tar") || mime.contains("compress") => {
            Kind::Archive
        }
        _ => Kind::Other,
    }
}

/// Whether the background pass should enrich `path` with metadata: known media/
/// image extensions, or any extension-less file when sniffing is on (the cheap
/// path that catches e.g. a PDF saved without a `.pdf` suffix).
pub fn is_meta_candidate(path: &Path) -> bool {
    let metadata_config = config();
    if !metadata_config.any {
        return false;
    }
    // A tagged file is a candidate whatever it is: the tag is the only thing
    // making it findable, and it lives on the file rather than in it.
    if !read_tags(path).is_empty() {
        return true;
    }
    match ext_of(path).as_deref() {
        Some(ext) => matches!(
            kind_from_ext(ext),
            Some(Kind::Audio | Kind::Video | Kind::Image)
        ),
        None => metadata_config.sniff_type,
    }
}

/// The extended attribute KDE, and now this desktop, store user tags in.
const TAGS_XATTR: &str = "user.xdg.tags";
/// Longest tag list this will read. A tag list is a handful of words; anything
/// larger is not one.
const MAX_TAGS_BYTES: usize = 4 * 1024;
/// The prefix a tag carries in the index.
///
/// Reserved on purpose: indexing the bare word would make a document *about*
/// holidays match a search for files *tagged* holidays, in both directions.
pub const TAG_TOKEN_PREFIX: &str = "xdgtag";

/// The tags on `path`, as the person wrote them.
#[must_use]
pub fn read_tags(path: &Path) -> Vec<String> {
    let Some(raw) = read_xattr(path, TAGS_XATTR) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(raw) else {
        return Vec::new();
    };
    text.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One extended attribute, bounded, or `None` when it is absent or too large.
fn read_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let name = std::ffi::CString::new(name).ok()?;
    let mut buffer = vec![0_u8; MAX_TAGS_BYTES];
    // SAFETY: both strings are NUL-terminated and the buffer is `len` bytes.
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    buffer.truncate(usize::try_from(read).ok()?);
    Some(buffer)
}

/// The tags of `path` as the tokens the index stores, or `None` when it has
/// none.
#[must_use]
pub fn tag_text(path: &Path) -> Option<String> {
    let tags = read_tags(path);
    if tags.is_empty() {
        return None;
    }
    Some(
        tags.iter()
            .map(|tag| format!("{TAG_TOKEN_PREFIX}{}", normalize_tag(tag)))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// A tag as it is searched: lowercased, and with the separators the tokenizer
/// would split on removed, so "Imposto 2026" is one token either way.
#[must_use]
pub fn normalize_tag(tag: &str) -> String {
    tag.to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

/// Searchable metadata text for `path` (kind keyword + media/image facts), or
/// `None` when nothing applies. Indexed into `body`, so `big-search content
/// <artist|1920x1080|video|…>` finds it.
pub fn metadata_text(path: &Path) -> Option<String> {
    let metadata_config = config();
    if !metadata_config.any {
        return None;
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > metadata_config.max_bytes {
        return None;
    }

    // The tags come first and are kept whatever the file turns out to be: a
    // tagged file with no readable metadata is still findable by its tag.
    let tags = tag_text(path);
    let ext = ext_of(path);
    let mut kind = ext.as_deref().and_then(kind_from_ext);
    let mut sniffed_mime: Option<String> = None;
    if kind.is_none()
        && metadata_config.sniff_type
        && let Ok(fmt) = file_format::FileFormat::from_file(path)
    {
        let mime = fmt.media_type();
        if mime != "application/octet-stream" {
            kind = Some(kind_from_mime(mime));
            sniffed_mime = Some(mime.to_string());
        }
    }
    let Some(kind) = kind else {
        // No kind, but tags: index those alone.
        return tags;
    };

    let mut parts: Vec<String> = tags.into_iter().collect();
    parts.push(kind_word(kind).to_string());
    match kind {
        Kind::Audio if metadata_config.audio => parts.extend(media_text(path)),
        Kind::Video if metadata_config.video => parts.extend(media_text(path)),
        Kind::Image if metadata_config.image => parts.extend(image_text(path)),
        // Extension-less document found by sniffing → extract its text so it is
        // searchable like a normally-suffixed file would be.
        Kind::Document => {
            if let Some(mime) = sniffed_mime.as_deref()
                && let Some(text) = crate::extract::content_for_mime(path, mime)
            {
                parts.push(text);
            }
        }
        _ => {}
    }
    if parts.len() == 1 && kind == Kind::Other {
        return None;
    }
    // Every path through `extract::content` caps its output; this one did not.
    // Only the *input* was bounded (64 MiB), and container tag values are pushed
    // verbatim, so one crafted file with a huge comment frame put tens of
    // megabytes into a single tantivy document.
    let mut text = parts.join(" ");
    let cap = crate::extract::max_extract_bytes();
    if text.len() > cap {
        crate::extract::truncate_to_char_boundary(&mut text, cap);
    }
    Some(text)
}

/// Audio/video facts via symphonia: container tag values (artist/album/title/…)
/// and duration in seconds. Probing reads the container header only — no decode.
fn media_text(path: &Path) -> Vec<String> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::units::{Duration, Timestamp};

    let mut out = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext_of(path) {
        hint.with_extension(&ext);
    }
    let Ok(mut format_reader) = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) else {
        return out;
    };

    let push_tags = |rev: &symphonia::core::meta::MetadataRevision, out: &mut Vec<String>| {
        for tag in &rev.media.tags {
            let value = tag.raw.value.to_string();
            if !value.is_empty() {
                out.push(value);
            }
        }
    };
    if let Some(rev) = format_reader.metadata().current() {
        push_tags(rev, &mut out);
    }
    let duration_track = format_reader
        .default_track(TrackType::Audio)
        .or_else(|| format_reader.default_track(TrackType::Video));
    if let Some(track) = duration_track
        && let Some(time_base) = track.time_base
    {
        let duration = track
            .duration
            .or_else(|| track.num_frames.map(Duration::new));
        if let Some(seconds) = duration
            .and_then(|duration| i64::try_from(duration.get()).ok())
            .and_then(|duration_ticks| time_base.calc_time(Timestamp::new(duration_ticks)))
            .map(|time| time.as_secs())
            && seconds > 0
        {
            out.push(seconds.to_string());
        }
    }
    out
}

/// Image dimensions via imagesize (header only, no decode).
fn image_text(path: &Path) -> Vec<String> {
    match imagesize::size(path) {
        Ok(size) => vec![format!("{}x{}", size.width, size.height)],
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;

    /// Set one extended attribute, reporting whether the filesystem accepted it.
    fn write_xattr(path: &Path, name: &str, value: &[u8]) -> bool {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        let Ok(name) = std::ffi::CString::new(name) else {
            return false;
        };
        // SAFETY: both strings are NUL-terminated and the value slice is `len`.
        let written = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        written == 0
    }

    /// A tag is stored under a reserved prefix so a document that merely
    /// *mentions* the word cannot answer a search for files *tagged* with it.
    #[test]
    fn tags_are_indexed_as_reserved_tokens() {
        assert_eq!(normalize_tag("Imposto 2026"), "imposto2026");
        assert_eq!(normalize_tag("férias-verão"), "fériasverão");
        assert_eq!(normalize_tag("  "), "");
    }

    #[test]
    fn a_tag_list_is_read_as_the_person_wrote_it() {
        let dir = std::env::temp_dir().join(format!("big-search-tags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        let file = dir.join("tagged.bin");
        std::fs::write(&file, b"contents").expect("write");
        assert!(read_tags(&file).is_empty(), "no attribute, no tags");
        assert_eq!(tag_text(&file), None);

        // Written the way the desktop writes it: comma separated, with spaces.
        // Through the syscall rather than a helper program, because the read
        // under test is the same syscall's counterpart.
        if write_xattr(&file, TAGS_XATTR, "Férias, Imposto 2026,, ".as_bytes()) {
            assert_eq!(read_tags(&file), ["Férias", "Imposto 2026"]);
            assert_eq!(
                tag_text(&file).as_deref(),
                Some("xdgtagférias xdgtagimposto2026")
            );
            assert!(
                is_meta_candidate(&file),
                "a tagged file must be indexed whatever its type"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_ext_maps_media() {
        assert_eq!(kind_from_ext("mp3"), Some(Kind::Audio));
        assert_eq!(kind_from_ext("mkv"), Some(Kind::Video));
        assert_eq!(kind_from_ext("mp4"), Some(Kind::Video));
        assert_eq!(kind_from_ext("png"), Some(Kind::Image));
        assert_eq!(kind_from_ext("jpeg"), Some(Kind::Image));
        assert_eq!(kind_from_ext("txt"), None); // text handled by the content pass
        assert_eq!(kind_from_ext("pdf"), None); // ditto
    }

    #[test]
    fn kind_from_mime_maps_major_type() {
        assert_eq!(kind_from_mime("audio/mpeg"), Kind::Audio);
        assert_eq!(kind_from_mime("video/x-matroska"), Kind::Video);
        assert_eq!(kind_from_mime("image/png"), Kind::Image);
        assert_eq!(kind_from_mime("application/pdf"), Kind::Document);
        assert_eq!(kind_from_mime("text/plain"), Kind::Document);
        assert_eq!(kind_from_mime("application/zip"), Kind::Archive);
        assert_eq!(kind_from_mime("application/x-spurious"), Kind::Other);
    }

    /// A minimal PNG (signature + IHDR only) so `imagesize` reads dimensions from
    /// the header without any decode — proves the image metadata path end to end.
    fn minimal_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth, colour type, etc.
        png.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder (imagesize ignores it)
        png
    }

    #[test]
    fn image_text_reads_png_dimensions() {
        let dir = std::env::temp_dir().join(format!("bsmeta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pic.png");
        std::fs::write(&path, minimal_png(320, 240)).unwrap();
        assert_eq!(image_text(&path), vec!["320x240".to_string()]);
        std::fs::remove_file(&path).ok();
    }
}
