# Contributing to Big

Help build the first stable release through a small, reproducible improvement.
Code, tests, translations, accessibility reviews, documentation and hardware
reports are all useful contributions. English and Brazilian Portuguese reports
are welcome; keep code identifiers and public API documentation consistent with
the surrounding source.

## Start with a real workflow

Describe the problem, expected result and steps to reproduce. Search existing
reports first. Use disposable files for destructive, remote, recording or recovery
tests. Include the product, commit, distribution, toolkit/driver versions and
whether the window ran standalone, dynamically hosted or in `builtin-session`.
Redact personal paths, contents, credentials and recordings. Sensitive issues
follow [SECURITY.md](SECURITY.md), not a public reproducer with private data.

Good first changes include an incorrect label, a broken documentation example,
a small keyboard regression or a test for one boundary. For architecture/API,
dependencies, storage formats or package changes, explain the approach and affected
consumers before broadening the patch. Do not promise a release date or response SLA.

## Find the owner, then build only what you need

Read [architecture](docs/handbook/architecture.md), the package README and nearby
tests. A product change belongs in that product; a shared API in the framework;
a cross-product assertion in integration. Do not copy the SDK into an application.
The [development workflow](docs/handbook/development.md) distinguishes the
integration checkout from an independently exported Git.

Prepare the [development environment](docs/handbook/getting-started.md), reproduce
the failure and add a focused regression test. Prefer existing dependencies and
plain functions over new layers. Keep formatting-only edits separate from behavior.
In a disposable test session, verify both standalone and hosted behavior whenever
the change affects activation, UI lifetime, resources or integration.

## Submit evidence, not just a green command

Use the [validation matrix](docs/handbook/validation.md). A successful check does
not prove linking, test execution, hardware behavior or stable readiness. Record
failures and skips honestly, including missing native services. Do not silence
Clippy, disable a sandbox or weaken an assertion to obtain a pass.

Before opening a pull request, review the diff and update the public docs and UI
translations affected by it. The PR should contain:

- Problem, scope/owner and the behavior before/after; explain any new dependency,
  unsafe block, public API, process boundary or persisted data shape.
- Exact commands and revision, pass/fail/skip counts, screenshots for visible
  changes, and tests not run with their reason. Never use a mockup as runtime proof.
- Compatibility/rollback considerations and follow-up work not included in the patch.

Use a focused title such as `bigterminal: release cancelled preview work`.
AI-assisted changes require the same review and evidence as any other contribution;
the submitter is responsible for the result. Coding agents also read [AGENTS.md](AGENTS.md).

## Help without writing Rust

Reproduce an issue with a small dataset, review natural translations, test keyboard
navigation and screen readers, or measure a repeatable workload on older hardware.
A performance report needs revisions, build settings, renderer, corpus and repeated
measurements. Never compare debug/software-rendered numbers to release/GPU numbers.

This is still pre-release. A contributor's successful run is evidence for its
specified scope, not approval to replace everyone's desktop. The
[maintenance guide](docs/handbook/maintenance.md) covers release review and the
multirepository transition.
