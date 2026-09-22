# Try and build Big

This guide covers development, not a claim that a stable installer is available.
Choose one application first, use test data, and keep your existing desktop usable.

## Identify your checkout

First check for `REPOSITORY.md`: it identifies an **exported Git**, including
`big-suite`. It has a root `Cargo.toml` and local README; sources retain their
original subdirectories. Run Cargo from that Git's root. A product needs its
selected sibling framework, not the other applications. The exported `big-suite`
needs the selected set of siblings. Some legacy source-only scripts are present
in the suite export; their mere presence does not make it the original layout.

The **source integration checkout** has the complete product tree under `apps/`,
shared crates and the exporter, with no generated root `REPOSITORY.md`. It also
has a separate workspace at `apps/big-search`. Source-only export/impact commands
in this handbook apply to that complete checkout, not the generated suite Git.

## Reuse the prepared environment

The exact toolchain is Rust/Cargo 1.98.1, edition 2024; `rust-toolchain.toml` and
manifests are authoritative. The supplied Debian 13 AMD64 bundle provides GTK
4.22.5, libadwaita 1.9.4, GtkSourceView 5.18.0 and VTE 0.84.1 alongside compatible
system libraries. Other native requirements are determined by the selected
package/build, not by assuming every dependency must be in the bundle.

Do not install Rust from the Internet or use rustup in the constrained executor.
Reuse an installed toolchain first, then supplied archives in an isolated prefix.
Source the activation script produced by the verified recovery kit in **each new
shell**. Do not execute an older source-deleting restore script over existing work.
The checkpoint README explains restoration into a new directory; the repo itself
does not depend on a hard-coded `/mnt/data` location.

Check the environment before a build:

```sh
rustc --version
cargo --version
cargo clippy --version
rustfmt --version
pkg-config --modversion gtk4 libadwaita-1
cargo metadata --locked --offline --format-version 1
```

A metadata pass proves dependency resolution, not native compilation or linking.
The vendor directory is `vendor-offline`, **not** `vendor` (which contains the
libspa patch). Keep vendor writable for the pinned bindgen/PipeWire build path.
The native bundle's `.pc` paths require `/opt/big-deps`; use the prepared activation
instead of copying the entire system library closure into another sysroot.

On a maintained development machine, provision the versions required by the
manifests using your distribution's supported process. Inspect `pkg-config` and
installed libraries before requesting a missing package. This guide does not run
a package-manager install or change the system automatically.

## Build one owner

From the integration checkout, or an exported terminal Git with its framework:

```sh
CARGO_BUILD_JOBS=1 timeout --signal=TERM --kill-after=20s 20m   cargo build -p bigterminal --bin bigterminal --locked --offline
```

Other package/binary pairs are listed in each product README. Use an explicit
`--bin` when a package contains helpers. A GUI command may enter its main loop even
with `--help`; launch in a graphical test session with a timeout and close it there.
Run GUI applications as an ordinary user. Built binaries need the native activation,
resources, schemas and relevant helpers; the offline prefix is not a redistribution
format. Test packaging separately rather than installing every debug binary globally.

## Shared or separate windows

In the integration checkout or exported `big-suite`, with all selected siblings:

```sh
cargo build -p bighost --features builtin-session,gtk-jemalloc --locked --offline
```

The Cargo package and build output are `bighost`; installed launchers commonly use
`big-host`. In a test session, launch `target/debug/bighost bigeditor`, then use the
same binary with `bigfiles`, `bigterminal` or `bigiris`. `--separate bigeditor`
requests that product's separate instance, not necessarily a fresh PID every time.
Keep the same binary/profile for the session; do not overwrite a running binary.

The desktop selector `shell` needs Wayland with layer-shell and appropriate session
setup. Xvfb is useful for application tests, not proof that the desktop works.
BigShot and the search engine remain separate processes. Runtime and A/B details
are in [architecture](architecture.md) and [validation](validation.md).

## Safely use a small executor

Use cgroup memory limits, not just `free` or logical CPU count. Start with one
Cargo job under 4 GiB/no swap. Serialize builds/linkers, reuse caches, give GUI and
subprocess tests finite deadlines, and check disk and `memory.events` between gates.
A timeout is a failure/incomplete run, never PASS. Stop heavy commands when even
`/bin/true` or `/bin/echo` fails through the transport.

Commit coherent work and create a source/Git/patch checkpoint before a large gate.
Exclude `target/`, vendor and toolchain caches. Verify the final compressed bytes
by extraction to a new destination before saving to persistent artifact storage.
Temporary executor files alone are not a backup.
