# Security and responsible testing

Big is pre-release; no supported stable-version matrix or response-time guarantee
has been published in this checkout. Treat native modules loaded into the host as
trusted code, not sandboxed extensions.

Do not disclose credentials, personal documents, recordings or an exploit against
someone else's system in a public issue. On the canonical project hosting page,
use **Security → Report a vulnerability** only if private reporting is enabled.
If it is unavailable, ask a maintainer for a private reporting channel without
including exploit details. This checkout does not invent an unverified contact
address. Maintainers must configure and test that channel before stable publication.

A useful private report identifies the exact revision, affected product/build mode,
impact, minimal non-sensitive reproducer and platform details. Test only systems
and data you own or are authorized to test. Retain sandboxing, permission checks
and consent flows; do not disable them to make a CI job pass.

Before distribution, audit the resolved dependencies, manifests and source notices
for the actual build. The MIT license at the repository root does not override
GPL components or third-party terms. Release review and recovery requirements are
in [maintenance](docs/handbook/maintenance.md).
