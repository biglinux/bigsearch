# Validation: choose the scope and prove it ran

Prepare the environment first. Use one heavy gate at a time, bounded execution and
record the exact commit. A valid command copied into a document is not a PASS.

## Documentation and tooling

From this repository root (Python 3.11+, Git; no native GTK build needed):

```sh
python3 scripts/check_documentation.py
python3 -m unittest discover -s scripts/tests -p test_check_documentation.py
```

This checks the maintained onboarding surface, local links/anchors and readable
agent instructions. It does not claim to parse all Markdown, validate every
historical report, check remote URLs offline, or run code blocks automatically.
In the source integration checkout, the opt-in tooling checks are
`python3 scripts/tests/property_repository_toml.py`
(generated manifest round trips) and `python3 scripts/fuzz_documentation.py --runs 10000`
(bounded Markdown mutations from committed seeds, without coverage instrumentation).
They are not part of the per-commit gate. Preserve any failing input as a named
regression fixture before correcting its parser or serializer.
For exporter edits, the integration checkout additionally runs the repository-docs
and repository-workspaces Python tests, a fresh export, every generated doc gate,
Cargo metadata for each root and verification of fixed revisions. The snapshot
verifier reads tracked bytes and executable modes against the commit, independently
of Git's stat cache, assume-unchanged, skip-worktree and core.filemode settings.
It does not reset those flags or overwrite a modified tree.

The current ownership gate requires known local packages to use their declared
path origins; disguised Git/registry copies are rejected, including optional,
build, development and target-specific dependencies. Literal include!, include_str!,
include_bytes! and path attributes are checked across owners. It is not a complete
Rust parser: generated paths, macro expansion and dynamic build scripts still need
the compiler, review and integration tests. Choosing remote SDK origins is an
explicit migration, not an exception to add merely to silence this gate.

## Rust: focused first, integration second

Choose a package actually present in the current workspace, and replace PACKAGE:

```text
cargo metadata --locked --offline --format-version 1
cargo fmt -p PACKAGE -- --check
cargo check -p PACKAGE --all-targets --all-features --locked --offline
cargo test -p PACKAGE --all-features --no-run --locked --offline
cargo clippy -p PACKAGE --all-targets --all-features --locked --offline -- -D warnings
cargo test -p PACKAGE --all-features --locked --offline
cargo test -p PACKAGE --all-features --doc --locked --offline
```

Run the new regression first. Then replace `-p PACKAGE` with `--workspace` at each
affected root and test the integration. In the original integration checkout,
BigSearch is a separate workspace at `apps/big-search`; in an export, use `bigsearch`.
A parent workspace command does not execute those separate suites.

`cargo nextest run --workspace --all-features --locked --offline` is useful when
installed; still run doctests separately. Verify selected test counts: a filter
that selected zero tests is not validation. Optional GTK test bodies must actually
be enabled; a test returning early for missing display is not graphical evidence.

## Runtime and native boundaries

Xvfb plus a private D-Bus session can exercise application windows. Use a finite
outer timeout with kill-after; record forced termination as incomplete execution,
not successful shutdown. Xvfb/software GL does not validate GPU hardware or a
Wayland layer-shell desktop. Use the existing KWin headless gate in the integration
checkout for appropriate UI journeys and report its dependency-based skips.

For hosted changes, test product identity/arguments/cwd, shared and separate modes,
close/reopen without losing other windows, controller destruction and pressure
handling. For composition/ABI changes, test both builtin-session and dynamic host
paths. For durable helpers, verify real reconnect/journal behavior, not a UI fallback.
Never start a second GTK main loop to make a hosted test appear to work.

Native package builds require a clean appropriate distribution environment. A raw
Cargo binary does not validate desktop entries, schemas, portal activation, helper
permissions or package upgrades. Do not run package builds as root or install
untrusted dependencies into the executor to mask a missing test prerequisite.

## Failure classification

| Class | Meaning |
|---|---|
| CODE | Program defect; fix the cause and add a regression. |
| TEST | Incorrect/stale expectation or fixture; preserve the real contract. |
| ENV | Environment cannot reproduce the requirement. |
| DEPENDENCY | Required library, loader or program is absent/incompatible. |
| UNKNOWN | Cause not yet investigated. |

Root permission semantics, locale, bubblewrap namespaces, glycin loaders, protected
backup tools and filesystem reflink support must be checked in the **current** run.
Historical environmental failures are leads, not a whitelist of acceptable failures.
Never remove isolation or change an assertion solely to obtain a green result.

## Evidence for review and release

Record revision, command, exit status, test identifiers/counts, environment and
logs. Separate check, linking, format, lint, unit/integration tests, GUI, physical
hardware and long-running tests. Preserve failures before successful repetitions;
do not add subset/repeated tests to unique totals. Include public rustdoc examples
and errors/panics/safety contracts when changing APIs.

Performance evidence requires a baseline/candidate pair, identical profile, corpus,
renderer, native versions and machine, warm/cold separation and repeated runs.
Report PSS/private memory separately from driver estimates; never sum potentially
shared UMA memory as if it were disjoint. Short tests do not prove months of uptime.
The [maintenance guide](maintenance.md) defines promotion requirements.
