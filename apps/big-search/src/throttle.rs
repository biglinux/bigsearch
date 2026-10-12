//! Background-priority hints and bounded backoff for indexing contention.
//! Applied per-thread (Linux scheduling policy, nice and ioprio are per-task):
//! call from the indexing thread, leaving the query thread at normal priority.

/// `struct sched_attr` as of Linux 5.3 (`SCHED_ATTR_SIZE_VER1`): libc's lacks
/// the utilization clamps.
#[repr(C)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

/// Highest utilization the indexer may ask the CPU frequency for, out of 1024:
/// half the CPU's own capacity, which under `schedutil` keeps a core just below
/// its base clock: no turbo for background work.
pub(crate) const INDEXER_UTIL_MAX: u32 = 512;

/// Lower the calling thread, and the programs it starts, to background
/// priority. Best-effort: each step stands alone, so a kernel that refuses one
/// still gets the others.
///
/// - The indexer's cgroup ([`prepare_idle_cgroup`]), when the daemon has one:
///   it runs on CPU time nobody else wants, against every other program, while
///   the query thread keeps a normal program's share.
/// - `SCHED_IDLE`: the thread runs on CPU time nobody else wants, and a normal
///   task that wakes preempts it at once. A thread's policy only counts inside
///   its cgroup, though: without the indexer's cgroup it only puts the indexer
///   behind the query thread, and a program sharing a core with the indexer
///   still loses a large share of it.
/// - The utilization clamp ([`INDEXER_UTIL_MAX`]) holds the clock down where
///   the frequency driver follows the scheduler (`schedutil`); drivers that pick
///   frequencies in hardware (`intel_pstate`/`amd-pstate` active) ignore it.
/// - nice 19 for kernels that refuse `SCHED_IDLE`, and idle I/O priority, which
///   the BFQ disk scheduler honours.
pub fn lower_priority() {
    let mut attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_policy: libc::SCHED_IDLE as u32,
        sched_flags: libc::SCHED_FLAG_UTIL_CLAMP_MAX as u64,
        sched_nice: 19,
        sched_priority: 0,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
        sched_util_min: 0,
        sched_util_max: INDEXER_UTIL_MAX,
    };
    // SAFETY: priority syscalls on the calling thread (pid/who = 0); `attr`
    // outlives the call and its `size` is the size of the struct passed.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        if libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) != 0 {
            // A kernel built without utilization clamping: SCHED_IDLE alone.
            attr.sched_flags = 0;
            libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0);
        }
        // ioprio_set(IOPRIO_WHO_PROCESS = 1, who = 0, IOPRIO_CLASS_IDLE = 3 << 13)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3i64 << 13);
    }
    if let Some(index) = IDLE_CGROUP.get() {
        // SAFETY: gettid has no preconditions.
        let tid = unsafe { libc::gettid() };
        if let Err(e) = std::fs::write(index.join("cgroup.threads"), tid.to_string()) {
            log::warn!("indexer thread stays in the service's cgroup: {e}");
        }
    }
}

/// The cgroup indexing threads join, once [`prepare_idle_cgroup`] made it.
static IDLE_CGROUP: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Make the indexer a cgroup of its own that runs only on CPU time nobody else
/// wants, so searching keeps a normal program's share while indexing takes
/// what is left. Returns how, for the log; `None` leaves every thread where
/// it is.
///
/// Only under systemd delegation: the unit's `Delegate=cpu` and
/// `DelegateSubgroup=main` put the daemon in `<unit>.service/main`, a group
/// systemd creates for nothing else, and hand it that subtree; with
/// `ProtectControlGroups=private` the unit's group is the root of the daemon's
/// own cgroup namespace and the path reads `/main`. Started any other way
/// (from a terminal, a session scope) the daemon's cgroup belongs to someone
/// else and is left alone.
///
/// The layout is cgroup v2's threaded mode, which lets the threads of one
/// process sit in different groups: `main` keeps the process and the query
/// thread at normal weight; `main/index`, threaded, gets `cpu.idle` (Linux
/// 5.15), or the lowest weight on older kernels.
pub fn prepare_idle_cgroup() -> Option<&'static str> {
    let own = delegated_cgroup()?;
    let unit = own.parent()?;
    let index = own.join("index");
    let step = |result: std::io::Result<()>, what: &str| {
        result
            .map_err(|e| log::warn!("indexer cgroup not set up ({what}): {e}"))
            .ok()
    };
    // systemd delegates the unit's group without enabling its controllers below.
    step(
        std::fs::write(unit.join("cgroup.subtree_control"), "+cpu"),
        "enable cpu",
    )?;
    if !index.is_dir() {
        step(std::fs::create_dir(&index), "create")?;
    }
    if std::fs::read_to_string(index.join("cgroup.type"))
        .ok()?
        .trim()
        != "threaded"
    {
        step(
            std::fs::write(index.join("cgroup.type"), "threaded"),
            "make threaded",
        )?;
    }
    step(
        std::fs::write(own.join("cgroup.subtree_control"), "+cpu"),
        "enable cpu below main",
    )?;
    let how = if std::fs::write(index.join("cpu.idle"), "1").is_ok() {
        "idle"
    } else {
        step(
            std::fs::write(index.join("cpu.weight"), "1"),
            "lowest weight",
        )?;
        "lowest weight"
    };
    IDLE_CGROUP.set(index).ok()?;
    Some(how)
}

/// This process's cgroup, when systemd delegated it as `<unit>.service/main`.
fn delegated_cgroup() -> Option<std::path::PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let own = std::path::Path::new("/sys/fs/cgroup").join(
        own.lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim_start_matches('/'),
    );
    let unit = own.parent()?;
    let delegated = own.file_name()? == "main"
        && (unit == std::path::Path::new("/sys/fs/cgroup")
            || unit
                .file_name()?
                .to_str()
                .is_some_and(|name| name.ends_with(".service")));
    delegated.then_some(own)
}

/// Return freed heap pages to the kernel. glibc retains them by default, so a
/// resident daemon would keep the indexing-burst peak as RSS forever. Called on
/// the transition to idle after an indexing burst; the idle footprint then
/// reflects the idle working set. Best-effort, glibc-only.
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
    /// I/O stall of this service's own tasks (cgroup PSI, `some avg10`).
    own_io: f64,
    /// I/O stall of everything else: system `some` minus this service's own.
    others_io: f64,
    memory: Option<Pressure>,
    /// CPU the rest of the machine used, in logical CPUs, since the previous batch.
    others_cores: f64,
    /// Physical cores this process may run on, and whether some carry two
    /// hardware threads (SMT); see [`topology`].
    cores: f64,
    smt: bool,
    /// Extraction workers that will run at once.
    workers: f64,
    on_battery: bool,
}

/// Classify how much this service would be in the way.
///
/// CPU time is not the question: the indexer only runs on CPU time nobody else
/// wants (`SCHED_IDLE`, its `cpu.idle` cgroup), and a program that wakes takes
/// its CPU back at once, so its own CPU stall only says others are busy, and on
/// a CPU without SMT that harms nobody. What still harms is a shared core: on
/// an SMT CPU a worker on the sibling thread of a busy program's core slows it
/// however low its priority. The scheduler gives each busy program a core of
/// its own while it can, so the free cores are the cores minus the CPUs the
/// others keep busy; with fewer free cores than workers, some worker shares
/// one.
///
/// Memory is system-wide, because page cache this service fills stalls
/// everyone else, and so is the disk: most disk schedulers ignore I/O priority.
/// Running on battery counts as busy: the charge is theirs.
fn classify_load(signals: Signals) -> Load {
    let Signals {
        own_io,
        others_io,
        memory,
        others_cores,
        cores,
        smt,
        workers,
        on_battery,
    } = signals;
    let free_cores = cores - others_cores;
    if memory.is_some_and(|p| p.full >= 2.0 || p.some >= 20.0)
        || own_io >= 50.0
        || (smt && free_cores <= 0.0)
    {
        Load::Heavy
    } else if on_battery || own_io >= 20.0 || others_io >= 10.0 || (smt && free_cores < workers) {
        Load::Busy
    } else if own_io >= 5.0 || others_io >= 3.0 {
        Load::Moderate
    } else {
        Load::Calm
    }
}

/// Physical cores and whether any carries two hardware threads, over the CPUs
/// this process may run on; read once. Hardware threads of one core list each
/// other in `thread_siblings_list`. A CPU whose topology cannot be read counts
/// as a core of its own.
fn topology() -> (f64, bool) {
    static TOPOLOGY: std::sync::OnceLock<(f64, bool)> = std::sync::OnceLock::new();
    *TOPOLOGY.get_or_init(|| {
        // SAFETY: a zeroed cpu_set_t is a valid empty set; sched_getaffinity
        // writes at most size_of::<cpu_set_t>() bytes into it.
        let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let read = unsafe {
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut allowed)
        } == 0;
        let mut cores = std::collections::HashSet::new();
        let mut logical = 0usize;
        for cpu in 0..libc::CPU_SETSIZE as usize {
            // SAFETY: cpu < CPU_SETSIZE, the bit count of a cpu_set_t.
            if !(read && unsafe { libc::CPU_ISSET(cpu, &allowed) }) {
                continue;
            }
            logical += 1;
            let siblings = std::fs::read_to_string(format!(
                "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
            ))
            .map_or_else(|_| cpu.to_string(), |list| list.trim().to_owned());
            cores.insert(siblings);
        }
        if logical == 0 {
            let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            return (cpus as f64, false);
        }
        log::info!(
            "pacing for {} cores, {logical} hardware threads",
            cores.len()
        );
        (cores.len() as f64, logical > cores.len())
    })
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
///
/// The duty cycle is kept on the CPU time the service used in the batch, not
/// on the batch's wall time: at idle priority on a busy machine most of that
/// time is spent waiting for a CPU nobody else wants, which harms nobody, and
/// resting four times as long as that waiting would turn a half-minute batch
/// into a two-minute pause.
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
    let (cores, smt) = topology();
    let (others_cores, own_cpu) = window_usage(cgroup.as_deref());
    let load = classify_load(Signals {
        own_io,
        others_io: (system_io - own_io).max(0.0),
        memory: read_pressure("/proc/pressure/memory"),
        others_cores,
        cores,
        smt,
        workers: crate::content::default_workers() as f64,
        on_battery: on_battery(),
    });
    let used = own_cpu.map_or(work, |cpu| cpu.min(work));
    (load, pause_after(used, load))
}

/// The last `(when, whole-machine busy CPU seconds, this cgroup's CPU seconds)`,
/// so each backfill cycle measures what the others used during it.
static CPU_SAMPLE: Mutex<Option<(Instant, f64, f64)>> = Mutex::new(None);

/// Start the window [`backfill_pace`] judges: what the others use while a batch
/// runs. Measured from the previous batch, the first batch after a quiet spell
/// would average minutes of idle machine and run flat out under load.
pub fn begin_cpu_window() {
    let own = own_cgroup_dir();
    if let (Some(busy), Some(own)) = (
        machine_busy_seconds(),
        own.as_deref().and_then(cgroup_cpu_seconds),
    ) {
        *CPU_SAMPLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some((Instant::now(), busy, own));
    }
}

/// Over the window since the previous call: the cores the rest of the machine
/// kept busy, and the CPU time this service used. Zero cores and no time on
/// the first call, and whenever a counter cannot be read.
fn window_usage(cgroup: Option<&std::path::Path>) -> (f64, Option<Duration>) {
    let (Some(busy), Some(own)) = (machine_busy_seconds(), cgroup.and_then(cgroup_cpu_seconds))
    else {
        return (0.0, None);
    };
    let now = Instant::now();
    let mut sample = CPU_SAMPLE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let previous = sample.replace((now, busy, own));
    let Some((then, busy_then, own_then)) = previous else {
        return (0.0, None);
    };
    let wall = now.duration_since(then).as_secs_f64();
    let own_used = (own - own_then).max(0.0);
    if wall <= 0.0 {
        return (0.0, None);
    }
    (
        (((busy - busy_then) - own_used) / wall).max(0.0),
        Some(Duration::from_secs_f64(own_used)),
    )
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

/// Where systemd mounts the cgroup v2 hierarchy.
pub(crate) const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// This process's cgroup v2 directory, from the `0::` line of `/proc/self/cgroup`.
pub(crate) fn own_cgroup_dir() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = relative.trim().trim_start_matches('/');
    (!relative.split('/').any(|part| part == ".."))
        .then(|| std::path::Path::new(CGROUP_ROOT).join(relative))
}

/// Whether the machine has a mains supply and none of them is plugged in.
/// A desktop with no mains entry at all is never "on battery".
pub(crate) fn on_battery() -> bool {
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
    fn reads_ten_second_pressure_averages() {
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
    fn load_class_follows_signals() {
        let calm = Pressure {
            some: 1.0,
            full: 0.0,
        };
        let contended = Pressure {
            some: 30.0,
            full: 5.0,
        };
        // Four SMT cores, two workers: nothing else running.
        let idle = Signals {
            memory: Some(calm),
            cores: 4.0,
            smt: true,
            workers: 2.0,
            ..Signals::default()
        };
        let with = |others_cores: f64, signals: Signals| Signals {
            others_cores,
            ..signals
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
                memory: Some(contended),
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
        // One busy program leaves three free cores for two workers.
        assert_eq!(classify_load(with(1.0, idle)), Load::Calm);
        // Two and a half leave one and a half: a worker would share a core.
        assert_eq!(classify_load(with(2.5, idle)), Load::Busy);
        // Every core busy: each worker would share one.
        assert_eq!(classify_load(with(4.0, idle)), Load::Heavy);
        // Without SMT a busy machine is no reason to wait: the idle class
        // already hands every CPU back the moment someone wants it.
        let no_smt = Signals { smt: false, ..idle };
        assert_eq!(classify_load(with(4.0, no_smt)), Load::Calm);
        // One SMT core and one worker: background noise already shares it.
        let one_core = Signals {
            cores: 1.0,
            workers: 1.0,
            ..idle
        };
        assert_eq!(classify_load(with(0.3, one_core)), Load::Busy);
    }

    #[test]
    fn backfill_rest_scales_with_load() {
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
    fn unknown_pressure_keeps_progress_moving() {
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
