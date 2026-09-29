# Contributing

Bug reports, fixes and measurements from real machines are all welcome, in
English or Portuguese.

## Reporting a problem

Say what you ran, what you expected and what happened, plus the commit or
version, the distribution and the filesystem the indexed files live on. For
indexing problems, `big-search status` and the service log
(`journalctl --user -u big-search`) usually tell most of the story. Leave out
file names and contents you would not want public.

## Building and testing

You need Rust 1.98.1 (rustup picks it up from `rust-toolchain.toml`), SQLite 3
and poppler. The settings page also needs GTK 4.22 and libadwaita 1.9.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

The integration tests start real daemons in temporary directories; they need
no running session and touch nothing under your home.

`apps/big-search/dev/` has a long-running leak probe (`stress.sh`) and an
adversarial-input probe (`security-probe.sh`); `apps/big-search/fuzz/` has the
fuzz targets for the document parsers. Run them when you change extraction or
the watcher.

## Changes

- Keep a pull request to one change. Formatting and refactoring go in their
  own commits, apart from behavior.
- Add a test that fails without the fix.
- A change that claims to be faster or lighter should say how it was measured:
  the corpus, the machine and the numbers before and after.
- The daemon must stay usable on a dual-core machine with 2 GB of RAM. New
  dependencies, threads, timers and caches need a reason.
- Commit subjects are short and say what changed (`watch: pace the content
  backfill by the machine's load`); the body says why.

By submitting a change you agree to license it under the MIT license.
