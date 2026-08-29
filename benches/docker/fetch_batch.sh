#!/usr/bin/env bash
# Batch-download PDFs, verifying %PDF magic. Prints progress per URL.
set -u
OUT="$1"; shift
mkdir -p "$OUT"
ok=0; fail=0
for entry in "$@"; do
    name="${entry%%|*}"
    url="${entry#*|}"
    fname="$OUT/$name.pdf"
    curl -sL --max-time 60 -A "Mozilla/5.0 (X11; Linux x86_64)" -o "$fname" "$url"
    magic=$(head -c 5 "$fname" 2>/dev/null)
    size=$(stat -c %s "$fname" 2>/dev/null || echo 0)
    if [ "$magic" = "%PDF-" ] && [ "$size" -gt 3000 ]; then
        ok=$((ok+1))
        printf "  [%3d ok] %-28s %8s\n" "$ok" "$name" "$(numfmt --to=iec "$size")"
    else
        fail=$((fail+1))
        echo "  [skip] $name (magic='$magic')"
        rm -f "$fname"
    fi
done
echo "== $ok valid, $fail failed -> $OUT"
