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

You need Rust 1.98.1 (rustup picks it up from `rust-toolchain.toml`), SQLite 3,
poppler and shared-mime-info.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

The integration tests start real daemons in temporary directories; they need
no running session and touch nothing under your home.

CI runs the three commands above. The checks below run by hand before a pull
request.

Three files at the root hold limits a change may not pass without saying why:

- `deny.toml`: `cargo deny check` (advisories, licenses, sources).
- `approved-crates.txt`: every crate name `Cargo.lock` may hold. A new name is
  checked on crates.io (owner, repository, a typo of a known crate) before it
  joins the list.
- `systemd-exposure.json`: the highest `systemd-analyze security --offline=yes`
  score each unit may reach.

`sgconfig.yml` and `rules/` are [ast-grep](https://ast-grep.github.io) rules
for this tree: `ast-grep scan` refuses a call that bypasses the function owning
it, such as reading `HOME` or `XDG_CONFIG_HOME` outside `big_search_config`,
and a value formatted into SQL. An exception carries `ast-grep-ignore` and its
reason on the line above.

A function long enough to read in stages, kept whole on purpose, carries a
`// shape:` line saying why: it declares a view (one line per setting) or maps
fields one by one.

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
