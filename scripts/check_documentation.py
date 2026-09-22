"""Check maintained onboarding documents without network or build dependencies.

Supported links: inline/image destinations (including balanced parentheses) and
full/collapsed reference links. Fenced/inline code is ignored. This is not a full
CommonMark parser, recursive historical-doc audit or remote URL availability test.
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from urllib.parse import unquote, urlsplit

ENTRYPOINTS = (
    "README.md",
    "README.pt-BR.md",
    "AGENTS.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "docs/README.md",
)
MAX_DOCUMENT_BYTES = 512 * 1024


def prose(text: str) -> str:
    """Remove code without changing line positions used in diagnostics."""
    lines = []
    fence = None
    for line in text.splitlines(keepends=True):
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        if fence:
            if marker and marker[1][0] == fence[0] and len(marker[1]) >= len(fence):
                fence = None
            lines.append("\n" if line.endswith("\n") else "")
        elif marker:
            fence = marker[1]
            lines.append("\n" if line.endswith("\n") else "")
        else:
            lines.append(line)
    return "".join(lines)


def links(text: str) -> list[tuple[int, str, str]]:
    """Return source-line, label, destination; never execute a document."""
    text = prose(text)
    code_spans = []
    runs = list(re.finditer(r"`+", text))
    i = 0
    while i < len(runs):
        end = next(
            (j for j in range(i + 1, len(runs)) if len(runs[j][0]) == len(runs[i][0])),
            None,
        )
        if end is None:
            i += 1
        else:
            code_spans.append((runs[i].start(), runs[end].end()))
            i = end + 1

    def in_code(position: int) -> bool:
        return any(start <= position < end for start, end in code_spans)

    refs = {}
    for match in re.finditer(
        r"^\s{0,3}\[([^\]\n]+)\]:\s*(<[^>\n]+>|\S+)", text, re.MULTILINE
    ):
        if not in_code(match.start()):
            refs[match[1].strip().casefold()] = match[2].strip("<>")
    result = []
    inline = re.compile(r"(!?)\[([^\]\n]*)\]\(")
    for match in inline.finditer(text):
        if in_code(match.start()):
            continue
        i = match.end()
        start = i
        if i < len(text) and text[i] == "<":
            end = text.find(">", i + 1)
            if end < 0:
                continue
            destination = text[i + 1 : end]
        else:
            depth = 0
            while i < len(text):
                c = text[i]
                if c == "\\" and i + 1 < len(text):
                    i += 2
                    continue
                if c == "(":
                    depth += 1
                elif c == ")":
                    if not depth:
                        break
                    depth -= 1
                elif c.isspace() and not depth:
                    break
                i += 1
            destination = text[start:i]
        destination = re.sub(r"\\([() ])", r"\1", destination)
        if match[1] and not match[2].strip():
            result.append((text.count("\n", 0, match.start()) + 1, "", destination))
        else:
            result.append(
                (text.count("\n", 0, match.start()) + 1, match[2], destination)
            )
    for match in re.finditer(r"\[([^\]\n]+)\]\[([^\]\n]*)\]", text):
        if in_code(match.start()):
            continue
        key = (match[2] or match[1]).strip().casefold()
        result.append(
            (
                text.count("\n", 0, match.start()) + 1,
                match[1],
                refs.get(key, "missing-reference:" + key),
            )
        )
    return result


def anchors(text: str) -> set[str]:
    result, counts = set(), {}
    for line in prose(text).splitlines():
        match = re.match(r"^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$", line)
        if match:
            title = re.sub(r"<[^>]*>", "", match[1])
            slug = re.sub(r"[^\w\- ]", "", title.lower(), flags=re.UNICODE).replace(
                " ", "-"
            )
            count = counts.get(slug, 0)
            counts[slug] = count + 1
            result.add(slug + (f"-{count}" if count else ""))
    result.update(re.findall(r'(?:id|name)=["\']([^"\']+)["\']', prose(text)))
    return result


def default_paths(root: Path) -> list[Path]:
    result = [root / p for p in ENTRYPOINTS]
    result += sorted((root / "docs/handbook").glob("*.md"))
    if (root / "docs/COMPONENTS.md").exists():
        result.append(root / "docs/COMPONENTS.md")
    return result


def check(root: Path, paths: list[Path] | None = None) -> dict:
    root = root.resolve()
    files = default_paths(root) if paths is None else paths
    failures, checked_links = [], 0
    for path in files:
        path = path.absolute()
        name = str(path.relative_to(root)) if path.is_relative_to(root) else str(path)
        if (
            not path.resolve().is_relative_to(root)
            or not path.is_file()
            or path.is_symlink()
        ):
            failures.append(f"{name}: missing, symlink or outside documentation root")
            continue
        if path.stat().st_size > MAX_DOCUMENT_BYTES:
            failures.append(f"{name}: document exceeds size limit")
            continue
        text = path.read_text(encoding="utf-8")
        if not text.strip() or not re.search(r"^#\s+\S", prose(text), re.MULTILINE):
            failures.append(f"{name}: missing top-level title")
        if path.name == "AGENTS.md" and len(text.splitlines()) > 200:
            failures.append(
                f"{name}: keep instructions within 200 lines; move detail to guides"
            )
        if re.search(r"\{\{[A-Za-z_][^}\n]*\}\}", prose(text)):
            failures.append(f"{name}: unresolved template marker")
        for line, label, destination in links(text):
            checked_links += 1
            location = f"{name}:{line}"
            if not label.strip():
                failures.append(f"{location}: empty link/alt text")
            try:
                url = urlsplit(destination)
            except ValueError as error:
                failures.append(f"{location}: invalid link: {error}")
                continue
            if url.scheme or url.netloc:
                if url.scheme not in ("http", "https", "mailto") and not (
                    url.netloc and not url.scheme
                ):
                    failures.append(
                        f"{location}: unsupported or missing-reference link {destination}"
                    )
                continue
            rel = unquote(url.path)
            if "\x00" in rel or "\\" in rel:
                failures.append(f"{location}: unsafe local link {destination}")
                continue
            target = (
                (
                    (root / rel.lstrip("/"))
                    if rel.startswith("/")
                    else (path.parent / rel)
                )
                if rel
                else path
            )
            target = target.resolve()
            if not target.is_relative_to(root):
                failures.append(f"{location}: link leaves repository: {destination}")
            elif not target.exists():
                failures.append(f"{location}: missing link {destination}")
            elif url.fragment and target.is_file() and target.suffix.lower() == ".md":
                if target.stat().st_size > MAX_DOCUMENT_BYTES:
                    failures.append(f"{location}: anchor target exceeds size limit")
                elif unquote(url.fragment) not in anchors(
                    target.read_text(encoding="utf-8")
                ):
                    failures.append(f"{location}: missing anchor {destination}")
    return {
        "status": "FAIL" if failures else "PASS",
        "documents": len(files),
        "links": checked_links,
        "failures": failures,
        "scope": "maintained entrypoints/handbook; no network, code execution or recursive archive audit",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1]
    )
    parser.add_argument(
        "--file", action="append", default=[], help="Explicit file relative to root"
    )
    args = parser.parse_args()
    try:
        result = check(
            args.root, [args.root / p for p in args.file] if args.file else None
        )
    except (OSError, ValueError, UnicodeError) as error:
        result = {"status": "FAIL", "error": str(error)}
    print(json.dumps(result, indent=2, ensure_ascii=False))
    raise SystemExit(result["status"] != "PASS")


if __name__ == "__main__":
    main()
