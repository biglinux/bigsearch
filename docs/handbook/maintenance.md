# Maintain and release Big without accumulating avoidable coupling

## One owner for each kind of information

| Information | Durable owner |
|---|---|
| Purpose, benefits, first run | Root/product README |
| Public API, examples, errors, panics, safety | Rustdoc next to the implementation |
| How to contribute/test/build | CONTRIBUTING and this handbook |
| Ownership and mandatory invariants | Contracts plus executable boundary checks |
| Future designs and unfinished work | Roadmap, explicitly marked proposed |
| Benchmarks, reviews, screenshots, logs | Dated evidence with exact revision/scope |
| Instructions for coding agents | AGENTS.md, with scoped additions only where needed |

Keep entry points short; do not paste audit logs or session transcripts into every
README. Do not claim a proposed feature is available. A required contract can still
be unfulfilled; record the gap rather than redefining “required” as “passed”.
Use relative links, meaningful labels, alt text and normal headings. Keep the English
and Brazilian Portuguese landing pages aligned when their claims or startup change.

Before changing a document, read its current owner and referenced code. Prefer fixing
a link to copying a second manual. Remove obsolete instructions only after preserving
useful historical evidence with a clear date/status. Do not rename hundreds of files
for cosmetic consistency or leave compatibility copies that silently diverge.

## Keep products independent and integration reproducible

Each product has its own tests and depends on a fixed framework, not another product.
The suite selects immutable revisions and validates one effective SDK source. Review
API changes with the affected consumers; do not fork the SDK privately to bypass a
contract. Changing a product need not edit its neighbors, but the integrated artifact
still needs rebuilding and appropriate consumer tests.

The current exporter creates local snapshots, not permanent remotes or filtered
history. Before publication: choose repository owners, preserve relevant history,
define SDK version/deprecation policy, choose registry/immutable Git origins, adapt
packaging/translations and establish per-repo plus integration CI. The existing
sibling paths are a development topology, not a promise of independently upgradeable
binary plugins. `[patch]`, profiles and locks belong to consuming Cargo roots.

The export records commits/trees. A new reviewed integration lock promotes a tested
set; following moving branches implicitly is not promotion. Do not overwrite an
edited export. Source import/reconciliation remains explicit during the transition.

## CI and documentation

The documentation workflow runs on hosted Linux with read-only repository permission,
a pinned checkout action and no credentials retained in Git. It runs the same Python
doc check and regression tests available locally, without downloading Cargo crates
or requiring GTK. Its presence does not mean a remote workflow was executed.
Native Rust, packaging, Wayland/GPU and integration pipelines remain separate gates;
a documentation badge must never stand in for them.

A documentation edit that changes commands must test those commands against metadata
and the correct layout. A script edit needs negative tests (missing files, stale
anchors, wrong destinations), not only one successful run. An exporter change needs
a fresh export and doc/metadata validation in every generated Git, including an
isolated product with its framework and no accidental original-root path.

## Review a release candidate

Use an immutable candidate and document the selected source/framework revisions,
toolchain, features, native library/driver versions, resources and vendor hashes.
Run format, check, link, strict Clippy, tests/doctests and package validation. Record
missing/ignored coverage rather than treating it as approval. Supply-chain advisory
checks need an up-to-date database: an unavailable offline database is a blocker,
not an empty vulnerability list.

Exercise activation, app identities, schemas/translations, accessibility, permissions,
shared/separate modes, pressure and recovery. Include kill/reconnect of the host and
helpers, interrupted transfers/recordings, full disk and migrations with rollback.
Validate clean builds and real-session package installation/update/uninstall. Keep
user data intact and separate reconstructible indexes from unique documents/journals.

Run older Intel/OpenGL/UMA/HDD, supported AMD/NVIDIA, multi-monitor/scaling and fallback
paths on actual machines. Long-running stress plus a longer user pilot must explain
a stable memory/resource plateau after warm-up. A periodic restart is not a leak fix.
Do not publish RAM/speed figures without reproducible data or call a candidate stable
solely because unit tests passed.

Publish only authorized repositories/packages after maintainer sign-off. Configure
and test private security reporting, notices and issue/PR routing. No fake maintainer
addresses, stable badges, support promises, download buttons or new remote URLs.
Licenses are per component; derive component inventories from Cargo metadata and
retain applicable source/third-party notices.

## Keep recoverable checkpoints

Keep small coherent commits. A checkpoint must contain actual tracked source,
fixtures/resources, Git history or explicit patches, state and hashes — not only
a README saying it exists. Exclude build/vendor/toolchain/font caches. Extract the
final distributed bytes into a fresh path and compare manifest, HEAD/tree/status,
Git integrity and Cargo metadata/source inputs before persistence. Keep historical
results labeled historical; never reuse them as tests of a new candidate.
