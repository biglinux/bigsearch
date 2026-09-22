# big-search

Lean, unprivileged filename + content search for the Linux desktop. A resident
daemon keeps a [Tantivy](https://github.com/quickwit-oss/tantivy) index fresh in
real time via inotify (scoped to *visible* directories — hidden and `.gitignored`
paths are skipped by design), and answers queries over a Unix socket with separate interactive-query and background-indexing paths.

Pre-release: benchmark latency and memory on the actual corpus, storage and machine.
The automatic profile accounts for effective memory limits; this is not a guarantee
of a particular resident size or comparison against other indexers.

## Features

- **Filename search** — substring, case-insensitive (ngram index + exact filter).
- **Content search** — full-text over `txt/md/rst/log/csv` (in-process), `pdf`
  (`pdftotext`), and `docx/odt/ods/odp/pptx/xlsx` (in-process zip + XML).
- **Real-time, incremental** — inotify per visible directory, no root required.
- **Throttled** — indexing runs at `nice 19` + idle I/O to reduce competition with interactive work; concurrent I/O still needs measurement.

## Usage

```sh
big-search <query>                   # filename search
big-search content <query>           # full-text content search
big-search both <query>              # filename OR content search
big-search config show               # show effective indexing profile
big-search config preset low-memory  # configure for old/low-RAM machines
big-search reindex [ROOT…]           # (re)build the index (default: $HOME)
big-search daemon                    # serve queries + keep the index fresh
```

Global color controls are available for interactive output: `--color auto`,
`--color always`, `--color never`, and `--no-color`. The CLI also respects
`NO_COLOR`, `CLICOLOR_FORCE`, and `CLICOLOR`.

The daemon builds the index on first run, listens on
`$XDG_RUNTIME_DIR/big-search.sock` (mode `0600`), and stores the index under
`$XDG_DATA_HOME/big-search/index`. CLI queries use the daemon when running, else
open the index read-only.

## Build

```sh
cargo build --release
```

Runtime dependency for PDF content: `poppler` (`pdftotext`).

## Run as a service

```sh
install -Dm755 target/release/big-search ~/.local/bin/big-search
install -Dm644 packaging/big-search.service ~/.config/systemd/user/big-search.service
systemctl --user enable --now big-search.service
```

The unit runs unprivileged and hardened (`ProtectSystem=strict`, `ProtectHome=read-only`
with a writable hole for the index, `RestrictAddressFamilies=AF_UNIX`, a syscall
filter, etc.). It deliberately sets no `Nice` — indexing throttles itself per-thread
so queries stay responsive.

## Customizing what is indexed

Hidden and `.gitignored` paths are always skipped. On top of that, big-search
applies built-in excludes for build artifacts and vendored/generated files
(`target/`, `node_modules/`, `dist/`, `*.lock`, `*.min.*`, rustdoc `*.rs.html`, …)
*regardless* of whether a directory is a git repo.

Override them in `~/.config/big-search/ignore` (gitignore syntax — your lines win):

```gitignore
# also skip my scratch dir
~/scratch/
# but DO index node_modules in this one project
!important-project/node_modules/
```

Content is extracted from text/markup/config/code files plus PDF and Office
documents. Results are uncapped by default; use `-n N` to limit.

The generated config lives at `~/.config/big-search/config.toml`. The CLI can
modify the most important defaults without requiring a text editor:

```sh
big-search config show
big-search config path
big-search config set names-only true
big-search config set content-index-mode auto   # auto | basic | freqs
big-search config set extract-max-mb 16         # 0 = adaptive
big-search config preset names-only             # smallest index
big-search config preset low-memory             # Basic postings + low caps
big-search config preset balanced               # automatic default
big-search config preset complete               # Freqs + larger caps
```

`names-only = true` indexes only paths/names and clears/skips the content
sidecar. `content_index_mode = "auto"` chooses `basic` on machines with less than
6 GiB of effective capacity and `freqs` on larger machines. Capacity uses physical
RAM constrained by the visible cgroup v2 `memory.high`/`memory.max` hierarchy;
unknown capacity keeps conservative defaults. `basic` stores only document IDs for
content terms; `freqs` stores document IDs plus term frequencies for better
ranking. Neither mode stores token positions for content.


`config show` reports physical RAM separately from the capacity used for sizing.
The latter is a cached startup baseline, not free RAM or a guarantee against OOM;
pressure monitoring is still required when competing jobs or runtime limits change.

## Low-memory operation

The scanner is designed to stay usable on old dual-core machines with about 2 GB
of RAM:

- Full reconciliation records “seen” paths in a SQLite temporary B-tree instead of
  a process-wide `HashSet<String>` containing the whole catalogue.
- Stale-path deletion and content backfill read the catalogue in 2,048-row
  batches, so large indexes do not require a `Vec<PathBuf>` for every file.
- SQLite state uses a small page cache, file-backed temp storage, WAL/NORMAL
  sync, and a capped mmap window.
- Content backfill prioritizes low estimated read cost and caps each daemon batch
  by file count and total estimated bytes. Its result channel is also tiny
  (2 items on low-RAM machines) so extracted bodies do not queue in memory.
- Tantivy writer memory budgets adapt downward on low-RAM machines and can be
  overridden with `BIG_SEARCH_BULK_WRITER_MB`, `BIG_SEARCH_BACKGROUND_WRITER_MB`,
  and `BIG_SEARCH_CONTENT_WRITER_MB`.
- Extracted text per file no longer uses a fixed 1 MiB ceiling.
  `extract_max_mb = 0` adapts to observed capacity: 4 MiB below 3 GiB, 8 MiB below 6 GiB,
  16 MiB below 12 GiB, and 32 MiB above that. `BIG_SEARCH_EXTRACT_MAX_MB`
  overrides it.
- PDF extraction and page lookup read `pdftotext` stdout through the same hard cap;
  they do not use `Command::output()` to buffer unlimited stdout. PDFs are not
  skipped by input file size by default; set `BIG_SEARCH_PDF_MAX_MB` only if a
  deployment explicitly wants to skip very large PDF inputs.
- Source policy booleans are packed into a one-byte bit mask (`SourceFlags`).

These changes do not remove indexed fields or add lossy result filtering in the
default balanced profile. The explicit `names-only` profile intentionally disables
content search, and the `basic` content mode trades frequency-based ranking detail
for a smaller content index.

## Design

See [DESIGN.md](DESIGN.md) for the architecture, scope decisions, and the
benchmarks behind them.

## License

MIT OR Apache-2.0.

## Arch package build

The Arch `PKGBUILD` lives in `packaging/arch/` to avoid colliding with the
Rust crate's `src/` directory. Build it with:

```sh
cd packaging/arch
makepkg -fi
```
