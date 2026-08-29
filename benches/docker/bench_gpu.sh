#!/usr/bin/env bash
# GPU-vs-CPU benchmark on large image-heavy PDFs (real photos, deterministic
# seeds). Builds the corpus, then times cpu-parallel / cpu-serial / cuda /
# ghostscript (best of 3) and prints a CSV.
#
# Requires: curl, ImageMagick (magick), ghostscript, a CUDA build of presse
#   (cargo build --release --features cuda), and an NVIDIA GPU + driver.
# Usage: bench_gpu.sh [out-dir]   (default: /tmp/gpubench)
set -u
OUT="${1:-/tmp/gpubench}"
PRESSE="${PRESSE:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/presse}"
mkdir -p "$OUT/img"
cd "$OUT/img"

echo "== fetching photos (Lorem Picsum, deterministic seeds) =="
i=1
while [ "$i" -le 20 ]; do
  [ -f "p6m_$i.jpg" ] || curl -sL -o "p6m_$i.jpg" "https://picsum.photos/seed/gpubench$i/3000/2000"
  i=$((i+1))
done
i=1
while [ "$i" -le 4 ]; do
  [ -f "p17m_$i.jpg" ] || curl -sL -o "p17m_$i.jpg" "https://picsum.photos/seed/gpubenchbig$i/5000/3500"
  i=$((i+1))
done

echo "== building PDFs =="
magick p6m_*.jpg ../photos20.pdf
magick p6m_1.jpg p6m_1.jpg p6m_1.jpg p6m_2.jpg p6m_2.jpg p6m_2.jpg p6m_3.jpg p6m_3.jpg p6m_3.jpg \
  p6m_4.jpg p6m_4.jpg p6m_4.jpg p6m_5.jpg p6m_5.jpg p6m_5.jpg p6m_6.jpg p6m_6.jpg p6m_6.jpg \
  p6m_7.jpg p6m_7.jpg p6m_7.jpg p6m_8.jpg p6m_8.jpg p6m_8.jpg p6m_9.jpg p6m_9.jpg p6m_9.jpg \
  p6m_10.jpg p6m_10.jpg p6m_10.jpg p6m_11.jpg p6m_11.jpg p6m_11.jpg p6m_12.jpg p6m_12.jpg p6m_12.jpg \
  p6m_13.jpg p6m_13.jpg p6m_13.jpg p6m_14.jpg p6m_14.jpg p6m_14.jpg p6m_15.jpg p6m_15.jpg p6m_15.jpg \
  p6m_16.jpg p6m_16.jpg p6m_16.jpg p6m_17.jpg p6m_17.jpg p6m_17.jpg p6m_18.jpg p6m_18.jpg p6m_18.jpg \
  p6m_19.jpg p6m_19.jpg p6m_19.jpg p6m_20.jpg p6m_20.jpg p6m_20.jpg ../photos60.pdf
magick p17m_*.jpg p17m_*.jpg p17m_1.jpg p17m_2.jpg ../photos10big.pdf
magick $(for i in $(seq 1 50); do printf "p6m_1.jpg "; done) ../dup50.pdf

benchdir="$OUT/bench"
mkdir -p "$benchdir"
echo "pdf,in_MB,cpu_par_s,cpu_ser_s,cuda_pool_s,gs_s,cpu_KB,cuda_KB,gs_KB" > "$benchdir/results.csv"

best_of_3() { # cmd... -> best seconds
  best=""
  for _ in 1 2 3; do
    t0=$(date +%s.%N)
    "$@" >/dev/null 2>&1
    t1=$(date +%s.%N)
    t=$(awk "BEGIN{printf \"%.3f\", $t1-$t0}")
    if [ -z "$best" ] || [ "$(awk "BEGIN{print ($t<$best)?1:0}")" = "1" ]; then best=$t; fi
  done
  printf "%s" "$best"
}

for f in "$OUT"/*.pdf; do
  b=$(basename "$f")
  in_mb=$(awk "BEGIN{printf \"%.1f\", $(stat -c %s "$f")/1e6}")
  cpu_par=$(best_of_3 env RAYON_NUM_THREADS=16 "$PRESSE" press -a cpu -q 50 "$f" -o "$benchdir/cpu-$b")
  cpu_ser=$(best_of_3 env RAYON_NUM_THREADS=1 "$PRESSE" press -a cpu -q 50 "$f" -o "$benchdir/cpus-$b")
  cuda=$(best_of_3 "$PRESSE" press -a cuda -q 50 "$f" -o "$benchdir/cuda-$b")
  gs_s=$(best_of_3 gs -q -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook -dNOPAUSE -dBATCH -sOutputFile="$benchdir/gs-$b" "$f")
  cpu_kb=$(( $(stat -c %s "$benchdir/cpu-$b") / 1024 ))
  cuda_kb=$(( $(stat -c %s "$benchdir/cuda-$b") / 1024 ))
  gs_kb=$(( $(stat -c %s "$benchdir/gs-$b") / 1024 ))
  echo "$b,$in_mb,$cpu_par,$cpu_ser,$cuda,$gs_s,$cpu_kb,$cuda_kb,$gs_kb" | tee -a "$benchdir/results.csv"
done
echo "== done: $benchdir/results.csv =="
