//! Library surface for big-search: the modules the `big-search` binary drives,
//! re-exported so test and fuzz harnesses can call into them directly (notably
//! `extract`, the untrusted-document parsing path the fuzz targets exercise).
//!
//! - `config`, `settings`: data and runtime paths; the configured sources and policy.
//! - `scan`, `scan_roots`, `watch`, `mounts`: walking the sources and following changes.
//! - `index`, `nosync_dir`, `catalogue`: the tantivy indexes and the state they are
//!   checked against.
//! - `content`, `content_dedupe`, `extract`, `extract_pdf`, `extract_mail`,
//!   `extract_rtf`, `prominent`, `meta`, `mime`: reading what files say.
//! - `cjk_tokenizer`, `simplified_chinese`: indexing scripts written without spaces.
//! - `query`, `ipc`: answering searches, in-process and over the socket.
//! - `origin`, `browser_downloads`, `history`: where a file came from and its
//!   earlier versions.
//! - `throttle`: staying out of the person's way.
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
pub mod mounts;
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
/// sets the clipboard, or a bidirectional override that shows `exe.txt` for
/// `txt.exe`.
#[must_use]
pub fn terminal_text(text: &str) -> std::borrow::Cow<'_, str> {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    };
    if !text.contains(hidden) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if hidden(c) {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    std::borrow::Cow::Owned(shown)
}
