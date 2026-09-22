#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Run the metadata-extraction benchmark: native system tools vs Rust crates vs
# baseline (stat only), across type detection / pdf / audio / image / video.
# Reports response time (per-file), indexing time (whole corpus), CPU and peak
# RSS. Cache is warmed first so the signal is parse/spawn cost, not disk seek
# (the file read is common to every approach).
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/rust-bench/target/release/metadata-bench"
CORPUS="$DIR/corpus"
RUNS="${RUNS:-3}"
[[ -x "$BIN" ]] || {
	echo "build first: (cd $DIR/rust-bench && cargo build --release)" >&2
	exit 2
}

# label | corpus-list | harness mode + args ({} = path placeholder for native)
BENCHES=(
	"type   | native  file        | mixed | native -- file -b --mime-type {}"
	"type   | rust    file-format | mixed | rust-type-ff"
	"type   | baseline stat       | mixed | baseline"
	"pdf    | native  pdfinfo     | pdf   | native -- pdfinfo {}"
	"pdf    | baseline stat       | pdf   | baseline"
	"audio  | native  ffprobe     | audio | native -- ffprobe -v quiet -show_format -show_streams {}"
	"audio  | native  exiftool    | audio | native -- exiftool {}"
	"audio  | rust    lofty       | audio | rust-audio"
	"audio  | baseline stat       | audio | baseline"
	"image  | native  identify    | image | native -- identify {}"
	"image  | native  exiftool    | image | native -- exiftool {}"
	"image  | rust    imagesize   | image | rust-image"
	"image  | rust    kamadak-exif| image | rust-exif"
	"image  | baseline stat       | image | baseline"
	"video  | native  ffprobe     | video | native -- ffprobe -v quiet -show_format -show_streams {}"
	"video  | rust    lofty       | video | rust-audio"
	"video  | baseline stat       | video | baseline"
)

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$0} END{print a[int((NR+1)/2)]}'; }

echo ">> warming page cache for the corpus ..."
for l in mixed pdf audio image video; do
	while IFS= read -r f; do [[ -r "$f" ]] && cat -- "$f" >/dev/null 2>&1 || true; done <"$CORPUS/$l.list"
done

printf '%-7s %-20s %6s %5s %5s %12s %11s %8s %9s\n' \
	GROUP APPROACH FILES OK ERR PERFILE_ms TOTAL_ms CPU_ms RSS_KB
printf '%.0s-' {1..92}
echo

for spec in "${BENCHES[@]}"; do
	group="$(echo "${spec%%|*}" | xargs)"
	rest="${spec#*|}"
	approach="$(echo "${rest%%|*}" | xargs)"
	rest="${rest#*|}"
	corpus="$(echo "${rest%%|*}" | xargs)"
	harness="${rest#*|}"
	harness="${harness#"${harness%%[![:space:]]*}"}" # trim leading spaces
	list="$CORPUS/$corpus.list"

	mode="${harness%% *}"      # first word
	margs="${harness#"$mode"}" # remainder (may be empty / leading space)
	walls=()
	line=""
	for _ in $(seq 1 "$RUNS"); do
		# shellcheck disable=SC2086
		line="$("$BIN" "$mode" "$list" $margs)"
		w="$(echo "$line" | sed -n 's/.*WALL_PER_FILE_MS=\([0-9.]*\).*/\1/p')"
		walls+=("$w")
	done
	pf="$(median "${walls[@]}")"
	files="$(echo "$line" | sed -n 's/.*FILES=\([0-9]*\).*/\1/p')"
	ok="$(echo "$line" | sed -n 's/.*OK=\([0-9]*\).*/\1/p')"
	err="$(echo "$line" | sed -n 's/.*ERR=\([0-9]*\).*/\1/p')"
	cpu="$(echo "$line" | sed -n 's/.*CPU_MS=\([0-9.]*\).*/\1/p')"
	rss="$(echo "$line" | sed -n 's/.*PEAK_RSS_KB=\([0-9-]*\).*/\1/p')"
	total="$(awk -v p="$pf" -v n="$files" 'BEGIN{printf "%.1f", p*n}')"
	printf '%-7s %-20s %6s %5s %5s %12s %11s %8s %9s\n' \
		"$group" "$approach" "$files" "$ok" "$err" "$pf" "$total" "$cpu" "$rss"
done
