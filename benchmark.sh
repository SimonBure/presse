#!/bin/bash
set -euo pipefail

PDF_DIR="${1:?Usage: benchmark.sh <pdf-directory>}"
RESULTS_DIR="results"
SIZES_CSV="$RESULTS_DIR/sizes.csv"
QUALITY=50

# Check required tools
for tool in hyperfine gs presse; do
    command -v "$tool" &>/dev/null || { echo "Error: '$tool' not found in PATH"; exit 1; }
done

mkdir -p "$RESULTS_DIR"
echo "filename,original_bytes,gs_bytes,presse_bytes" > "$SIZES_CSV"

for pdf in "$PDF_DIR"/*.pdf; do
    [ -f "$pdf" ] || { echo "No PDF files found in $PDF_DIR"; exit 1; }

    name=$(basename "$pdf" .pdf)
    echo "--- $name ---"

    out_gs=$(mktemp /tmp/bench_gs_XXXXXX.pdf)
    out_presse=$(mktemp /tmp/bench_presse_XXXXXX.pdf)

    hyperfine --warmup 1 --ignore-failure \
        --export-json "$RESULTS_DIR/${name}.json" \
        -n "ghostscript" "gs -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook -dNOPAUSE -dQUIET -dBATCH -sOutputFile=\"$out_gs\" \"$pdf\"" \
        -n "presse"      "presse --quality $QUALITY -o \"$out_presse\" \"$pdf\" > /dev/null 2>&1" \
        || true

    orig=$(wc -c < "$pdf")
    gs_size=$([ -f "$out_gs" ]     && wc -c < "$out_gs"     || echo "N/A")
    presse_size=$([ -f "$out_presse" ] && wc -c < "$out_presse" || echo "N/A")
    echo "$name,$orig,$gs_size,$presse_size" >> "$SIZES_CSV"

    rm -f "$out_gs" "$out_presse"
done

echo ""
echo "Done. Timing results: $RESULTS_DIR/*.json"
echo "Size comparison:      $SIZES_CSV"
