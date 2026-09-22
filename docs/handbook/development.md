# Make a change without coupling the products

## Integration checkout

Find the owner in `docs/PACKAGES.md`, `docs/TASKS.md` and
`docs/contracts/REPOSITORIES.json` when working in the source integration checkout.
Read its manifest, package README, public rustdoc and nearby tests first. With the
offline environment active, the integration tool can show affected consumers:

```sh
python3 scripts/repository_workspaces.py audit
python3 scripts/repository_workspaces.py impact --changed apps/bigterminal/src/main.rs
```

This selects dependencies conservatively, not every standalone test suite. Changes
to framework, policy, lockfiles or unowned inputs broaden the scope. Runtime behavior
can still cross an interface even if the dependency graph has not changed.

## Exported repositories

Use the current repository's root workspace and the fixed sibling framework. Do
not run a monorepo-only script just because it appears in an old product document.
The root README and generated `docs/COMPONENTS.md` identify local members, source
paths and direct repository dependencies. `REPOSITORY.md` records provenance.

After editing a product, run its focused tests, then its workspace gates and the
affected integration. Keep integration tests in `big-suite`; its explicit source
map resolves approved siblings rather than accidental paths into the old monorepo.
To work only on terminal code, the terminal and framework suffice. Full session
integration needs the selected set; cloning one product does not clone its SDK.

Export uses immutable committed source, creates **new** destinations and records
`repositories.lock.json`. Verification intentionally refuses dirty or different
commits: it verifies a promoted snapshot, not whether local development is permitted.
Do not overwrite edited product Gits by re-exporting, or modify a lock to disguise
a mismatch. During transition the integration source remains authoritative; reconcile
independent work explicitly until permanent repositories and promotion are established.

## Example: add a terminal preference

Keep product behavior in BigTerminal. Use the existing settings and lifecycle owners,
apply the preference to both standalone and hosted panes, and test invalid/default
values plus an already-open pane. Do not add a host dependency or global worker.
If an API must change, put the minimal public contract in the framework and test
consumers. Update visible labels, gettext inputs/catalogs, keyboard behavior and
the package docs in the same reviewed change.

For a visual change, verify focus, scaling, light/dark presentation, touch targets
and accessibility in the actual UI. Capture the tested state; do not substitute a
mockup. Consult the desktop UX/visual contracts when the shell is affected.

## Generated files and commands

Cargo.lock, resources and committed test fixtures are source inputs, not disposable
build caches. Change a lock only for an intentional dependency/composition change;
regenerate the matching vendor when required. A consuming workspace owns its Cargo
patches/profiles; do not assume a dependency's `[patch]` is inherited.

The integration export transforms manifests/paths and seeds approved locks. It does
not modify product Rust sources. Exported onboarding docs are generated from the
shared handbook and `docs/repositories/*.md` introductions in the integration source.
Edit those owners before exporting again; after permanent publication each Git owns
its documentation and needs explicit reviewed updates, not silent synchronization.

For translations, use the owning catalogs and `POTFILES.in`; run `msgfmt -c` and
check every changed visible state. The existing `scripts/regen-pot.sh` is a source-
integration command and is not yet a general per-repo translation runner. Never mark
a locale complete merely because it compiles. Do not duplicate Portuguese/Chinese
variants or invent translations via mechanical text replacement.

Do not install or replace a pre-commit hook automatically. The optional existing
integration hook assumes the monorepo; inspect it before installing and preserve
any local hook. The same gate should also run in CI, not depend on developer setup.
