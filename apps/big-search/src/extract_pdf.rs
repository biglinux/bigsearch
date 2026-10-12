#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "text extraction is the one place the daemon runs other programs: \
              pdftotext (and, for big-docinfo, pdfinfo) for PDF, antiword for Word 97-2003, \
              under a CPU-time limit and a wall-clock backstop, with RLIMIT_CPU and RLIMIT_AS \
              set in pre_exec and stdout read through a hard cap (see run_capped)"
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
    let bytes = pdftotext_capped(path, cap, &[])?;
    // The bytes become the string when they are valid UTF-8, which they are for
    // every ordinary PDF: `from_utf8_lossy(&bytes).into_owned()` copied the whole
    // text layer a second time.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    };
    text_layer(text, cap)
}

/// Pages read to describe a PDF. Its title, kind and date are in its opening,
/// where ten pages answer as well as the whole text and a long book costs no
/// more than a short one; past them, a signature on its last pages ("Goiânia,
/// 06 de janeiro de 2026") can carry the date.
const DESCRIBED_PAGES: u32 = 10;
const CLOSING_PAGES: u32 = 2;

/// The opening pages of a PDF, as [`read_pdf`] reads the whole, plus the title
/// it declares in its metadata, from the same `pdftotext` run: `-htmlmeta` puts
/// the metadata in an HTML head (escaped) and the text, unchanged, in a `<pre>`.
pub(crate) fn read_pdf_titled(path: &Path) -> Option<(String, Option<String>)> {
    let cap = crate::extract::max_extract_bytes();
    let opening = DESCRIBED_PAGES.to_string();
    let bytes = pdftotext_capped(path, cap, &["-htmlmeta", "-l", &opening])?;
    let html = String::from_utf8_lossy(&bytes);
    let (title, body) = split_html_meta(&html);
    let mut text = body.to_owned();
    // pdftotext ends every page with a form feed: as many as the pages read
    // means there may be more, and only then is the page count worth a process.
    let read = u32::try_from(text.matches('\u{000C}').count()).unwrap_or(u32::MAX);
    if read >= DESCRIBED_PAGES
        && let Some(pages) = page_count(path)
        && pages > DESCRIBED_PAGES
    {
        let first = (pages - CLOSING_PAGES + 1)
            .max(DESCRIBED_PAGES + 1)
            .to_string();
        let last = pages.to_string();
        if let Some(closing) = pdftotext_capped(path, cap, &["-f", &first, "-l", &last]) {
            text.push_str(&String::from_utf8_lossy(&closing));
        }
    }
    Some((text_layer(text, cap)?, title))
}

/// The number of pages `pdfinfo` reports.
fn page_count(path: &Path) -> Option<u32> {
    let report = run_capped("pdfinfo", path, 64 * 1024, &[])?;
    String::from_utf8_lossy(&report)
        .lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
}

/// Page 1 as `pdftotext -bbox-layout` lays it out: XHTML with a box for every
/// line and word, from which `prominent` reads type sizes. Capped like the text.
pub(crate) fn read_pdf_first_page_layout(path: &Path) -> Option<Vec<u8>> {
    let cap = crate::extract::max_extract_bytes();
    pdftotext_capped(path, cap, &["-f", "1", "-l", "1", "-bbox-layout"])
}

/// The metadata title and the text of `pdftotext -htmlmeta` output.
fn split_html_meta(html: &str) -> (Option<String>, &str) {
    let title = html
        .split_once("<title>")
        .and_then(|(_, rest)| rest.split_once("</title>"))
        .map(|(title, _)| unescape_html(title.trim()))
        .filter(|title| !title.is_empty());
    // The text may itself contain "</pre>": the block ends at the last one, or
    // at the end of the output when the cap cut it short.
    let body = html.split_once("<pre>\n").map_or("", |(_, rest)| {
        rest.rfind("</pre>").map_or(rest, |end| &rest[..end])
    });
    (title, body)
}

/// The five entities `pdftotext -htmlmeta` writes in metadata values.
fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of a PDF, cut to the cap, or `None` when it has none (a scanned page).
fn text_layer(mut text: String, cap: usize) -> Option<String> {
    crate::extract::truncate_to_char_boundary(&mut text, cap);
    if text.trim().is_empty() {
        None // scanned / image-only PDF (no text layer) → name-only, opt-in OCR later
    } else {
        Some(text)
    }
}

fn pdftotext_capped(path: &Path, max_bytes: usize, options: &[&str]) -> Option<Vec<u8>> {
    if exceeds_pdf_input_cap(path) {
        return None;
    }
    run_capped("pdftotext", path, max_bytes, options)
}

/// The text of a Word 97-2003 document: `antiword -m UTF-8.txt -w 0 -- <path>`,
/// a paragraph per line. Same deadline, memory limit and cap as `pdftotext`.
pub(crate) fn read_word(path: &Path) -> Option<String> {
    let cap = crate::extract::max_extract_bytes();
    let bytes = run_capped("antiword", path, cap, &["-m", "UTF-8.txt", "-w", "0"])?;
    text_layer(String::from_utf8_lossy(&bytes).into_owned(), cap)
}

/// Runs `program` (`pdftotext` to stdout, `pdfinfo` or `antiword`) on
/// `path` with a deadline, an address-space limit and capped output.
fn run_capped(
    program: &'static str,
    path: &Path,
    max_bytes: usize,
    options: &[&str],
) -> Option<Vec<u8>> {
    // Prefer one direct extractor process guarded by a pidfd. The capability
    // probe is not sufficient by itself: pidfd_open or watcher-thread creation
    // can still fail for this specific child under fd/thread pressure. In that
    // case, kill and reap the unguarded child before respawning through
    // timeout(1), so every successful spawn has a wall-clock deadline.
    let use_watchdog = pidfd_supported();
    let mut child = spawn_capped(program, path, !use_watchdog, options)?;
    let watchdog = if use_watchdog {
        match SubprocessDeadline::arm(&child, Duration::from_secs(wall_deadline_secs())) {
            Some(watchdog) => Some(watchdog),
            None => {
                kill_child_tree(&mut child);
                let _ = child.wait();
                child = spawn_capped(program, path, true, options)?;
                None
            }
        }
    } else {
        None
    };

    // Returning early here must still reap: `Child`'s drop does not wait, so a
    // bare `?` would leave a zombie (and the watchdog thread alive) until the
    // daemon exits, one per unreadable file.
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

/// `program` and `options` are fixed strings from this file, never caller input;
/// `path` follows `--`, so a file named "-f" stays a file.
fn spawn_capped(
    program: &'static str,
    path: &Path,
    use_timeout_wrapper: bool,
    options: &[&str],
) -> Option<Child> {
    let mut command = if use_timeout_wrapper {
        let mut wrapped = Command::new("timeout");
        wrapped.arg(wall_deadline_secs().to_string()).arg(program);
        wrapped
    } else {
        Command::new(program)
    };
    let to_stdout = program == "pdftotext";
    if to_stdout {
        command.arg("-q");
    }
    command.args(options).arg("--").arg(path);
    if to_stdout {
        command.arg("-");
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
        let address_space_cap = crate::settings::pdf_memory_limit_bytes();
        let cpu_seconds = cpu_limit_secs();
        // SAFETY: pre_exec runs post-fork/pre-exec in the child. setrlimit is
        // async-signal-safe, and a failed limit setup is propagated so an
        // arbitrary file is never parsed without the promised caps.
        unsafe {
            command.pre_exec(move || {
                let memory = libc::rlimit {
                    rlim_cur: address_space_cap,
                    rlim_max: address_space_cap,
                };
                // SIGXCPU at the limit, SIGKILL a second later if it is caught.
                let cpu = libc::rlimit {
                    rlim_cur: cpu_seconds,
                    rlim_max: cpu_seconds + 1,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &memory) != 0
                    || libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
                {
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
/// stable handle to that exact process: the liveness wait cannot race a
/// recycled PID, and the poll wakes as soon as the child exits. On expiry the
/// whole process group is killed (the child runs in its own group), as
/// `timeout(1)` does: helpers the extractor forked die with it.
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
                        // readable), so it is unreaped and its process group id (the
                        // child's own pid) is still valid: group-kill cannot hit a
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

/// CPU time an extractor may use: the configured seconds of work, scaled by
/// the clock the indexer is held to ([`crate::throttle::INDEXER_UTIL_MAX`]),
/// so the setting buys the same work as when it was a wall-clock limit.
///
/// The work is bounded by CPU time rather than wall-clock time because the
/// indexer runs at idle priority: on a busy machine a PDF waits its turn, and a
/// wall-clock limit would kill it there and record the file as having no text.
fn cpu_limit_secs() -> u64 {
    pdf_timeout_secs().saturating_mul(1024) / u64::from(crate::throttle::INDEXER_UTIL_MAX)
}

/// Wall-clock time an extractor may take, against one that hangs without
/// using CPU (a stalled network mount).
fn wall_deadline_secs() -> u64 {
    cpu_limit_secs().saturating_mul(3)
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
    let Some(bytes) = pdftotext_capped(path, cap, &[]) else {
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

    /// Tests below change PATH and timeout variables, which every test in the
    /// crate shares: they hold the crate-wide lock.
    fn subprocess_env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env::ENV_LOCK
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

    /// The title is escaped in the HTML head; the text in `<pre>` is not.
    #[test]
    fn htmlmeta_splits_title_and_text() {
        let html = "<html><head>\n<title>Fish &amp; chips: &lt;a&gt; &quot;study&quot;</title>\n</head>\n<body>\n<pre>\nA & B </pre> C\n</pre>\n</body>\n</html>\n";
        let (title, body) = split_html_meta(html);
        assert_eq!(title.as_deref(), Some("Fish & chips: <a> \"study\""));
        assert_eq!(body, "A & B </pre> C\n");
        // No title, and output cut by the cap before the block closed.
        let (title, body) = split_html_meta("<head>\n<title></title>\n</head>\n<pre>\ncut sho");
        assert_eq!((title, body), (None, "cut sho"));
    }

    #[test]
    fn subprocess_stdout_capped_before_wait() {
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
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        let _path = crate::test_env::EnvVarGuard::set("PATH", &test_path);
        let _limit = crate::test_env::EnvVarGuard::set("BIG_SEARCH_EXTRACT_MAX_MB", "1");

        let text = read_pdf(&path).unwrap();
        assert_eq!(text.len(), crate::extract::max_extract_bytes());
        assert!(text.bytes().all(|byte| byte == b'a'));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }

    /// A `.doc` is read by what it holds: OLE2 through `antiword` (argv checked by
    /// a fake one on PATH), RTF saved as `.doc` by the RTF reader, text as text.
    #[test]
    fn doc_files_route_by_content() {
        let _env_guard = subprocess_env_guard();
        let dir = temporary_extract_dir("doc");
        let bin = temporary_executable_dir("doc");
        let fake_antiword = bin.join("antiword");
        std::fs::write(&fake_antiword, "#!/bin/sh\necho \"Relatório $*\"\n").unwrap();
        let mut permissions = std::fs::metadata(&fake_antiword).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_antiword, permissions).unwrap();
        let word = dir.join("-word.doc");
        let mut ole2 = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        ole2.resize(512, 0);
        std::fs::write(&word, ole2).unwrap();
        let rtf = dir.join("saved-as.doc");
        std::fs::write(
            &rtf,
            br"{\rtf1\ansi{\info{\title Ata}}Ata da reuni\'e3o\par}",
        )
        .unwrap();
        let other = dir.join("other.doc");
        std::fs::write(&other, b"PK\x03\x04\0\0 binary").unwrap();
        let text = dir.join("README.DOC");
        std::fs::write(&text, b"Keymap notes\n").unwrap();

        let previous_path = std::env::var_os("PATH");
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        let _path = crate::test_env::EnvVarGuard::set("PATH", &test_path);
        let word_text = crate::extract::content(&word);
        let rtf_described = crate::extract::described(&rtf);
        let other_text = crate::extract::content(&other);
        let plain_text = crate::extract::content(&text);

        assert_eq!(
            word_text.as_deref().map(str::trim_end),
            Some(format!("Relatório -m UTF-8.txt -w 0 -- {}", word.display()).as_str())
        );
        let rtf_described = rtf_described.expect("rtf text");
        assert!(rtf_described.text.contains("Ata da reunião"));
        assert_eq!(rtf_described.declared_title.as_deref(), Some("Ata"));
        assert!(other_text.is_none());
        assert_eq!(plain_text.as_deref(), Some("Keymap notes\n"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }

    /// Work is bounded by CPU time: an extractor that spins is stopped by
    /// RLIMIT_CPU (2 s for a 1 s setting at half clock), well before the
    /// wall-clock backstop (6 s), and the text it wrote first is kept.
    #[test]
    fn spinning_extractor_stopped_by_cpu_time() {
        let _env_guard = subprocess_env_guard();
        let dir = temporary_extract_dir("pdfspin");
        let bin = temporary_executable_dir("pdfspin");
        let fake_pdftotext = bin.join("pdftotext");
        std::fs::write(
            &fake_pdftotext,
            "#!/bin/sh\necho 'first page'\nwhile :; do :; done\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_pdftotext).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_pdftotext, permissions).unwrap();
        let path = dir.join("spinning.pdf");
        std::fs::write(&path, b"%PDF fake").unwrap();

        let previous_path = std::env::var_os("PATH");
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        let _path = crate::test_env::EnvVarGuard::set("PATH", &test_path);
        let _limit = crate::test_env::EnvVarGuard::set("BIG_SEARCH_PDF_TIMEOUT_SECS", "1");

        let started = std::time::Instant::now();
        let text = read_pdf(&path);
        let elapsed = started.elapsed();

        assert_eq!(text.as_deref(), Some("first page\n"));
        assert!(
            elapsed < Duration::from_secs(5),
            "CPU limit not enforced: took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }

    #[test]
    fn hung_extractor_killed_at_deadline() {
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
        let test_path = previous_path
            .as_ref()
            .map(|path| format!("{}:{}", bin.display(), path.to_string_lossy()))
            .unwrap_or_else(|| bin.to_string_lossy().into_owned());
        let _path = crate::test_env::EnvVarGuard::set("PATH", &test_path);
        let _limit = crate::test_env::EnvVarGuard::set("BIG_SEARCH_PDF_TIMEOUT_SECS", "1");

        let started = std::time::Instant::now();
        let text = read_pdf(&path);
        let elapsed = started.elapsed();

        assert!(text.is_none(), "hung extractor must yield no text");
        assert!(
            elapsed < Duration::from_secs(10),
            "deadline not enforced: took {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bin).ok();
    }
}
