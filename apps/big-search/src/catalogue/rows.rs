//! Row work on the catalogue table: a page of a scan's rows merged with the
//! stored ones, and [`State::persist`]'s pending changes applied in one
//! transaction.

use super::{PendingChange, ScanChange, db_size_to_u64, u64_to_db_size};
use anyhow::{Context, Result};
use rusqlite::params;
use std::collections::HashMap;
use std::path::PathBuf;

/// Merge one page of seen rows with one page of catalogue rows, both in path
/// order, into what changed; rows past `end` wait for the next page.
pub(super) fn merge_scan_rows(
    seen: Vec<(String, Option<i64>, Option<i64>)>,
    known: Vec<(String, Option<i64>, Option<i64>)>,
    end: Option<&String>,
) -> Vec<ScanChange> {
    let within = |path: &String| end.is_none_or(|end| path <= end);
    let mut changes = Vec::new();
    let mut seen = seen.into_iter().filter(|row| within(&row.0)).peekable();
    let mut known = known.into_iter().filter(|row| within(&row.0)).peekable();
    loop {
        let order = match (seen.peek(), known.peek()) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(s), Some(k)) => s.0.cmp(&k.0),
        };
        match order {
            std::cmp::Ordering::Greater => {
                let (path, _, _) = known.next().expect("peeked");
                changes.push(ScanChange::Gone(PathBuf::from(path)));
            }
            order => {
                let (path, mtime, size) = seen.next().expect("peeked");
                let recorded = if order.is_eq() {
                    let (_, mtime, size) = known.next().expect("peeked");
                    Some((mtime.unwrap_or(0), db_size_to_u64(size.unwrap_or(0))))
                } else {
                    None
                };
                // Marked without metadata: seen, nothing to compare.
                let (Some(mtime), Some(size)) = (mtime, size) else {
                    continue;
                };
                let size = db_size_to_u64(size);
                if recorded != Some((mtime, size)) {
                    changes.push(ScanChange::Seen {
                        path: PathBuf::from(path),
                        mtime,
                        size,
                        recorded,
                    });
                }
            }
        }
    }
    changes
}

/// Write each pending row change inside the caller's transaction.
pub(super) fn write_pending(
    tx: &rusqlite::Savepoint<'_>,
    namespace: &str,
    pending: &HashMap<String, PendingChange>,
) -> Result<()> {
    let mut upsert = tx
        .prepare_cached(
            "INSERT INTO file_state(namespace, path, mtime, size)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(namespace, path) DO UPDATE
                 SET mtime = excluded.mtime, size = excluded.size",
        )
        .context("prepare state upsert")?;
    let mut delete = tx
        .prepare_cached("DELETE FROM file_state WHERE namespace = ?1 AND path = ?2")
        .context("prepare state delete")?;
    for (path, change) in pending {
        match *change {
            PendingChange::Set(mtime, size) => {
                upsert
                    .execute(params![namespace, path, mtime, u64_to_db_size(size)])
                    .with_context(|| format!("upsert state {}", path))?;
            }
            PendingChange::Remove => {
                delete
                    .execute(params![namespace, path])
                    .with_context(|| format!("delete state {}", path))?;
            }
        }
    }
    Ok(())
}
