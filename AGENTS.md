# Working on Big

Big is a pre-release Rust/GTK4/libadwaita desktop suite for Linux AMD64, including
older machines. Product ownership and process composition are different boundaries.
This is the canonical repository policy for coding agents; do not maintain a
second copy in a tool-specific instruction file.

## Before editing

1. Read [README.md](README.md), [the architecture](docs/handbook/architecture.md),
   and the owning package's manifest, rustdoc and nearby tests. Discover actual
   targets with Cargo metadata; never invent a binary or infer an API from a plan.
2. Inspect `git status --short` and recent commits. Preserve unrelated work.
   Reuse existing toolchains, native libraries, vendor and caches. Restoration
   must use a new directory and a verified source checkpoint.
3. Read nested `AGENTS.md` files on the path you edit. For desktop changes, the
   `docs/contracts/DESKTOP-UX.md`, `DESKTOP-VISUAL.md` and `DESKTOP-BEAUTY-GATE.md`
   contracts are normative. They specify required behavior, not evidence it passed.
   For applets, also read [compact layout](docs/handbook/applet-layout.md).
   Use [interface quality](docs/handbook/interface-quality.md) to test discovery,
   visual states and evidence limits; a screenshot is not an accessibility pass.
4. For visible changes, read [Big experience 2026](docs/handbook/big-experience-2026.md).
   Keep labelled presentation, task discovery and honest file provenance coherent
   across products; do not replace a desktop Look just to enable text labels.
5. Distinguish implemented code, required contracts, roadmap and historical
   evidence. Do not implement a roadmap as if its names were existing APIs.

## Make the smallest complete change

- Fix the lowest existing owner. Products depend on the framework, not each other
  or the host. Cross-product orchestration and tests belong to integration.
- Do not add a crate, dependency, trait, cache, runtime, option or compatibility
  wrapper without a present requirement. Prefer concrete functions and types.
  Preserve behavior; remove obsolete code rather than stacking adapters.
- In a hosted product, use the existing application/main context: no second
  `Application::run`, process exit for one window, or independent global runtime.
- Keep GTK objects on the main thread. Keep I/O, decoding, process execution and
  network work out of callbacks. Use explicit owners for controllers, tasks,
  signals, timers and buffers; closing invalidates old work and disconnects it.
- Shared pixel storage owns one reservation until its last consumer. Do not
  charge every Arc clone or free a charge while the texture remains alive.
- Do not cross the C ABI with Rust-owned String/Vec/trait objects. Preserve version,
  size, handle and producer-destruction rules. Never hot-unload live GTK modules.
- Preserve argv as arrays, validation at boundaries, sandboxing and user consent.
  Explain each unsafe block. Never weaken data-integrity tests or silently discard
  undo, documents, journals or incomplete transfers to make a test pass.
- Use GTK list models for recycled collections. Use Relm4 for actual state/async
  lifetime, not stateless fixed presentation. Preserve keyboard and AT-SPI behavior.

## Execute safely

Use Rust/Cargo 1.98.1 with rustfmt and Clippy. In the offline executor, do not use
rustup/download installers or replace system libraries. Activate the prepared
prefix before each new shell. Check cgroup memory limits/events and disk; use one
Cargo job on the 4 GiB executor, no concurrent heavy gates and no unlimited GUI runs.
[Development setup](docs/handbook/getting-started.md) documents prerequisites.

Choose a real package from the current workspace and substitute it for PACKAGE:

```text
cargo metadata --locked --offline --format-version 1
cargo fmt -p PACKAGE -- --check
cargo check -p PACKAGE --all-targets --all-features --locked --offline
cargo test -p PACKAGE --all-features --no-run --locked --offline
cargo clippy -p PACKAGE --all-targets --all-features --locked --offline -- -D warnings
cargo test -p PACKAGE --all-features --locked --offline
```

Run with a finite external timeout. Run changed tests first, then affected
consumers/integration. A workspace gate does not run sibling workspaces; BigSearch
is separate in the integration checkout. Nextest does not replace doctests.
[Validation](docs/handbook/validation.md) defines the full matrix and GTK requirements.
For docs/tooling changes, run `python3 scripts/check_documentation.py` and its tests.

If trivial commands fail with transport timeouts, stop launching heavy work.
Record UNKNOWN/ENV and preserve verified state; do not report submitted commands
as completed. Never remove sandboxing or convert a missing test body into PASS.

## Keep the project maintainable

Public behavior belongs in rustdoc (examples, errors, panics and safety where
applicable), package adoption in its README, workflows in the handbook, constraints
in contracts, future work in roadmap, and dated evidence in the archive/artifact.
Update the owning documents in the same change. Keep operational instructions
short; link instead of copying a manual. See [maintenance](docs/handbook/maintenance.md).

Do not claim lower memory, faster startup or stable readiness without matching
before/after evidence. Do not commit credentials, personal recordings, caches,
installed fonts or generated dependency trees. Do not publish/remap repositories,
alter licensing or discard work without explicit authorization.

## Before handing off

Review the diff; record exact revision, commands, exit status and test counts.
Classify failures CODE / TEST / ENV / DEPENDENCY / UNKNOWN. State what was not run.
After a coherent set of changes, preserve source, Git and patches; exclude target
and vendor caches. Extract the final archive into a new directory and compare
hashes, HEAD, tree, status, fsck and source inputs before calling it recoverable.
Save through the available persistent artifact store; a temporary path is not a
backup. Report implemented, corrected, tested, artifacts and real blockers.

[Tool discovery and compatibility](docs/handbook/agent-tools.md) explains how to
verify that an agent actually loaded this file. Support varies by version/session.

## Current repository: bigsearch

Local Cargo members: `big-search`.
Read [Components](docs/COMPONENTS.md) for owned source paths. Use this root
workspace; do not invoke missing monorepo scripts. Product tests do not execute
sibling suites. Keep the fixed framework and rebuild affected integration.
