# presse -- PDF manipulation command-line tool
<p align="center">
    <img src="./logo.webp" alt="Presse's logo" width=150 height=150/>
</p>

![demo](demo/demo.gif)

<p align="center">
    <img src="https://img.shields.io/crates/v/presse.svg?style=for-the-badge" alt="Crates.io">
    <a href="https://github.com/SimonBure/presse"><img src="https://img.shields.io/github/stars/SimonBure/presse?style=for-the-badge&label=Stars%20&logo=github&logoColor=white" alt="Presse stars" /></a>
</p>

A fast command-line tool for PDF manipulation written in Rust.

**Compress and merge** PDF files naturally and easily with this ready-to-use command line tool.
**Convert images** of any format into ready-to-use pdfs.

**~7× faster** than ghostscript (median, per-document on a 100-PDF corpus) and **~2× faster than single-threaded** rayon. Benchmarked in [`benches/docker/RESULTS.md`](benches/docker/RESULTS.md) — the headline figure is a median; best case is much higher, mean is lower.

## Features

- **Image recompression** — re-encodes images at a target quality, skipping CMYK images
- **Engine optimizations** — rayon-parallel re-encoding, mimalloc allocator, zlib-rs deflate backend, memory-mapped reads, deduplicated identical image streams
- **Structural compression** — object stream packing, xref stream compression
- **Batch processing** — compress multiple files in one command via shell wildcards
- **Smart output paths** — sensible defaults, explicit naming, or output to a directory
- **PDF merging** — combine multiple documents into one, with optional compression
- **Image conversion** — transform any image (.png, .jpg) into a .pdf
- **Merge docs and images** in a single command, with smart format detection

## Installation

### Shell installer (macOS/Linux)
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SimonBure/presse/releases/latest/download/presse-installer.sh | sh
```

### PowerShell (Windows)
```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/SimonBure/presse/releases/latest/download/presse-installer.ps1 | iex"
```

### MSI (Windows)
Download the `.msi` from the [latest release](https://github.com/SimonBure/presse/releases/latest).

### Cargo

```bash
# JPEG-only pipeline (the default, smaller/faster to build)
cargo install presse
# + the optimize passes (--compression, --dedup, --zopfli, --font-subset,
#   --jbig2, --jpeg2000, --mrc)
cargo install presse --features optimize
```

## Benchmark

Measured over 100 public real-world PDFs (≈300 MB, 5,268 pages), comparing
`presse press --quality 50` against Ghostscript `/ebook`. Full methodology,
per-document data, and correctness gates in
[`benches/docker/RESULTS.md`](benches/docker/RESULTS.md).

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

A reproducible containerized benchmark harness lives in [`benches/docker/`](benches/docker/):

```bash
docker build -t presse-bench -f benches/docker/Dockerfile.bench .
docker run --rm presse-bench
```

The container builds with `RUSTFLAGS="-C target-cpu=native"` (AVX2/FMA auto-vectorization) and reports wall time, throughput, and peak RSS for a generated corpus (text-heavy, image-heavy, scanned), comparing the rayon pipeline against a `RAYON_NUM_THREADS=1` baseline.

## Usage

### Compress — `presse press`

```bash
# Single file — outputs document_compressed.pdf alongside the original
presse press document.pdf

# Custom output name
presse press document.pdf -o small.pdf

# Output to a directory
presse press document.pdf -o compressed/

# Batch — multiple files into a directory
presse press *.pdf -o compressed/

# Set JPEG quality (0–100, default 80)
presse press document.pdf --quality 60

# Cap image resolution to 150 dpi (75 screen, 150 ebook, 300 printer, 600 prepress)
presse press document.pdf -d 150

# Show size comparison after each file
presse press document.pdf --verbose

# Combine everything in a merged.pdf
presse merge *.pdf

# Convert images into pdfs
presse convert img1.png img2.jpg img3.jpeg

# Convert all png images, compressed them and merged them into a single pdf 
presse convert *.png -m -q 50

# Merge image and pdf together into a single pdf
presse merge *.png *.pdf
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>_compressed.pdf` | Output file or directory |
| `-q, --quality` | `80` | Image recompression quality (0–100) |
| `-d, --dpi` | source resolution | Cap placed image resolution to DPI pixels/inch (75 screen, 150 ebook, 300 printer, 600 prepress); omitted = keep source resolution |
| `-s, --ssim` | `1.0` | Target output fidelity as measured SSIM (calibrated on grainy scans — the worst case for JPEG — so smoother content exceeds the target); lower = smaller and faster. `1.0` = use `-q` as given |
| `--palette` | `false` | Also build an `/Indexed` palette candidate for eligible flat-color images (figures, charts, scans) and keep the smallest of original / JPEG / indexed; exact palettes are lossless, lossy ones are gated on 0.9999 native SSIM |
| `--jpeg-encoder` | `false` | Use the pure-Rust `jpeg-encoder` codec (YCbCr 4:2:0, box-averaged chroma — libjpeg's default model, which Ghostscript/qpdf use) instead of the `image` crate's 4:4:4 encoder; smaller RGB output at the same `-q`, faster, but opt-in because luminance-SSIM courts don't see chroma loss |
| `--raster-classify` | `false` | Run the raster classifier on every decoded image: bitonal text/rules stored as a photographic RGB raster are re-stored as a 1-bit CCITT G4 opaque `DeviceGray` image (an RGB page → a few KB of G4). The G4 encoding of the 1-bit payload is lossless; the RGB→bitonal conversion itself is lossy, which is why only near-perfect black-and-white content is masked. Flat-color figures get the `/Indexed` palette candidate; photos and mixed pages are never masked. The smallest of original / JPEG / indexed / mask wins per image |
| `--recompress-flate` | `false` | qpdf-style structural recompression: decode existing `/FlateDecode` streams and re-encode them at the writer's level 9, keeping each only when smaller. Lossless (no content-byte changes); recovers the compression-level gap form tools leave behind |
| `-v, --verbose` | `false` | Print size comparison after each file |

### Resolution capping (`--dpi`)

`-d <dpi>` downsamples images to at most `w·dpi/72 × h·dpi/72` pixels,
where `w × h` is the image's placed size on the page in points — the same
placement-aware rule Ghostscript applies with `-dPDFSETTINGS`. The cap is
strict: it **never up-samples**, and images whose on-page placement cannot
be determined (or that are already below the cap) keep their source
resolution. Only `press` takes the flag today.

```bash
presse press big.pdf -o small.pdf -d 150   # ~ebook: 150 dpi cap
presse press scans.pdf -d 75               # ~screen: 75 dpi cap
presse press deck.pdf -s 0.86              # ~q9: fidelity-targeted, 60% smaller on photos
presse press figures.pdf --palette         # also try indexed-color palettes
presse press photos.pdf --jpeg-encoder     # libjpeg-style 4:2:0 chroma (smaller, faster)
presse press scans.pdf --raster-classify   # bitonal pages -> 1-bit CCITT G4 masks
presse press forms.pdf --recompress-flate  # re-encode existing Flate streams at level 9
```

### Chroma subsampling (`--jpeg-encoder`)

`--jpeg-encoder` swaps the CPU JPEG encoder from the `image` crate's
full-resolution chroma (effectively 4:4:4) to the pure-Rust `jpeg-encoder`
codec at **YCbCr 4:2:0 with box-averaged chroma downsampling** — exactly
libjpeg/libjpeg-turbo's default RGB pipeline (`jpeg_set_defaults` +
`h2v2_downsample`), which is the model Ghostscript and qpdf use. That is
half the DCT chroma blocks of 4:4:4, so RGB output at the same `-q` is
smaller (−8% on the photo corpus at q50) and the AVX2 `simd` path encodes
faster. Grayscale images stay single-component on both paths. The flag is
opt-in rather than the default because a luminance-SSIM quality court does
not see chroma loss — the benchmark's native-image witness reports
per-channel SSIM so 4:2:0 output is judged on what it actually changed.

Why not the default: chroma subsampling is a quality tradeoff (invisible on
most content, visible on sharp saturated edges), and the default output
stays byte-identical to the pre-flag behavior. Only `press` takes the flag
today.

### Duplicate-image collapsing and palette quantization

Two structural optimizations are always on:

- **Coalescing** — after re-encoding, image streams that are semantically
  identical (same dictionary and same payload) collapse onto one object,
  and every reference is rewritten to it. Documents that embed the same
  photo, logo, watermark or figure many times (60 photos that are really
  20 unique images × 3 copies) shrink to the unique content once — e.g.
  22.96 → 6.46 MB at `-q 30` on the photos60 corpus file, below MuPDF's
  dedup result, with identical rendering.
- **Flate-wrapped JPEG** — DCT streams are normally incompressible, but
  padded/progressive JPEGs occasionally shrink under zlib; the
  `[FlateDecode, DCTDecode]` chain is used only when the full flate result
  is smaller.

`--palette` (off by default) additionally tries an `/Indexed` color-space
candidate for eligible flat-color images: one index byte per pixel plus a
≤256-entry palette, Flate-compressed — the representation that beats JPEG
on figures, charts, diagrams and screenshots. Images with ≤256 unique
colors convert losslessly; larger rasters go through a deterministic
median-cut quantizer and are accepted only above a 0.9999 native-image SSIM
gate, so a lossy palette can never visibly degrade a figure. Photos
effectively never qualify. The smallest of original / JPEG / indexed wins
per image. Only `press` takes the flag today.

### Raster classification (`--raster-classify`)

A scanned page stored as one RGB raster is usually not a photograph — it
is text and rules on paper, and the JPEG representation pays photographic
cost for document content. `--raster-classify` (off by default) runs a
small classifier on every decoded image — an adaptive Otsu threshold,
4-connected-component density and color statistics, all on a bounded
sample window (≤1024 px on the long edge) so a 28 MB scan classifies in
one pass — and routes the content:

- **Bitonal text / rules** → a 1-bit CCITT Group 4 opaque `DeviceGray`
  image — deliberately *not* an `/ImageMask` stencil, because a stencil's
  white is transparent and its ink inherits the current graphics color,
  so it is not a substitute for an opaque raster. An RGB text page
  becomes a few KB of G4, decoded pixel-identically by every viewer. The
  G4 encoding is lossless; the RGB→bitonal conversion itself is lossy,
  which is why the flag only fires on near-perfect black-and-white
  content.
- **Flat-color figures** → the `/Indexed` palette candidate.
- **Photos / mixed pages** → the JPEG path (the within-image split into
  mask + continuous-tone layers is future work).

The decision is deliberately conservative — an image is masked only when
it is *mostly* black-and-white with glyph-sized components — so photos and
colored figures are never masked. The mask drops color and anti-aliasing
(bitonal content is black-on-white by definition), which is why the flag
is opt-in rather than the default. The smallest of original / JPEG /
indexed / mask wins per image. Only `press` takes the flag today.

### Structural recompression (`--recompress-flate`)

Form tools and older writers store `/FlateDecode` streams at a lower
compression level than this writer uses, so decoding and re-encoding the
*same bytes* shrinks the file without touching a single content byte —
qpdf's `--recompress-flate` trick (on the irs_fw2 scan corpus: qpdf's
1.81 → 1.34 MB). Only pure-Flate streams with no `/DecodeParms` are
touched (DCT/LZW/multi-filter chains and anything that fails to
decompress are left alone), each re-encoded stream is kept only when
smaller, and the pass is lossless: pixels, text, metadata and fonts are
unchanged, only the compressed representation differs. Only `press` takes
the flag today.

### Optimize-feature passes (`--compression`, codecs, fonts, dedup)

The heavyweight candidates live behind the `optimize` Cargo feature
(`cargo install --features optimize` or `cargo build --release --features
optimize`) and default-off CLI flags. Each pass follows the same

discipline as the image pipeline: a representation is applied only when it
is *strictly smaller* **and** within its measured fidelity budget — the
lossless structural rewrites (`--dedup`, `--zopfli`, `--recompress-flate`,
font subsetting) are safe by construction, while the lossy candidates are
gated on a per-image measurement (palette on ≥0.9999 native SSIM, the
bitonal masks on the classifier's reconstruction-error gate, JPEG2000 on
its rate target plus the fixture-level oracle) — and degrades to a no-op
whenever a conservative gate rejects it. The flags are independent and
compose; `--compression` presets expand to them:

- **`--dedup`** — the duplicate-image coalescer extended to every stream:
  byte-identical FontFile programs, ICC profiles, XForms, patterns and
  arbitrary streams collapse onto one canonical object with every
  reference rewritten. Identical stream + identical dictionary renders
  identically, so this is lossless by construction.
- **`--zopfli`** — the `--recompress-flate` pass with the Zopfli deflate
  search instead of level-9 zlib: same decoded bytes, a few percent
  smaller on text/content streams, kept only when strictly smaller.
- **`--font-subset`** — embedded TrueType fonts are subset to the
  glyphs the content streams actually show (typst's `subsetter`) and
  rewritten as CID fonts (`/Identity-H` + identity `CIDToGIDMap`) with the
  text strings remapped in place and a rebuilt `/ToUnicode`, so rendering
  and text extraction are unchanged. A font is skipped whenever the
  used-glyph mapping cannot be resolved exactly (unusual encodings,
  resource-less forms, unparseable streams) or the program is CFF
  (`FontFile3` — CFF subsetting is not implemented, only TrueType
  `FontFile2` programs are subset), and the subset is kept only
  when strictly smaller. A font-heavy PDF (194 KB with a full TrueType
  program) becomes ~7 KB with pixel-identical rendering.
- **`--jbig2`** — a lossless JBIG2 candidate (Rust `jbig2enc-rust`,
  symbol-dictionary mode) for the same 1-bit masks the G4 candidate uses:
  repeated glyph shapes share one dictionary entry, which is where G4
  loses to JBIG2 on text pages. The encoder's output is oracled against
  poppler/ghostscript/mutool in the regression suite (its A.3.6 flush
  marker makes poppler print a benign "extraneous byte" notice, and its
  sample polarity is the *identity* `/Decode`, the opposite of G4's —
  both verified pixel-identically). The size court keeps the smaller of
  G4 / JBIG2 / original.
- **`--jpeg2000`** — a JPEG2000 candidate (pure-Rust `j2k` codec) for
  continuous-tone images, emitted as a minimal JP2 file (signature /
  file-type / image-header + sRGB / codestream boxes — poppler parses the
  JP2 file form cleanly, where a raw codestream only renders after noisy
  fallback), rate-targeted at 85% of the JPEG candidate's bytes. The rate
  target is only a sizing hint: every candidate is decoded back and
  measured against the source pixels on the native 512-px window
  (`CandidateEvidence` — SSIM plus luma/chroma/edge error), and admitted
  to the size court only above a 0.98 native-SSIM gate, so `smallest`
  can never trade readability for bytes. On the 18 MB image-heavy corpus
  the photo pages re-encode at a mean luma difference of ~1.3 levels.
  Ghostscript's JPX decoder is broken on *all* JP2 files (including
  OpenJPEG's own), so the regression oracle is poppler + mutool +
  OpenJPEG.
- **`--mrc`** — the mixed-raster composite commercial scan compressors
  use: a solid paper-color background (the median paper color, emitted as
  a 1×1 image — deliberately *not* a JPEG, because near-flat JPEG
  bitstreams are mis-decoded as full-page gradients by poppler and
  Ghostscript), a solid ink-color foreground, and a high-resolution
  lossless CCITT G4 mask composited as the foreground's `/SMask` (a full
  image XObject — Ghostscript silently drops a soft mask whose stream
  lacks `/Type /XObject` + `/Subtype /Image`); the content stream is
  rewritten to draw background then foreground *without* re-applying the
  placement `cm` (the source image's transform is already current there;
  re-emitting it squares the scale). This is the
  *intended* home for mask compositing — unlike a stencil dropped in for
  an opaque raster, the graphics state and the layers beneath are under
  presse's control, so the composite cannot leak background through the
  paper or recolor the ink (regression-tested pixel-identically under a
  blue rectangle with a red current color). The mask is always G4:
  poppler's JBIG2 decoder inverts its samples, which would make the mask
  polarity viewer-dependent. This is **flat / two-tone MRC**: solid paper
  color + solid ink color + full-resolution lossless mask. On a 3.4 MB
  grainy scan corpus it lands at ~2.3 KB — the paper texture is *not*
  preserved (the median color replaces it), which is exactly the
  deliberate distinction from future **textured / continuous-tone MRC**
  (a compressed photo background + foreground + mask, the ABBYY-style
  mechanism) that would sit between plain JPEG and the flat composite on
  the candidate ladder:
  `source scan 3.40 MB → JPEG ~260 KB → textured MRC (future) → flat MRC 2.3 KB → JBIG2 1.3 KB` —
  the fidelity court decides where a document belongs.
  poppler/mutool/ghostscript render the composite pixel-identically; the
  earlier poppler "Bogus memory allocation size" notice on large
  full-bleed masked images was the double-`cm` rewrite bug, fixed here.

`--compression small` / `smallest` are the "leave nothing on the table"
presets: `smallest` on the scan corpus reaches ~1.1 KB (JBIG2 masks) and
on the photo corpus ~2.6 MB at near-lossless measured fidelity.

### Merge — `presse merge`

```bash
# Merge two or more files — outputs merged.pdf in the current directory
presse merge a.pdf b.pdf c.pdf

# Custom output name
presse merge a.pdf b.pdf -o result.pdf

# Output to a directory
presse merge a.pdf b.pdf -o output/

# Also compress images while merging
presse merge a.pdf b.pdf --compress
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `merged.pdf` | Output file or directory |
| `-c, --compress` | `false` | Compress images in the merged document |

## GPU acceleration (experimental)

`presse press` can offload JPEG re-encoding to a GPU with
`--acceleration cuda` (NVIDIA nvJPEG) or `--acceleration rocm` (AMD
rocJPEG). This is experimental, opt-in work — neither backend is linked
into the release binaries, neither runs in CI, and the CPU backend is the
default and always available. On a default build, `--acceleration cuda`
or `rocm` fails with an explicit "requires a build with the … feature"
error; build with the feature enabled to use them:

```bash
# NVIDIA
cargo install presse --features cuda --locked

# AMD
cargo install presse --features rocm --locked
```

`--locked` matters: the `baracuda` requirement is a caret range over
pre-releases, so without it Cargo may resolve a version the code has not
been tested against.

Runtime needs:

- **cuda** — an NVIDIA driver and the `nvjpeg` shared library. No CUDA
  toolkit is needed to *build*: the vendor library is loaded at runtime.
  Baseline 4:2:0 JPEGs are additionally decoded on the NVDEC hardware
  engine through the Video Codec SDK. The SDK's library (`libnvcuvid.so.1`)
  ships with the driver; its API declarations come from the SDK headers
  (`ffnvcodec-headers` on Debian/Ubuntu — the NVDEC stage is written
  against the cuvid API they declare), so install both. The NV12→planar
  conversion runs a tiny kernel that ships as embedded PTX, JIT-compiled
  by the driver. Progressive / 4:2:2 / 4:4:4 JPEGs keep the nvJPEG decode
  path. Validated on an RTX 4080 SUPER with CUDA 13.3 (verify with
  `cargo run --release --features cuda --example nvdec_verify`).
- **rocm** — compile-tested only; requires a ROCm installation with
  `rocjpeg` at runtime.

If the driver or library is missing at runtime, presse warns and falls
back to the CPU encoder per stream — a broken GPU can never drop or
corrupt a stream. The NVDEC stage is optional the same way: when
`libnvcuvid` or the PTX JIT is unavailable, decode stays on nvJPEG
(`PRESSE_NO_NVDEC=1` forces that path). Measured behavior (threshold
routing, speed vs size tradeoffs) is documented in
[`benches/docker/RESULTS.md`](benches/docker/RESULTS.md).

## Limitations

- CMYK images are not compressed (not currently handled by `image` crate)

## Dependencies

**A Rust toolchain (1.85+ — the workspace uses edition 2024) is the only
build requirement.** The default CPU pipeline is pure Rust: Cargo compiles
every dependency from source, and no system C library is linked or needed.
The optional GPU backends load vendor libraries at runtime; building them
needs no CUDA toolkit. The `optimize` feature's codec crates compile from
source. No feature needs system packages beyond what its runtime loads:

| To use… | You need… |
|---|---|
| Default CPU build | nothing beyond Rust |
| `--features cuda` (nvJPEG) | an NVIDIA driver and `libnvjpeg` (loaded at runtime — no CUDA toolkit needed to *build*) |
| `--features cuda`, NVDEC stage | the above plus the Video Codec SDK: the driver's `libnvcuvid.so.1` **and** the SDK headers (`ffnvcodec-headers` on Debian/Ubuntu, `/usr/include/ffnvcodec/dynlink_nvcuvid.h`) |
| `--features rocm` | a ROCm installation with `rocjpeg` at runtime |
| `--features optimize` (JBIG2 / JPEG2000 / MRC / font subset / Zopfli / dedup) | nothing beyond Rust — the codec crates (`jbig2enc-rust`, `j2k`, `zopfli`, `subsetter`) compile from source; the build is heavy, not dependent |


The regression suite and the benchmark harness additionally want the usual
PDF tooling (never needed to build, and never needed just to compress a
PDF):

- **poppler-utils** (`pdftoppm`, `pdfinfo`) — the render witnesses and page
  counts used by `tests/regression.rs` and `benches/docker/pareto.py`
- **qpdf** — structural validation (`qpdf --check`) in the tests and in
  `ci/validate_corpus.py`
- **ghostscript** — a third renderer/validator and the `/ebook` benchmark
  opponent
- **mupdf-tools** (`mutool`) — a second renderer/oracle and the
  lossless-repack benchmark opponent
- **pngquant + tesseract-ocr-eng** — the OCRmyPDF leg of the Pareto sweep
  (ocrmypdf's `--optimize` mode hard-requires pngquant)
- **python3 + numpy + Pillow** — the `benches/docker/*.py` harness scripts
  (`pip install -r benches/docker/requirements.txt`)

Debian/Ubuntu one-liner for the full set (driver packages come from
NVIDIA's repository; the rest from distro packages):

```bash
sudo apt install qpdf poppler-utils ghostscript mupdf-tools \
                 pngquant tesseract-ocr-eng ffnvcodec-headers
```

Rust crates this project is built on:

- [lopdf](https://github.com/niclasberg/lopdf) — PDF parsing and manipulation
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [indicatif](https://github.com/console-rs/indicatif) — Progress bars
- [image](https://github.com/image-rs/image) — JPEG decoding and encoding
- [rayon](https://github.com/rayon-rs/rayon) — parallel image re-encoding

## Contributions
We are happy to welcome contributions! Pull requests are welcome.

## License
[GPL-3.0](LICENSE)


Made with ❤️ in Paris.