#!/usr/bin/env python3
"""Write the licenses of every crate compiled into a binary, for distribution.

Follows the normal dependencies of one package as Cargo resolves them for one
target, the way they end up in the binary: build and dev dependencies are not
shipped, and neither are procedural macros or what only they depend on. For
each crate the license files it publishes are copied; identical texts are
printed once, followed by the crates they cover.

A crate that publishes no license file gets the file of a crate from the same
repository (tantivy splits one repository into many crates, and only the main
one carries LICENSE), or, when it may be used under Apache-2.0, the standard
text of that license. Anything else is an error, so a new dependency without a
license text is noticed instead of shipped silently.

    scripts/third-party-notices.py > THIRD-PARTY-NOTICES
    scripts/third-party-notices.py --package big-search-settings

Cargo is run as $CARGO when that is set.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

LICENSE_FILE = re.compile(
    r"^(licen[cs]e|copying|notice|copyright|unlicense)([-._].*)?$", re.IGNORECASE
)


def cargo_metadata(target: str) -> dict:
    cargo = os.environ.get("CARGO", "cargo")
    command = [
        cargo,
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        target,
    ]
    return json.loads(
        subprocess.run(command, check=True, capture_output=True, text=True).stdout
    )


def shipped_packages(metadata: dict, root_name: str) -> list[dict]:
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [
        p["id"]
        for p in metadata["packages"]
        if p["name"] == root_name and p["source"] is None
    ]
    if not roots:
        sys.exit(f"no workspace package named {root_name}")
    seen, pending = set(), roots
    while pending:
        package_id = pending.pop()
        if package_id in seen:
            continue
        seen.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if not any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                continue
            target_kinds = {
                k for t in packages[dependency["pkg"]]["targets"] for k in t["kind"]
            }
            if "proc-macro" not in target_kinds:
                pending.append(dependency["pkg"])
    # Our own crates are covered by the repository's LICENSE.
    return sorted(
        (packages[i] for i in seen if packages[i]["source"] is not None),
        key=lambda p: (p["name"], p["version"]),
    )


def license_files(package: dict) -> list[Path]:
    directory = Path(package["manifest_path"]).parent
    files = [
        p for p in directory.iterdir() if p.is_file() and LICENSE_FILE.match(p.name)
    ]
    if package.get("license_file"):
        files.append(directory / package["license_file"])
    return sorted(set(files))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--package", default="big-search", help="workspace package to describe"
    )
    parser.add_argument(
        "--target", default="x86_64-unknown-linux-gnu", help="target triple"
    )
    arguments = parser.parse_args()

    metadata = cargo_metadata(arguments.target)
    packages = shipped_packages(metadata, arguments.package)
    files = {p["id"]: license_files(p) for p in metadata["packages"]}

    by_repository = defaultdict(list)
    for package in metadata["packages"]:
        if package.get("repository") and files[package["id"]]:
            by_repository[package["repository"].rstrip("/")].append(
                files[package["id"]]
            )
    apache_text = next(
        (
            f
            for fs in files.values()
            for f in fs
            if re.fullmatch(r"LICENSE-APACHE(\.txt|\.md)?", f.name)
        ),
        None,
    )

    texts = defaultdict(list)  # license text -> crates it covers
    problems = []
    for package in packages:
        label = f"{package['name']} {package['version']}"
        found = files[package["id"]]
        repository = (package.get("repository") or "").rstrip("/")
        if not found and by_repository.get(repository):
            found = by_repository[repository][0]
            label += " (license file of its repository)"
        if not found and apache_text and "Apache-2.0" in (package.get("license") or ""):
            found = [apache_text]
            label += " (published without a license file; Apache-2.0 text)"
        if not found:
            problems.append(
                f"{package['name']} {package['version']}: {package.get('license')}"
            )
            continue
        for path in found:
            texts[path.read_text(encoding="utf-8", errors="replace").strip()].append(
                label
            )

    if problems:
        sys.exit("no license text for:\n  " + "\n  ".join(problems))

    print(f"Third-party software in {arguments.package} ({arguments.target})")
    print()
    print("The crates below are compiled into this program. Each one's source is")
    print("available from crates.io at the version listed.")
    print()
    for package in packages:
        print(f"  {package['name']} {package['version']}: {package.get('license')}")
    for text, covered in sorted(texts.items(), key=lambda item: item[1][0]):
        print()
        print("=" * 78)
        print("\n".join(sorted(set(covered))))
        print("-" * 78)
        print(text)


if __name__ == "__main__":
    main()
