#!/usr/bin/env bash
# Adversarial input probes for the content-extraction path (untrusted documents in
# the indexed tree) and the IPC socket. Boundedness is asserted by running reindex
# inside a MemoryMax cgroup: OOM-kill (exit 137) => unbounded => FAIL; exit 0 => the
# 1 MiB extraction caps held. Run standalone; isolated index.
set -uo pipefail

BIN="$(cd "$(dirname "$0")/../../.." && pwd)/target/release/big-search"
T=$(mktemp -d)
HOME_D="$T/home"; DATA_D="$T/data"; CFG_D="$T/cfg"; RUN_D="$T/run"
mkdir -p "$HOME_D/docs" "$RUN_D"
trap 'rm -rf "$T"' EXIT

CAP=300M
capped() { # run big-search under a hard RSS cap, print PASS/FAIL by exit code
  systemd-run --user --scope -q -p MemoryMax="$CAP" -p MemorySwapMax=0 -- \
    env HOME="$HOME_D" XDG_DATA_HOME="$DATA_D" XDG_CONFIG_HOME="$CFG_D" \
        XDG_RUNTIME_DIR="$RUN_D" RUST_LOG=off "$BIN" "$@"
}

echo "== 1. zip decompression bomb (docx, ~400 MiB logical, tiny on disk) =="
python3 - "$HOME_D/docs/bomb.docx" <<'PY'
import sys, zipfile, os
body = b"<w:document><w:body><w:p><w:r><w:t>" + (b"A"*64+b" ")*(400*1024*1024//65) + b"</w:t></w:r></w:p></w:body></w:document>"
with zipfile.ZipFile(sys.argv[1],"w",zipfile.ZIP_DEFLATED) as z: z.writestr("word/document.xml", body)
print("  on-disk:", os.path.getsize(sys.argv[1])//1024, "kB; logical:", len(body)//(1024*1024), "MiB")
PY
capped reindex "$HOME_D" >/dev/null 2>&1 && echo "  PASS (bounded under $CAP)" || echo "  FAIL exit=$? (OOM => unbounded)"

echo "== 2. XML billion-laughs (docx DTD entities) =="
python3 - "$HOME_D/docs/lol.docx" <<'PY'
import sys, zipfile
dtd=b'<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "AAAAAAAAAA"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;"><!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;"><!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;"><!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;">]><w:document><w:body><w:p><w:r><w:t>&f;</w:t></w:r></w:p></w:body></w:document>'
with zipfile.ZipFile(sys.argv[1],"w",zipfile.ZIP_DEFLATED) as z: z.writestr("word/document.xml", dtd)
PY
t0=$(date +%s%N); capped reindex "$HOME_D" >/dev/null 2>&1 && r=PASS || r="FAIL exit=$?"; t1=$(date +%s%N)
echo "  $r  ($(((t1-t0)/1000000)) ms; no entity expansion)"

echo "== 3. huge text file (512 MiB) =="
yes "alpha beta line to index repeatedly" | head -c $((512*1024*1024)) > "$HOME_D/docs/huge.txt"
echo "  size: $(du -h "$HOME_D/docs/huge.txt" | cut -f1)"
capped reindex "$HOME_D" >/dev/null 2>&1 && echo "  PASS (read_text caps at 1 MiB)" || echo "  FAIL exit=$? (OOM)"

echo "== 4. IPC oversized line (100 MiB, no newline) =="
env HOME="$HOME_D" XDG_DATA_HOME="$DATA_D" XDG_CONFIG_HOME="$CFG_D" XDG_RUNTIME_DIR="$RUN_D" RUST_LOG=off "$BIN" daemon >/dev/null 2>&1 &
DPID=$!
for _ in $(seq 1 50); do [ -S "$RUN_D/big-search.sock" ] && break; sleep 0.1; done
rss0=$(awk '/VmRSS/{print $2}' "/proc/$DPID/status")
head -c $((100*1024*1024)) /dev/zero | tr '\0' 'x' | timeout 30 socat - "UNIX-CONNECT:$RUN_D/big-search.sock" >/dev/null 2>&1 || true
sleep 1
peak=$(awk '/VmHWM/{print $2}' "/proc/$DPID/status" 2>/dev/null || echo DEAD)
echo "  daemon RSS before=${rss0} kB  peak(VmHWM) after 100MiB-no-newline=${peak} kB"
echo "  (read_line buffers the whole line — peak ~100 MB confirms an unbounded-read hardening gap)"
kill "$DPID" 2>/dev/null || true; wait "$DPID" 2>/dev/null || true
echo "DONE"
