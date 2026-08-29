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
bytes. On the GPU path, baseline 4:2:0 JPEGs decode on the NVDEC engine
(~1.4× per-image on large photos; a wash on the batched corpus — see the
GPU section below).

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

## GPU acceleration (`--acceleration cuda`, opt-in `cuda` feature)

The same 100-PDF corpus run through `presse press -a cuda -q 50` (nvJPEG
backend on an RTX 4080 SUPER / CUDA 13.3; streams < 128 KiB stay on CPU per
the PCIe-latency guard):

- **Correctness: 100/100 docs compressed with zero failures.** qpdf gate
  matches the CPU path (94 fully clean + 3 benign "input stream is complete"
  warnings + the pre-existing `issue7229` source artifact), and the visual
  sweep is **97/100 clean** — the only flag is that same pre-existing
  malformed-source artifact; two known corpus files are skipped at load.
- **Grayscale guard:** nvJPEG has no single-channel input format, so
  1-component JPEGs (and `/DeviceGray` streams) are routed to the CPU encoder.
  Without this guard, grayscale JPEGs were re-encoded as RGB inside
  `/DeviceGray` streams and rendered as garbage (found via the visual sweep on
  `pdfjs_22060_A1_01_Plans`; fixed in `src/transcode/`).
- **Size:** nvJPEG's optimized Huffman tables shrink image-heavy docs further
  than the CPU encoder at the same quality — measured 9–22% smaller on
  duplicate/image-heavy arXiv papers (gpt3 4.3 vs 5.6 MB, styleGAN 5.4 vs
  6.7 MB).
- **Wall time:** slower than the CPU path on this corpus (0.62× overall on
  the 25 image-heavy docs). The current backend serializes every stream
  behind a mutex with host-memory round-trips and pays a per-process CUDA
  context init (~0.15 s), so it is a correctness/size feature today; the
  CUDA-stream + pinned-memory batching needed to beat the CPU engine is
  future work.

## Large image-heavy PDFs (generated photo corpus)

`benches/docker/bench_gpu.sh` fetches real photos (deterministic Lorem Picsum
seeds, 6 MP and 17.5 MP) and assembles four PDFs (12–36 MB, 10–60 images),
then times cpu-parallel / cpu-serial / cuda / ghostscript (best of 3).
RTX 4080 SUPER, 16-core CPU, `-C target-cpu=native`, `-q 50`:

| PDF | cpu-par 16c | cpu-serial | cuda (pool) | gs /ebook |
|---|---|---|---|---|
| photos20 (20×6 MP) | 0.173 s | 1.201 s | 0.447 s | 0.248 s |
| photos60 (60 imgs) | 0.323 s | 1.330 s | 0.716 s | 0.660 s |
| photos10big (10×17.5 MP) | 0.273 s | 0.736 s | 0.556 s | 0.330 s |

- The GPU handle pool (per-worker nvjpeg handles instead of one mutex) cut
  the multi-image case 1.054 → 0.716 s (1.47×) and beats 2-core CPU
  parallel (photos20 0.440 vs 0.626 s); 16-core CPU parallel still wins
  overall on wall time.
- **NVDEC hardware decode stage (this PR):** baseline 4:2:0 JPEGs decode on
  the NVDEC engine (Video Codec SDK, `libnvcuvid`). Per-image decode is
  ~1.4× faster than the nvJPEG entropy-decode path on large high-quality
  photos (2400×1800 q95: 15.7 vs 22.1 ms including the NV12→planar
  conversion) and a wash on small/low-quality images — but the engine
  serializes at ~5 MB/s per stream (ffmpeg's `mjpeg_cuvid` shows the same
  ~24 ms/frame on this driver), so on the 16-way batched photo corpus the
  stage is a wash vs the nvJPEG batch (photos20: 0.255–0.28 s with NVDEC vs
  0.243–0.25 s without). The stage is optional and self-degrading
  (`PRESSE_NO_NVDEC=1` forces the nvJPEG decode); it is verified
  pixel-equivalent (≤1 IDCT-rounding delta on <0.01% of pixels) by
  `examples/nvdec_verify.rs`.
- GPU output is visually identical (SSIM 1.0000 on all 20 photo pages,
  qpdf clean) and 16–25 % smaller than the CPU encoder at the same quality
  (nvJPEG optimized Huffman).
- Crossover: the CUDA pool ≈ or > CPU parallel at ≤ 2 cores; CPU wins from
  4 cores up. GPU is the right default when cores are scarce or size
  matters more than wall time.
