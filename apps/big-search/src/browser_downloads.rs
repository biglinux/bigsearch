//! The download list the web browsers keep, read once at start.
//!
//! On a real machine this is where the answer to "where did this file come
//! from?" actually lives. The freedesktop way, a `user.xdg.origin.url`
//! extended attribute on the file, was never implemented by Firefox and was
//! removed from Chrome, so almost no file in a Downloads folder carries it. The
//! browser's own list holds thousands of rows, and it is the only place that
//! knows the file saved straight into the home folder came from a particular
//! site.
//!
//! ## Off unless the person asks
//!
//! Reading it is gated on `[origin] browser_history` in the configuration file,
//! which the desktop's search settings window switches, and which is false
//! by default. That list belongs to the browser; nobody expects a search service
//! to have read it, so it is only read once somebody says so in as many words.
//!
//! ## What is kept, and what is refused
//!
//! Only the host of the address, the date, the kind and the size. Never the
//! path or the query string: many download addresses carry a signed token in
//! the query, which makes the address a credential. A record nobody can leak
//! is better than a record with a good reason for existing.
//!
//! ## The file is never copied
//!
//! The browser's history is opened read-only through SQLite's `immutable=1`,
//! which reads it without taking a lock and without caring that the browser
//! has the file open. Copying it first was the obvious alternative and it is
//! not affordable: the main profile's history on this machine is 277 MB, and
//! the service is capped at 512 MB of memory.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::origin::{OriginWriter, domain_from_url};

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
    let before = std::fs::metadata(history)?;
    let conn = open_readonly(history)?;
    let mut statement = conn.prepare(
        "
        SELECT COALESCE(NULLIF(d.current_path, ''), d.target_path) AS file,
               c.url,
               d.start_time,
               d.end_time
          FROM downloads d
          LEFT JOIN downloads_url_chains c
            ON c.id = d.id AND c.chain_index = 0
         WHERE d.state = 1
         ORDER BY d.id DESC
         LIMIT ?1
        ",
    )?;
    let rows = statement
        .query_map([MAX_ROWS], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let after = std::fs::metadata(history)?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        bail!("the browser wrote its history during the read");
    }
    writer.register_web_downloads_at(&still_on_disk(rows))
}

/// The downloads whose file is still the one the browser saved, newest row
/// first, one per path.
///
/// A deleted download leaves its row behind, and whatever takes that name next
/// must not inherit its site: a file missing, or modified after the download
/// finished, is skipped. Only the newest row counts for a path downloaded more
/// than once.
fn still_on_disk(rows: Vec<(String, Option<String>, i64, i64)>) -> Vec<(PathBuf, String, i64)> {
    // A download's last write lands just before the browser records it as done.
    const SLACK_SECONDS: i64 = 60;
    let mut seen = HashSet::new();
    let mut downloads = Vec::new();
    for (file, url, started, ended) in rows {
        if file.is_empty() || !seen.insert(file.clone()) {
            continue;
        }
        let path = PathBuf::from(file);
        let finished = webkit_epoch_to_unix(started.max(ended));
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.mtime() > finished + SLACK_SECONDS {
            continue;
        }
        let Some(domain) = url.as_deref().and_then(domain_from_url) else {
            continue;
        };
        downloads.push((path, domain, webkit_epoch_to_unix(started)));
    }
    downloads
}

/// Open a browser's history without copying it and without disturbing it.
///
/// `immutable=1` promises SQLite the file will not change underneath it, which
/// is how it reads a database another process has open without taking a lock.
/// When the browser writes anyway, SQLite may return an error or wrong rows.
/// The caller drops the batch when the file's size or mtime moved during the
/// read; a write the browser started before the read began goes unnoticed.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The date a browser records is the date the person sees.
    #[test]
    fn browser_timestamp_becomes_unix_time() {
        // A row from a Chrome history: 2026-09-03 03:49:59 UTC, which is
        // 1 788 407 399 seconds since 1970.
        assert_eq!(webkit_epoch_to_unix(13_432_880_999_211_718), 1_788_407_399);
        assert_eq!(webkit_epoch_to_unix(0), 0);
        assert_eq!(webkit_epoch_to_unix(-5), 0);
    }

    /// A file that took a deleted download's name keeps no origin from it,
    /// and a name downloaded twice gets its newest download's site.
    #[test]
    fn only_the_newest_unchanged_download_counts() {
        let dir = crate::test_env::scratch_dir("bs-browser-downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            path.to_string_lossy().into_owned()
        };
        let webkit = |unix: i64| (unix + 11_644_473_600) * 1_000_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (fresh, replaced) = (file("fresh.pdf"), file("replaced.pdf"));
        let url = |host: &str| Some(format!("https://{host}/f"));
        let rows = vec![
            (
                fresh.clone(),
                url("new.example"),
                webkit(now - 5),
                webkit(now),
            ),
            (
                fresh.clone(),
                url("old.example"),
                webkit(now - 99),
                webkit(now - 90),
            ),
            (
                replaced,
                url("gone.example"),
                webkit(now - 7200),
                webkit(now - 7200),
            ),
            (
                dir.join("missing.pdf").to_string_lossy().into_owned(),
                url("x.example"),
                0,
                0,
            ),
        ];
        assert_eq!(
            still_on_disk(rows),
            [(PathBuf::from(fresh), "new.example".to_string(), now - 5)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
