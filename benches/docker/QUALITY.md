# Quality benchmark

How output quality relates to speed for the tools measured in `RESULTS.md` —
presse (parallel), ghostscript `/ebook`, and the two Rust tools that do
**not** compress PDFs (unpdf, pdfrs/pdfcli), included so their speed can be
judged against what they actually produce.

## TL;DR

- **presse** is the only tool whose speed reflects real, full-resolution
  compression: 16–33% size reduction at SSIM ≥ 0.998, and ~7× faster than
  ghostscript on the same corpus.
- **ghostscript**'s size wins are resolution loss, not compression:
  `/ebook` downsamples images placed above 150 dpi (6483 → 1902 ppi on an
  IRS scan) and is a *no-op* on images placed at ≤ 150 dpi (photo PDFs pass
  through unchanged).
- **unpdf and pdfrs do not compress.** unpdf extracts text (0.859
  precision / 0.921 recall vs poppler) and skips images entirely (0 images
  extracted from a 20-photo PDF). pdfrs's speed is the speed of doing
  almost nothing: its `merge` drops images (28 KB blank-page output from
  48 MB of inputs) and `pdf-to-md` panics or emits a 0-byte file on 93/100
  corpus files.
- **MuPDF (`mutool clean`)** does not re-encode either: it is a *lossless
  repack* — images pass through byte-identical (verified 20/20 on the photo
  corpus) and renders are SSIM 1.0, but its size effect is mostly a wash:
  it shrank 62 of the 99 corpus files that produced output (median −6.3% on
  the shrinkers) and *grew* the other 37, with image-heavy PDFs barely
  moving (photos20 12.08 → 12.07 MB). Per-file it is fast (0.015 s median),
  but the commonly-recommended `-gggg` duplicate-scan shows severe
  superlinear scaling in this corpus: 200+ s on the two large spec
  documents, where the plain `-z` repack takes 0.1–0.6 s.

## Methodology

- Hardware/software: RTX 4080 SUPER, 16-core CPU, CUDA 13.3, Ghostscript
  10.07.1, unpdf 0.12.0, pdfrs 0.1.9 (binary `pdfcli`), poppler + qpdf for
  validation. presse built with `RUSTFLAGS="-C target-cpu=native"`.
- Commands:
  - presse: `presse press -q <30|50> -a cpu <in> -o <out>`
  - ghostscript: `gs -q -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook -dNOPAUSE
    -dBATCH -sOutputFile=<out> <in>`
  - unpdf: `unpdf text <in>` (extraction; no PDF output)
  - pdfrs: `pdfcli pdf-to-md <in> <out>`, `pdfcli merge -o <out> <in>...`
  - MuPDF: `mutool clean -gggg -z <in> <out>` (1.28.0)
- Quality metric: whole-image SSIM of 60 dpi `pdftoppm` renders (output vs
  source), plus the maximum image resolution (`pdfimages -list`, x-ppi) as
  the resolution-preservation check.
- Times: quality tables are best-of-1 per file; the speed table is the
  per-file median over the 100-file corpus from `RESULTS.md` (excluding the
  60 s `specs_pdfref17old` outlier that is a lopdf single-threaded parse
  bottleneck — parallel == serial there).

## Quality × size × time on image-bearing files

| file (source size) | presse q50 → MB / SSIM | presse q30 → MB / SSIM | gs /ebook → MB / SSIM | mutool → MB / SSIM | gs max image ppi |
|---|---|---|---|---|---|
| arxiv_gpt3_2005 (6.77 MB) | 5.69 / 0.9999 | 5.03 / 0.9999 | 2.04 / 0.9944 | 6.60 / 1.0000 | 1234 → 1030 |
| arxiv_transformer_xl1901 (4.57 MB) | 4.41 / 1.0000 | 4.40 / 1.0000 | 1.04 / 0.9931 | 4.26 / 1.0000 | 1119 → 544 |
| irs_fw2 (2.15 MB) | 2.03 / 1.0000 | 2.02 / 1.0000 | 0.18 / 0.9989 | 2.71 / 1.0000 (grew) | 6483 → 1902 |
| specs_pymupdf_guide (7.11 MB) | 4.74 / 0.9999 | 4.35 / 0.9999 | 4.61 / 0.9985 | 8.36 / 1.0000 (grew) | 8736 → 6422 |
| photos20 (12.08 MB) | 11.04 / 0.9981 | 7.65 / 0.9971 | 12.07 / 1.0000 (no-op) | 12.07 / 1.0000 | 274 (unchanged) |

Time (seconds, best-of-1): presse 0.16–0.47 s per file across the set;
ghostscript 0.48–2.48 s (3–10× slower); mutool 0.01–3.01 s. On photos20
(72 dpi image placement) ghostscript is a pass-through no-op at 0.25 s —
identical size, SSIM 1.0, because `/ebook` only downsamples images above
its 150 dpi target. Mutool is also a pass-through for the images but does
repack the structural streams; where that repack is expensive
(pymupdf_guide: 124 images, 3.01 s) it is slower than presse, and on
already-tight files (irs_fw2, pymupdf_guide) the repack *grows* the file.

Resolution is the whole story of the size gap: presse preserves every
image's resolution (max ppi unchanged at every quality); ghostscript
downsamples scans to 544–1902 ppi. presse q30 reaches within ~9% of
ghostscript's scan sizes (e.g. irs_fw2 2.02 vs 0.18 MB — gs still smaller
because it throws away most of the pixels) while holding SSIM ≥ 0.997 at
full resolution.

## Resolution (`--dpi`) sweep

`presse press -d <dpi>` caps the effective resolution of every placed
image: an image drawn at `w×h` points is downsampled to at most
`w·dpi/72 × h·dpi/72` pixels (Ghostscript `/ebook`-style), so only images
placed above the target are touched — images already below it pass through
at source resolution, and the default (no `-d`) never downsamplies.
Placement is read from the page content stream's transform matrix at each
`Do` (the largest placement wins for shared images); images whose
placement cannot be parsed are left at source resolution.

Measured `-q 50` over high-resolution-placed files (60 dpi renders, SSIM
vs source, best-of-1):

| file (source) | no cap | 600 dpi | 300 dpi | 150 dpi | 75 dpi |
|---|---|---|---|---|---|
| irs_i1040 (4.43 MB, scans) | 4.17 MB / 1.0000 | 4.17 / 1.0000 | 3.88 / 1.0000 | 3.58 / 1.0000 | 3.47 / 1.0000 |
| irs_f1099r (0.62 MB) | 0.48 / 1.0000 | 0.48 / 1.0000 | 0.48 / 1.0000 | 0.45 / 1.0000 | 0.44 / 0.9999 |
| irs_fw2 (2.15 MB) | 2.03 / 1.0000 | 1.96 / 1.0000 | 1.95 / 1.0000 | 1.92 / 1.0000 | 1.91 / 0.9999 |
| arxiv_cyclegan1703 (37.6 MB) | 6.86 / 1.0000 | 6.86 / 1.0000 | 6.86 / 1.0000 | 6.86 / 1.0000 | 6.86 / 1.0000 |
| arxiv_gpt3_2005 (6.77 MB) | 5.69 / 1.0000 | 5.69 / 1.0000 | 5.69 / 1.0000 | 5.69 / 1.0000 | 5.69 / 1.0000 |
| photos20 (12.08 MB, 72 dpi placement) | 11.04 / 1.0000 | 11.04 / 1.0000 | 11.04 / 1.0000 | 11.04 / 1.0000 | 11.04 / 1.0000 |

Readings:

- SSIM stays ≥ 0.9999 at every level — downsampling is visually lossless
  on these renders; the size win is pure resolution removal, and 75/150
  dpi costs nothing perceptible on scans (irs_i1040: 21.7% / 19.3%
  reduction at SSIM 1.0000).
- The cap only bites where it should: paper PDFs whose figures are placed
  at ~72–150 dpi (gpt3, cyclegan) and photo PDFs placed at 72 dpi
  (photos20) are byte-identical to the no-cap run at every level — the
  same pass-through Ghostscript exhibits below its 150 dpi target.
- `-d 600` is a no-op for documents whose images are all placed below
  600 dpi, and a strict resolution cap (never an upscale) above it.

Corpus-wide gate (100-file corpus, `-d 150`, `-q 50`): 100/100 runs
completed with 0 failures; qpdf check matches the CPU corpus exactly
(94 clean + the same 4 benign warnings: 3× "input stream is complete"
and the pre-existing `issue7229` damaged-source artifact); visual sweep
97/97 valid docs clean, worst SSIM 0.98 (`arxiv_deeplab1706` — figures
placed above 150 dpi, downsampled as asked), the only flag being the same
`issue7229` source artifact the original file triggers.

## Pareto frontier vs modern full-resolution compressors

The render-SSIM gate was upgraded in response to a fair critique: SSIM at
60 dpi proves 60-dpi viewing fidelity, not print or native fidelity. Two
witnesses were added — render SSIM at **72 and 300 dpi**, and a
**native-image witness** (`native_image_ssim.py`): every image stream is
extracted (`pdfimages -j`), decoded, the source resampled to the *shipped*
dimensions, and SSIM computed with a 512 px analysis window instead of the
screen-fidelity 64×64. The two witnesses disagree on purpose, and the
native one is the stricter of the two:

> gs `/ebook` scores **1.0000 at a 300 dpi render** on an IRS scan PDF, yet
> ships its 5784×1448 scans as **165×41** images at **native SSIM 0.86**.
> No render scale at the 64×64 window can expose that; the native witness
> can. The same check gives presse/qpdf/mutool/pdf-optimizer native SSIM
> ≥ 0.9999 — they never down-sample, so their fidelity is exactly what the
> render shows.

Tools, compared at *equal measured fidelity* (SSIM thresholds, not equal
quality numbers), `benches/docker/pareto.py`: presse (`-q 30/50/75` plus the
full `-d {none,75,150,300,600} × -s {1.0,0.86,0.72}` cross matrix), qpdf
12.3 (`--optimize-images --jpeg-quality`, explicitly does not resample),
MuPDF 1.28 recompression (`clean -i -gggg -z
--{color,gray}{,-lossless}-image-recompress-method jpeg:N`, recompress-only-
when-smaller), pdf-optimizer 1.0 (MuPDF-based, `--dpi 0`), ghostscript
`/screen…/prepress`, and OCRmyPDF 17.10 (`--optimize 3 --jpeg-quality`,
see below). Sizes at SSIM ≥ 0.9999 (300 dpi render) on sampled pages:

| file | presse | qpdf | mutool | pdf-optimizer | gs /ebook |
|---|---|---|---|---|---|
| irs_fw2 scans (2.15 MB) | 2.00 (q30) / 1.92 (-d150) | **1.52** | 2.54 | 2.55 | 0.18¹ |
| arxiv_gpt3 paper (6.77 MB) | 3.53 (q30) | 4.53 | **4.34** | 6.20 | 2.04¹ |
| photos20 (12.08 MB, 72 dpi) | 6.44 (q30) | **6.22** | 6.38 | 10.58 | 12.07 (no-op) |
| photos60 (36 MB, 60 imgs) | **6.46** (q30, dedup) | 18.67 | 6.76 (dedup) | 31.72 | 12.08 (no-op) |

¹ below any full-res tool's size *because it down-samples* — native SSIM
0.86 on the scans; on papers/photos it is a pass-through.

The presse column is the **current** tree: the numbers in the size table of
the 100-file RESULTS.md sweep (2.45× vs gs, etc.) are the pre-coalescing
ones; the gap-closing work below is newer.

Speed at the same fidelity (wall time, best-of-1):

| file | presse | qpdf | mutool | pdf-optimizer |
|---|---|---|---|---|
| irs_fw2 | **0.16 s** | 0.09 s | 0.56 s | 0.30 s |
| photos20 | **0.25 s** | 0.99 s | 1.27 s | 5.92 s |
| photos60 | **0.26 s** | 2.93 s | 3.87 s | 17.82 s |

Readings:

- **At equal measured fidelity the full-res tools are visually
equivalent** (SSIM 1.0000, native 0.9999+) — the frontier is decided by
size and speed. qpdf is the size leader on scans; presse's object
coalescing (below) now closes the duplicate-heavy gap that used to belong
to mutool's `-gggg` dedup (photos60 q30: **6.46 vs 6.76 MB**, presse vs
mutool); presse's parallel path is **4–70× faster** on image-heavy
documents at the same fidelity (photos20/60: 0.25–0.26 s vs qpdf
0.99–2.93 s, mutool 1.27–3.87 s, pdf-optimizer 5.9–17.8 s). qpdf is
faster on a single-scan file (irs_fw2: 0.09 vs 0.16 s) — the speedup is
the parallelized image phase, which only pays off with many images.
- **gs wins size only by removing pixels**, which only the native witness
reveals — at equal *native* fidelity its /ebook output would sit at
~1.5–2.0 MB on the scans, next to qpdf.
- **The Pareto claim this supports**: among open-source full-resolution
PDF image compressors, presse is the fastest measured point on
image-heavy documents at SSIM ≥ 0.9999 (4–70×), qpdf the smallest on
scan corpora, mutool on duplicate-heavy ones — and the speed gap is
what the rayon pipeline buys.

Caveats: sizes/times are best-of-1 on sampled pages (≤ 8/file) and the
photo PDFs' 41-inch pages exceed the 300 dpi render budget (their witness
is the 72 dpi render + native-image check, both 1.0000); quality numbers
are page-render SSIM + image-level SSIM, not a full semantic-preservation
court (ICC, annotations, forms, layers are validated by the qpdf/gs gates
but not separately measured).

## Closing the remaining Pareto size gaps

Three measured losses against the comparators were investigated and closed
(in one commit, `perf: close remaining Pareto compression gaps`):

1. **Duplicate-heavy documents (mutool's `-gggg` win)** — the dedup cache
already made identical images *encode* once; the missing half was that 60
identical JPEG objects were still *stored* 60 times. A coalescing pass now
groups image streams by semantically-equal dictionary + exact payload
(`/Length` and the cosmetic `/Name` hint are ignored; indirect `/ColorSpace`
and `/SMask` references are followed, so two byte-identical objects at
different ids don't block dedup), keeps the lowest id, rewrites every
reference, and drops the duplicates. photos60 q30: **22.96 → 6.46 MB**
(below mutool's 6.76 MB) with zero visual change — the same pixels render
from shared objects. Also helps any logo/watermark-heavy document.

2. **qpdf scan gap** — irs_fw2's remaining images are *not* CMYK (the
investigation that ruled CMYK out: every image is DeviceRGB); the gap is
JPEG-encoder efficiency on the single photo image, which is an encoder
lever, not a structural one. Out of scope here; the honest statement is
that qpdf still holds the scan-size lead (1.52 vs 2.00 MB).

3. **Flate-wrapped JPEG (OCRmyPDF's cheap trick)** — DCT bytes normally
don't deflate, but padded/progressive JPEGs occasionally do; the
`[FlateDecode, DCTDecode]` chain is applied only when the complete flate
result is smaller. Small but free on every retained/re-encoded JPEG.

4. **`--palette` (OCRmyPDF's paper win, default off)** — for plain 8-bit
`DeviceRGB` raster streams (no mask / custom decode), a second candidate
is built: an `/Indexed` color space (≤256-entry palette + one index byte
per pixel, Flate-compressed). Exact palettes (≤256 unique colors) are
lossless; larger rasters go through a deterministic median-cut quantizer
and are accepted only above a 0.9999 native-image SSIM gate, so a lossy
palette can never visibly degrade a figure. The smallest of original /
JPEG / indexed wins per image. On the corpus it does not fire (the paper
figures are already Flate-compressed and the >256-color ones fail the
fidelity gate — their JPEG re-encode is *larger* than the source, so they
stay untouched) — it is a safety-capped option for content stored in
palette-unfriendly encodings, exercised by the regression suite's flat-
figure fixture.

Pareto sweep re-run on the four fixtures above after this commit: the
`q30` presse column in the table is the new state; wall times are
unchanged (the coalescing pass is a cheap serial map walk).

## The 4:2:0 experiment (`--jpeg-encoder`) and what the falsifier found

Hypothesis under test: qpdf's lead on scan corpora is its JPEG chroma
sampling — libjpeg's default RGB pipeline is YCbCr **4:2:0**, while presse's
pinned `image` encoder writes effectively **4:4:4** (full-resolution Cb/Cr).

`--jpeg-encoder` (default off) swaps the CPU encoder to the pure-Rust
`jpeg-encoder` codec at 4:2:0 with box-averaged chroma (`h2v2_downsample`),
AVX2 `simd` under native codegen. Verified on the actual stream:

| encoder | sampling (SOF0) | img 0 payload (irs_fw2, q30) |
|---|---|---|
| `image` (default) | (1,1),(1,1),(1,1) — 4:4:4 | 40,977 B (16.9K flate-wrapped) |
| `jpeg-encoder` 4:2:0 | (2,2),(1,1),(1,1) | 29,263 B (15.2K flate-wrapped) |
| qpdf/libjpeg | (2,2),(1,1),(1,1) | 28,578 B (not flate-wrapped) |

So the chroma hypothesis is **confirmed per-image**: 4:2:0 encodes half the
chroma DCT blocks, and with the flate-wrap trick presse's img 0 (15.2K)
now *beats* qpdf's (28.6K). On the photo corpus the flag also wins: −8.3%
size (photos20/60 q50) and faster (304→162 ms, photos20).

**But the falsifier did not reproduce the claimed 2.00 → ~1.5 MB on
irs_fw2** — 4:2:0 lands at 1.994 MB, not 1.5. The reason is structural,
not chroma: irs_fw2's image content is only ~265 KB; its 119 FlateDecode
streams (1.81 MB) are stored at a low compression level, and qpdf
**re-compresses existing Flate streams** (1.81 → 1.34 MB) while presse
skips already-Flate streams. That 0.47 MB is the actual residual — a
separate lever (a recompress-Flate pass in the writer), not the encoder.
The claim "qpdf's win is 4:2:0" was wrong for this file; the claim "the
`image` encoder's 4:4:4 doubles chroma resolution vs libjpeg" is correct
and is what `--jpeg-encoder` fixes.

**Witness update.** Because a luma-only court cannot see chroma loss,
`native_image_ssim.py` now reports per-channel (min over R/G/B) SSIM next
to luma, and `pareto.py` uses `min(luma, min-channel)` as the fidelity
witness — a compressor that subsamples chroma is judged on what it changed.

## The `-d` × `-s` matrix (quality × speed, vs the tools)

`-ssim <target>` replaces the arbitrary `-q` knob with a fidelity target:
the JPEG quality is derived from a committed calibration curve
(`calibrate_ssim.py` → `SSIM_CALIBRATION` in `src/pdf/images.rs`), measured
with the same native 512-px-window SSIM as the Pareto witness on **grainy
gray scans — the worst case for JPEG**. The curve is conservative: smooth
content (photos, paper figures) always *exceeds* the requested target, so
the sweep reports requested vs achieved honestly. The two knobs are
orthogonal — `-d` removes *pixels* (only where the placement exceeds the
cap), `-s` removes *bits* (everywhere) — and compose additively.

presse matrix cells (size MB / wall s / achieved SSIM at a 300 dpi render,
best-of-1, `-q 50` base):

| file | `d0-s1.0` | `d0-s0.86` | `d0-s0.72` | `d75-s0.72` | `d150-s0.72` | `d300-s0.72` |
|---|---|---|---|---|---|---|
| irs_i1040 (126p scans) | 4.17 / 3.59 / 1.0000 | 3.77 / 3.56 / 1.0000 | 3.72 / 3.57 / 1.0000 | **3.43 / 3.77 / 0.9999** | 3.47 / 3.79 / 1.0000 | 3.59 / 3.91 / 1.0000 |
| irs_fw2 (scans) | 2.03 / 0.17 / 1.0000 | 2.01 / 0.17 / 0.9998 | 2.01 / 0.17 / 1.0000 | 1.90 / 0.08 / 0.9999 | 1.91 / 0.10 / 1.0000 | 1.93 / 0.08 / 1.0000 |
| arxiv_gpt3 (paper) | 5.69 / 0.40 / 1.0000 | 4.04 / 0.38 / 0.9997 | 3.86 / 0.39 / 0.9998 | 3.86 / 0.41 / 0.9998 (dpi no-op) | same | same |
| photos20 (72-dpi photos) | 11.04 / 0.27 / 1.0000 | 4.41 / 0.26 / 0.9997 | 3.93 / 0.25 / 0.9992 | 3.93 (dpi no-op) | same | same |

Readings:

- **`-d` only bites above the placement's effective resolution**: on
  photos20 (72 dpi placement) and gpt3 (figures ≤ 150 dpi) every dpi
  column is byte-identical to `d0` — the cleanest demonstration that the
  cap never touches content below it. On scans it removes a quarter of the
  bytes and *speeds the encode up* (irs_fw2: 0.17 → 0.08 s — smaller
  images encode faster).
- **`-s` bites everywhere**: photos20 11.04 → 4.41 MB at `-s 0.86`
  (achieved 0.9997 — smooth content exceeds the worst-case target) and
  3.93 MB at `-s 0.72` (achieved 0.9992), at the same or faster wall
  time. On scans it is gentler (their flate content is size-insensitive).
- **Composed they add**: irs_i1040 4.17 (baseline) → 3.72 (`-s 0.72`)
  → 3.43 (`-d 75 -s 0.72`), all at SSIM ≥ 0.9999.

vs the other tools at the same 300-dpi-render threshold (0.9999),
smallest / fastest per file:

| file | smallest @ ≥0.9999 | fastest @ ≥0.9999 |
|---|---|---|
| irs_i1040 | **presse `d75-s0.72` 3.43 MB** (qpdf is flat at 4.48 — its optimizer leaves flate scans that jpeg would grow) | qpdf q75 0.52 s |
| irs_fw2 | gs /ebook 0.18 MB¹ | **presse `d150-s1.0` 0.08 s** |
| arxiv_gpt3 | mutool jpeg:30 4.34 MB | qpdf q75 0.36 s |

¹ gs's size win is resolution theft invisible to render SSIM — native
witness 0.86 on the scans (see the Pareto section). presse's `d75-s0.72`
is the smallest full-fidelity (native ≥ 0.9999) cell on both scan files.

OCRmyPDF 17.10 (`-m skip -l eng --output-type pdf --optimize 3 --jpeg-quality`)
— measured once `pngquant` + the tesseract `eng`/`hocr` config were
installed. Its optimizer is a real contender on some content:

| file | OCRmyPDF q50 | vs the field at SSIM ≥ 0.9999 |
|---|---|---|
| arxiv_gpt3 (paper) | **2.57 MB** / 2.5 s / 1.0000 | smallest of any tool (presse q30 5.03, mutool 4.34, qpdf 4.53) |
| irs_fw2 (scans) | 1.72 MB / 0.54 s / 1.0000 | smaller than presse (2.03) and mutool (2.54), just above qpdf (1.52) |
| photos20 (photos) | 9.05 MB / 3.8 s / 1.0000 | presse `-s 0.86` is smaller (4.41) at 0.26 s |
| irs_i1040 (126p scans) | **exit 4 — invalid output** | OCRmyPDF's own validator rejects its result ("The generated PDF is INVALID"); qpdf tolerates it with the same benign source-truncation warning every tool gets on this file |

Quality is effectively flat in `--jpeg-quality` on non-JPEG-heavy docs
(q30 = q50 = q75 on gpt3/irs_fw2 — the flag only governs JPEG
recompression, and its quantization does the rest). Speed is 6–10×
slower than presse on image-heavy files (photos20 3.8 vs 0.26 s). The
irs_i1040 failure is not a harness artifact: the sweep runs it exactly
as the CLI would, and its own validator exits 4 — a genuine validity
finding the Pareto harness catches that a size-only comparison would
have missed.

## The non-compressors

Measured so their speed is not mistaken for compression throughput:

- **unpdf** (0.357 s median/file): text extraction only. Word fidelity vs
  poppler `pdftotext` on a paper PDF: 2101 unique words, 0.859 precision /
  0.921 recall. `unpdf extract` returned **0 images** from a 20-photo PDF —
  image extraction is non-functional on that input. No PDF output; no
  compression.
- **pdfrs / pdfcli** (0.1.9): `pdf-to-md` panics on 16/100 corpus files (a
  non-char-boundary panic in its text search) and silently emits a 0-byte
  file on another 77; only 7 files produce any markdown at all. Its `merge`
  produced an 80-page PDF of **28,894 bytes from 48,326,372 bytes of inputs
  (photos20 + photos60) — rendered blank** (the images are dropped, not
  compressed). `md-to-pdf` creates a 1.4 KB PDF from 64 B of markdown (a
  creator, not a compressor).

## MuPDF / mutool — a lossless repack, not a re-encoder

`mutool clean -gggg -z` is the standard "optimize" invocation, but it never
touches the image data: extracted JPEGs are byte-identical to the source
(verified 20/20 on the photo corpus) and renders match pixel-for-pixel
(SSIM 1.0000 on every page of all 99 valid corpus outputs — visual sweep,
pdftoppm 72 dpi + SSIM, long docs sampled at 80 pages). Its size effect is
limited to re-deflating structural streams and dropping dead objects: on
the 100-file corpus it *shrank* 62 of the 99 files that produced output
(median −6.3% on the shrinkers) and *grew* the other 37 — none came out
byte-identical — so its "compression" is bounded by how wasteful the
source's stream packing was, and it has no quality knob and no resolution
change. Outputs are deterministic across runs: a re-run on 2026-08-19
produced byte-identical files (0 size mismatches in 99/99) with the same
62/37 split.

Speed is bimodal. Per-file median is 0.015 s — faster than presse — but the
`-gggg` duplicate-object scan scales severely superlinearly in this corpus:
`specs_pdf32000.pdf` 210 s, `specs_pdfref17old.pdf` 352 s (killed, no
output), `irs_i1040.pdf` 70 s, versus 0.1–0.6 s for the same files with the
plain `-z` repack. Total over the corpus: 692 s (worst of any tool here,
driven entirely by that tail; 340 s excluding the killed file).

## Speed in context (100-file corpus, excluding the 60 s spec outlier)

| tool | operation | total | median/file |
|---|---|---|---|
| presse parallel | compress, full res | 14.3 s | 0.017 s |
| presse serial | compress | 22.1 s | 0.023 s |
| unpdf | text extraction | 36.2 s | 0.357 s |
| pdfrs | md conversion (7/100 produce output) | 11.4 s | 0.017 s |
| ghostscript /ebook | compress (resolution-lossy) | 293 s | 0.152 s |
| mutool clean -gggg -z | lossless repack | 692 s¹ | 0.015 s |

¹ dominated by the `-gggg` duplicate scan's severe superlinear scaling on
`specs_pdfref17old.pdf` (killed without output; 352 s in the original run,
still running when re-measured under a 600 s cap) and `specs_pdf32000.pdf`
(210 s); excluding the killed file the total is 340 s. The plain `-z`
repack is 0.1–0.6 s on the same files.

## The `optimize` feature: codec candidates, fonts and structural passes

The `optimize` Cargo feature (`cargo build --release --features optimize`)
enables the default-off heavy passes behind `--compression
{fast,balanced,small,smallest}` and the individual flags `--dedup`,
`--zopfli`, `--font-subset`, `--jbig2`, `--jpeg2000`, `--mrc`. Each is a
*candidate* in the same size court as the image pipeline, or a
strictly-smaller-gated structural rewrite, and every one is validated by
per-image fidelity gates (palette ≥0.9999 native SSIM; the bitonal masks —
G4/JBIG2/MRC — on the classifier's measured reconstruction-error gate;
JPEG2000 on a runtime decode-back gate — the candidate is decoded and
measured against the source pixels on the native 512-px window and
admitted to the size court only above 0.98 SSIM, so the 85%-of-JPEG rate
target is a sizing hint rather than a quality assumption)
plus the regression suite (qpdf/poppler/mutool/gs gates, pixel-identical
rendering checks, text-extraction checks, and the same brutal
"colored rectangle underneath + non-black current color" trap that
`--raster-classify` already passes).

Measured on the synthetic corpus (`/tmp/presse_smoke/corpus`,
deterministic, best-of-1, `-q 50`, current tree; the Pareto sweep in
`benches/docker/pareto.py --optimize` reproduces every cell):

| corpus file | default (`fast`) | `--mrc` | `--jbig2` | `--compression smallest` |
|---|---|---|---|---|
| scanned.pdf (3.40 MB, 3 grainy gray pages) | 0.27 MB | 2.3 KB | 1.3 KB | 1.1 KB |
| image-heavy.pdf (17.98 MB, 60 photos, 24 pages) | 1.51 MB | — | — | 1.38 MB |

Where the candidates land on the Pareto matrix (300 dpi render SSIM vs
the source, `pareto.py --optimize`, same corpus):

- **scanned.pdf — `--mrc` wins the ≥0.999 frontier outright.** The flat
  two-tone composite is 2.3 KB at render SSIM 0.9997, against qpdf q50
  0.36 MB / mutool 0.29 MB / presse q50 0.27 MB at the same or lower
  fidelity, and gs `/screen` 0.48 MB. At ≥0.9999 every full-res tool sits
  at ~0.56–0.66 MB (presse q75 0.60 MB, qpdf 0.66 MB, mutool 0.56 MB) —
  the grain-preserving JPEG path, where the flat composite cannot enter
  because replacing the paper texture costs render fidelity; the
  ≤0.98 frontier belongs to the bitonal masks (`--jbig2` 1.3 KB and
  `smallest` 1.1 KB, both at SSIM 0.981 — the texture is gone, which is
  what "flat" means). On this synthetic grainy-scan fixture, flat MRC
  reaches 2.3 KB at 0.9997 300-dpi render SSIM — roughly two orders of
  magnitude smaller than the measured full-raster alternatives at
  comparable render fidelity. That is a *render-fidelity* claim: the
  underlying image information is not equivalent (the 0.9999 court
  exposes the missing native paper texture), which is exactly the region
  commercial MRC engines aim at — and the residual their texture-aware
  variants attack.
- **image-heavy.pdf — the codec candidates are honest, not Pareto-new.**
  `--jpeg2000` wins the per-image size court on the photos (1.38 MB vs
  1.51 MB at q50) but at render SSIM 0.9931 vs 0.9995 — the runtime
  gate's per-image ≥0.98 native-window admission is met, yet the
  document-level point is inside the frontier, dominated by the ssim
  mode (`d0-s0.86`: 0.28 MB at 0.9967). At ≥0.9999 the frontier is
  unchanged (presse q75 2.53 MB vs qpdf 1.71 MB, mutool 2.40 MB — the
  J2K candidate never qualifies there because its admitted reconstructions
  sit below the gate on this corpus). This is the expected shape for a
  rate-targeted lossy candidate; nothing claims otherwise.
- `--dedup` / `--zopfli` / `--font-subset` on these three files are
  structural no-ops or near-no-ops (photos have no duplicate streams,
  `smallest` still pays their CPU: 1.46 s vs 0.03 s at q50) — their wins
  are on duplicate-heavy corpora, where the existing `photos60` Pareto
  row (6.46 MB, dedup) already shows the effect.

Readings and honest caveats:

- The **scanned** row shows the three bitonal regimes: `fast` pays
  photographic cost for document content (0.26 MB), `--mrc` builds the
  **flat two-tone** composite — solid paper color (a 1×1 fill, median of
  the classified paper) + solid ink color + full-resolution lossless G4
  mask (2.3 KB, mean luma diff 1.6 vs the source at 30 dpi — the earlier
  downsampled-JPEG background was dropped because near-flat JPEG
  bitstreams are mis-decoded as full-page gradients by poppler and
  Ghostscript), and `--jbig2` / `smallest` collapse the page to a flat
  1-bit mask (~1 KB, pixel-identical to the equivalent
  `--raster-classify` G4 output — the grain is gone, as with any bitonal
  representation). Note what "flat" means here: the median paper color
  *replaces* the paper texture, so flat MRC sits at the bottom of the
  candidate ladder (`source scan 3.40 MB → JPEG ~260 KB → textured MRC —
  compressed background + foreground + mask, the ABBYY-style mechanism,
  future work → flat MRC 2.3 KB → JBIG2 1.3 KB`) and the fidelity court
  decides where a document belongs. The size court picks the smallest per
  image, so on real scans the winner depends on how much paper texture is
  worth keeping.
- **`--jpeg2000`** re-encodes the 60-photo corpus to ~2.6 MB (the same
  order as `--jpeg2000` alone: 2.59 MB) at a mean luma difference of
  ~1.3 levels vs the source on sampled pages — visually equivalent, but
  the JPEG candidate wins or ties on most photos, so the flag mainly pays
  off where J2K's rate-distortion genuinely beats JPEG. The codestream is
  wrapped in a minimal JP2 file (signature/file-type/image-header+sRGB/
  codestream boxes): poppler decodes that cleanly where a raw codestream
  only renders after noisy fallback. Ghostscript's JPX decoder is broken
  on *all* JP2 files (reproduces with OpenJPEG's own output), so the
  regression oracle for JPX is poppler + mutool + OpenJPEG.
- **`--font-subset`** on a font-heavy document (194 KB with a full
  TrueType program) reaches ~7 KB with pixel-identical rendering and
  intact text extraction (rebuilt `/ToUnicode`, CID-font rewrite). The
  pass is deliberately conservative and TrueType-only: fonts whose
  used-glyph mapping cannot be resolved exactly (unusual encodings,
  resource-less forms, unparseable streams) and CFF (`FontFile3`)
  programs are skipped rather than risk a glyph change.
- **`--dedup` / `--zopfli`** are structural passes with no fidelity
  surface: identical-stream coalescing and a strictly-smaller Zopfli
  re-encode, both verified byte-equivalent on the decoded content.
- **Poppler notes discovered while oracling these codecs**: its JBIG2
  decoder emits *inverted* samples (so JBIG2 images use the identity
  `/Decode`, the opposite of G4's `[1 0]`), its JBIG2 parser warns
  "extraneous byte after segment" on the encoder's spec-mandated A.3.6
  flush marker (non-fatal, output correct), and its Splash soft-mask path
  overflowed its `int`-based mask allocation ("Bogus memory allocation
  size") whenever the MRC content rewrite re-applied the placement `cm`
  for the foreground — squaring the scale (1600×1200 became
  2,560,000×1,440,000). The rewrite now draws the foreground at the
  already-current transform, and Ghostscript additionally requires the
  soft mask to be a typed `/Subtype /Image` XObject or it silently drops
  it; both are fixed and rendered pixel-identically across
  poppler/mutool/gs. These are renderer quirks, not presse defects — each
  is documented next to the code that works around it.

