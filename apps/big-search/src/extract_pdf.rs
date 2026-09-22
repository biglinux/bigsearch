#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "PDF text extraction spawns pdftotext under a per-process pidfd watchdog \
              with RLIMIT_AS via pre_exec and a hard-capped streaming stdout read \
              (see pdftotext_capped); the big-os-kit subprocess wrapper does not express \
              this pidfd/streaming-cap hardening, so raw Command is used deliberately here \
              on the highest-risk (arbitrary-PDF) path, mirroring ptyd.rs's scoped allow."
)]

use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Extract PDF text: `timeout <N> pdftotext -q -- <path> -` → stdout. No shell.
/// PDFs are not skipped by file size by default. Stdout is read with a hard cap;
/// `Command::output()` would buffer the whole PDF text layer before truncation,
/// which can spike RSS on pathological documents. Set `BIG_SEARCH_PDF_MAX_MB` if
/// a deployment explicitly wants to skip very large PDF inputs.
pub(crate) fn read_pdf(path: &Path) -> Option<String> {
    let cap = crate::extract::max_extract_bytes();
    let bytes = pdftotext_capped(path, cap)?;
    // The bytes become the string when they are valid UTF-8, which they are for
    // every ordinary PDF: `from_utf8_lossy(&bytes).into_owned()` copied the whole
    // text layer a second time.
    let mut text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    };
    crate::extract::truncate_to_char_boundary(&mut text, cap);
    if text.trim().is_empty() {
        None // scanned / image-only PDF (no text layer) → name-only, opt-in OCR later
    } else {
        Some(text)
    }
}

fn pdftotext_capped(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    if exceeds_pdf_input_cap(path) {
        return None;
    }

    // Prefer one direct pdftotext process guarded by a pidfd. The capability
    // probe is not sufficient by itself: pidfd_open or watcher-thread creation
    // can still fail for this specific child under fd/thread pressure. In that
    // case, kill and reap the unguarded child before respawning through
    // timeout(1), so every successful spawn has a wall-clock deadline.
    let use_watchdog = pidfd_supported();
    let mut child = spawn_pdftotext(path, !use_watchdog)?;
    let watchdog = if use_watchdog {
        match SubprocessDeadline::arm(&child, Duration::from_secs(pdf_timeout_secs())) {
            Some(watchdog) => Some(watchdog),
            None => {
                kill_child_tree(&mut child);
                let _ = child.wait();
                child = spawn_pdftotext(path, true)?;
                None
            }
        }
    } else {
        None
    };

    // Returning early here must still reap: `Child`'s drop does not wait, so a
    // bare `?` would leave a zombie (and the watchdog thread alive) until the
    // daemon exits — one per unreadable PDF.
    let Some(mut stdout) = child.stdout.take() else {
        kill_child_tree(&mut child);
        let _ = child.wait();
        if let Some(watchdog) = watchdog {
            watchdog.reap();
        }
        return None;
    };
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    if stdout
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        kill_child_tree(&mut child);
        let _ = child.wait();
        if let Some(watchdog) = watchdog {
            watchdog.reap();
        }
        return None;
    }

    let was_truncated = bytes.len() > max_bytes;
    if was_truncated {
        bytes.truncate(max_bytes);
        kill_child_tree(&mut child);
    }
    drop(stdout);

    let status = match child.wait() {
        Ok(status) => status,
        Err(_) => {
            if let Some(watchdog) = watchdog {
                watchdog.reap();
            }
            return None;
        }
    };
    if let Some(watchdog) = watchdog {
        watchdog.reap();
    }
    if !was_truncated && !status.success() && bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

fn spawn_pdftotext(path: &Path, use_timeout_wrapper: bool) -> Option<Child> {
    let mut command = if use_timeout_wrapper {
        let mut wrapped = Command::new("timeout");
        wrapped.arg(pdf_timeout_secs().to_string()).arg("pdftotext");
        wrapped
    } else {
        Command::new("pdftotext")
    };
    command
        .arg("-q")
        .arg("--")
        .arg(path)
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
        let address_space_cap = crate::settings::pdf_memory_limit_bytes();
        // SAFETY: pre_exec runs post-fork/pre-exec in the child. setrlimit is
        // async-signal-safe, and a failed limit setup is propagated so an
        // arbitrary PDF is never parsed without the promised memory cap.
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: address_space_cap,
                    rlim_max: address_space_cap,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().ok()
}

/// Wall-clock deadline for one extraction subprocess, enforced through a pidfd:
/// a watcher thread polls the pidfd and SIGKILLs on expiry. The pidfd is a
/// stable handle to that exact process — the liveness wait cannot race a
/// recycled PID, and the poll wakes as soon as the child exits. On expiry the
/// whole process group is killed (the child runs in its own group), matching
/// what `timeout(1)` did before: helpers the extractor forked die with it.
struct SubprocessDeadline {
    watcher: std::thread::JoinHandle<()>,
}

impl SubprocessDeadline {
    fn arm(child: &Child, timeout: Duration) -> Option<Self> {
        let child_pid = child.id() as libc::pid_t;
        // SAFETY: pidfd_open on the live child PID we just spawned and still own.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid, 0u32) };
        if pidfd < 0 {
            return None;
        }
        let pidfd = pidfd as libc::c_int;
        let watcher = match std::thread::Builder::new()
            .name("big-search-pdf-deadline".to_owned())
            .spawn(move || {
                let deadline = std::time::Instant::now() + timeout;
                loop {
                    let remaining_ms = deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .as_millis()
                        .min(i32::MAX as u128)
                        as libc::c_int;
                    let mut fds = libc::pollfd {
                        fd: pidfd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: poll on one valid pidfd; the buffer outlives the call.
                    let rc = unsafe { libc::poll(&mut fds, 1, remaining_ms) };
                    if rc == 0 {
                        // Deadline reached and the child still runs (the pidfd is not
                        // readable), so it is unreaped and its process group id — the
                        // child's own pid — is still valid: group-kill cannot hit a
                        // recycled pid. Also signal through the pidfd for certainty.
                        // SAFETY: kill(-pgid) on the live child's group; the pidfd
                        // signal targets exactly the process the fd was opened for.
                        unsafe {
                            libc::kill(-child_pid, libc::SIGKILL);
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                pidfd,
                                libc::SIGKILL,
                                0usize,
                                0u32,
                            );
                        }
                        break;
                    }
                    if rc > 0 {
                        break; // child exited
                    }
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EINTR) {
                        log::warn!("PDF deadline watchdog poll failed: {error}");
                        // Fail closed: if the deadline mechanism itself becomes
                        // unusable, terminate the exact child and its helpers.
                        // SAFETY: same live process-group/pidfd invariants as
                        // the timeout branch above.
                        unsafe {
                            libc::kill(-child_pid, libc::SIGKILL);
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                pidfd,
                                libc::SIGKILL,
                                0usize,
                                0u32,
                            );
                        }
                        break;
                    }
                }
                // SAFETY: this thread owns the pidfd; closed exactly once.
                unsafe {
                    libc::close(pidfd);
                }
            }) {
            Ok(watcher) => watcher,
            Err(error) => {
                // SAFETY: no watcher thread was created, so this scope still
                // owns the pidfd and must close it exactly once.
                unsafe {
                    libc::close(pidfd);
                }
                log::warn!("could not arm PDF deadline watchdog: {error}");
                return None;
            }
        };
        Some(Self { watcher })
    }

    /// Join the watcher (returns promptly: the pidfd polls readable once the
    /// child has exited, even after it was reaped).
    fn reap(self) {
        let _ = self.watcher.join();
    }
}

/// Whether this kernel supports pidfds (Linux ≥ 5.3), probed once.
fn pidfd_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // SAFETY: probing pidfd_open on our own PID; the fd is closed at once.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) };
        if pidfd < 0 {
            return false;
        }
        // SAFETY: closing the probe fd we just opened.
        unsafe {
            libc::close(pidfd as libc::c_int);
        }
        true
    })
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let child_signal_group = child.id() as libc::pid_t;
        let _ = libc::kill(-child_signal_group, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn exceeds_pdf_input_cap(path: &Path) -> bool {
    let cap = pdf_max_input_bytes();
    cap > 0
        && std::fs::metadata(path)
            .map(|m| m.len() > cap)
            .unwrap_or(false)
}

fn pdf_max_input_bytes() -> u64 {
    crate::settings::pdf_max_input_bytes()
}

fn pdf_timeout_secs() -> u64 {
    crate::settings::pdf_timeout_secs()
}

/// Full PDF text split into pages by the form-feed pdftotext emits per page.
/// Hand each page's text to `on_page`, one page at a time.
///
/// `pdftotext` separates pages with a form feed, so the pages are slices of the
/// one buffer it produced. Nothing is copied per page: a 3000-page document used
/// to become a `String` of the whole text and then a `String` per page on top of
/// it, for an answer that is a handful of page numbers.
pub(crate) fn for_each_pdf_page(path: &Path, on_page: &mut dyn FnMut(&str)) {
    let cap = crate::extract::max_extract_bytes();
    let Some(bytes) = pdftotext_capped(path, cap) else {
        return;
    };
    for page in String::from_utf8_lossy(&bytes).split('\u{000C}') {
        on_page(page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Tests below mutate process-wide globals (PATH, timeout env). `cargo
    /// test` runs tests as threads of one process (unlike nextest's
    /// process-per-test), so they must be serialized or they poison each other.
    static SUBPROCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn subprocess_env_guard() -> std::sync::MutexGuard<'static, ()> {
        SUBPROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn temporary_extract_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lsearch-ex-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temporary_executable_dir(label: &str) -> std::path::PathBuf {
        let base = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("lsearch-exec-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pdf_extraction_caps_subprocess_stdout_before_waiting() {
        let _env_guard = subprocess_env_guard();
        let dir = temporary_extract_dir("pdfcap");
        let bin = temporary_executable_dir("pdfcap");
        let fake_pdftotext = bin.join("pdftotext");
        std::fs::write(
            &fake_pdftotext,
            "#!/bin/sh\ndd if=/dev/zero bs=1048576 count=2 2>/dev/null | tr '\\0' a\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_pdftotext).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_pdftotext, permissions).unwrap();
        let path = dir.join("large-output.pdf");
        std::fs::write(&path, b"%PDF fake").unwrap();

        let previous_path = std::env::var_os("PATH");
        let previous_extract_cap = std::env::var_os("BIG_SEARCH_EXTRACT_MAX_MB");
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        unsafe {
            std::env::set_var("PATH", test_path);
            std::env::set_var("BIG_SEARCH_EXTRACT_MAX_MB", "1");
        }

        let text = read_pdf(&path).unwrap();
        assert_eq!(text.len(), crate::extract::max_extract_bytes());
        assert!(text.bytes().all(|byte| byte == b'a'));

        unsafe {
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match previous_extract_cap {
                Some(value) => std::env::set_var("BIG_SEARCH_EXTRACT_MAX_MB", value),
                None => std::env::remove_var("BIG_SEARCH_EXTRACT_MAX_MB"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }

    #[test]
    fn pdf_extraction_kills_hung_extractor_at_deadline() {
        let _env_guard = subprocess_env_guard();
        let dir = temporary_extract_dir("pdfhang");
        let bin = temporary_executable_dir("pdfhang");
        let fake_pdftotext = bin.join("pdftotext");
        std::fs::write(&fake_pdftotext, "#!/bin/sh\nsleep 60\n").unwrap();
        let mut permissions = std::fs::metadata(&fake_pdftotext).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_pdftotext, permissions).unwrap();
        let path = dir.join("hung.pdf");
        std::fs::write(&path, b"%PDF fake").unwrap();

        let previous_path = std::env::var_os("PATH");
        let previous_timeout = std::env::var_os("BIG_SEARCH_PDF_TIMEOUT_SECS");
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        unsafe {
            std::env::set_var("PATH", test_path);
            std::env::set_var("BIG_SEARCH_PDF_TIMEOUT_SECS", "1");
        }

        let started = std::time::Instant::now();
        let text = read_pdf(&path);
        let elapsed = started.elapsed();

        unsafe {
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match previous_timeout {
                Some(value) => std::env::set_var("BIG_SEARCH_PDF_TIMEOUT_SECS", value),
                None => std::env::remove_var("BIG_SEARCH_PDF_TIMEOUT_SECS"),
            }
        }

        assert!(text.is_none(), "hung extractor must yield no text");
        assert!(
            elapsed < Duration::from_secs(10),
            "deadline not enforced: took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }
}
