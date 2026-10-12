//! How the store stays small: one document's versions thinned by age, the
//! budget across every document, and objects a crash left behind.
//!
//! `History::finish_version` thins a document after each version it keeps;
//! the worker in `super::worker` enforces the budget and gives space back
//! during housekeeping.

use anyhow::Result;
use rusqlite::params;

use super::store::now_secs;
use super::{History, Reason};

/// Most of the budget one document may hold.
///
/// Without it, one large file saved ten times in a day owns the whole history
/// and evicts the small letter somebody actually wanted back. Age alone is the
/// wrong yardstick when sizes differ a thousandfold.
const MAX_BUDGET_SHARE_PERCENT: u64 = 10;

/// The rows that actually cost space: every version except the newest of its
/// document, which shares the live file's blocks.
const NOT_THE_NEWEST: &str = "v.id <> (SELECT id FROM versions
                                        WHERE file_id = v.file_id
                                        ORDER BY saved_at DESC, id DESC LIMIT 1)";

impl History {
    /// Thin one document's versions by time, then by count.
    ///
    /// Ten presses of Ctrl+S in five minutes are ten nearly identical versions,
    /// and keeping "the newest ten" would throw away yesterday's, the one that
    /// mattered. So: the last hour keeps one, today keeps one per hour, this
    /// week one per day, then one per week. The count is a ceiling that is
    /// rarely reached.
    pub(super) fn prune_file(&self, file_id: i64) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let mut kept: Vec<(i64, String)> = Vec::new();
        let mut dropped: Vec<(i64, String)> = Vec::new();
        let mut seen_buckets: Vec<i64> = Vec::new();
        let now = now_secs();
        let oldest_allowed = now - i64::from(self.keep_days) * 24 * 60 * 60;

        let mut statement = conn.prepare(
            "SELECT id, saved_at, object, reason FROM versions
              WHERE file_id = ?1 ORDER BY saved_at DESC, id DESC",
        )?;
        let rows: Vec<(i64, i64, String, String)> = statement
            .query_map(params![file_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .flatten()
            .collect();
        let oldest_index = rows.len().saturating_sub(1);

        for (index, (id, saved_at, object, reason)) in rows.into_iter().enumerate() {
            // The newest is never dropped, whatever its age: a document nobody
            // has touched in a year still deserves the one version it has.
            let newest = index == 0;
            // Nor is the state the document was in before this feature ever saw
            // it. Thinning by time would drop it the moment somebody saves, and
            // it is the only copy of what they started from ("como estava antes
            // de eu mexer" is the whole point). Age still retires it.
            //
            // The protection keys on the oldest row, whatever its reason: a
            // document created while the service is watching earns its first
            // version from the watcher, which calls that a save, and that covers
            // most files.
            let first_ever = index == oldest_index;
            // And neither is the state a document was in just before somebody
            // asked to go back to an older one. The dialog that asks promises
            // "o jeito que ele está agora também fica guardado, para você poder
            // desfazer", and hourly thinning would quietly break that promise:
            // going back and undoing it happen minutes apart, inside one slot.
            let is_the_undo = Reason::parse(&reason) == Reason::BeforeRestore;
            let bucket = time_bucket(now, saved_at);
            let too_old = saved_at < oldest_allowed;
            let too_many = kept.len() >= self.keep_versions as usize;
            if newest
                || (!too_old
                    && (first_ever
                        || is_the_undo
                        || (!too_many && !seen_buckets.contains(&bucket))))
            {
                seen_buckets.push(bucket);
                kept.push((id, object));
            } else {
                dropped.push((id, object));
            }
        }
        self.drop_versions(&dropped)
    }

    /// Versions to drop because one document holds more than its share of the
    /// budget: all but its three newest. The first pass of keeping the store
    /// inside its budget; [`Self::over_budget`] is the second.
    pub(super) fn budget_hogs(&self) -> Result<Vec<(i64, String)>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        if self.budget_bytes == 0 {
            return Ok(Vec::new());
        }
        let share = self.budget_bytes / 100 * MAX_BUDGET_SHARE_PERCENT;
        // Same accounting as `total_bytes`: what a document costs is what it
        // holds beyond its newest version.
        let hogs: Vec<i64> = conn
            .prepare(&format!(
                "SELECT v.file_id FROM versions v
                  WHERE {NOT_THE_NEWEST}
                  GROUP BY v.file_id HAVING SUM(v.size) > ?1"
            ))?
            .query_map(params![share as i64], |row| row.get::<_, i64>(0))?
            .flatten()
            .collect();
        let mut dropped = Vec::new();
        for file_id in hogs {
            dropped.extend(
                conn.prepare(
                    "SELECT id, object FROM versions WHERE file_id = ?1
                      ORDER BY saved_at DESC, id DESC LIMIT -1 OFFSET 3",
                )?
                .query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .flatten(),
            );
        }
        Ok(dropped)
    }

    /// The oldest versions to drop so the store fits its budget again.
    pub(super) fn over_budget(&self) -> Result<Vec<(i64, String)>> {
        if self.conn.is_none() || self.budget_bytes == 0 {
            return Ok(Vec::new());
        }
        let total = self.total_bytes()?;
        if total <= self.budget_bytes {
            return Ok(Vec::new());
        }
        self.oldest_versions_totalling(total - self.budget_bytes)
    }

    pub(super) fn all_object_names(&self) -> Result<Vec<String>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        Ok(conn
            .prepare("SELECT object FROM versions")?
            .query_map([], |row| row.get::<_, String>(0))?
            .flatten()
            .collect())
    }

    /// What the store actually costs, in bytes.
    ///
    /// The newest version of each document is left out of the count. A clone
    /// shares the live file's blocks, so the newest version costs nothing until
    /// the document is saved again; what costs is every version that has been
    /// superseded. Counting the whole sum would have the budget evicting
    /// versions that are free.
    pub(super) fn total_bytes(&self) -> Result<u64> {
        let Some(conn) = &self.conn else {
            return Ok(0);
        };
        let total: i64 = conn.query_row(
            &format!("SELECT COALESCE(SUM(size), 0) FROM versions v WHERE {NOT_THE_NEWEST}"),
            [],
            |row| row.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// The oldest superseded versions, enough of them to cover `bytes`.
    ///
    /// One query and one walk instead of "take the oldest, measure everything
    /// again, repeat": the second shape ran a full sum of the table per version
    /// evicted.
    pub(super) fn oldest_versions_totalling(&self, bytes: u64) -> Result<Vec<(i64, String)>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT v.id, v.object, v.size FROM versions v
              WHERE {NOT_THE_NEWEST} ORDER BY v.saved_at ASC"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut chosen = Vec::new();
        let mut covered = 0u64;
        for row in rows.flatten() {
            let (id, object, size) = row;
            chosen.push((id, object));
            covered = covered.saturating_add(size.max(0) as u64);
            if covered >= bytes {
                break;
            }
        }
        Ok(chosen)
    }

    /// Remove versions: the objects first, then the rows.
    pub(super) fn drop_versions(&self, versions: &[(i64, String)]) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        for (id, object) in versions {
            let _ = std::fs::remove_file(self.objects.join(object));
            conn.execute("DELETE FROM versions WHERE id = ?1", params![id])?;
        }
        Ok(())
    }

    /// Throw away objects nobody claims and rows whose object is gone.
    ///
    /// A crash between the clone and the row leaves a file holding disk space
    /// that nothing else would ever free.
    pub fn sweep_orphans(&mut self) -> Result<()> {
        if self.conn.is_none() {
            return Ok(());
        }
        // A set, not a list: the walk below asks "is this object known" once per
        // file on disk, and a linear scan of ten thousand names per file is the
        // difference between a tidy-up and a stall at every start.
        let known: std::collections::HashSet<String> =
            self.all_object_names()?.into_iter().collect();
        let missing_rows: Vec<&String> = known
            .iter()
            .filter(|name| !self.objects.join(name).exists())
            .collect();
        if let Some(conn) = &self.conn {
            for name in &missing_rows {
                conn.execute("DELETE FROM versions WHERE object = ?1", params![name])?;
            }
        }
        let Ok(entries) = std::fs::read_dir(&self.objects) else {
            return Ok(());
        };
        for shard in entries.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for object in files.flatten() {
                let name = format!(
                    "{}/{}",
                    shard.file_name().to_string_lossy(),
                    object.file_name().to_string_lossy()
                );
                if !known.contains(name.as_str()) {
                    let _ = std::fs::remove_file(object.path());
                }
            }
        }
        Ok(())
    }
}

/// Which coarse slot in time a version falls in, so only one is kept per slot.
///
/// Hours today, days this week, weeks after that. Older versions are worth
/// keeping further apart: nobody wants the eleven o'clock and the noon copies of
/// something they wrote three weeks ago.
fn time_bucket(now: i64, saved_at: i64) -> i64 {
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    let age = (now - saved_at).max(0);
    if age < HOUR {
        0
    } else if age < DAY {
        1 + saved_at / HOUR
    } else if age < 7 * DAY {
        1_000_000 + saved_at / DAY
    } else {
        2_000_000 + saved_at / (7 * DAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten saves in five minutes must not push out yesterday's version.
    ///
    /// The bucket is what does it: everything inside the last hour shares one
    /// slot, so only the newest of the burst is kept, and yesterday sits in a
    /// slot of its own.
    #[test]
    fn save_burst_keeps_yesterday() {
        let now = 1_800_000_000;
        let burst: Vec<i64> = (0..10).map(|minute| now - minute * 30).collect();
        let buckets: Vec<i64> = burst.iter().map(|at| time_bucket(now, *at)).collect();
        assert!(
            buckets.windows(2).all(|pair| pair[0] == pair[1]),
            "the whole burst has to share one slot: {buckets:?}"
        );
        let yesterday = time_bucket(now, now - 30 * 3600);
        assert_ne!(buckets[0], yesterday);
    }

    /// Older versions are kept further apart: hours today, days this week,
    /// weeks after that.
    #[test]
    fn versions_thin_out_with_age() {
        let now = 1_800_000_000;
        const HOUR: i64 = 3600;
        const DAY: i64 = 24 * HOUR;
        // Two saves three hours ago and two hours ago are different slots.
        assert_ne!(
            time_bucket(now, now - 3 * HOUR),
            time_bucket(now, now - 2 * HOUR)
        );
        // Two saves an hour apart, three days ago, are the same slot.
        assert_eq!(
            time_bucket(now, now - 3 * DAY),
            time_bucket(now, now - 3 * DAY - HOUR)
        );
        // Two saves a day apart, three weeks ago, are the same slot.
        assert_eq!(
            time_bucket(now, now - 21 * DAY),
            time_bucket(now, now - 21 * DAY - DAY)
        );
    }
}
