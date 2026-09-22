# Implemented metadata extraction — benchmark (ON vs OFF)

Measures the *shipped* daemon path (not the round-1/2 microbenchmarks): a full
`big-search reindex` of a media-rich tree with metadata reading **enabled**
(`BIG_SEARCH_META=1`, default) vs **disabled** (`BIG_SEARCH_META=0`).

## What's implemented (`src/meta.rs`, env-configurable)

- **Type sniff** (`file-format`) for extension-less files → content-based `kind`.
- **Audio/video** (`symphonia`, no-`unsafe`): container tags + duration, header
  only (MKV/WebM/MP4/OGG/FLAC/WAV), no decode.
- **Image** (`imagesize`): dimensions, header only.
- All indexed as searchable `body` text (no schema change, PROTOCOL_VERSION
  unchanged). `content video`, `content 1920x1080`, `content <artist>` now match;
  an extension-less PDF is found by its text (sniff → `pdftotext`).
- Bounded: `BIG_SEARCH_META_MAX_MB` (default 512); runs only in the throttled
  background pass (nice 19 + idle ioprio). Each feature individually switchable
  (`BIG_SEARCH_META[_SNIFF|_AUDIO|_VIDEO|_IMAGE]`) → also a per-parser CVE
  kill-switch. Parsers are pure safe Rust (no C lib, no spawn for media).

## Method

- Fixture: 226 files symlinked from the corpus (40 pdf, 40 mp3, 40 mp4, 26 mkv,
  40 jpg, 40 png) into a temp tree; isolated `XDG_DATA_HOME`; cold-ish (symlinks
  to real files scattered on disk).
- `BIG_SEARCH_BENCH=1 big-search reindex <tree>` prints wall / CPU
  (`getrusage` RUSAGE_SELF, aggregates worker threads) / peak RSS (`VmHWM`).
  Median of 3. Host i5-13400, rustc 1.95, debug build.

## Results

| Config | names | enriched (body) | wall (median) | CPU | peak RSS |
|--------|------:|----------------:|--------------:|----:|---------:|
| `META=1` (on) | 226 | 225 | 5633 ms | 1627 ms | 70.2 MB |
| `META=0` (off) | 226 | 39 (pdf text only) | 2964 ms | 1199 ms | 69.2 MB |

Delta = metadata for ~187 media/image files:

- **Index time:** +2.67 s for the run (~**14 ms/file**, dominated by cold header
  I/O — the CPU delta is only ~2.3 ms/file). Warm, it is far less; the isolated
  round-2 bench measured symphonia 0.05 ms/file, imagesize 0.006 ms/file.
- **CPU:** +429 ms (+36 %).
- **Memory:** **+1 MB** (70.2 vs 69.2). Flat — the pure-safe parsers avoid the
  92–94 MB spikes that `lopdf` / `ffprobe` would have cost (round-1 finding).
- **Query latency:** unchanged, ~5–7 ms (CLI cold-start dominated; the query path
  is identical — metadata only adds `body` terms).

## Reading

Metadata reading roughly **doubles a cold full-reindex wall time** for a
media-heavy tree (one-time, background, throttled — invisible to the desktop) and
costs ~1 MB RAM and +36 % CPU during that pass; steady-state is incremental
(only changed files, via inotify). Turning it off restores the lean
name+document-text baseline exactly. The cost is in extraction I/O, not memory —
which is the whole point of choosing pure-safe header-only Rust parsers over
spawning `ffprobe`/`exiftool`.

## Full home (real ~/, release build)

The decisive test: a full `reindex` of the actual home (104,870 entries),
release binary, isolated temp index (live daemon untouched). Run OFF then ON.

| Config | content+meta files | wall | CPU | peak RSS |
|--------|-------------------:|-----:|----:|---------:|
| `META=0` (off) | 43 923 | 52.1 s | 68.2 s | 897 MB |
| `META=1` (on)  | 57 855 (+13 932 media/image) | 69.0 s | 74.1 s | 806 MB |

Delta = metadata for ~13.9K media/image files on a cold-ish full index:

- **Index time:** +16.9 s (+32 %) ≈ **~1.2 ms per media/image file** (far below the
  symlink-fixture's 14 ms/file — real media files index closer to warm).
- **CPU:** +5.9 s (+8.6 %). Most CPU is the shared content pass (pdftotext spawns,
  text/office), common to both; metadata parsing is cheap.
- **Memory:** effectively unchanged — ON's 806 MB is *lower* than OFF's 897 MB;
  the ~90 MB swing is tantivy index-merge run-variance, not metadata (the
  controlled fixture showed +1 MB). Both peaks are the 104K-doc *reindex* cost
  (64 MB writer + segment merges + in-flight bodies), not the daemon's
  steady-state serving footprint (~31 MB).

So on the whole machine, turning metadata on costs **about a third more wall time
on the one-time full index** (background, nice 19 + idle ioprio → invisible to
the desktop), trivial extra CPU, and no extra memory. Steady-state stays
incremental (inotify, changed files only). Caveat: ON ran second, so the page
cache warmed the shared name/document reads — the name pass alone dropped 2.52 s →
1.31 s — meaning the true metadata-only wall delta is marginally higher than the
measured +16.9 s; the ~13.9K media header reads it adds are cold in either order.

## Reproduce

```sh
cargo build
TREE=$(mktemp -d); DATA=$(mktemp -d); RUN=$(mktemp -d)
# symlink corpus media into $TREE (see corpus/*.list), then:
for M in 1 0; do
  XDG_DATA_HOME=$DATA XDG_RUNTIME_DIR=$RUN BIG_SEARCH_META=$M BIG_SEARCH_BENCH=1 \
    ./target/debug/big-search reindex "$TREE" | grep '^BENCH'
done
```

## Not yet (documented follow-ups)

- **OS sandbox** (seccomp deny-by-default + Landlock + cgroup `MemoryMax`) in a
  separate persistent extractor process — the RESEARCH.md defense-in-depth layer.
  Needs its own process (per-extraction memory caps require it); rushing seccomp
  into the single daemon process risks breaking it. v1 safety = pure-safe-Rust
  parsers + size bounds + throttle.
- **Protocol v2** `kind`/`duration` in `Filter`/`Hit` (client + big-shell), so
  GUIs can filter by kind. v1 keeps PROTOCOL_VERSION=1 (metadata searchable via
  `content`).
- **Tags by default** breadth + image EXIF (`kamadak-exif`) — opt-in later.
