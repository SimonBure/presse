# Benchmark


Measured over 100 public real-world PDFs (≈300 MB, 5,268 pages), comparing
`presse press --quality 50` against Ghostscript `/ebook`. Full methodology,
per-document data, and correctness gates in
[`benches/docker/RESULTS.md`](../benches/docker/RESULTS.md).

| | presse (rayon) | presse (serial) | ghostscript `/ebook` |
|---|---|---|---|
| Total wall time (100 PDFs) | **81 s** | 123 s | 319 s |
| Mean per document | **0.81 s** | 1.23 s | 3.22 s |
| Mean size reduction | 19.5 % | 19.5 % | 60.5 % |

- **Parallel vs single-threaded**: 2.2× mean, 1.5× median. Best case ~6× on
  an image-heavy PDF (24 images, 16 cores) — the headline speedup of the
  rayon pipeline.
- **Parallel vs ghostscript**: ~7× median per document, ~4× total wall time.
- **Size**: presse reduces output without touching image resolution;
  ghostscript's `/ebook` downsamples images, so it shrinks files more but
  changes their resolution. Both figures are means over the corpus.

A reproducible containerized benchmark harness lives in [`benches/docker/`](../benches/docker/):

```bash
docker build -t presse-bench -f benches/docker/Dockerfile.bench .
docker run --rm presse-bench
```

The container builds with `RUSTFLAGS="-C target-cpu=native"` (AVX2/FMA auto-vectorization) and reports wall time, throughput, and peak RSS for a generated corpus (text-heavy, image-heavy, scanned), comparing the rayon pipeline against a `RAYON_NUM_THREADS=1` baseline.
