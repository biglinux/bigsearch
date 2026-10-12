//! Where a finished download lands, and the site it came from.
//!
//! Browsers following the freedesktop convention write the source address into
//! the `user.xdg.origin.url` extended attribute. [`super::OriginWriter`] reads
//! it for a regular file sitting directly in the downloads folder, and
//! `browser_downloads` passes the addresses from a browser's own list through
//! [`domain_from_url`]. Only the host of an address is ever kept.
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(super) fn should_probe_xdg_origin(path: &Path) -> bool {
    should_probe_xdg_origin_in_downloads(path, &downloads_dir())
}

fn should_probe_xdg_origin_in_downloads(path: &Path, downloads: &Path) -> bool {
    if !is_regular_file(path) {
        return false;
    }
    looks_like_completed_download(path, downloads)
}

fn looks_like_completed_download(path: &Path, downloads: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            !name.ends_with(".crdownload")
                && !name.ends_with(".part")
                && !name.ends_with(".download")
                && path.parent().is_some_and(|parent| parent == downloads)
        })
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn downloads_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_DOWNLOAD_DIR").map(PathBuf::from) {
        return expand_home(path);
    }
    let user_dirs = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|config| config.join("user-dirs.dirs"));
    if let Some(path) = user_dirs.and_then(|path| parse_user_download_dir(&path)) {
        return path;
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("/Downloads"))
}

fn parse_user_download_dir(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let value = value.trim_matches('"');
        return Some(expand_user_dir_value(value));
    }
    None
}

fn expand_user_dir_value(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("$HOME/") {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(rest));
    }
    expand_home(PathBuf::from(value))
}

fn expand_home(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or(path);
    }
    path
}

pub(super) fn read_xdg_origin_domain(path: &Path) -> Option<String> {
    let path_c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let attr_c = CString::new("user.xdg.origin.url").ok()?;
    let mut buffer = vec![0u8; 4096];
    // SAFETY: `path_c` and `attr_c` are NUL-terminated C strings; `buffer` points
    // to valid writable memory for its declared length. getxattr does not retain
    // any pointer after returning.
    let read = unsafe {
        libc::getxattr(
            path_c.as_ptr(),
            attr_c.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    buffer.truncate(read as usize);
    let url = std::str::from_utf8(&buffer).ok()?.trim();
    domain_from_url(url)
}

/// The host an address names, and nothing else from it.
///
/// The path and the query never reach the store: most recorded download
/// addresses carry a signed token, and a database of file origins must not
/// double as a database of credentials. The address comes from a browser's
/// list or a file's xattr, so anything but a plain host name or IP literal is
/// refused rather than stored and later printed.
pub(crate) fn domain_from_url(url: &str) -> Option<String> {
    let url = url.strip_prefix("blob:").unwrap_or(url);
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(rest) = authority.strip_prefix('[') {
        let literal = rest.split(']').next()?;
        let valid = literal.len() <= 45
            && literal
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.');
        valid.then_some(literal)?
    } else {
        let name = authority.split(':').next()?;
        let name = name.strip_prefix("www.").unwrap_or(name);
        let valid = name.len() <= 253
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte));
        valid.then_some(name)?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{ENV_LOCK, EnvVarGuard};

    /// Only the site survives: no credential, path or query, and nothing that
    /// is not a host name or IP literal.
    #[test]
    fn address_keeps_only_the_site() {
        let cases = [
            ("https://Example.com/files/a.pdf", Some("example.com")),
            ("https://user@[2001:db8::1]/a", Some("2001:db8::1")),
            (
                "https://chatgpt.com/backend-api/estuary/content?id=file_00&sig=4c69600f",
                Some("chatgpt.com"),
            ),
            (
                "blob:https://web.whatsapp.com/a9c27299",
                Some("web.whatsapp.com"),
            ),
            (
                "https://user:secret@www.example.org:8443/x.zip",
                Some("example.org"),
            ),
            ("https://\x1b]0;evil\x07/", None),
            ("https://a b.example/", None),
            ("https://[::1\x1b]/", None),
            ("about:blank", None),
            ("not a url", None),
        ];
        for (url, host) in cases {
            assert_eq!(domain_from_url(url).as_deref(), host, "{url:?}");
        }
        let long = format!("https://{}.example/", "a".repeat(250));
        assert_eq!(domain_from_url(&long), None);
    }

    #[test]
    fn downloads_default_to_home_downloads() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _home = EnvVarGuard::set("HOME", "/tmp/bs-origin-home");
        let _config = EnvVarGuard::unset("XDG_CONFIG_HOME");
        let _download = EnvVarGuard::unset("XDG_DOWNLOAD_DIR");
        assert_eq!(
            downloads_dir(),
            PathBuf::from("/tmp/bs-origin-home/Downloads")
        );
    }

    #[test]
    fn probe_accepts_completed_direct_downloads() {
        let base =
            std::env::temp_dir().join(format!("bs-origin-download-probe-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let downloads = base.join("Downloads");
        let nested = downloads.join("project");
        std::fs::create_dir_all(&nested).unwrap();
        let direct = downloads.join("recipe.pdf");
        let partial = downloads.join("recipe.pdf.crdownload");
        let nested_file = nested.join("package.json");
        std::fs::write(&direct, b"download").unwrap();
        std::fs::write(&partial, b"partial").unwrap();
        std::fs::write(&nested_file, b"nested").unwrap();

        assert!(should_probe_xdg_origin_in_downloads(&direct, &downloads));
        assert!(!should_probe_xdg_origin_in_downloads(&partial, &downloads));
        assert!(!should_probe_xdg_origin_in_downloads(
            &nested_file,
            &downloads
        ));
        assert!(!should_probe_xdg_origin_in_downloads(&nested, &downloads));
        std::fs::remove_dir_all(&base).ok();
    }
}
