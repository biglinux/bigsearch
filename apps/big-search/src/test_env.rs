//! Test isolation from the process environment: serialized access to
//! environment variables, and scratch directories outside `TMPDIR`.
//!
//! Unit tests share one process and run in parallel, so any two that set
//! `XDG_*` race: one test's guard restores a value the other is still using, and
//! the loser reads a config path that has been removed. That failure is
//! intermittent and looks like a defect in whatever code was under test.
//!
//! Every test that mutates the environment must therefore take [`ENV_LOCK`]:
//! one lock for the whole crate, because a per-module lock only serializes that
//! module and the modules collide with each other.
use std::ffi::OsString;
use std::sync::Mutex;

pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets an environment variable for as long as it is alive, then restores what
/// was there before. Hold [`ENV_LOCK`] for the guard's whole lifetime.
pub struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    pub fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: callers hold ENV_LOCK; nextest isolates test processes.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }

    pub fn unset(name: &'static str) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: callers hold ENV_LOCK; nextest isolates test processes.
        unsafe { std::env::remove_var(name) };
        Self { name, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the value saved under the same ENV_LOCK.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

#[path = "../tests/scratch/mod.rs"]
mod scratch;
pub use scratch::{ScratchDir, runs_as_root, scratch_dir};

#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "coreutils makes the FIFO, outside this crate"
)]
pub fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo {}", path.display());
}

/// A child process holding a write lease on `path` (as a Samba oplock or an NFS
/// delegation does) that ignores the break signal; kill and wait it to release.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the lease holder is a Python child: taking a lease in-process needs unsafe fcntl"
)]
pub fn hold_write_lease(path: &std::path::Path) -> std::process::Child {
    use std::io::BufRead as _;
    const HOLD_LEASE: &str = "import fcntl, os, signal, sys
signal.signal(signal.SIGIO, signal.SIG_IGN)
fcntl.fcntl(os.open(sys.argv[1], os.O_RDWR), fcntl.F_SETLEASE, fcntl.F_WRLCK)
print('leased', flush=True)
sys.stdin.read()";
    let mut holder = std::process::Command::new("python3")
        .args(["-I", "-c", HOLD_LEASE])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "leased\n", "the holder could not take the lease");
    holder
}
