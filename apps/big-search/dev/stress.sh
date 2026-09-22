#!/usr/bin/env bash
# Long-run leak/growth probe for the daemon: hammer it with file churn (→ inotify
# → reconcile → commit) interleaved with queries, sampling RSS / thread count /
# open-fd count over time. Flat curves => no leak; monotonic growth => leak.
set -euo pipefail

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/big-search"
T=$(mktemp -d)
export HOME="$T/home" XDG_DATA_HOME="$T/data" XDG_CONFIG_HOME="$T/cfg" \
       XDG_RUNTIME_DIR="$T/run" RUST_LOG=off
mkdir -p "$HOME/churn" "$HOME/docs" "$XDG_RUNTIME_DIR"

# Seed some indexable content.
for i in $(seq 1 50); do printf 'alpha beta gamma report %s\n' "$i" > "$HOME/docs/seed_$i.txt"; done
"$BIN" reindex "$HOME" >/dev/null 2>&1

"$BIN" daemon >/dev/null 2>&1 &
DPID=$!
trap 'kill "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null; rm -rf "$T"' EXIT

# Wait for the socket.
for _ in $(seq 1 50); do [ -S "$XDG_RUNTIME_DIR/big-search.sock" ] && break; sleep 0.1; done

sample() { # label
  local rss nthreads fds
  rss=$(awk '/VmRSS/{print $2}' "/proc/$DPID/status")
  nthreads=$(find "/proc/$DPID/task" -mindepth 1 -maxdepth 1 | wc -l)
  fds=$(find "/proc/$DPID/fd"   -mindepth 1 -maxdepth 1 | wc -l)
  printf '%-8s RSS=%6s kB  threads=%3s  fds=%4s\n' "$1" "$rss" "$nthreads" "$fds"
}

ROUNDS=${1:-1500}
sample "warmup"
for r in $(seq 1 "$ROUNDS"); do
  # churn: create + modify + delete files in a watched dir
  printf 'alpha churn payload %s\n' "$r" > "$HOME/churn/f_$r.txt"
  [ "$r" -gt 5 ] && rm -f "$HOME/churn/f_$((r-5)).txt"
  # queries against the warm daemon (name, content, both)
  "$BIN" report          >/dev/null 2>&1 || true
  "$BIN" content alpha   >/dev/null 2>&1 || true
  "$BIN" both gamma      >/dev/null 2>&1 || true
  if [ $((r % 150)) -eq 0 ]; then sample "r=$r"; fi
done
sample "final"
echo "(churn=$ROUNDS rounds, ~$((ROUNDS*3)) queries + $ROUNDS commits)"
