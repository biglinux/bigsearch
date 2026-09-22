//! The download list the web browsers keep, read once at start.
//!
//! On a real machine this is where the answer to "where did this file come
//! from?" actually lives. The freedesktop way — a `user.xdg.origin.url`
//! extended attribute on the file — was never implemented by Firefox and was
//! removed from Chrome: on the machine this was written for, exactly one file in
//! the whole Downloads folder carries it, and it is a test fixture. The
//! browser's own list, by contrast, holds thousands of rows, and it is the only
//! place that knows the file saved straight into the home folder came from a
//! particular site.
//!
//! ## Off unless the person asks
//!
//! Reading it is gated on `[origin] browser_history` in the configuration file,
//! which the desktop's search settings window switches, and which is **false**
//! by default. That list belongs to the browser; nobody expects a search service
//! to have read it, so it is only read once somebody says so in as many words.
//!
//! ## What is kept, and what is refused
//!
//! Only the **host** of the address, the date, the kind and the size. Never the
//! path or the query string: on this machine 950 of the recorded download
//! addresses carry a query, and 577 of those carry a signed token — an address
//! that is a credential. A record nobody can leak is better than a record with a
//! good reason for existing.
//!
//! ## The file is never copied
//!
//! The browser's history is opened read-only through SQLite's `immutable=1`,
//! which reads the committed contents without taking a lock and without caring
//! that the browser has the file open. Copying it first was the obvious
//! alternative and it is not affordable: the main profile's history on this
//! machine is 277 MB, and the service is capped at 512 MB of memory.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use crate::origin::OriginWriter;

/// Most rows read from one profile. The newest ones, because the oldest
/// downloads are the least likely to still be on the disk.
const MAX_ROWS: i64 = 5_000;

/// Chromium-family browsers, by the folder each keeps its profiles in.
///
/// All six share one history schema, so the only difference between them is
/// this path. Firefox keeps a different one and is not read here.
const CHROMIUM_BROWSERS: [&str; 6] = [
    "google-chrome",
    "chromium",
    "BraveSoftware/Brave-Browser",
    "microsoft-edge",
    "opera",
    "vivaldi",
];

/// Read every browser's download list into the origin store.
///
/// Returns how many files were recorded. Never fails outward: a browser with a
/// history this cannot read is a browser that contributes nothing, not a reason
/// for the service to stop.
pub fn import(writer: &OriginWriter) -> usize {
    if !crate::origin::settings().enabled || !crate::origin::settings().browser_history {
        return 0;
    }
    let mut recorded = 0;
    for history in history_files() {
        match import_one(writer, &history) {
            Ok(count) => {
                if count > 0 {
                    log::info!("read {count} downloads from {}", history.display());
                }
                recorded += count;
            }
            Err(error) => {
                log::debug!("could not read {}: {error:#}", history.display());
            }
        }
    }
    recorded
}

/// Every `History` file belonging to a Chromium-family profile.
///
/// Named exactly: the profile folder also holds `Cookies`, `Login Data` and
/// `Web Data`, and this must never be a directory walk that could open one of
/// them by accident.
fn history_files() -> Vec<PathBuf> {
    let config = home().join(".config");
    let mut found = Vec::new();
    for browser in CHROMIUM_BROWSERS {
        let root = config.join(browser);
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let history = entry.path().join("History");
            if history.is_file() {
                found.push(history);
            }
        }
    }
    found
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn import_one(writer: &OriginWriter, history: &Path) -> Result<usize> {
    let conn = open_readonly(history)?;
    let mut statement = conn.prepare(
        "
        SELECT COALESCE(NULLIF(d.current_path, ''), d.target_path) AS file,
               c.url,
               d.start_time
          FROM downloads d
          LEFT JOIN downloads_url_chains c
            ON c.id = d.id AND c.chain_index = 0
         WHERE d.state = 1
         ORDER BY d.id DESC
         LIMIT ?1
        ",
    )?;
    let rows = statement.query_map([MAX_ROWS], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    // Collected first, written in one transaction: a row at a time is a commit
    // at a time, and this list runs to thousands.
    let mut downloads = Vec::new();
    for row in rows.flatten() {
        let (file, url, start_time) = row;
        let path = PathBuf::from(&file);
        // A download the person has since deleted or moved elsewhere is a row
        // about a file that is not there; recording it would put an origin on
        // whatever takes that name next.
        if file.is_empty() || !path.is_file() {
            continue;
        }
        let Some(domain) = url.as_deref().and_then(domain_of) else {
            continue;
        };
        downloads.push((path, domain, webkit_epoch_to_unix(start_time)));
    }
    writer.register_web_downloads_at(&downloads)
}

/// Open a browser's history without copying it and without disturbing it.
///
/// `immutable=1` promises SQLite the file will not change underneath it, which
/// is how it reads a database another process has open without taking a lock.
/// The browser writes through a rollback journal, so what this sees is the last
/// committed state; a read that lands mid-write comes back as an error and the
/// next start tries again.
fn open_readonly(path: &Path) -> Result<Connection> {
    let uri = format!("file:{}?immutable=1", path.display());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(Into::into)
}

/// Chromium counts microseconds since 1601; the rest of the world counts
/// seconds since 1970.
fn webkit_epoch_to_unix(microseconds: i64) -> i64 {
    const EPOCH_DIFFERENCE_SECONDS: i64 = 11_644_473_600;
    if microseconds <= 0 {
        return 0;
    }
    microseconds / 1_000_000 - EPOCH_DIFFERENCE_SECONDS
}

/// The host an address names, and nothing else from it.
///
/// The path and the query are deliberately dropped and never reach the store:
/// most of the recorded addresses carry a signed token, and a database of file
/// origins must not double as a database of credentials.
fn domain_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("blob:").unwrap_or(url);
    let (_, after_scheme) = rest.split_once("://")?;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date a browser records is the date the person sees.
    #[test]
    fn a_browser_timestamp_becomes_a_real_date() {
        // A real row from this machine's Chrome history: 2026-09-03 03:49:59
        // UTC, which is 1 788 407 399 seconds since 1970.
        assert_eq!(webkit_epoch_to_unix(13_432_880_999_211_718), 1_788_407_399);
        assert_eq!(webkit_epoch_to_unix(0), 0);
        assert_eq!(webkit_epoch_to_unix(-5), 0);
    }

    /// Only the site survives; the credential in the address does not.
    #[test]
    fn only_the_site_is_kept_from_an_address() {
        assert_eq!(
            domain_of("https://chatgpt.com/backend-api/estuary/content?id=file_00&sig=4c69600f")
                .as_deref(),
            Some("chatgpt.com")
        );
        assert_eq!(
            domain_of("blob:https://web.whatsapp.com/a9c27299").as_deref(),
            Some("web.whatsapp.com")
        );
        assert_eq!(
            domain_of("https://user:secret@files.example.org:8443/x.zip").as_deref(),
            Some("files.example.org")
        );
        assert_eq!(domain_of("about:blank"), None);
    }

    /// Only a file named exactly `History` is ever opened.
    #[test]
    fn nothing_but_the_history_file_is_looked_for() {
        for browser in CHROMIUM_BROWSERS {
            assert!(!browser.contains(".."), "{browser}");
        }
        let listed = history_files();
        assert!(
            listed
                .iter()
                .all(|path| path.file_name().is_some_and(|name| name == "History")),
            "{listed:?}"
        );
    }
}
