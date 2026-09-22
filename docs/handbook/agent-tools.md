# One AGENTS.md, with verified tool discovery

Big keeps repository instructions in `AGENTS.md`, not a duplicate `CLAUDE.md`.
Read the actual file before editing and verify your tool loaded it. Automatic
recognition depends on the tool, version, settings and session; “all agents read
it automatically” is not a supported guarantee.

Official documentation checked on **2026-09-20**:

| Tool/source | What to verify |
|---|---|
| [AGENTS.md specification](https://agents.md/) | Root instructions and more-specific nested instructions; some tools require a configured filename. |
| [Codex](https://developers.openai.com/codex/guides/agents-md) | Root-to-working-directory discovery, nearer overrides and the combined document-size cap; confirm the applicable instruction files. |
| [Claude Code](https://code.claude.com/docs/en/memory#agentsmd) | Direct support requires v2.1.277+ and an eligible session. Default discovery prefers an existing CLAUDE.md/CLAUDE.local.md in the project ancestry; provider/feature-flag/policy restrictions can disable direct AGENTS loading. |
| [GitHub Copilot](https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/add-custom-instructions/add-repository-instructions) | The specific agent/surface must support agent instructions; the nearest AGENTS.md takes precedence. |

Ask the tool to identify the instruction files and summarize the project's ownership
and validation rules before allowing writes. Do not infer loading from the filename
alone. For Claude, check its documented `AGENTS.md loaded` indication; the memory
file list does not necessarily show a directly loaded AGENTS file.

On a tool/session without automatic support, explicitly provide this same file as
context or configure its supported instruction filename. Do not disable telemetry
privacy choices or organization policy just to make instruction discovery work.
An optional local compatibility import is tool setup, not a second project policy;
this repository does not generate or require a CLAUDE.md workaround.

Nested instructions apply to their directory, not to unrelated products. The desktop
configuration instructions remain under `apps/bigshell/AGENTS.md`; they do not
replace the root coding/lifecycle rules. Instructions guide behavior but are not a
security boundary: enforce permissions, offline restrictions and required checks
with the environment/CI, not merely a sentence in Markdown.
