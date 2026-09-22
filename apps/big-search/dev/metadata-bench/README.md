# metadata-bench

Throwaway benchmark answering: detect file **type** (magic) + **metadata** via
**system commands** or **Rust crates**, and what it costs. Drives the
configurable per-feature metadata design. Not shipped, not a daemon dependency.

## Layout

- `rust-bench/` — standalone Rust harness (own `[workspace]`, isolated `Cargo.lock`;
  deps `infer`/`lofty`/`imagesize`/`lopdf`/`kamadak-exif` never touch the daemon).
- `run-bench.sh` — runs every approach × corpus, warms cache, median of `RUNS` (3).
- `corpus/` — per-type path lists (gitignored: machine-specific, hold personal
  absolute paths, regenerable).
- `RESULTS.md` — findings + recommendation (committed).

## Reproduce

```sh
# 1. corpus from the live index (daemon running):
mkdir -p corpus
big-search list -a | grep '/' > /tmp/idx.txt
for e in pdf mp3 mp4 mkv jpg png; do grep -iE "\.$e\$" /tmp/idx.txt | head -40 > corpus/$e.list; done
cat mp3.list > corpus/audio.list; cat corpus/mp4.list corpus/mkv.list > corpus/video.list
cat corpus/jpg.list corpus/png.list > corpus/image.list
for e in pdf mp3 mp4 mkv jpg png; do head -12 corpus/$e.list; done > corpus/mixed.list
# 2. build + run:
(cd rust-bench && cargo build --release)
RUNS=3 bash run-bench.sh
```

Measurement notes (warm cache, RSS semantics native vs rust, ERR meaning) are in
`RESULTS.md`.
