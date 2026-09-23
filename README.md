# BigSearch

**Find local files by name and content, with incremental indexing.**

An unprivileged search engine, Unix-socket service and CLI maintain a Tantivy index. Automatic profiles account for available memory; indexing uses throttling. Performance must be measured on the actual dataset, storage and machine.

[Português brasileiro](README.pt-BR.md) · [Get started](docs/handbook/getting-started.md) · [Contribute](CONTRIBUTING.md)

**Pre-release.** No first stable release yet. Use disposable data and a test session.
Low resource use is a design goal, not a benchmark claim on this page.

## Try this component

Run from this Git's root with Rust/Cargo 1.98.1, native libraries and Cargo vendor
already prepared. Direct repository dependencies: none. Other programs use this one through
`big-indexd-client` and embed `big-search-settings`.
Keep required siblings at the revisions selected by integration; Cargo does not
clone them automatically. Source directories retain their original names.
[Components](docs/COMPONENTS.md) lists the local members, manifests and entry points.

```sh
cargo build -p big-search --bin big-search --locked --offline
```

Graphical programs need a normal-user test session and finite test deadlines.
Building a binary does not install helpers, schemas or portals. Follow
[validation](docs/handbook/validation.md), not an unqualified installation shortcut.

## Develop independently, integrate deliberately

This Git owns its component; `big-suite` selects compatible revisions. The optional
`builtin-session` composition keeps eligible desktop, terminal, file, editor and
image interfaces in one host. BigShot is not hosted yet; search/indexing, compositor
and helpers remain separate. Source separation does not guarantee native crash
isolation or independent binary upgrades.

[Architecture](docs/handbook/architecture.md) · [Maintenance](docs/handbook/maintenance.md) · [Provenance](REPOSITORY.md)

## Help shape the first stable release

Report a reproducible workflow, improve a translation, test accessibility or submit
a focused fix. Star the project if this is the desktop direction you would like
to follow. Start with [CONTRIBUTING.md](CONTRIBUTING.md); coding agents read
[AGENTS.md](AGENTS.md).

The [component inventory](docs/COMPONENTS.md) reports license declarations from
manifests. The root license does not override component or third-party notices.
