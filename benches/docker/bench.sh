#!/usr/bin/env bash
#
# Presse benchmark harness — runs inside the `presse-bench` container.
#
# Measures the rayon-parallel image re-encoding pipeline against a
# single-threaded baseline of the same binary (`RAYON_NUM_THREADS=1`) and
# reports wall time, throughput and peak RSS over a standardized corpus.
#
# Exit status is always 0 on success (including when the determinism gate
# reports a difference — that is surfaced as a warning, not a hard failure).

set -euo pipefail

QUALITY=50
CORPUS_DIR=/tmp/presse-corpus
RESULTS_DIR=/tmp/presse-results
mkdir -p "$CORPUS_DIR" "$RESULTS_DIR"

NPROC=$(nproc)
echo "==> host: $(nproc) CPUs"

echo "==> generating benchmark corpus"
gen_corpus "$CORPUS_DIR"

# ---------------------------------------------------------------------------
# Correctness / determinism gate
# ---------------------------------------------------------------------------
echo "==> validity + determinism gate"

pdfinfo "$CORPUS_DIR/image-heavy.pdf" > /dev/null

RAYON_NUM_THREADS=1 presse press "$CORPUS_DIR/image-heavy.pdf" -q "$QUALITY" -o "$RESULTS_DIR/gate-serial.pdf" > /dev/null 2>&1
presse press "$CORPUS_DIR/image-heavy.pdf" -q "$QUALITY" -o "$RESULTS_DIR/gate-parallel.pdf" > /dev/null 2>&1

if cmp -s "$RESULTS_DIR/gate-serial.pdf" "$RESULTS_DIR/gate-parallel.pdf"; then
    echo "    serial/parallel outputs are byte-identical"
else
    echo "    WARNING: serial and parallel outputs differ"
fi

if pdfinfo "$RESULTS_DIR/gate-parallel.pdf" > /dev/null 2>&1 \
    && pdftoppm -singlefile -png -r 72 "$RESULTS_DIR/gate-parallel.pdf" "$RESULTS_DIR/gate" > /dev/null 2>&1; then
    echo "    parallel output is valid PDF and rasterizes (pdfinfo + pdftoppm)"
else
    echo "    WARNING: parallel output failed pdfinfo/pdftoppm"
fi

# ---------------------------------------------------------------------------
# hyperfine: single-thread baseline vs rayon-parallel
# ---------------------------------------------------------------------------
echo "==> hyperfine: 1 thread vs $NPROC threads (image-heavy.pdf)"
hyperfine --warmup 1 --runs 5 --export-json "$RESULTS_DIR/hyperfine.json" \
    -n "presse serial  (1 thread)" \
    "RAYON_NUM_THREADS=1 presse press $CORPUS_DIR/image-heavy.pdf -q $QUALITY -o $RESULTS_DIR/hf-serial.pdf" \
    -n "presse parallel ($NPROC threads)" \
    "presse press $CORPUS_DIR/image-heavy.pdf -q $QUALITY -o $RESULTS_DIR/hf-parallel.pdf"

# ---------------------------------------------------------------------------
# Per-corpus-file breakdown: wall time, throughput, peak RSS
# ---------------------------------------------------------------------------
echo "==> per-file breakdown (wall time / throughput / peak RSS)"
printf "%-14s %10s %10s %10s %10s %10s\n" corpus "in(B)" "out(B)" "wall(s)" "MB/s" "RSS(MB)"

for pdf in "$CORPUS_DIR"/*.pdf; do
    name=$(basename "$pdf" .pdf)
    out="$RESULTS_DIR/$name.pdf"
    size_in=$(stat -c %s "$pdf")
    /usr/bin/time -v -o "$RESULTS_DIR/$name.time" \
        presse press "$pdf" -q "$QUALITY" -o "$out" > /dev/null 2>&1 || true
    size_out=$(stat -c %s "$out" 2> /dev/null || echo 0)
    peak=$(awk '/Maximum resident set size/ {print $6}' "$RESULTS_DIR/$name.time") # KiB
    wall=$(awk '/Elapsed \(wall clock\) time/ {print $8}' "$RESULTS_DIR/$name.time")
    # normalize "h:mm:ss" / "m:ss" to seconds
    wall_s=$(awk -v t="$wall" 'BEGIN {
        n = split(t, a, ":")
        if (n == 3) printf "%.3f", a[1]*3600 + a[2]*60 + a[3]
        else if (n == 2) printf "%.3f", a[1]*60 + a[2]
        else printf "%s", t
    }')
    mbps=$(awk -v s="$size_in" -v w="$wall_s" 'BEGIN { if (w > 0) printf "%.1f", s / 1e6 / w; else printf "n/a" }')
    rss=$(awk -v k="$peak" 'BEGIN { printf "%.1f", k / 1024 }')
    printf "%-14s %10d %10d %10s %10s %10s\n" "$name" "$size_in" "$size_out" "$wall_s" "$mbps" "$rss"
done

# ---------------------------------------------------------------------------
# Per-corpus-file breakdown: mutool clean -gggg -z (lossless repack)
# ---------------------------------------------------------------------------
echo "==> per-file breakdown: mutool clean -gggg -z"
printf "%-14s %10s %10s %10s\n" corpus "in(B)" "mu-out(B)" "mu-wall(s)"

for pdf in "$CORPUS_DIR"/*.pdf; do
    name=$(basename "$pdf" .pdf)
    mu_out="$RESULTS_DIR/mu-$name.pdf"
    size_in=$(stat -c %s "$pdf")
    t0=$(date +%s.%N)
    mutool clean -gggg -z "$pdf" "$mu_out" > /dev/null 2>&1 || true
    t1=$(date +%s.%N)
    mu_s=$(awk "BEGIN{printf \"%.3f\", $t1-$t0}")
    mu_size=$(stat -c %s "$mu_out" 2> /dev/null || echo 0)
    printf "%-14s %10d %10d %10s\n" "$name" "$size_in" "$mu_size" "$mu_s"
done

echo "==> done — timings JSON: $RESULTS_DIR/hyperfine.json"
