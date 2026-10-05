# Lean Local Search Daemon — Design

> **Errata (2026-08).** Parts of this document now describe decisions the code
> has moved past. Read it for the reasoning, not as a description of the tree:
>
> - **§3.1 "one Tantivy index"** — there are two: a name index and a separate
>   content sidecar.
> - **§3.1 "Tantivy commit is the durability boundary (atomic, fsync)"** — the
>   daemon no longer fsyncs at all (`src/nosync_dir.rs`). Both indexes are a
>   cache derived from the filesystem, so a torn index is recreated on open
>   rather than paid for on every commit; measured, fsync was 4.4x of the
>   daemon's entire write volume.
> - **§4 decision 4 "flat atomic append-log"** — the state store is SQLite
>   (`state.sqlite`), one row per path per namespace.
> - **§6 socket path `lsd.sock`** — it is `$XDG_RUNTIME_DIR/biglinux/indexd.sock`.
> - **§3.1 deferred in-RAM name table** — built and measured in 2026-08, then
>   reverted: isolated it beat the ngram index 8x, but in the daemon it changed
>   nothing (the engine was never the bottleneck) and cost 104 MB of RSS.
> - **the body tokenizer is a stream** (2026-09). It used to build a vector of
>   every character in the document and then a `String` per token: a 32 MiB body
>   cost 925 MiB of resident memory and 534 ms, against a daemon whose whole
>   budget is 63 MB. Now it holds one token at a time and reuses it — 101 ms and
>   nothing beyond the text itself. The tokens, byte offsets and positions are
>   proved identical to the old implementation, which stays in the tests as the
>   thing it is compared against.
> - **page search reads one page at a time** (2026-09). Answering "which pages
>   hold these words" used to materialize every page of the document and fold a
>   copy of each. On a PDF with a 32 MiB text layer: 4.9 s and 106 MiB before,
>   3.9 s and 67 MiB after, same answer.
> - **reindex of a 94 MB corpus** (2026-09, 57 real documents): 3.8 s wall,
>   4.9 s CPU and 298 MiB peak before; 2.4 s, 3.1 s and 63 MiB after. Query
>   latency was 1-3 ms warm before and after — it was never the problem.

Status: design draft. Supersedes the old btrfs-ioctl + Python/SQLite approach
(measured slower than `fd`/`plocate`; the "btrfs B-tree" path was dead code).

## 1. Goal & non-goals

**Goal.** Resident, unprivileged, low-footprint daemon that answers two queries
over the user's *visible* home subtree:

1. **Filename search** — substring/prefix, interactive latency (target < 10 ms warm).
2. **Content search** — full-text over a small set of common document types
   (`txt md pdf docx odt ods pptx epub eml rtf doc`), keyword, ranked.

Stays fresh in real time. Lean code: only what these two jobs need.

**Non-goals (deliberately excluded — this is where the efficiency comes from):**
- No hidden-dir indexing (`.cache`, `.config`, browser profiles…). ~90% of home
  dirs and ~all write churn. Skipping them is the core scope decision.
- No rich metadata (EXIF/ID3/MIME ontology), no RDF/SPARQL — that is Tracker.
- No D-Bus, no HTTP server, no network, ever.
- No semantic/embedding search.
- No daemon running as root (see §6). No btrfs ioctl.
- No media/audio/video, no OCR.

## 2. Measured premises (this machine, /home, 2026-06)

| Fact | Value | Consequence |
|---|---|---|
| Total files | 2,193,804 | full-home index is wasteful |
| Visible files (no hidden, gitignore) | 243,209 | the real index target |
| **Visible dirs** | **28,332** | inotify watch count — fits easily |
| `fs.inotify.max_user_watches` | 600,000 | 21× headroom; no sysctl bump needed |
| inotify, visible-only | unprivileged, ~30 MB | **chosen** over fanotify |
| fanotify whole-fs | 1.8 MB but **root** + all-fs noise | rejected: root + churn filter cost |
| Live extraction (1315 pdf, rga) | 45 s, 16 cores @100% | **extraction must be throttled + cached** |
| plocate selective query | 1.5 ms | latency bar for filenames |

## 3. Architecture

Single binary, two modes (`daemon`, and a thin `query` client). Synchronous core
+ std threads + channels. **No async runtime** (no tokio) — fewer deps, leaner.

```
            ┌──────────────────────────── daemon (user, unprivileged) ───────────┐
  startup → │  scan  ──recs──►  index queue ──►  IndexWriter (Tantivy)            │
            │   (ignore crate: skip hidden + .gitignore)         │               │
  runtime → │  watch (inotify, ~28k wd) ──events──► debounce ──► index queue     │
            │      │ new dir → add watch + rescan                                 │
            │      │ overflow → mark subtree dirty                                │
            │  extract pool (N=2, nice+ionice, rlimit, timeout) ──text──►         │
            │  state map  path → (mtime,size,doc_id)  [dedup + deletes]           │
            │  query: warm Tantivy reader ◄── Unix socket (0600, $XDG_RUNTIME_DIR)│
            └────────────────────────────────────────────────────────────────────┘
                                              ▲
                              thin CLI client ─┘ (connect, send query, print)
```

### 3.1 Index engine — Tantivy (one engine, two fields)

One Tantivy index. Schema:

| field | type | tokenizer | purpose |
|---|---|---|---|
| `path` | STRING stored | raw | identity, delete-by-term, display |
| `name` | TEXT indexed | **ngram(2,3)** | filename substring/prefix |
| `ext`  | STRING indexed | raw, lowercase | type filter: one term per extension |
| `dir`  | STRING indexed | raw, one per ancestor folder | folder filter: `under` is one term |
| `mime` | STRING indexed + stored | raw: the type, then `major/*` | type filter; from the shared-mime-info globs, the first 40 KiB read only when the name has no type or several |
| `mtime`,`size` | u64 FAST | — | sort/filter |
| `body` | TEXT indexed | default+lowercase | content full-text (no store) |

Rationale: one engine = lean code. ngram field gives filename substring without a
separate trigram structure. If filename latency ever misses the < 10 ms bar, add a
dedicated in-RAM `memchr` name table later — **deferred, not built now.**

Deletes: `delete_term(path)` then re-add. Commits batched (debounce window or N docs)
— Tantivy commit is the durability boundary (atomic, fsync).

**Segments and reads (2026-10, measured on an i3-2375M with a 5400 rpm disk).**
Queries read through their own index instances mapped `MADV_RANDOM`
(`index::open_for_queries`): with the kernel's read-ahead the first query on a
cold cache read 35 MB of a 47 MB index whatever it asked for. The writer keeps
read-ahead, which merges need (a build without it took 104 s instead of 49 s).
A cold query pays per segment (~70-100 ms each on that disk), and tantivy's
merge policy leaves 7-12 segments and never merges a large one for its deletes
(the daemon's largest segments held a third of dead documents). So when a
content backfill pass finishes, off battery, each index is merged into one
segment once a tenth of it changed, and otherwise its small segments are joined
into one (`watch::compact_when_worth_it`). Over 18 000 simulated edits that wrote
less than the default policy (334 vs 355 MB) and left 2 segments instead of 4 at
its worst point: cold queries 124-374 ms instead of 335-655 ms, 55-213 ms right
after a full merge. On the notebook's own index a full merge took 61 MB to 32 MB
and its cold queries from 743-1041 ms to 42-62 ms. SQLite FTS5 was measured
against this on the same machine and corpus: 7-65x slower with a warm cache on
common terms, slower to build, larger — tantivy stays.

**Name search (2026-10).** Names are folded once, before the grams
(`index::name_text`: `extract::fold_lower` — accents precomposed or decomposed,
Latin through tantivy's `AsciiFoldingFilter`, ё→е, Greek tonos and ς — with
separators as spaces and a space at each end), and the post-filter reads the
same text, so recall and verification can no longer disagree. Hits are ranked:
the whole name (without extension) first, then the query as a word of the name,
as the start of a word, anywhere; within a kind the shorter name (field norms on
`name`), and among equal scores the newest (`mtime_day` fast field, a pure
tie-break — as a stronger factor it moved 42 of 165 known-item queries, as many
down as up). One or two letters stand for a whole name or a whole word ("go" →
GO.pdf, then "Contrato GO.pdf", never Google.pdf); longer words use 3-grams only.
On 165 known-item queries from a real home (`local-agent-mini/tools/
name_known_item.py`), first-place hits went from 15-19 % to 19-25 % and every
short-name file is now found (14 of 15 were not). A new name schema rebuilds only
the name index: the content rows stay unless the content index is empty.

**Content prefix and paging (2026-10).** The last content word is read as typed
so far (`query::last_word_query`): it also matches the indexed words it begins,
most frequent first, within 32 words and 50 000 documents, the exact word boosted
— as-you-type, and Portuguese/Russian suffixes ("relatorio" → "relatorios",
"квартал" → "квартале"). Every word, wherever it stands, also matches its plural
or singular by suffix rules on folded words (`query::inflections`: pt/es -ões,
-ais, -eis, -ns, -res, -zes and -s, en -es after x/ch/sh), ranked below the word
as typed. Snowball was measured on this index's own 5.5 million-term vocabulary
first: on folded words it joined none of 670 -ões, 421 -ais, 261 -eis and 194
-ns pairs to their singulars, merged "casa" with "caso", "casado" and "case",
and in Russian did not even join "квартал" to "квартале"; the rules joined every
such pair, in both directions. Totals went from 363 to the ceiling for
"relatorios", 898 to 1509 for "acoes", 367 to 764 for "responsaveis", with no
change in latency. Russian inflection beyond the last-word prefix ("кварталы" →
"квартале") is not covered. Content words are folded at index time like names
(`index::ScriptFold` over `extract::fold_script_into`, then ASCII folding:
combining marks, ё, Greek accents and ς, Arabic hamza alef, alef maqsura, vowel
signs, tatweel and article), and the tokenizer keeps combining marks inside a
word so a decomposed accent no longer splits it. One CJK character finds the
bigrams it opens, in content and names. A query-time variant scheme was built
first to spare installed machines a re-index; the program is not installed
anywhere yet, so the one-time re-index won (88 055 documents in about 13 minutes
here, part of it under heavy load). The 42-query test in 12 languages went from
23 to 39 hits; left are simplified/traditional Chinese (needs a dictionary) and
German "oe" for "ö".

The backfill's duty cycle follows the CPU time the service used in a batch, not
the batch's wall time: at idle priority on a busy machine a 250-file batch took
29 s of mostly waiting, and resting four times that put the re-index on course
for more than ten hours. On the i3's 5400 rpm disk
the expansion cost nothing measurable cold ("zz relatorio" 1013 ms against
"relatorio zz" 1019 ms, both dominated by opening the indexes), and a tighter
budget (16/10 000) changed no timing while dropping Portuguese words from short
prefixes in a code-heavy home. A query reads only the candidates its page needs
when the index answers every filter, and counts its total from the postings up to
the ceiling (`count_capped`); warm, a name query went from 17-24 ms to 3-5 ms and
"both" from 19-25 ms to 5-18 ms. Names carry their folded stem as a term so the
exact name stays first among few candidates.

**Memory and build (2026-10).** Query readers keep 8 decompressed store blocks
per segment instead of tantivy's 100 (`index::query_reader`): on fresh daemons
after the same 60 queries, 23.6 MB of anonymous memory against 26.2-27.0 MB.
tantivy is built with only `mmap` and `lz4-compression`: the stemmer, stopword
lists and columnar zstd went unused, and the release binary went from 9.1 to
8.1 MB. The docstore compressor thread stays: it lives only while a segment is
written, and it is spawned by the indexing thread, whose idle policy and cgroup
it inherits. The state migrations for earlier installs are gone; the program is
not installed anywhere yet.

### 3.2 Initial scan — `scan.rs`

`ignore` crate `WalkBuilder` (parallel, the engine behind `fd`/`rg`): respects
`.gitignore`, skips hidden by default — exactly our scope, for free. Emits
`(path, mtime, size, is_dir)` to the index queue. Runs at low priority (§3.5).
Measured walk for this set ≈ tens of ms warm; one-time at first start.

### 3.3 Change monitoring — `watch.rs`

`notify` crate (inotify backend), recursive over each visible root.
- ~28k watches, unprivileged.
- **New dir** (`Create(dir)`): add watch + rescan it (closes the race).
- **Debounce/coalesce**: collapse rapid repeats per path (notify debouncer).
- **Overflow** (`max_queued_events` / inotify `IN_Q_OVERFLOW`): mark the affected
  root subtree *dirty*, schedule a scoped rescan; never lose correctness.
- Hidden/gitignored paths filtered at event intake (cheap) so we never index them.

### 3.4 Content extraction — `extract.rs`

Bounded pool, **default 2 workers**, each at background priority (§3.5) + `setrlimit`
(CPU time + address space) + a wall-clock backstop against hangs → kill on overrun,
keeping the text written so far.
Per format, lightest tool, **argv arrays only — never a shell**:

| Format | Method | Dep | Phase |
|---|---|---|---|
| txt, md | read directly, cap size, binary-sniff | — | **P3 (MVP)** |
| pdf | `pdftotext` subprocess, bounded parallel + throttled (beat `pdf_oxide` 4× speed, 12× memory in P3.1) | poppler (runtime) | P3.1 |
| docx, odt, ods, pptx | unzip + parse the body XML (`zip`+`quick-xml`) | `zip` + `quick-xml` | P3.2 |
| epub | container → package → spine chapters, read as HTML (`zip`+`quick-xml`) | — | done |
| eml | headers + plain/HTML body; MIME parts, base64, quoted-printable, RFC 2047 (`extract_mail.rs`) | — | done |
| rtf | control words, `\'hh` / `\u` escapes, skipped destinations (`extract_rtf.rs`) | — | done |
| doc (legacy, OLE2) | `antiword` subprocess, same deadline/cap as `pdftotext`; RTF saved as `.doc` goes to the RTF reader | antiword (runtime) | done |

**Extraction — PDF via `pdftotext` subprocess, bounded parallelism (P3.1 head-to-head).**
The P3.1 spike compared the in-process pure-Rust path (`pdf_oxide`) against the
classic subprocess path (`pdftotext`/poppler) on the same 1316-PDF corpus:

| metric (1316 pdf, -P4) | **pdftotext** | mutool convert -F text | pdf_oxide (4 threads) |
|---|---|---|---|
| wall time | **16.0 s** | 40.4 s | 43 s |
| files with text | **1263** | 1121 (−142 coverage) | 1263 |
| peak RSS | **158 MB** | 690 MB | 1900 MB |
| memory isolation | auto (proc exits) | auto | accumulates |
| license (subprocess, no contamination) | GPL | AGPL | MIT |

Three engines tested at depth, each with its *correct* invocation (the first try
used the wrong one twice — `pdf_oxide` editor API, `mutool draw` which renders):
- on a 200-file *clean* subset, mutool (3.0 s) and pdf_oxide (single-file) look
  competitive with or faster than pdftotext;
- on the *full heterogeneous corpus* (OCR scans, encrypted, exotic fonts, mixed
  sizes — i.e. a real home) **pdftotext wins every axis**: fastest, highest text
  coverage, lowest memory. mutool `convert -F text` is ~2.5× slower with 142 fewer
  files yielding text; pd f_oxide accumulates ~1.9 GB.
The clean-subset wins do not survive the messy real corpus → **pdftotext**.

**Decision: `pdftotext`, NOT `pdf_oxide`.** Note the headline table is the *full*
corpus, which is skewed by a few pathological files (a 3000-page manual = 11 s alone,
OCR scans). Controlled re-measurement is fairer to pdf_oxide:
- single file, normal PDF: pdf_oxide ≈ pdftotext (30 files: 3.0 s vs 2.83 s, 72 MB).
  The editor-API vs low-level `PdfDocument` API made no difference — not an impl bug.
- 200 normal PDFs, 4-way parallel: pdf_oxide 5.42 s / 226 MB vs pdftotext 4.02 s /
  67 MB → **1.35× slower, ~3.4× memory**.
- full corpus (with outliers): the 4×/12× gap above — pdf_oxide degrades badly on
  pathological files and accumulates memory across a long multi-thread run.

So pdf_oxide is *competitive per normal file*, not garbage — but pdftotext still wins
on every axis that matters here: faster (process isolation parallelizes cleanly, no
allocator contention), 3–12× lighter, and **robust on outliers** (where pdf_oxide
blows up to seconds + GB, a separate pdftotext process degrades gracefully and frees
on exit). Process-per-file is fine: Linux spawn ≈ 8 ms, free memory isolation, no
recycled-pool machinery. pd f_oxide's "0.8 ms/doc, 5× PyMuPDF" claim is false on real
data (~100 ms/file, like every engine); its 167-crate toolkit is also dead weight.

*Correction to an earlier note in this doc's history:* the "16-core/45 s" figure was
**`rga` (ripgrep-all)** overhead (cache DB + search + adapter spawn), NOT pdftotext.
Pure pdftotext is 10.8 s. The "never fork-per-file" rule does not apply at this scale.

- **pdf** → `pdftotext -q - -` (stdin/stdout, argv array, no shell), worker pool
  bounded to ~`min(4, ncpu/4)`, each `nice(19)+ioprio idle`, per-call wall-clock
  timeout + `RLIMIT_AS`. Process exit reclaims memory automatically. Runtime dep:
  poppler (ubiquitous; GPL, fine for BigLinux). 4 % text-less scans → filename-only,
  opt-in OCR later.
- **docx/odt/ods/pptx** → in-process `zip`+`quick-xml`. These formats *are* zip+XML;
  the maintained "native" office crates (`undoc`, `text_analysis`, `litchi`) do the
  exact same thing internally (read `word/document.xml`, `content.xml`, …). Manual
  parsing = same technique, fewer/vetted deps, full control (zip-bomb caps: entry
  count, decompressed-size, nesting depth). One unified extractor handles all four —
  body path per family: docx `word/document.xml`; odt/ods/odp `content.xml`;
  xlsx `xl/sharedStrings.xml`+sheets; pptx `ppt/slides/*.xml`. No LibreOffice/Tika/JVM
  (1–2 s startup per file). Rejected: `undoc` (no ODT), `text_analysis` (bundles a
  pdf path we already own), `dotext` (dead 2017).
- **epub / eml / rtf** → read in-process with no new dependency: EPUB is zip+XHTML;
  a saved message is MIME text (`extract_mail.rs`); RTF is markup whose text part a
  small state machine reads (`extract_rtf.rs`). Measured `unzip`+`sed` against the
  in-process office path: 14 ms vs 1.5 ms per file — a subprocess per file costs
  more than parsing the XML. Files of these formats indexed before they were read
  are sent back to extraction once (`state.rs`, `forget_newly_read_formats`).
- **doc (OLE2)** → NOT zip+XML (compound binary): `antiword -m UTF-8.txt -w 0`
  through the same runner as `pdftotext` (deadline, `RLIMIT_AS`, capped stdout). The
  file is told by its first bytes: OLE2 goes to `antiword`, RTF (which Word also saves
  as `.doc`) to the RTF reader. Measured against `catdoc` on 19 real Word files and
  31 LibreOffice-saved ones (LibreOffice's text as reference): `antiword` recall
  0.99 / 1.00, accented words 243/246 and 10842/10858; `catdoc` 0.88 / 0.00 (empty
  on every LibreOffice-saved file), 188/246 accents, and it needs `-s cp1252` or reads
  the text as Cyrillic. Both about 1–2 ms a file. `litchi` (native OLE2) stays the
  watch item if a crate should one day replace the subprocess.
- **Cross-cutting**: extract cache keyed `(path,mtime,size)` — never re-extract
  unchanged; OCR off by default (opt-in, hard-throttled); raw-text mode (no layout);
  page/byte cap + streaming; header-sniff to skip encrypted/corrupt/image-only;
  MIME by content bytes, not just extension.

P3 MVP ships **txt+md content only** (zero new deps, exercises the pool/caps/state
path end-to-end). Binary formats land incrementally after the throttled pipeline is
proven, so a misbehaving extractor never lands on an unproven core.

Caps: skip files > `max_file_bytes` (default 32 MiB), skip detected binaries,
**skip if `(mtime,size)` unchanged** vs state map (no re-extract). Extraction only
runs for content-eligible extensions; everything else is name-only.

### 3.5 Throttle — `throttle.rs`

Scan, extract and history threads move to a cgroup of their own with `cpu.idle`
and take `SCHED_IDLE` with a utilization clamp of 512/1024, nice 19 as the
fallback, and `IOPRIO_CLASS_IDLE`; the programs they start inherit all of it. The
query thread stays at a normal program's weight. Pool width small (2). Goal: never the cause of a perceptible slowdown — the explicit
lesson from the 16-core pdftotext spike.

Measured on an i5-13400 (6P+4E cores, SMT, `intel_cpufreq` + `schedutil`), a CPU
loop in the program's place against the indexer's:

| Indexer | Program sharing its core loses |
|---|---|
| nice 19, unit `CPUWeight=20` | 27 % |
| `SCHED_IDLE`, unit `CPUWeight=20` | 32 % |
| unit `CPUWeight=idle`, or the indexer's own `cpu.idle` group | ~0–1 % |
| on the core's SMT sibling, any policy | 42–50 % |

A thread's policy only counts inside its cgroup, so the unit's weight decides
against other programs. Making the whole unit idle cost the searches: with 16
saturated CPUs a 5000-hit query took 412–831 ms instead of 109–162 ms. So the
indexer gets a group of its own instead, in cgroup v2's threaded mode, which
lets the threads of one process sit in different groups: the unit delegates the
cpu controller (`Delegate=cpu`, `DelegateSubgroup=main`, and
`ProtectControlGroups=private` so the daemon writes only inside its own
namespace), the process and query thread stay in `main`, and
`throttle::prepare_idle_cgroup` makes `main/index` threaded with `cpu.idle` (the
lowest `cpu.weight` before Linux 5.15). Measured with it: with 16 saturated
CPUs the same queries take what they take on an idle machine (77–83 ms by name,
14–15 ms by content), and `pdftotext` starts in `main/index`. Without
delegation (started from a terminal) the daemon leaves cgroups alone and the
thread policies still apply. On every stop systemd logs "Failed to kill control
group …, ignoring: Operation not supported": the kernel refuses `cgroup.kill`
and process listing in a threaded group. It is noise — measured with a child
process placed in `main/index`, the unit still stopped and the child died. No policy helps against the SMT sibling — the two
share one core — which is what the backfill's pacing (`backfill_pace`) is still
for.

The pacing is relative to the hardware it reads (`throttle::topology`: physical
cores and SMT over the CPUs the process may use). The scheduler gives each busy
program a core of its own while it can, so the free cores are the cores minus
the CPUs the others keep busy during the batch. Measured on machines simulated
with `taskset` on P-core pairs (a CPU loop in the program's place, two workers
flat out in an idle cgroup):

| Machine | Program loses |
|---|---|
| no SMT (2 and 4 cores), program on 1 thread or every core | ~0 % (±10 % noise) |
| SMT, free cores ≥ workers | -1–8 % |
| SMT, one free core short | 23–28 % |
| SMT, no free core | 42–49 % at 100 % duty, 17–24 % at 50 %, 2–12 % at 20 % |

So without SMT the CPU never makes the backfill wait; with SMT it runs at a 20 %
duty cycle when the free cores are fewer than its workers and rests 30 s when
none is free. Memory pressure, disk stall (most disk schedulers ignore I/O
priority) and battery still slow it on any machine. Its own CPU stall is no
longer a signal: at idle priority it only says that others are busy.

The clamp holds the clock below turbo where the frequency driver follows the
scheduler: a CPU loop ran at 4.1 GHz unclamped, 2.3 GHz at 512 (base 2.5) on a
P-core; 3.3 and 1.7 GHz (base 1.8) on an E-core. Drivers that choose frequencies
in hardware (`intel_pstate`/`amd-pstate` in active mode) ignore it. Since the
work runs slower and waits for idle CPU, extraction is bounded by CPU time
(`RLIMIT_CPU`, the configured seconds scaled by 1024/512), not wall-clock time:
a PDF waiting its turn on a busy machine must not be killed and recorded as
having no text. The wall-clock limit (3× that) only catches hangs.

### 3.6 State & crash-safety — `state.rs`

SQLite-backed persisted map `namespace,path → (mtime, size)`, stored as a
`WITHOUT ROWID` table keyed by `(namespace, path)`. Used for incremental dedup,
delete detection, crash reconcile on start, and separate name/content freshness
state. Writes are batched in transactions; Tantivy's own commit is the full-text
durability boundary.


### 3.7 Low-memory reconciliation and backfill

The state catalogue is persisted in SQLite, but full reconciliation used to keep a
process-wide `HashSet<String>` of every seen path and then compare it with every
cached path. That made peak RSS proportional to the catalogue size. The low-memory
path now moves the large set operation into SQLite:

- `scan::sync` and overflow/rebuild reconciliation create a temporary
  `current_scan_seen(path TEXT PRIMARY KEY) WITHOUT ROWID` table.
- Seen paths are inserted as the walker emits them; pending state changes flush
  every 2,048 rows.
- Stale paths are selected with `NOT EXISTS` against the temporary table in
  ordered 2,048-row batches, so deletion never materializes the full stale set.
- Offline removable/network source prefixes are inserted into the same temporary
  table before stale deletion, preserving their catalogues when unmounted.
- Content backfill streams the name catalogue with `paths_after(after, limit)` and
  selects a bounded batch by estimated read bytes. A daemon batch is capped by
  count and MiB, so expensive PDFs/offices cannot monopolize RAM.
- `count` without `--under` reads Tantivy's document count directly.
- PDF page lookup uses the same capped `pdftotext` pipe reader as indexing.

SQLite tuning for the state DB is intentionally conservative for old machines:
small cache (`cache_size=-2048`), file-backed temporary storage, WAL with
`NORMAL` sync, and a capped 64 MiB mmap window. Source booleans are packed into
a single-byte `SourceFlags` bit mask; this is not the main saving, but it removes
several separately-stored bool fields from cloned source descriptors.

### 3.8 Query path — `query.rs` + `ipc.rs`

Daemon holds a warm `IndexReader` (reloads on commit). Query protocol over a
**Unix domain socket** in `$XDG_RUNTIME_DIR/lsd.sock`, perms `0600`:
- request: `{mode: name|content|both, q: str, limit: u16, ext?: [str]}` (serde_json, line-framed)
- response: ranked `[{path, score, kind, snippet?}]`
Thin CLI client (`lsd <query>`) connects, prints. No daemon running → client can
fall back to a one-shot in-process search (open index read-only). Interactive
front-ends (fzf, KRunner plugin) feed off the socket later — out of scope now.

## 4. Module layout & size budget

| File | Responsibility | Target LOC |
|---|---|---|
| `main.rs` | arg parse, mode dispatch, wiring | 80 |
| `config.rs` | roots, caps, extensions, XDG paths | 80 |
| `scan.rs` | initial parallel walk → queue | 90 |
| `watch.rs` | inotify, new-dir, debounce, overflow→dirty | 160 |
| `extract.rs` | per-format text, pool, caps, timeout | 200 |
| `index.rs` | Tantivy schema, writer, commit batch, delete | 150 |
| `state.rs` | dedup map, deletes, crash reconcile, atomic IO | 140 |
| `query.rs` | reader, name/content/both search, rank | 120 |
| `ipc.rs` | unix socket server + client + protocol | 130 |
| `throttle.rs` | nice/ioprio/rlimit helpers | 60 |
| **total** | | **~1200 LOC** |

## 5. Dependencies (minimal, pin + lockfile)

`tantivy`, `notify`, `ignore`, `zip`, `quick-xml`, `serde`+`serde_json`,
`anyhow`, `log`+`env_logger`, `libc` (nice/ioprio/rlimit/sockets).
No tokio, no reqwest, no D-Bus. Each confirmed against the BigLinux crate policy
before adding; `deny.toml` crates.io-only; committed `Cargo.lock`.

## 6. Security model

- **Unprivileged user service.** No root, no capabilities, no setuid. (This is why
  inotify+visible-scope beats fanotify: fanotify needs CAP_SYS_ADMIN.)
- **Untrusted input** = every path/event/file. Canonicalize; reject paths escaping
  configured roots; never index symlink targets outside roots (no-follow by default).
- **Subprocess** extraction: argv arrays only, never `sh -c`; closed stdin/stdout
  pipes; rlimit + timeout; non-zero/timeout → skip, log, never crash.
- **Socket**: `$XDG_RUNTIME_DIR`, mode `0600`, AF_UNIX only.
- **No secrets, no network, no logs of file contents** (paths + counts only;
  redact on error).
- **systemd user unit hardening**: `NoNewPrivileges=yes`, `ProtectSystem=strict`,
  `ProtectControlGroups=yes`, `RestrictAddressFamilies=AF_UNIX`,
  `SystemCallFilter=@system-service` (deny @privileged,@mount,@reboot…),
  `MemoryDenyWriteExecute=yes`, `RestrictNamespaces=yes`, `LockPersonality=yes`,
  `ReadWritePaths=` only the index/state dir, home `ReadOnlyPaths`. `WatchdogSec`.

## 7. Phasing (ship usable early)

- **P0** skeleton: config, schema, `main` modes, error type. No behavior.
- **P1** name search MVP: scan → Tantivy name field → query → socket → CLI.
  Usable filename search, unprivileged, no content yet.
- **P2** incremental: inotify watch + new-dir + debounce + state dedup + deletes.
- **P3** content MVP: extract pool **txt/md only**, throttled, caps; `body` field.
- **P3.1** pdf via `pdftotext`. **P3.2** docx/odt via zip+XML. (incremental)
- **P4** robustness: overflow→dirty rescan, crash reconcile, systemd hardened unit,
  resource budgets, `balooctl`-style status/control subcommands.

Each phase ends green on the quality floor before the next.

## 8. Quality gate (per BigLinux Rust policy)

- Floor every change: `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo nextest run`, `cargo doc`.
- Property tests: path canonicalization (no `..` escape), extension routing.
- Fuzz/robustness: malformed pdf/zip/docx must not panic, must time out.
- inotify overflow simulated → correctness preserved (dirty rescan).
- Mutation testing (`cargo mutants`) on `state.rs` dedup/delete + `watch.rs`
  overflow logic — the correctness-critical paths.
- `cargo deny`/`audit`, `gitleaks` pre-release.

## 9. Decisions (locked 2026-06-20)

1. **Name engine**: Tantivy ngram field — one engine, leanest. Dedicated trigram
   deferred; only revisit if filename latency misses < 10 ms.
2. **Query UX**: resident daemon + Unix socket + thin CLI client. Front-ends
   (fzf/KRunner) plug into the socket later.
3. **Content formats, P3 MVP**: **txt + md only**. pdf → P3.1, docx/odt → P3.2,
   ods/pptx/doc → later. Proves the throttled pipeline before any binary extractor.
4. **State store**: flat atomic append-log (tmp+rename+fsync, periodic compaction).
   Zero extra dep.

Resulting MVP dep set (P1–P3): `tantivy notify ignore serde serde_json anyhow
log env_logger libc`. No `zip`/`quick-xml` until P3.2; no `sled`; no tokio.
```
