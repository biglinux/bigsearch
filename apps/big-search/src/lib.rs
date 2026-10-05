//! Library surface for big-search: the modules the `big-search` binary drives,
//! re-exported so test and fuzz harnesses can call into them directly (notably
//! `extract`, the untrusted-document parsing path the fuzz targets exercise).
pub mod browser_downloads;
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
pub mod state;
#[cfg(test)]
mod test_env;
pub mod throttle;
pub mod watch;
