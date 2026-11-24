# big-indexd-client

Client and wire types for the `big-search` socket. It uses `serde` and `std`
only, so a graphical program never links the index engine; the daemon uses the
same types, so both ends read one definition of the format. BigDesktop takes
this crate and `big-search-config` from this repository by git revision.

## The socket

`$XDG_RUNTIME_DIR/biglinux/indexd.sock`, a Unix stream socket. There is no
fallback when `XDG_RUNTIME_DIR` is unset or relative: another account could bind
a socket in a shared directory, and the client does not check the owner of the
one it reaches.

One request is one JSON object on one line; the answer is one JSON object on one
line. A connection may carry several exchanges in a row. Every object carries
`"v": 2`. A request names its command in `cmd`; an answer names its kind in
`kind`.

| `cmd` | Fields | Answer `kind` |
|---|---|---|
| `query` | `q`, `mode` (`name`, `content`, `both`, `list`, `largest`), `filter` {`under`, `ext`, `mime`, `mtime`, `size`, `kind`}, `page` {`offset`, `limit`}, `include_origin` | `query`: `hits`, `total`, `next_offset`, `content_pending`, `truncated` |
| `status` | | `status`: `indexed`, `paused`, `activity`, `capabilities`, counts, index sizes and `unwatched_dirs` |
| `sources` | | `sources`: `roots` {`path`, `name`, `content`, `available`} |
| `origin` | `path` | `origin`: `origin` or null |
| `register_copy`, `register_move`, `register_derived` | `src`, `dst`, `app_id` | `ok` |
| `register_created` | `path`, `app_id` | `ok` |
| `versions` | `path` | `versions`: `versions`, `unavailable`, `paused_for_space` |
| `save_version`, `forget_versions` | `path` | `ok` |
| `reindex` | `path` | `ok` |
| `pause`, `resume`, `rescan`, `rebuild`, `clear_origin`, `clear_history` | | `ok` |

Any command can be answered with `error` and a `message`, which the client
returns as `ClientError::Daemon`. An unknown command is answered that way too.

### Changing the format

- A new field in an answer carries a default on the client side
  (`#[serde(default)]`), and the client ignores fields it does not know.
- A new request field a client may send to an older daemon is left out of the
  line when unset, and the daemon names it in `capabilities`: an older daemon
  ignores the field (`filter.kind`, `KIND_FILTER`), so a client asks only a
  daemon that lists it.
- Anything else (a field renamed, removed or given another meaning) raises `v`.

### What the client promises the daemon

- Each read and write on the socket waits at most 15 seconds, so a daemon
  that stops answering fails the request; an answer that keeps arriving may
  take longer. An answer larger than 8 MiB is refused.
- `PersistentClient` sends a request again only when a reused connection turns
  out closed (`EPIPE`, `ECONNRESET`). On a Unix socket already in use, Linux
  reports `ECONNRESET` only when the daemon closed it with bytes unread: it
  stopped before the end of the request line, and a request it stopped
  reading is never carried out. It drops the connection after any other
  failure, so a late answer is never read as the next one. A daemon that stops
  answering costs one deadline.
- A connection the daemon closes after taking a request, before answering, is
  an error to the caller and is not sent again: the request may have been
  carried out.

### How it is checked

`tests/wire.jsonl` holds an exchange for every command, answered by the daemon
itself, and one more for each request field a client sends only when the daemon
lists it. `daemon_exchanges_round_trip` sends every request through the types
unchanged, decodes every answer, and fails when a command has no exchange.
`scripts/capture-search-wire.py [DAEMON]` writes the file again from a daemon
run with a private home and runtime directory; run it after the format changes,
and review the diff.

```sh
cargo test -p big-indexd-client
cargo run -p big-indexd-client --example probe -- relatorio
```
