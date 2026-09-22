//! Serialized access to process-wide environment variables, for tests only.
//!
//! Unit tests share one process and run in parallel, so any two that set
//! `XDG_*` race: one test's guard restores a value the other is still using, and
//! the loser reads a config path that no longer exists. That failure is
//! intermittent and looks like a defect in whatever code was under test.
//!
//! Every test that mutates the environment must therefore take [`ENV_LOCK`] —
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
