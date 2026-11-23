#!/usr/bin/env python3
"""Capture the big-search daemon's answer to every request big-indexd-client sends.

Runs the daemon built from this tree in a private HOME and runtime directory, sends each
request line below over its socket and writes the pairs, with the private
directory spelled `/home/p`, to the client's contract fixtures. The client's
tests decode every answer and send every request back through its own types.

    scripts/capture-search-wire.py [DAEMON]   (default: target/release/big-search)

Exit: 0 written, 1 the daemon did not answer, 2 no daemon found.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "crates/foundation/big-indexd-client/tests/wire.jsonl"
DAEMON = ROOT / "target/release/big-search"
SPELLED = "/home/p"


# shape: declares a view
def requests(home: str) -> list[dict]:
    doc = f"{home}/Documents/relatorio.txt"
    copy = f"{home}/Documents/relatorio-copia.txt"
    moved = f"{home}/relatorio-copia.txt"
    return [
        {"v": 2, "cmd": "status"},
        {"v": 2, "cmd": "sources"},
        {
            "v": 2,
            "cmd": "query",
            "q": "relatorio",
            "mode": "both",
            "filter": {
                "under": f"{home}/Documents",
                "ext": ["txt"],
                "mime": [],
                "mtime": None,
                "size": None,
            },
            "page": {"offset": 0, "limit": 50},
            "include_origin": True,
        },
        {
            "v": 2,
            "cmd": "query",
            "q": "documents",
            "mode": "name",
            "filter": {
                "under": home,
                "ext": [],
                "mime": [],
                "mtime": None,
                "size": None,
                "kind": "folder",
            },
            "page": {"offset": 0, "limit": 50},
            "include_origin": False,
        },
        {
            "v": 2,
            "cmd": "register_copy",
            "src": doc,
            "dst": copy,
            "app_id": "br.com.biglinux.bigfiles",
        },
        {"v": 2, "cmd": "register_move", "src": copy, "dst": moved, "app_id": None},
        {
            "v": 2,
            "cmd": "register_created",
            "path": doc,
            "app_id": "br.com.biglinux.bigeditor",
        },
        {
            "v": 2,
            "cmd": "register_derived",
            "src": doc,
            "dst": moved,
            "app_id": "br.com.biglinux.bigimages",
        },
        {"v": 2, "cmd": "origin", "path": moved},
        {"v": 2, "cmd": "save_version", "path": doc},
        {"v": 2, "cmd": "versions", "path": doc},
        {"v": 2, "cmd": "forget_versions", "path": doc},
        {"v": 2, "cmd": "pause"},
        {"v": 2, "cmd": "resume"},
        {"v": 2, "cmd": "rescan"},
        {"v": 2, "cmd": "reindex", "path": f"{home}/Documents"},
        {"v": 2, "cmd": "rebuild"},
        {"v": 2, "cmd": "clear_origin"},
        {"v": 2, "cmd": "clear_history"},
        {"v": 2, "cmd": "no_such_command"},
    ]


def ask(sock_path: Path, line: str) -> str:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(15)
        stream.connect(str(sock_path))
        stream.sendall(line.encode() + b"\n")
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = stream.recv(65536)
            if not chunk:
                break
            reply += chunk
    return reply.decode().strip()


def scratch_home(scratch: Path) -> tuple[Path, Path, dict[str, str]]:
    """A home with one document, its runtime directory and the daemon's environment."""
    home = scratch / "home"
    runtime = scratch / "run"
    (home / "Documents").mkdir(parents=True)
    runtime.mkdir(mode=0o700)
    (home / "Documents/relatorio.txt").write_text("relatório trimestral\n")
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin"),
        "HOME": str(home),
        "XDG_RUNTIME_DIR": str(runtime),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_STATE_HOME": str(home / ".local/state"),
        "LANG": "C.UTF-8",
    }
    return home, runtime, env


def await_indexed(sock_path: Path, server: subprocess.Popen) -> str | None:
    """Wait until the daemon answers with something indexed; why not, otherwise."""
    deadline = time.monotonic() + 30
    while not sock_path.exists():
        if time.monotonic() > deadline or server.poll() is not None:
            return "the daemon did not open its socket"
        time.sleep(0.1)
    while json.loads(ask(sock_path, '{"v":2,"cmd":"status"}')).get("indexed", 0) == 0:
        if time.monotonic() > deadline:
            return "the daemon indexed nothing"
        time.sleep(0.2)
    return None


def exchanges(sock_path: Path, home: Path) -> list[str] | str:
    """Each request with its answer, the home spelled `/home/p`; or what failed."""
    pairs = []
    for request in requests(str(home)):
        line = json.dumps(request, ensure_ascii=False, separators=(",", ":"))
        reply = ask(sock_path, line)
        if not reply:
            return f"no answer to {request['cmd']}"
        pairs.append(
            json.dumps(
                {"request": json.loads(line), "reply": json.loads(reply)},
                ensure_ascii=False,
                separators=(",", ":"),
            ).replace(str(home), SPELLED)
        )
    return pairs


def main() -> int:
    daemon = sys.argv[1] if len(sys.argv) > 1 else str(DAEMON)
    if shutil.which(daemon) is None:
        print(
            f"{daemon} not found; build it with cargo build --release", file=sys.stderr
        )
        return 2
    # In /tmp: the daemon skips what lies below a hidden or build folder, such
    # as ~/.cache or target/. The scratch home holds data only.
    with tempfile.TemporaryDirectory(prefix="search-wire-", dir="/tmp") as scratch:
        home, runtime, env = scratch_home(Path(scratch))
        server = subprocess.Popen(
            [daemon, "daemon"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            sock_path = runtime / "biglinux/indexd.sock"
            failure = await_indexed(sock_path, server)
            pairs = failure or exchanges(sock_path, home)
        finally:
            server.terminate()
            server.wait(timeout=10)
    if isinstance(pairs, str):
        print(pairs, file=sys.stderr)
        return 1
    FIXTURES.write_text("\n".join(pairs) + "\n")
    print(f"{len(pairs)} exchanges written to {FIXTURES.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
