# BigSearch (big-search)

Find files on Linux by name and by content, kept current as they change, light on old hardware.

Pre-release: the socket protocol, the configuration file and the on-disk format may still change.

## Features

- **Names:** case-insensitive substring search over the full path, filtered by
  folder and extension from the CLI and, through the socket, by media type and
  by whether an entry is a folder.
- **Content:** plain text, Markdown, source code, HTML, PDF (through
  `pdftotext`), OpenDocument and Office Open XML documents, EPUB, saved e-mail,
  RTF and Word 97-2003 (through `antiword`). Chinese, Japanese, Korean and
  South-East Asian scripts are indexed as overlapping bigrams, so they are
  searchable without a dictionary.
- **Live index:** inotify on every visible folder, so a saved file is
  searchable within seconds. A restart finds files created, renamed or deleted
  while the service was down without reading every file, then, a minute
  later, walks the folders once in the background for files edited in place.
- **Earlier versions:** on filesystems with reflinks (Btrfs, XFS) a copy of a
  document is kept before each save, within a disk budget.
- **Origin:** where a file came from (a browser download, a copy, a move),
  when that is known.
- **Light on the machine:** extraction runs at idle priority and paces itself
  by CPU load, I/O and memory pressure and whether the laptop is on battery.
  An idle daemon sleeps until a file changes.

BigSearch runs as a user service. It needs no root, watches the folders you can
see (hidden files and anything a `.gitignore` excludes are skipped) and answers
queries over a Unix socket. The index is [Tantivy](https://github.com/quickwit-oss/tantivy);
the bookkeeping around it is SQLite.

## Install

On BigLinux and other systems with the BigLinux repositories:

```sh
sudo pacman -S big-search
systemctl --user enable --now big-search.service
```

`sudo pacman -S big-docinfo` adds "Suggest a name…" to Dolphin's menu for
documents.

### Build from source

Requirements:

- Linux with inotify, and systemd for the user service.
- Rust 1.100.0; with rustup, `rust-toolchain.toml` selects it.
- SQLite 3; poppler (`pdftotext`) for PDF content and `antiword` for Word
  97-2003 documents.
- shared-mime-info, which names a file's type from its name.

```sh
cargo build --release
install -Dm755 target/release/big-search ~/.local/bin/big-search
install -Dm644 -t ~/.config/systemd/user apps/big-search/packaging/big-search.service \
    apps/big-search/packaging/big-search-config.path \
    apps/big-search/packaging/big-search-config.service
systemctl --user enable --now big-search.service
```

On Arch-based systems, `cd pkgbuild && makepkg -si` builds and installs the
`big-search` and `big-docinfo` packages from this checkout instead.

## Use

```sh
big-search contract                     # file names
big-search content invoice 2026         # file contents
big-search both report -n 20            # both, without duplicates
big-search contract --ext pdf --under ~/Documents

big-search status                       # what the daemon is doing
big-search pause                        # stop indexing; searches still answer
big-search resume
big-search rebuild                      # read every folder again
big-search rescan                       # changes made while the service was stopped
big-search versions ~/notes/todo.md     # earlier versions kept of a file
big-search origin ~/Downloads/file.zip  # where a file came from
```

Queries go to the daemon when it is running and read the index directly
otherwise. The exit status is 0 on success, a search with no hits included, and
1 on any error. `big-search --help` lists every command. Output is colored on a
terminal; `--color never`, `NO_COLOR` and `CLICOLOR` are honored.

## Configuration

`~/.config/big-search/config.toml` is written, with every key documented, by
the first `big-search config` or `big-search reindex` command; the service
itself never writes it. With no `[[source]]` block, the home folder is indexed. The CLI
covers the common changes:

```sh
big-search config show
big-search config preset low-memory     # names-only | low-memory | balanced | complete
big-search config set extract-max-mb 16
```

The service reads the file when it starts; `big-search-config.path` restarts it
two seconds after the file changes. A changed setting covers files indexed from
then on; `big-search rebuild` reads the indexed ones again under it.

Extra exclusions go in `~/.config/big-search/ignore`, in gitignore syntax. A
`!` line brings back something the built-in rules skip (`node_modules/`,
`target/`, minified files and the like).

The indexes and state live in `~/.local/share/big-search/`; the socket is
`$XDG_RUNTIME_DIR/biglinux/indexd.sock`, mode `0600`.

## Repository layout

| Path | Contents |
|---|---|
| [`apps/big-search`](apps/big-search) | The daemon and the CLI. [DESIGN.md](apps/big-search/DESIGN.md) covers the architecture and the measurements behind it. |
| [`crates/foundation/big-indexd-client`](crates/foundation/big-indexd-client) | Socket protocol types and a small client for other programs. |
| [`crates/foundation/big-search-config`](crates/foundation/big-search-config) | Reads and writes `config.toml` without losing the user's comments. |
| [`apps/big-docinfo`](apps/big-docinfo) | The `big-docinfo` command: suggests a file name for a document from what it says, and renames it without overwriting. |
| [`crates/foundation/big-docinfo`](crates/foundation/big-docinfo) | Reads a document's kind, date, author and key sentences from its text, without a model. |

`cargo build` compiles only the daemon; add `--workspace` for `big-docinfo` and
the libraries. The settings window that edits `config.toml` belongs to
BigDesktop, which takes the two
libraries above from this repository.

## Help

Report a problem at
[github.com/biglinux/bigsearch/issues](https://github.com/biglinux/bigsearch/issues);
[CONTRIBUTING.md](CONTRIBUTING.md#reporting-a-problem) says what to include.
`big-search --help` and `big-search config --help` list every command and
setting.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE). The table that folds Traditional Chinese to
Simplified comes from Unicode data, under
[LICENSES/Unicode-3.0.txt](LICENSES/Unicode-3.0.txt).
`scripts/third-party-notices.py` writes the licenses of the crates compiled
into the binary, for anyone redistributing it; the Arch package installs them
next to LICENSE.
