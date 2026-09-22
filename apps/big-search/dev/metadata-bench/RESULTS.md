# Metadata extraction benchmark — native tools vs Rust crates vs none

Question: should big-search detect file **type by content** (magic) and extract
**metadata**, and if so via existing **system commands** (the `pdftotext` pattern)
or **in-process Rust crates**? This measures the cost of each, to drive a
per-feature, configurable design.

## Method

- Harness: `rust-bench/` (standalone crate, outside the workspace). One process
  loops over a corpus (one path/line), runs one approach per file, reports wall
  (monotonic), CPU (`getrusage`), peak RSS.
- **Warm page cache** (corpus pre-read once). The file read is common to every
  approach, so warm isolates the *parse / process-spawn* delta — the only thing
  that differs. Cold disk adds the same I/O to all rows.
- Median of 3 runs. Real files from the live index (`~/`, btrfs/nvme).
- RSS semantics differ by design: Rust rows = `VmHWM` of the harness (the
  **steady in-process** cost a daemon pays once); native rows = `ru_maxrss` of
  the spawned child (the **transient per-file spike**, paid on every file).
- Host: i5-13400 (16 threads), rustc 1.95.0 release (`lto`, `opt-level=3`).
  Tools: file 5.47, pdfinfo (poppler) 26.05, ffprobe n8.1.1, exiftool 13.55,
  ImageMagick 7.1.2.
- Reproduce: `(cd rust-bench && cargo build --release) && bash run-bench.sh`.

## Results (warm, median of 3)

| Group | Approach            | Files | OK | ERR | **ms/file** | total ms | CPU ms | RSS KB |
|-------|---------------------|------:|---:|----:|------------:|---------:|-------:|-------:|
| type  | native `file`       |    72 | 72 |   0 |       2.150 |    154.8 |  139.2 | 20 848 |
| type  | **rust `infer`**    |    72 | 62 |  10 |   **0.003** |      0.2 |    0.2 |  3 080 |
| type  | baseline (stat)     |    72 | 72 |   0 |       0.001 |      0.1 |    0.0 |  2 980 |
| pdf   | native `pdfinfo`    |    40 | 40 |   0 |       8.191 |    327.6 |  318.6 | 15 576 |
| pdf   | rust `lopdf`        |    40 | 40 |   0 |       1.905 |     76.2 |  285.3 | **92 264** |
| pdf   | baseline (stat)     |    40 | 40 |   0 |       0.002 |      0.1 |    0.0 |  2 940 |
| audio | native `ffprobe`    |    40 | 28 |  12 |      36.859 |   1474.4 | 1428.9 | 49 872 |
| audio | native `exiftool`   |    40 | 28 |  12 |      95.013 |   3800.5 | 3770.9 | 32 856 |
| audio | **rust `lofty`**    |    40 | 28 |  12 |   **0.026** |      1.0 |    1.3 |  3 668 |
| audio | baseline (stat)     |    40 | 40 |   0 |       0.001 |      0.0 |    0.1 |  2 968 |
| image | native `identify`   |    86 | 86 |   0 |       5.305 |    456.2 |  444.9 | 12 944 |
| image | native `exiftool`   |    86 | 86 |   0 |      65.678 |   5648.3 | 5734.4 | 37 428 |
| image | **rust `imagesize`**|    86 | 86 |   0 |   **0.006** |      0.5 |    0.6 |  3 004 |
| image | rust `kamadak-exif` |    86 | 33 |  53 |       0.090 |      7.7 |    6.8 |  3 304 |
| image | baseline (stat)     |    86 | 86 |   0 |       0.001 |      0.1 |    0.1 |  2 984 |
| video | native `ffprobe`    |    66 | 63 |   3 |      76.243 |   5032.0 | 4890.1 | **94 288** |
| video | rust `lofty`        |    66 | 40 |  26 |       0.016 |      1.1 |    1.0 |  3 284 |
| video | baseline (stat)     |    66 | 66 |   0 |       0.001 |      0.1 |    0.1 |  2 976 |

`ERR` is not always failure: audio's 12 ERR appear across **every** tool
(ffprobe, exiftool, lofty) → genuinely unreadable/foreign `.mp3` in the corpus,
not a tool defect. Image-EXIF's 53 ERR = images with **no EXIF** (PNGs, JPEGs
without it). Video-lofty's 26 ERR ≈ the 26 `.mkv` files (lofty is an
audio/MP4 library; it does not parse Matroska). infer's 10 ERR = types outside
its (smaller-than-libmagic) magic table.

## Per-category reading

- **Type (magic):** `infer` ~**700×** faster than `file`, **7× less RAM**, no
  process spawn. Cost: smaller type table (10/72 unknown vs 0). → use infer for
  the hot path; fall back to `file` *only on an infer miss* if full coverage is
  needed. Solves "PDF without `.pdf`" essentially for free.
- **Audio:** `lofty` ~**1400×** faster than ffprobe, ~**3600×** vs exiftool,
  ~10× less RAM, **same coverage** (identical 12 ERR). Unambiguous Rust win.
- **Image dimensions:** `imagesize` ~**880×** faster than `identify`, full
  coverage, ~3 MB. Unambiguous Rust win.
- **Image EXIF:** `kamadak-exif` ~**730×** faster than exiftool, ~3 MB. Cheap.
- **PDF:** `lopdf` is 4× faster wall than `pdfinfo` but its CPU is the same and
  it costs **92 MB RSS** (loads the whole document) vs poppler's 15 MB
  incremental. For a lean daemon (RSS target ~31 MB flat) that is a regression.
  → keep the **system command** (`pdfinfo`), matching the existing `pdftotext`
  choice. This is the "intelligent: reuse the tool already a dependency" case.
- **Video:** the hard one. `ffprobe` is slow (76 ms/file) and heavy (94 MB) but
  covers **every** container. `lofty` is instant but MP4-only (misses MKV/WebM —
  the 26 ERR). No cheap pure-Rust universal video-duration option exists. → make
  video metadata **opt-in, off by default**; when on, either ffprobe (full,
  throttled) or lofty (MP4 only). This is exactly why per-feature config matters.

## Recommendation → configurable, tiered, "intelligent"

| Feature        | Approach                         | Default | Why |
|----------------|----------------------------------|---------|-----|
| type sniff     | `infer` (+ `file` on miss, opt)  | **on**  | ~free, fixes wrong/missing extensions |
| audio meta     | `lofty` (Rust)                   | **on**  | 1400×, same coverage, ~3 MB |
| image dims     | `imagesize` (Rust)               | **on**  | 880×, full coverage |
| image EXIF     | `kamadak-exif` (Rust)            | opt     | cheap but only some files carry it |
| pdf meta       | **system `pdfinfo`**             | opt     | avoids lopdf's 92 MB; reuses poppler |
| video meta     | `ffprobe` (full) / `lofty` (mp4) | **off** | 76 ms/file + 94 MB — gate it |

Proposed daemon config (TOML), each feature individually switchable:

```toml
[index]
sniff_type = true          # infer; cheap content-based type

[metadata]
audio = true               # lofty
image = true               # imagesize
image_exif = false         # kamadak-exif
pdf = "pdfinfo"            # "off" | "pdfinfo" (system) | "lopdf"
video = "off"             # "off" | "ffprobe" (all) | "mp4" (lofty)
```

### Memory guardrails (daemon target ~31 MB flat)

Pure-Rust audio/image/type stay ~3 MB. The two RSS hazards are `lopdf` (92 MB)
and `ffprobe`-on-video (94 MB transient/spawn). Both are gated off/system-command
above. Whatever ships must keep the existing throttle (nice 19 + idle ioprio,
bounded workers) and extend the fuzz/security-probe corpus to the new parsers
(new untrusted-input surface).

## Caveats

- Warm-cache numbers; cold-disk adds equal I/O to every row (does not change the
  ranking). 40–86 files/type — enough for a stable ranking, not a census.
- Native CPU includes process spawn (fork+exec); for short tools (`file`) spawn
  dominates, which is the realistic per-file daemon cost.
- Outcome: `file-format` / `symphonia` / `imagesize` were **adopted** by the
  daemon (added to the crate registry + `approved-crates.txt`). `infer` and
  `lopdf` were **rejected** (file-format = wider coverage at the same speed;
  lopdf = 92 MB RSS) and their harness modes were **removed** — the numbers above
  are kept as the record, but the crates are no longer dependencies of anything.
  `lofty` / `kamadak-exif` stay bench-only (already in the registry; symphonia
  replaced lofty for the daemon).
