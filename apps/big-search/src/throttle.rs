//! Background-priority hints and bounded backoff for indexing contention.
//! Applied per-thread (Linux nice/ioprio are per-task): call from the indexing
//! thread, leaving the query thread at normal priority.

/// Lower the calling thread to nice 19 + idle I/O priority. Best-effort.
pub fn lower_priority() {
    // SAFETY: both are simple priority syscalls on the calling thread (who = 0).
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        // ioprio_set(IOPRIO_WHO_PROCESS = 1, who = 0, IOPRIO_CLASS_IDLE = 3 << 13)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3i64 << 13);
    }
}

/// Return freed heap pages to the kernel. glibc retains them by default, so a
/// resident daemon would keep the indexing-burst peak (~50 MB measured) as RSS
/// forever. Called on the transition to idle after an indexing burst; the idle
/// footprint then reflects the idle working set. Best-effort, glibc-only.
pub fn release_idle_memory() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim only releases free arena pages; thread-safe.
    unsafe {
        libc::malloc_trim(0);
    }
}

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Read on demand, at most once per second across extraction workers. No new
// monitor thread, timer, or privileged PSI trigger. Never called by queries.
static PRESSURE: Mutex<Option<(Instant, Duration)>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pressure {
    some: f64,
    full: f64,
}

fn parse_pressure(text: &str) -> Option<Pressure> {
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let kind = fields.next()?;
        if !matches!(kind, "some" | "full") {
            continue;
        }
        let value = fields
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse::<f64>()
            .ok()?;
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return None;
        }
        let slot = if kind == "some" { &mut some } else { &mut full };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    Some(Pressure {
        some: some?,
        full: full?,
    })
}

fn read_pressure(path: &str) -> Option<Pressure> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > 4096 {
        return None;
    }
    parse_pressure(&text)
}

fn pressure_delay(memory: Option<Pressure>, io: Option<Pressure>) -> Duration {
    // These are bounded scheduling policy thresholds, not claims about memory
    // capacity or benchmark-derived universal optima. Unknown PSI adds no delay.
    if memory.is_some_and(|p| p.full >= 2.0 || p.some >= 20.0) {
        Duration::from_millis(250)
    } else if io.is_some_and(|p| p.full >= 10.0 || p.some >= 50.0) {
        Duration::from_millis(100)
    } else {
        Duration::ZERO
    }
}

/// Yield briefly before expensive background extraction when PSI reports
/// contention. The finite wait preserves forward progress and never changes
/// the user's preferences or the priority of the query/UI thread.
pub fn yield_for_pressure() {
    let delay = {
        let mut sample = PRESSURE.lock().unwrap_or_else(|poison| poison.into_inner());
        if sample
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(1))
        {
            *sample = Some((
                Instant::now(),
                pressure_delay(
                    read_pressure("/proc/pressure/memory"),
                    read_pressure("/proc/pressure/io"),
                ),
            ));
        }
        sample.as_ref().map_or(Duration::ZERO, |(_, delay)| *delay)
    };
    // No lock is held while waiting. This code runs only in extraction workers.
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ten_second_percentages_not_cumulative_totals() {
        let p = parse_pressure("some avg10=21.5 avg60=0 total=999\nfull avg10=2.5 total=123\n")
            .unwrap();
        assert_eq!(
            p,
            Pressure {
                some: 21.5,
                full: 2.5
            }
        );
        assert_eq!(pressure_delay(Some(p), None), Duration::from_millis(250));
    }

    #[test]
    fn rejects_incomplete_duplicate_or_nonfinite_pressure() {
        for text in [
            "",
            "some avg10=0",
            "some avg10=NaN\nfull avg10=1",
            "some avg10=-1\nfull avg10=1",
            "some avg10=101\nfull avg10=1",
            "some avg10=1\nsome avg10=2\nfull avg10=0",
        ] {
            assert!(parse_pressure(text).is_none(), "{text}");
        }
    }

    #[test]
    fn unknown_is_not_recorded_as_zero_but_does_not_block_progress() {
        assert_eq!(pressure_delay(None, None), Duration::ZERO);
        assert_eq!(
            pressure_delay(
                None,
                Some(Pressure {
                    some: 50.0,
                    full: 1.0
                })
            ),
            Duration::from_millis(100)
        );
        assert_eq!(
            pressure_delay(
                Some(Pressure {
                    some: 100.0,
                    full: 100.0
                }),
                None
            ),
            Duration::from_millis(250)
        );
    }
}
