# 100-PDF benchmark results

Measured on a 16-core machine with the release build
(`RUSTFLAGS="-C target-cpu=native"`), `presse press -q 50`.

For how output quality (SSIM, resolution preservation) relates to these
speeds — including ghostscript, the lossless-repack `mutool clean`, and the
non-compressing Rust tools unpdf and pdfrs — see [`QUALITY.md`](QUALITY.md).

## Performance work

The engine uses: rayon for concurrent image re-encoding, `mimalloc` as the
global allocator, the `zlib-rs` deflate backend for flate2 (pure Rust),
memory-mapped input reads, and a re-encode cache that deduplicates identical
image streams — keyed by quality, input kind, dimensions, and exact pixel
bytes, with one JPEG encode serving every duplicate (including duplicates
found simultaneously by parallel workers).

Build note: the figures below describe a glibc build with `mimalloc` and
`-C target-cpu=native`. The x86_64 Linux release artifact is the musl build;
it now ships `mimalloc` too (it builds against musl given `musl-gcc`), so the
numbers apply to both — a native-code musl build measured within ~10% of the
glibc build on the image-heavy corpus.

## Corpus

100 public real-world PDFs (≈300 MB, 5,268 pages):

| category | count | source |
|---|---|---|
| research papers | 32 | arXiv (image- and text-heavy, 200 KB–36 MB) |
| government forms | 29 | IRS.gov forms + instructions |
| specs / reports | 6 | PDF 1.7 & 1.4 references, Unicode 15.0, PyMuPDF guide, Census ACS, Adobe sample |
| real-world tests | 33 | mozilla/pdf.js test corpus (books, plans, forms, XFA, scans, stress cases) |

Reproduce the corpus with `benches/docker/fetch_batch.sh` and a sparse clone of
`https://github.com/mozilla/pdf.js` (`git sparse-checkout set test/pdfs`).

## Methodology

- One run per PDF per tool: `presse press -q 50` (parallel), the same with
  `RAYON_NUM_THREADS=1` (serial baseline), and
  `gs -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook`.
- Sizes measured on the produced files; reduction = `(1 − out/in) × 100`.
- Raw timings: `benches/docker/bench3.py` (resumable CSV).

## Wall time

| tool | mean | median | total (100 PDFs) |
|---|---|---|---|
| presse parallel (rayon) | 0.81 s | 16 ms | 81 s |
| presse serial (1 thread) | 1.23 s | 34 ms | 123 s |
| ghostscript /ebook | 3.22 s | 144 ms | 319 s |

Parallel vs serial: **mean 2.2×, median 1.5×** — ghostscript: **mean 98×,
median 6.9×** (total wall time ~4× faster than gs).

The allocator / deflate-backend / mmap / dedup changes cut presse wall time
by **24% parallel / 19% serial** vs the previous build (mean 1.06 → 0.81 s
parallel; total 106 → 81 s). Largest single-doc wins are flate-heavy
workloads (IRS 1040 instructions 1.5×, the 1310-page PDF 1.4 reference
1.4×, Unicode 15.0 1.4× — zlib-rs) and duplicate-heavy papers (cycleGAN
1.1×, 41% duplicate images in GPT-3 — dedup).

Follow-up writer/decode work (this PR): the save path's object renumbering
is now a linear hash-map pass instead of lopdf's O(n²) `Vec::contains`
walk — the 67k-object document's save drops **530 → 62 ms** — and the dedup
cache's key hash is bounded to length + first/last 4 KiB so a unique
multi-MB scan costs a few KiB of hashing, with equality still on exact
bytes.

Speedup scales with image count (the parallelized phase):

| images in doc | docs | mean speedup |
|---|---|---|
| 0 | 43 | 1.5× (noise) |
| 1–2 | 17 | 1.4× |
| 3–9 | 7 | 3.2× |
| 10+ | 33 | 4.0× |

Largest single-doc win: 7.8× (a 36 MB image-heavy arXiv paper). Documents
whose runtime is dominated by object-tree repacking (e.g. the 127k-object PDF
spec) show ~1× — the image phase is a small fraction of their time.

## Size reduction

| tool | mean | median |
|---|---|---|
| presse (parallel = serial, deterministic) | 19.5 % | 9.1 % |
| ghostscript /ebook | 60.5 % | 64.8 % |

presse only re-encodes image streams (JPEG q50) and repacks the object tree;
ghostscript `/ebook` additionally downsamples images and re-compresses all
streams, so it shrinks files more on this corpus. presse's advantage is speed,
not size.

## Correctness gates on all 100 outputs

- **qpdf `--check`**: 96/100 fully clean. The 4 exceptions are source-PDF
  quirks passed through (3× "input stream is complete but output may still be
  valid"; 1× page-tree loop in a deliberately malformed pdf.js regression
  file that poppler already flags on the *original*).
- **Visual sweep** (`benches/docker/visual_sweep.py`, pdftoppm render + SSIM
  + luminance-ratio heuristics, all pages capped at 40 for long docs):
  **98/100 at SSIM ≥ 0.99** (97 at exactly 1.000). The two exceptions were
  both reproduced with a bare `lopdf::Document::load` + `save` (zero presse
  code) — pre-existing lopdf parse artifacts on pathological sources, not
  regressions from the compression pipeline.

One corpus candidate (a Brotli-compressed file) cannot be loaded by lopdf at
all (`BrotliDecode` unsupported) and is skipped with a warning.
