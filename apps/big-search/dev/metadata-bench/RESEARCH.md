# Deep research — more efficient AND safe metadata detection

Follow-up to `RESULTS.md`. Goal: find faster *and* safer ways to detect file
type + metadata than either spawning system tools or the first-round Rust crates.
Web research (sources at end) + empirical re-measurement on the same corpus.

## 1. Threat model & prior art (why "safe" is the hard part)

Metadata extraction parses **untrusted, attacker-supplied files** with complex
parsers — historically the #1 RCE surface of desktop indexers. GNOME's
`tracker-extract` (now LocalSearch) is the reference design:

- Extraction runs in a **separate process** from the index/query engine (since
  before 2016) — a parser crash or exploit cannot take down or compromise the
  searcher.
- **seccomp** allowlist, **deny-by-default** (every syscall → `SIGSYS` unless
  explicitly needed): no network, no filesystem writes, no `fork`/`exec`. Added
  in 2016, extended to the whole extractor process in Tracker-Miners 3.7.
- **Landlock** added in 3.7: the process can touch only the files it needs.
- Motivation is concrete: **CVE-2023-43641** was a heap corruption in `libcue`
  reachable just by downloading a `.cue` file that the extractor then parsed.

Takeaway: the safe architecture is **sandboxed, isolated extraction** —
independent of which parser you pick. The parser choice then minimises the
attack surface *inside* that sandbox.

## 2. Efficiency landscape

### Type detection (content/magic, not extension)
| Crate | Approach | Coverage | Notes |
|---|---|---|---|
| `infer` | magic, no DB, no_std | small (missed 10/72 here) | tiny/fast |
| **`file-format`** | magic + structure, no DB | broad (hundreds) | pure Rust, in-proc |
| `mimetype-detector` | magic, zero-dep | ~550 formats | broad |
| `tree_magic_mini` | shared-mime-info DB | libmagic-class, fewer FPs | needs system MIME DB |
| `file` (libmagic) | C, spawned | broad | process spawn + C surface |
| Magika 1.0 (Google) | AI (ONNX) | ~200 types, ~99% | **~5 ms/file**, C++ ONNX runtime, model load — too heavy for an always-on daemon; good as an offline high-accuracy pass |

### Audio + video (duration, codec, tags)
| Crate | Covers | Safety | Notes |
|---|---|---|---|
| **`symphonia`** | AAC/ALAC/FLAC/MP3/MP4/OGG/Vorbis/WAV/**MKV/WebM** | **100% safe, no `unsafe`** | one crate for audio *and* the common video containers; metadata feature-gated; probing reads the container header only (no decode) |
| `lofty` | audio tags + MP4 | fast | tag-focused; **no MKV** |
| `mp4parse` (Mozilla) | ISO-BMFF/MP4/MOV | continuously fuzzed in Firefox | MP4 only |
| `matroska-demuxer` | MKV/WebM | pure Rust | MKV/WebM only |
| `ffprobe` (ffmpeg) | everything | huge C surface, spawned | universal but heavy |

### Images / PDF
- `imagesize` (dimensions, header-only) + `kamadak-exif` (EXIF) — pure Rust, ~3 MB.
- PDF: `lopdf` loads the whole doc (**92 MB** in round 1); `pdf` (pdf-rs) is lazier;
  poppler `pdfinfo`/`pdftotext` is C but already a dependency. With a sandbox,
  sandboxed poppler is acceptable; pure-Rust avoids the C surface + the spawn.

## 3. Measured verification (same corpus, warm, median of 3)

The two highest-impact claims, re-measured against round 1 and native:

| Approach | Corpus | OK/ERR | ms/file | CPU ms | RSS KB |
|---|---|---|---:|---:|---:|
| type: `infer` (r1) | mixed | 62/10 | 0.003 | 0.2 | 3 172 |
| type: **`file-format`** | mixed | **72/0** | 0.006 | 0.6 | 3 092 |
| type: native `file` | mixed | 72/0 | 2.232 | 150.7 | 20 968 |
| audio: `lofty` (r1) | audio | 28/12 | 0.025 | 1.0 | 3 856 |
| audio: **`symphonia`** | audio | 28/12 | 0.018 | 1.0 | 3 460 |
| audio: native `ffprobe` | audio | 28/12 | 41.13 | 1501 | 50 040 |
| video: `lofty` (mp4-only, r1) | video | 40/**26** | 0.017 | 0.9 | 3 628 |
| video: **`symphonia`** | video | **63/3** | 0.052 | 3.0 | 3 616 |
| video: native `ffprobe` | video | 63/3 | 76.69 | 4861 | 94 612 |

Findings:

1. **`file-format` > `infer`**: full coverage (72/0 vs 62/10) at the same speed
   (~0.006 ms, ~370× faster than `file`), ~3 MB, no spawn, no system MIME DB.
2. **`symphonia` ≈ `lofty` on audio** but pure no-`unsafe` and broader.
3. **`symphonia` closes the video gap**: *identical coverage to ffprobe*
   (63/3 — the 3 ERR are the same unreadable/exotic files) at **~1475× the speed**
   and **~26× less memory**, in safe Rust. Round 1's "video ⇒ heavy ffprobe ⇒
   opt-in only" conclusion is **overturned** — video duration can be a cheap,
   safe, default-on pure-Rust feature for MKV/WebM/MP4.

## 4. Safety techniques (defense in depth)

1. **Pure, safe Rust parsers** (`symphonia` is `#![forbid(unsafe)]`-class,
   `file-format`, `imagesize`, `kamadak-exif`). Eliminates the C
   memory-corruption RCE class (the CVE-2023-43641 / libcue family) at the source
   and removes the process-spawn tax.
2. **Isolated, persistent, sandboxed extractor process** (mirror tracker-extract,
   but keep it long-lived and fed paths over a pipe so there is no per-file spawn
   cost): seccomp **deny-by-default** + Landlock (read-only over the index roots)
   + `RLIMIT_AS`/`RLIMIT_CPU` + per-file timeout + max-file-size. Idiomatic in
   Rust via **`extrasafe`** (high-level seccomp+Landlock+userns, deny-by-default
   presets) or **`seccompiler`** (rust-vmm, lower level). Unprivileged: user-ns +
   seccomp + Landlock + setrlimit need no root.
3. **cgroup v2 `MemoryMax`** on the worker for a hard memory ceiling (rlimit
   `RLIMIT_AS` is cooperative/address-space; cgroups bound resident pages and are
   stronger against adversarial inputs). The repo's `security-probe.sh` already
   demonstrates `systemd-run MemoryMax` — reuse it for the extractor unit.
4. **Per-feature config = kill-switch**: a CVE in one parser → flip its toggle
   off, no redeploy.
5. **Fuzz every parser** (extend the existing `cargo-fuzz` harness to magic
   sniffing + symphonia probe + image headers); keep the throttle (nice 19 + idle
   ioprio + bounded workers).

## 5. Efficiency techniques (beyond crate choice)

- **Read the header region only** — never the whole file. Magic lives in the
  first 512 B–8 KB; container metadata in the head (MP4 `moov` may be at the end →
  one seek). `symphonia`/`file-format` already do bounded reads; pair with
  `memmap2` for zero-copy where it helps.
- **Cache by `(path, mtime, size)`** and invalidate via the existing inotify
  watch — re-extract only what changed (extends the current skip-unchanged scan).
- **Tiered/lazy**: sniff type for everything (cheap); index *searchable*
  metadata (duration, tags) only for enabled kinds; leave *display-only* metadata
  to lazy read at query time (the player already does this for its result page).
- **No full decode** — probing/demuxing for duration never decodes frames.

## 6. Revised recommendation (supersedes RESULTS.md §Recommendation)

| Feature | Approach | Default | Change vs round 1 |
|---|---|---|---|
| type sniff | **`file-format`** | on | was `infer (+file fallback)` → now full coverage, no fallback/spawn |
| audio meta | **`symphonia`** | on | was `lofty` → no-`unsafe`, broader |
| **video meta** | **`symphonia`** (MKV/WebM/MP4) | **on** | was `ffprobe`/opt-in → now cheap+safe+default-on |
| video (exotic) | `ffprobe` fallback | off | only AVI/MPEG-TS/WMV/FLV symphonia can't read |
| image dims | `imagesize` | on | unchanged |
| image EXIF | `kamadak-exif` | opt | unchanged |
| pdf meta | sandboxed `pdftotext`/`pdfinfo` (reuse poppler) or re-measure `pdf` (pdf-rs) | opt | lopdf rejected (92 MB) |

All behind per-feature toggles; all run inside the sandboxed extractor worker.

```toml
[index]
sniff_type = true          # file-format (content-based; fixes wrong/missing ext)
[metadata]
audio = true               # symphonia
video = true               # symphonia (mkv/webm/mp4); ffprobe only as exotic fallback
image = true               # imagesize
image_exif = false         # kamadak-exif
pdf = "poppler"           # "off" | "poppler" (sandboxed) | "pdf-rs"
[sandbox]
seccomp = true             # deny-by-default; no net/write/exec
landlock = true            # read-only over index roots
mem_max_mb = 256           # cgroup ceiling per extraction
file_max_mb = 512          # skip larger
timeout_s = 10             # per-file
```

## 7. Caveats / open items

- `symphonia` MP3 **VBR duration without a Xing/Info header** may scan more of the
  file (CBR + headered VBR are header-cheap). Measured audio cost stayed ~0.018
  ms/file here, but a pathological VBR-no-header corpus could be slower —
  re-measure if MP3 duration accuracy matters.
- Enable only the symphonia **format** features you need (`mp3`, `isomp4`, `mkv`);
  do **not** pull codec/decode features — we only probe.
- Exotic video containers (AVI/MPEG-TS/WMV/FLV) still need ffprobe; keep it an
  off-by-default, sandboxed, spawned fallback.
- Supply chain: `file-format`, `symphonia`, `imagesize`, `kamadak-exif` must be
  added to the registry + `approved-crates.txt`, with `deny.toml` review, before
  adoption. None are daemon deps yet.

## Sources

- Tracker/LocalSearch seccomp + Landlock: <https://samthursfield.wordpress.com/2024/03/20/status-update-20-03-2024-tinysparql-and-tracker-miners/>, <https://blogs.gnome.org/carlosg/2016/12/08/oh-the-security/>, <https://gitlab.gnome.org/GNOME/tracker-miners/-/blob/master/src/libtracker-miners-common/tracker-seccomp.c>
- CVE-2023-43641 (libcue via extractor): <https://blogs.gnome.org/carlosg/2023/10/10/on-cve-2023-43641/>
- Type detection: <https://docs.rs/tree_magic_mini>, <https://github.com/bojand/infer>, <https://crates.io/crates/file-format>, <https://lib.rs/crates/mimetype-detector>, Magika 1.0 <https://www.infoq.com/news/2025/12/magika-rust-file-type-detector/>
- Media: <https://github.com/pdeljanov/Symphonia>, <https://github.com/mozilla/mp4parse-rust>, <https://github.com/hasenbanck/matroska-demuxer>
- Sandboxing: <https://lib.rs/crates/extrasafe>, <https://github.com/rust-vmm/seccompiler>, <https://landlock.io/rust-landlock/landlock/>, <https://oneuptime.com/blog/post/2026-01-07-rust-sandboxing-seccomp-landlock/view>
