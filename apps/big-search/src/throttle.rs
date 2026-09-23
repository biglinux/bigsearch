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

fn read_pressure(path: impl AsRef<std::path::Path>) -> Option<Pressure> {
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

/// How contended the machine is, as far as background extraction should care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Load {
    Calm,
    Moderate,
    Busy,
    Heavy,
}

/// What the machine looked like over the last backfill cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Signals {
    /// Stall of this service's own tasks (cgroup PSI, `some avg10`).
    own_cpu: f64,
    own_io: f64,
    /// I/O stall of everything else: system `some` minus this service's own.
    others_io: f64,
    memory: Option<Pressure>,
    /// CPU the rest of the machine used, in cores, since the previous batch.
    others_cores: f64,
    cpus: f64,
    on_battery: bool,
}

/// Classify how much this service would be in the way.
///
/// Stall alone misses the common case: on a two-core machine with SMT, one
/// busy program leaves logical CPUs free, so nobody queues and CPU pressure
/// stays at zero — while our workers on the sibling thread of the same core
/// slow that program by double digits. So the CPU the *others* are using counts
/// as much as the stall we feel. Memory is system-wide, because page cache this
/// service fills stalls everyone else. Running on battery counts as busy: the
/// charge is theirs.
fn classify_load(signals: Signals) -> Load {
    let Signals {
        own_cpu,
        own_io,
        others_io,
        memory,
        others_cores,
        cpus,
        on_battery,
    } = signals;
    if memory.is_some_and(|p| p.full >= 2.0 || p.some >= 20.0)
        || own_cpu >= 50.0
        || own_io >= 50.0
        || others_cores >= (cpus / 2.0).max(1.0)
    {
        Load::Heavy
    } else if on_battery
        || own_cpu >= 20.0
        || own_io >= 20.0
        || others_io >= 10.0
        || others_cores >= 0.5
    {
        Load::Busy
    } else if own_cpu >= 5.0 || own_io >= 5.0 || others_io >= 3.0 || others_cores >= 0.2 {
        Load::Moderate
    } else {
        Load::Calm
    }
}

/// Rest after a batch that took `work`: a duty cycle of 80 % on a calm
/// machine, 50 %, 20 %, and a fixed long rest under real pressure.
fn pause_after(work: Duration, load: Load) -> Duration {
    match load {
        Load::Calm => (work / 4).max(Duration::from_millis(50)),
        Load::Moderate => work.max(Duration::from_secs(1)),
        Load::Busy => (work * 4).max(Duration::from_secs(5)),
        Load::Heavy => Duration::from_secs(30),
    }
}

/// How long the content backfill should rest after a batch that took `work`,
/// and the load that decided it.
pub fn backfill_pace(work: Duration) -> (Load, Duration) {
    let cgroup = own_cgroup_dir();
    let own = |name: &str| {
        cgroup
            .as_ref()
            .and_then(|dir| read_pressure(dir.join(name)))
            .map_or(0.0, |p| p.some)
    };
    let own_io = own("io.pressure");
    let system_io = read_pressure("/proc/pressure/io").map_or(0.0, |p| p.some);
    let load = classify_load(Signals {
        own_cpu: own("cpu.pressure"),
        own_io,
        others_io: (system_io - own_io).max(0.0),
        memory: read_pressure("/proc/pressure/memory"),
        others_cores: others_cores(cgroup.as_deref()),
        cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64,
        on_battery: on_battery(),
    });
    (load, pause_after(work, load))
}

/// The last `(when, whole-machine busy CPU seconds, this cgroup's CPU seconds)`,
/// so each backfill cycle measures what the others used during it.
static CPU_SAMPLE: Mutex<Option<(Instant, f64, f64)>> = Mutex::new(None);

/// Cores the rest of the machine kept busy since the previous call. Zero on the
/// first call, and whenever a counter cannot be read.
fn others_cores(cgroup: Option<&std::path::Path>) -> f64 {
    let (Some(busy), Some(own)) = (machine_busy_seconds(), cgroup.and_then(cgroup_cpu_seconds))
    else {
        return 0.0;
    };
    let now = Instant::now();
    let mut sample = CPU_SAMPLE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let previous = sample.replace((now, busy, own));
    let Some((then, busy_then, own_then)) = previous else {
        return 0.0;
    };
    let wall = now.duration_since(then).as_secs_f64();
    if wall <= 0.0 {
        return 0.0;
    }
    (((busy - busy_then) - (own - own_then)) / wall).max(0.0)
}

/// Busy CPU time of the whole machine, in seconds: the first line of
/// `/proc/stat`, everything except idle and iowait (guest time is already
/// inside user and nice).
fn machine_busy_seconds() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = text
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_ascii_whitespace()
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    let [user, nice, system, _idle, _iowait, irq, softirq, steal, ..] = fields[..] else {
        return None;
    };
    // SAFETY: sysconf has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (ticks > 0).then(|| (user + nice + system + irq + softirq + steal) as f64 / ticks as f64)
}

/// CPU time used by everything in `cgroup`, in seconds (`usage_usec`).
fn cgroup_cpu_seconds(cgroup: &std::path::Path) -> Option<f64> {
    let text = std::fs::read_to_string(cgroup.join("cpu.stat")).ok()?;
    let usec: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()?;
    Some(usec as f64 / 1e6)
}

/// This process's cgroup v2 directory, from the `0::` line of `/proc/self/cgroup`.
fn own_cgroup_dir() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = relative.trim().trim_start_matches('/');
    (!relative.split('/').any(|part| part == ".."))
        .then(|| std::path::Path::new("/sys/fs/cgroup").join(relative))
}

/// Whether the machine has a mains supply and none of them is plugged in.
/// A desktop with no mains entry at all is never "on battery".
fn on_battery() -> bool {
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    let mut has_mains = false;
    for supply in supplies.flatten() {
        let path = supply.path();
        let read = |name| std::fs::read_to_string(path.join(name)).unwrap_or_default();
        if read("type").trim() != "Mains" {
            continue;
        }
        has_mains = true;
        if read("online").trim() == "1" {
            return false;
        }
    }
    has_mains
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
    fn backfill_rests_in_proportion_to_its_work_and_the_load() {
        let calm = Pressure {
            some: 1.0,
            full: 0.0,
        };
        let contended = Pressure {
            some: 30.0,
            full: 5.0,
        };
        let idle = Signals {
            memory: Some(calm),
            cpus: 4.0,
            ..Signals::default()
        };
        assert_eq!(classify_load(idle), Load::Calm);
        assert_eq!(classify_load(Signals::default()), Load::Calm);
        assert_eq!(
            classify_load(Signals {
                on_battery: true,
                ..idle
            }),
            Load::Busy
        );
        assert_eq!(
            classify_load(Signals {
                own_cpu: contended.some,
                ..idle
            }),
            Load::Busy
        );
        assert_eq!(
            classify_load(Signals {
                memory: Some(contended),
                ..idle
            }),
            Load::Heavy
        );
        // One program busy on a four-thread machine: no stall anywhere, but we
        // would be sharing its core.
        assert_eq!(
            classify_load(Signals {
                others_cores: 1.0,
                ..idle
            }),
            Load::Busy
        );
        assert_eq!(
            classify_load(Signals {
                others_cores: 2.0,
                ..idle
            }),
            Load::Heavy
        );
        assert_eq!(
            classify_load(Signals {
                others_io: 12.0,
                ..idle
            }),
            Load::Busy
        );

        let work = Duration::from_secs(2);
        assert_eq!(pause_after(work, Load::Calm), Duration::from_millis(500));
        assert_eq!(pause_after(work, Load::Moderate), work);
        assert_eq!(pause_after(work, Load::Busy), Duration::from_secs(8));
        assert_eq!(pause_after(work, Load::Heavy), Duration::from_secs(30));
        // A tiny batch still leaves room for the kernel to notice others.
        assert_eq!(
            pause_after(Duration::ZERO, Load::Calm),
            Duration::from_millis(50)
        );
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
