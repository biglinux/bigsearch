//! Library surface for big-search: the modules the `big-search` binary drives,
//! re-exported so test and fuzz harnesses can call into them directly (notably
//! `extract`, the untrusted-document parsing path the fuzz targets exercise).
pub mod browser_downloads;
pub mod catalogue;
pub mod cjk_tokenizer;
pub mod config;
pub mod content;
mod content_dedupe;
pub mod extract;
mod extract_mail;
mod extract_pdf;
mod extract_rtf;
pub mod history;
pub mod index;
pub mod ipc;
pub mod meta;
pub mod mime;
mod mounts;
mod nosync_dir;
pub mod origin;
pub mod prominent;
pub mod query;
pub mod scan;
pub mod scan_roots;
pub mod settings;
mod simplified_chinese;
#[cfg(test)]
mod test_env;
pub mod throttle;
pub mod watch;

/// `text` for a terminal, its control characters written as escapes (`\u{1b}`):
/// a file name or a document can hold a sequence that retitles the window or
/// sets the clipboard.
#[must_use]
pub fn terminal_text(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(char::is_control) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if c.is_control() {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    std::borrow::Cow::Owned(shown)
}
