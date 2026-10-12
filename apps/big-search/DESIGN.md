# big-search daemon design

How the daemon is built and why. How to run and configure it is in the
[README](../../README.md); the socket protocol is described once, in the
`big-indexd-client` rustdoc (`crates/foundation/big-indexd-client`).

Measurements quoted here justify a rule; each names its machine where the
hardware matters. "The notebook" is an i3-2375M with a 5400 rpm disk.

## 1. Scope

A resident, unprivileged daemon that answers two questions over the folders a
person can see:

1. Which files have this in their name: substring and prefix, at interactive
   latency (under 10 ms warm).
2. Which files say this: keyword full-text search over plain text, source code,
   HTML, PDF, OpenDocument and Office Open XML (`odt ods odp docx xlsx pptx`),
   EPUB, saved e-mail (`eml`), RTF and Word 97-2003 (`doc`), ranked.

It stays current in real time, and the code holds only what those two jobs need.

Out of scope, and the reason the daemon is light:

- Hidden folders (`.cache`, `.config`, browser profiles). They hold about 90 %
  of a home folder's files and nearly all of its write churn.
- Rich metadata ontologies (EXIF, ID3, RDF/SPARQL): that is Tracker's job.
- D-Bus, HTTP and any network access.
- Semantic or embedding search, OCR, and the content of audio and video.
- Running as root, and btrfs ioctls for discovery.

### Premises

Measured on one real home folder:

| Fact | Value | Consequence |
|---|---|---|
| Total files | 2,193,804 | indexing the whole home is wasteful |
| Visible files (no hidden, gitignore applied) | 243,209 | the real index target |
| Visible folders | 28,332 | one inotify watch each fits easily |
| `fs.inotify.max_user_watches` | 600,000 | 21x headroom, no sysctl change |
| inotify over visible folders | unprivileged, about 30 MB | chosen over fanotify |
| fanotify over the whole filesystem | 1.8 MB, but root and every write on the system | rejected |
| `pdftotext` over 1316 PDFs, 4 in parallel | 16 s with every core busy | extraction is throttled and never repeated for an unchanged file |
| `plocate` selective query | 1.5 ms | the latency bar for names |

## 2. Shape

One binary: `big-search daemon` is the service, every other subcommand is a
client. The core is synchronous: standard threads and channels, no async
runtime.

```text
 daemon (user service, unprivileged)
 +------------------------------------------------------------------------+
 | start    scan (ignore walker) ----> name index      state.sqlite       |
 |            reconciles against the state store                          |
 |                                                                        |
 | running  watcher: one inotify watch per visible folder                 |
 |            250 ms coalescing, then upsert or delete                    |
 |            +--> name index                                             |
 |            +--> content pass: 1-2 workers ----> content index          |
 |            +--> history worker: reflink copies of documents            |
 |          (all of these at idle priority, in their own cgroup)          |
 |                                                                        |
 | queries  socket threads at normal priority read both indexes           |
 +------------------------------------------------------------------------+
        ^  $XDG_RUNTIME_DIR/biglinux/indexd.sock, mode 0600, same UID
        |
   big-search CLI, settings page, other BigLinux programs
```

A query never waits for extraction, and indexing runs on CPU time nobody else
wants (section 7).

| Module | Owns |
|---|---|
| `scan.rs`, `scan_roots.rs` | the walk of visible files; resolving configured roots and top-level symlinks |
| `watch.rs` | inotify, coalescing, overflow rescans, segment compaction |
| `content.rs`, `content_dedupe.rs` | the background content pass; reading reflinked copies once |
| `extract*.rs`, `mime.rs`, `meta.rs` | text, media type and metadata of an untrusted file |
| `index.rs`, `nosync_dir.rs`, `cjk_tokenizer.rs` | the two tantivy indexes, their analyzers and directory |
| `query.rs` | name, content and combined search, ranking, page lookup |
| `catalogue.rs` | the `path -> (mtime, size)` catalogue |
| `ipc.rs` | the socket server |
| `throttle.rs` | background priority, the indexer's cgroup, backfill pacing |
| `history.rs`, `origin.rs`, `browser_downloads.rs` | earlier versions and where a file came from |
| `settings.rs`, `config.rs` | configuration and XDG paths |

## 3. Indexes

### Two tantivy indexes

The name index holds one document per visible file:

| Field | Indexing | Purpose |
|---|---|---|
| `path` | raw, stored | identity, delete by term, display |
| `name` | 2- and 3-grams of the folded name (`NameGrams`), field norms on | substring and prefix search |
| `source` | stored | the device a hit lives on, kept while it is offline |
| `dir` | raw, one term per ancestor folder | "under this folder" is one posting list |
| `ext` | raw, lowercase | "only this extension" is one posting list |
| `mime` | raw, stored: the type, then `major/*` | type filter; from the shared-mime-info globs, reading the first bytes only when the name has no type or several |
| `stem` | raw: the whole folded name without extension | the exact name ranks first |
| `mtime_day` | u64 fast field | newest first among equal scores |

The content index holds `path` and `body`. `body` stores no text and keeps
either document lists or term frequencies (`content_index_mode`, `auto` by
default); changing the mode changes the schema and recreates the content index.
Keeping the two apart lets a name schema change rebuild only the name index:
the content rows stay unless the content index is empty.

A separate in-RAM name table is not used: measured in isolation it beat the
gram index 8x, but in the daemon it changed no query time, since the engine was
never the bottleneck, and cost 104 MB of RSS.

### No fsync

Both indexes and the state store are a cache derived from the filesystem, so
the daemon never calls `fsync` on the indexes (`nosync_dir.rs`). The worst a
forced reboot costs is re-indexing the files whose commits did not reach the
disk, which the startup reconcile does anyway. A torn index fails to open and
`index::open_or_create` recreates it. Measured over ten minutes of near-idle
churn, tantivy's syncs were 145 calls to write a few kilobytes, and 4.07 GB of
logical writes landed as 17.9 GB at the block layer. The directory also watches
`meta.json` with inotify instead of letting tantivy poll it every 500 ms.

### Segments and reads

Queries read through their own index instances mapped `MADV_RANDOM`
(`index::open_for_queries`): on the notebook, with the kernel's read-ahead, the
first query on a cold cache read 35 MB of a 47 MB index whatever it asked for.
The writer keeps read-ahead, which merges need (a build without it took 104 s
instead of 49 s).

A cold query pays per segment, about 70 to 100 ms each on a 5400 rpm disk, and
tantivy's merge policy leaves 7 to 12 segments and never merges a large one for
its deletes (the daemon's largest segments held a third of dead documents). So
when a content backfill pass finishes, off battery, each index is merged into
one segment once a tenth of it changed, and otherwise its small segments are
joined into one (`watch::compact_when_worth_it`, `index::worth_compacting`). On
the notebook, over 18,000 simulated edits this wrote less than the default
policy (334 against 355 MB) and left at most 2 segments instead of 4: cold
queries took 124 to 374 ms instead of 335 to 655 ms, and 55 to 213 ms right
after a full merge. On the notebook's own index a full merge took 61 MB to 32 MB
and cold queries from 743 to 1041 ms down to 42 to 62 ms.

SQLite FTS5, measured on the same machine and corpus, was 7 to 65x slower on
common terms with a warm cache, slower to build and larger, so the engine is
tantivy.

### Name search

Names are folded once, before the grams (`index::name_text` over
`extract::fold_lower`: precomposed or decomposed accents, Latin through
tantivy's `AsciiFoldingFilter`, ё to е, Greek tonos and final sigma, separators
as spaces and a space at each end). The post-filter reads the same text, so
recall and verification agree.

Hits rank in this order: the whole name without extension, the query as a word
of the name, as the start of a word, anywhere. Within a kind the shorter name
wins (field norms on `name`), and among equal scores the newest (`mtime_day`).
Recency is only a tie-break: as a stronger factor it moved 42 of 165 known-item
queries, as many down as up. One or two letters stand for a whole name or a
whole word ("go" finds GO.pdf, then "Contrato GO.pdf", never Google.pdf);
longer words use 3-grams only. On 165 known-item queries from a real home this
puts the wanted file first in 19 to 25 % of queries, against 15 to 19 %
without these rules, and finds every short-name file, 14 of 15 of which were
missed without them.

### Content queries

The last word is read as typed so far (`query::word_query`): it also matches
the indexed words it begins, most frequent first, within 32 words and 50,000
documents (`MAX_PREFIX_TERMS`, `PREFIX_DOC_BUDGET`), with the exact word
boosted. That serves as-you-type search and Portuguese and Russian suffixes
("relatorio" finds "relatorios", "квартал" finds "квартале"). A tighter budget
(16 words, 10,000 documents) changed no timing and dropped Portuguese words
from short prefixes in a code-heavy home. On the 5400 rpm disk the expansion
cost nothing measurable cold ("zz relatorio" 1013 ms against "relatorio zz"
1019 ms, both dominated by opening the indexes).

Every word also matches its plural or singular by suffix rules on folded words
(`query::inflections`: Portuguese and Spanish -ões, -ais, -eis, -ns, -res, -zes
and -s; English -es after x, ch and sh), ranked below the word as typed.
Snowball was measured on this index's own 5.5-million-term vocabulary first:
on folded words it joined none of 670 -ões, 421 -ais, 261 -eis and 194 -ns
pairs to their singulars, merged "casa" with "caso", "casado" and "case", and
in Russian did not join "квартал" to "квартале". The rules join every such pair
in both directions, at no cost in latency: "acoes" finds 1509 documents
against 898 without them, "responsaveis" 764 against 367, and "relatorios"
reaches the count ceiling against 363.

Russian and Ukrainian words match their other case endings
(`query::slavic_stem`): the word without its longest noun, adjective or
participle ending that leaves four letters, then the indexed words that are that
stem plus an ending ("кварталы" finds "квартале"). On hunspell lemma groups from
120 Russian catalogs this finds 94 % of the documents holding a form of the
typed word's lemma, against 50 % without, at 0.33 unrelated documents per query
instead of 0.15, mostly kin ("фотограф" finding "фотографии"). Stems that
change inside ("щелчок", "щелчком") stay apart.

An Arabic word still being typed keeps the article the index dropped, so the
last word also begins words without it ("اعادة الات" finds "إعادة الاتصال",
indexed as "اتصال"). That finds 91 % of Arabic as-you-type queries against 41 %;
the rest had one letter after the article.

A query reads only the candidates its page needs when the index answers every
filter, and counts its total from the postings up to a ceiling
(`query::count_capped`). Warm, a name query takes 3 to 5 ms and a combined one
5 to 18 ms, against 17 to 24 and 19 to 25 ms when every candidate is read.

### Folding and scripts

Every text is put in NFKC before anything else reads it (`extract::compose`,
used by `fold_lower` and by the content tokenizer, so names, pages, content and
queries agree): Korean syllables written as jamo, kana with a separate voicing
mark (macOS names, some PDFs), halfwidth katakana, Arabic presentation forms,
compatibility ideographs and fullwidth Latin become what people type. It has to
come before the tokenizer, because a jamo sequence is not a Hangul syllable and
would be indexed as one long word instead of bigrams. Text already in NFKC,
nearly all of it, is not copied. On a translation-catalog test in 13 languages
this finds 100 % of Korean NFD documents (0 % without), and of Japanese NFD and
halfwidth documents (79 % and 37 % without), with no other result changed.

Content words are folded at index time like names (`index::ScriptFold` over
`extract::fold_script_into`, then ASCII folding): combining marks, ё, Greek
accents and final sigma, Arabic hamza alef, alef maqsura and Persian yeh,
Persian kaf, ta marbuta as heh, vowel signs, tatweel and the article. The
tokenizer keeps combining marks inside a word, so a decomposed accent does not
split it. Folding at index time means a change to the fold re-indexes content:
the analyzer's name changes with what it emits (`index::BODY`, `index::NGRAM`),
so the on-disk schema differs and the index rebuilds; 88,055 documents took
about 13 minutes on the notebook, part of it under heavy load. A 42-query test
in 12 languages finds 41 with folding and the Chinese table below, against 23
without. German "oe" for "ö" stays unfolded: it would make every "oe" in
Portuguese or Dutch match "ö".

CJK, Thai, Lao, Khmer and Myanmar text is written without spaces between words,
so a word tokenizer would index a sentence as one token. The index holds every
character of such a run alone as well as its overlapping bigrams, in content
(`CjkFriendlyTokenizer { unigrams: true }`) and in names (`index::NameGrams`).
Queries of two characters or more ask for bigrams only (`index::BODY_QUERY`).
Indexing single characters finds every document holding a typed character; the
alternative, expanding one typed character to the indexed bigrams it begins
under a cap, found all of them for only 35 to 65 % of queries, and none for a
character that only ends runs. It makes the content index of the 13-language
test 3 % larger.

Traditional Chinese characters are folded to their simplified forms
(`simplified_chinese.rs`: 3079 pairs from ICU's `Traditional-Simplified`
transform taken one character at a time, under the Unicode License v3 notice
the file carries), so "会议记录" finds "會議記錄" and back, in names and content.
A character table needs no dictionary: zh_CN and zh_TW message pairs that differ
only in characters are all found across scripts, none without it. Pairs whose
words differ by region (文档/文件, 打印/列印, 音频/音訊) are found 24 % of the time
(9 % without); joining those needs a phrase list, which this does not attempt.
Japanese results are unaffected.

### Memory and build

Query readers keep 8 decompressed store blocks per segment instead of
tantivy's 100 (`index::query_reader`), since a query reads only the documents of
its page: 23.6 MB of anonymous memory after the same 60 queries, against 26.2 to
27.0 MB.

tantivy is built with only `mmap` and `lz4-compression`: the stemmer, stopword
lists and columnar zstd go unused, and leaving them out takes the release binary
from 9.1 to 8.1 MB. The doc store's compressor thread lives only while a segment
is written and is spawned by the indexing thread, whose idle policy and cgroup
it inherits.

BigLinux mounts btrfs with `compress-force=zstd`, and every index measurement in
this section ran on it. It stores the content index in 81 of its 120 MB (term
dictionary 75 to 56 MB, postings 43 to 23 MB). Uncompressed (a NOCOW folder,
since `compression=none` does not override `compress-force`), the same index
answered cold queries on the notebook 1 to 36 % slower: reading fewer bytes
beats decompressing whole 128 KB extents. tantivy's own lz4 covers only the doc
store, 1.6 MB of stored paths, so it stays as is.

## 4. Scan

`scan.rs` walks each root with the `ignore` crate's `WalkBuilder`, the walker
behind `fd` and `rg`: hidden entries are skipped, every gitignore source applies
(the repository's, the global one, `.git/info/exclude`, parent folders), and the
effective ignore file, the built-in defaults followed by the user's
`~/.config/big-search/ignore` so the user's lines win, takes precedence.
Symlinks are never followed during the walk; a symlinked folder at the top of a
root is resolved into a root of its own by `scan_roots::resolve_roots` according
to the symlink policy, and network mounts, virtual mounts and operating-system
trees are rejected as roots. The walk runs at background priority (section 7).

## 5. Watch

`watch.rs` holds one non-recursive inotify watch per visible folder, so the
hidden and gitignored churn never wakes the daemon.

- A new visible folder gets a watch and a scoped rescan, which closes the race
  between its creation and the watch.
- Events are coalesced over 250 ms (`DEBOUNCE`), then reconciled by existence:
  a present path is upserted, a missing one deleted.
- At most 16,384 events wait for the watcher (`MAX_QUEUED_EVENTS`), so a backup
  or a recursive copy cannot queue its whole tree in memory. Past that, or when
  the kernel's own queue overflows, the events are dropped and the watcher
  rescans every root.

## 6. Extraction

The content pass runs a small pool of extraction workers: 2, or 1 on a
low-memory or dual-core machine (`content::default_workers`;
`BIG_SEARCH_CONTENT_WORKERS` overrides it, up to 4). Workers extract
concurrently and a single writer applies the results through a bounded channel,
so workers block when the writer falls behind. A changed file is left alone for
`content_cooldown_secs` (900 s by default) before its text is read again, so a
document being edited is not re-read on every save. Files whose `(mtime, size)`
match the state store are never read again, and reflinked copies sharing the
same physical extents are read once (`content_dedupe.rs`).

| Format | Method |
|---|---|
| plain text, Markdown, source code, HTML | read in-process; a NUL in the first 8 KiB marks it binary |
| PDF | `pdftotext` subprocess |
| `docx odt ods odp pptx xlsx` | in-process `zip` and `quick-xml` |
| EPUB | container, package and spine chapters, read as HTML |
| `eml` | headers and the plain or HTML body: MIME parts, base64, quoted-printable, RFC 2047 (`extract_mail.rs`) |
| RTF | control words, `\'hh` and `\u` escapes, skipped destinations (`extract_rtf.rs`) |
| `doc` (OLE2) | `antiword` subprocess; an RTF file saved as `.doc` goes to the RTF reader |

Every indexed file is untrusted input. Subprocesses get argv arrays, never a
shell, a null stdin and a capped stdout, with `RLIMIT_AS`, a CPU-time limit and
a wall-clock deadline (`extract_pdf::run_capped`). The text kept per file is
capped by `extract_max_mb`; at 0, the default, the cap follows the memory the
service may use, from 4 MiB under 3 GiB to 32 MiB from 12 GiB. Office archives
are refused past `office_max_mb` (0 chooses 64 or 256 MiB by machine), 4096
entries or 512 slides, and each part's decompressed size is capped. Plain text
and PDF have optional size limits (`text_max_mb`, `pdf_max_mb`, 0 by default)
and PDF a deadline (`pdf_timeout_secs`, 5 s).

### PDF through `pdftotext`

`pdftotext -q -- <path> -` runs once per file. On a 1316-PDF corpus from a real
home, 4 in parallel:

| | `pdftotext` | `mutool convert -F text` | `pdf_oxide`, 4 threads |
|---|---|---|---|
| wall time | 16.0 s | 40.4 s | 43 s |
| files with text | 1263 | 1121 | 1263 |
| peak RSS | 158 MB | 690 MB | 1900 MB |
| memory isolation | the process exits | the process exits | accumulates |
| license | GPL, as a subprocess | AGPL, as a subprocess | MIT |

On a clean 200-file subset `mutool` and the in-process `pdf_oxide` look as fast
as or faster than `pdftotext`, and on single normal files `pdf_oxide` matches
it (30 files: 3.0 s against 2.83 s). The real corpus, with OCR scans, encrypted
files, exotic fonts and a 3000-page manual that alone takes 11 s, is where they
part: 200 normal PDFs in parallel took `pdf_oxide` 5.42 s and 226 MB against
4.02 s and 67 MB, and on pathological files it grows to seconds and gigabytes,
while a separate `pdftotext` process degrades on its own and frees everything
on exit. A process per file costs about 8 ms to spawn and gives memory isolation
without a recycled pool. Scans without a text layer, 4 % of the corpus, are
found by name only.

### Office documents, EPUB, mail and RTF in-process

These formats are zip and XML, MIME text or a small markup, so they are read
in-process with no subprocess: `word/document.xml` for docx, `content.xml` for
ODF, `xl/sharedStrings.xml` for xlsx, `ppt/slides/*.xml` for pptx. The
maintained office crates (`undoc`, `text_analysis`, `litchi`) read the same
entries; parsing them here needs fewer dependencies and keeps the zip-bomb caps
in one place. `undoc` lacks ODT, `text_analysis` bundles a PDF path already
covered, and `dotext` is unmaintained. LibreOffice, Tika or a JVM would cost 1
to 2 s of startup per file. An `unzip` and `sed` pipeline took 14 ms per file
against 1.5 ms in-process.

### Word 97-2003 through `antiword`

OLE2 is a compound binary format, not zip and XML. `antiword -m UTF-8.txt -w 0`
runs through the same runner as `pdftotext`. The file is told by its first
bytes: OLE2 goes to `antiword`, RTF (which Word also saves as `.doc`) to the RTF
reader. Against `catdoc`, on 19 Word files and 31 saved by LibreOffice, with
LibreOffice's text as reference: `antiword` recall 0.99 and 1.00, accented words
243 of 246 and 10,842 of 10,858; `catdoc` 0.88 and 0.00 (empty on every
LibreOffice file), 188 of 246, and it reads the text as Cyrillic without
`-s cp1252`. Both take 1 to 2 ms a file. `litchi`, a native OLE2 crate, is the
candidate should a crate replace the subprocess.

## 7. Throttle

`throttle::lower_priority` is called by the watcher, the content workers, the
history worker and a foreground reindex. It moves the calling thread into the
indexer's cgroup, which has `cpu.idle`, and gives it `SCHED_IDLE` with a
utilization clamp of 512 out of 1024, nice 19 as the fallback, and
`IOPRIO_CLASS_IDLE`; the programs it starts inherit all of it. The query
threads keep a normal program's weight. The goal is that indexing is never the
cause of a perceptible slowdown.

Measured on an i5-13400 (6 P and 4 E cores, SMT, `intel_cpufreq` with
`schedutil`), with a CPU loop in a program's place against the indexer:

| Indexer | Program sharing its core loses |
|---|---|
| nice 19, unit `CPUWeight=20` | 27 % |
| `SCHED_IDLE`, unit `CPUWeight=20` | 32 % |
| unit `CPUWeight=idle`, or the indexer's own `cpu.idle` group | about 0 to 1 % |
| on the core's SMT sibling, any policy | 42 to 50 % |

A thread's policy only counts inside its cgroup, so the unit's weight decides
against other programs. Making the whole unit idle costs the searches: with 16
saturated CPUs a 5000-hit query took 412 to 831 ms instead of 109 to 162 ms. So
the indexer gets a group of its own, in cgroup v2's threaded mode, which lets
the threads of one process sit in different groups. The unit delegates the cpu
controller (`Delegate=cpu`, `DelegateSubgroup=main`, and
`ProtectControlGroups=private` so the daemon writes only inside its own
namespace); the process and the query threads stay in `main`, and
`throttle::prepare_idle_cgroup` makes `main/index` threaded with `cpu.idle`, or
the lowest `cpu.weight` before Linux 5.15. With it, on 16 saturated CPUs, the
same queries take what they take on an idle machine (77 to 83 ms by name, 14 to
15 ms by content), and `pdftotext` starts in `main/index`. Started without
delegation, from a terminal, the daemon leaves cgroups alone and the thread
policies still apply.

On every stop systemd logs "Failed to kill control group …, ignoring: Operation
not supported": the kernel refuses `cgroup.kill` and process listing in a
threaded group. It is harmless: with a child process placed in `main/index`,
the unit still stopped and the child died.

No policy helps against the SMT sibling, since the two share one core. That is
what the backfill's pacing (`throttle::backfill_pace`) is for. It reads the
hardware (`throttle::topology`: physical cores and SMT over the CPUs the process
may use); the scheduler gives each busy program a core of its own while it can,
so the free cores are the cores minus the CPUs the others kept busy during the
batch. Measured on machines simulated with `taskset` on P-core pairs, a CPU loop
in a program's place and two workers flat out in an idle cgroup:

| Machine | Program loses |
|---|---|
| no SMT (2 and 4 cores), program on 1 thread or every core | about 0 % (±10 % noise) |
| SMT, free cores at least the workers | -1 to 8 % |
| SMT, one free core short | 23 to 28 % |
| SMT, no free core | 42 to 49 % at 100 % duty, 17 to 24 % at 50 %, 2 to 12 % at 20 % |

So without SMT the CPU never makes the backfill wait. With SMT it runs at a
20 % duty cycle when the free cores are fewer than its workers, and rests 30 s
when none is free. Memory pressure, its own and others' I/O pressure (most disk
schedulers ignore I/O priority) and battery slow it on any machine; the duty
cycle is 80 % on a calm machine, then 50 %, 20 % and a 30 s rest. Its own CPU
stall is not a signal: at idle priority it only says that others are busy.

The duty cycle follows the CPU time the service used in a batch, not the
batch's wall time: at idle priority on a busy machine a 250-file batch took
29 s of mostly waiting, and resting four times that would stretch a re-index of
88,000 documents over a night.

The clamp holds the clock below turbo where the frequency driver follows the
scheduler: a CPU loop ran at 4.1 GHz unclamped and 2.3 GHz at 512 (base 2.5)
on a P-core, 3.3 and 1.7 GHz (base 1.8) on an E-core. Drivers that choose
frequencies in hardware (`intel_pstate` or `amd-pstate` in active mode) ignore
it. Since the work runs slower and waits for idle CPU, extraction is bounded by
CPU time (`RLIMIT_CPU`: `pdf_timeout_secs` scaled by 1024/512), not wall-clock
time: a PDF waiting its turn on a busy machine must not be killed and recorded
as having no text. The wall-clock deadline, three times that, only catches hangs
such as a stalled network mount.

## 8. State

`catalogue.rs` keeps `namespace, path -> (mtime, size)` in SQLite, a `WITHOUT ROWID`
table keyed by `(namespace, path)`, with separate `name` and `content`
namespaces. It drives incremental dedup, delete detection and the reconcile on
start. Writes are batched in transactions; SQLite runs WAL with `NORMAL` sync.
Like the indexes, the state is derived from the filesystem, so a crash costs a
reconcile on the next start.

The settings are conservative for old machines: `cache_size=-2048` (2 MiB),
file-backed temporary storage (`SQLITE_TMPDIR` points at the data folder), and
a 64 MiB mmap window.

Peak memory does not grow with the catalogue:

- A reconcile (`scan::sync`, overflow and rebuild) records the paths it sees in
  a temporary `current_scan_seen` `WITHOUT ROWID` table as the walker emits
  them, flushing pending state changes every 2048 rows.
- Stale paths are selected with `NOT EXISTS` against that table in ordered
  2048-row pages, so deletion never holds the whole stale set.
- The roots of offline removable and network sources are marked seen before
  the stale pass, so an unplugged device keeps its catalogue.
- The content backfill streams the name catalogue (`paths_after`) and picks a
  batch bounded by count and by estimated bytes to read, so expensive PDFs and
  office files cannot take over the memory.
- `big-search count` without `--under` reads tantivy's document count.
- PDF page lookup uses the same capped `pdftotext` reader as indexing.

## 9. Queries

The daemon holds warm readers on both indexes and answers on a Unix socket at
`$XDG_RUNTIME_DIR/biglinux/indexd.sock`, mode `0600`, admitting only peers with
its own UID (`SO_PEERCRED`). The protocol is newline-framed JSON, versioned
(`"v"`), with requests tagged by `"cmd"` and replies by `"kind"`; a request line
over 64 KiB is refused. Other BigLinux programs speak it, so its description
lives in the `big-indexd-client` rustdoc. The `big-search` CLI uses that client,
and with no daemon running it searches the indexes in-process, read-only.

## 10. Dependencies

`apps/big-search/Cargo.toml` is the list: tantivy, `rusqlite` for the state,
origin and history stores, `notify` and `ignore`, `symphonia` for audio and
video durations, and the extractors' parsers. No tokio, no HTTP client, no
D-Bus. `deny.toml` admits crates.io only, and `Cargo.lock` is committed.

## 11. Security model

- Unprivileged user service: no root, no capabilities, no setuid. This is why
  inotify over the visible folders beats fanotify, which needs
  `CAP_SYS_ADMIN`.
- Every path, event and file is untrusted. The walk never follows symlinks;
  configured roots are resolved and checked before anything is indexed.
- Extraction subprocesses get argv arrays, never `sh -c`; a failure or a
  timeout skips the file and is logged, and never stops the daemon.
- The socket lives in `$XDG_RUNTIME_DIR`, mode `0600`, `AF_UNIX` only, same UID.
- No network, and logs carry paths and counts, never file contents.
- The systemd user unit (`packaging/big-search.service`) runs with
  `ProtectSystem=strict` and `ProtectHome=read-only`; `ReadWritePaths=` holds
  only the data folder (`~/.local/share/big-search`) and the socket folder
  (`%t/biglinux`), both created by a pre-start step outside the sandbox. A
  parser compromised by an indexed file must not be able to write user units,
  `.desktop` files or D-Bus service files that run outside it. The unit also
  sets `ProtectControlGroups=private` (the daemon writes only its own delegated
  subtree), `NoNewPrivileges=yes`, `RestrictAddressFamilies=AF_UNIX`,
  `SystemCallFilter=@system-service` minus `@privileged @mount @reboot @swap
  @obsolete`, `MemoryDenyWriteExecute=yes`, `RestrictNamespaces=yes`,
  `LockPersonality=yes` and `PrivateTmp=yes`.
- The daemon never writes `config.toml` or the user ignore file; the CLI seeds
  them.
