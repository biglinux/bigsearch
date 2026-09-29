# BigSearch

Desktop search for Linux that finds files by name and by content, keeps its
index current as files change, and stays out of the way on old hardware.

BigSearch runs as a user service. It needs no root, watches the folders you can
see (hidden files and anything a `.gitignore` excludes are skipped) and answers
queries over a Unix socket. The index is [Tantivy](https://github.com/quickwit-oss/tantivy);
the bookkeeping around it is SQLite.

[Português](README.pt-BR.md)

> **Status:** pre-release. The socket protocol, the configuration file and the
> on-disk format may still change between versions.

## Features

- **Names:** case-insensitive substring search over the full path, filtered by
  folder and extension from the CLI, and by media type through the socket.
- **Content:** plain text, Markdown, source code, PDF (through `pdftotext`) and
  OpenDocument / Office Open XML documents. Chinese, Japanese, Korean and
  South-East Asian scripts are indexed as overlapping bigrams, so they are
  searchable without a dictionary.
- **Live index:** inotify on every visible folder, so a saved file is
  searchable within seconds. After a restart only what changed while the
  service was down is read again.
- **Earlier versions:** on filesystems with reflinks (Btrfs, XFS) a copy of a
  document is kept before each save, within a disk budget.
- **Origin:** where a file came from — a browser download, a copy, a move —
  when that is known.
- **Light on the machine:** extraction runs at idle priority and paces itself
  by CPU load, I/O and memory pressure and whether the laptop is on battery.
  An idle daemon sleeps until a file changes.

## Requirements

- Linux with inotify, and systemd for the user service.
- Rust 1.98.1; with rustup, `rust-toolchain.toml` selects it.
- SQLite 3, and poppler (`pdftotext`) for PDF content.
- For the settings page only: GTK 4.22 and libadwaita 1.9.

## Build and install

```sh
cargo build --release
install -Dm755 target/release/big-search ~/.local/bin/big-search
install -Dm644 apps/big-search/packaging/big-search.service \
    ~/.config/systemd/user/big-search.service
systemctl --user enable --now big-search.service
```

The unit is hardened: `ProtectSystem=strict`, `ProtectHome=read-only` with a
writable data directory, `RestrictAddressFamilies=AF_UNIX` and a syscall
filter. On Arch-based systems, [`apps/big-search/packaging/arch`](apps/big-search/packaging/arch)
builds a package instead.

## Usage

```sh
big-search contract                     # file names
big-search content invoice 2026         # file contents
big-search both report -n 20            # both, without duplicates
big-search contract --ext pdf --under ~/Documents

big-search status                       # what the daemon is doing
big-search pause | resume | rebuild
big-search versions ~/notes/todo.md     # earlier versions kept of a file
big-search origin ~/Downloads/file.zip  # where a file came from
```

Queries go to the daemon when it is running and read the index directly
otherwise. `big-search --help` lists every command. Output is colored on a
terminal; `--color never`, `NO_COLOR` and `CLICOLOR` are honored.

## Configuration

`~/.config/big-search/config.toml` is written on first run with every key
documented. With no `[[source]]` block, the home folder is indexed. The CLI
covers the common changes:

```sh
big-search config show
big-search config preset low-memory     # names-only | low-memory | balanced | complete
big-search config set extract-max-mb 16
```

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
| [`crates/ui/generic/big-search-settings`](crates/ui/generic/big-search-settings) | The GTK settings page other programs embed. |

`cargo build` compiles only the daemon; add `--workspace` for the settings page.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE). `scripts/third-party-notices.py` writes the
licenses of the crates compiled into the binary, for anyone redistributing it;
the Arch package installs them next to LICENSE.
