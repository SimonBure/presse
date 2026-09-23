# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **`--jpeg-encoder`** (`press --jpeg-encoder`, off by default) — swap the
  CPU JPEG encoder from the `image` crate's 4:4:4 to the pure-Rust
  `jpeg-encoder` codec at YCbCr 4:2:0 with box-averaged chroma
  (libjpeg's default model, AVX2 `simd`): smaller RGB output at the same
  `-q` (−8% on the photo corpus at q50) and faster encodes. Grayscale
  stays single-component. Chroma-aware per-channel SSIM added to the
  native-image benchmark witness so the tradeoff is measured.
- **Duplicate-image coalescing** — image streams that are semantically
  identical (same dictionary, same payload; `/Length` and the cosmetic
  `/Name` hint ignored, indirect `/ColorSpace`/`/SMask` references
  followed) collapse onto one canonical object with every reference
  rewritten. Documents embedding the same image many times shrink to the
  unique content once (photos60 q30: 22.96 → 6.46 MB, below MuPDF's dedup
  result, rendering unchanged).
- **`--palette`** (`press --palette`, off by default) — an `/Indexed`
  color-space candidate for eligible flat-color images (figures, charts,
  scans): exact palette when ≤256 unique colors (lossless), deterministic
  median-cut above that, accepted only above a 0.9999 native-image SSIM
  gate; the smallest of original / JPEG / indexed wins per image.
- **`--raster-classify`** (`press --raster-classify`, off by default) — a
  raster classifier (adaptive Otsu threshold, connected-component density,
  color statistics on a bounded ≤1024-px sample window) routes bitonal
  text/rules to a 1-bit CCITT Group 4 opaque `DeviceGray` image — not an
  `/ImageMask` stencil, whose transparent white and current-color ink
  would change rendering (an RGB text page → a few KB of G4). The G4
  encoding is lossless; the RGB→bitonal conversion itself is lossy, which
  is why only near-perfect black-and-white content is masked
  and flat-color figures to the `/Indexed` candidate; photos and mixed
  pages stay on the JPEG path. Conservative by design: only mostly
  black-and-white rasters with glyph-sized components are masked. The
  smallest of original / JPEG / indexed / mask wins per image.
- **`optimize` feature + `--compression` presets** — a new Cargo feature
  (`--features optimize`) backs the default-off heavyweight passes:
  `--compression {fast,balanced,small,smallest}` presets that expand to
  `--dedup`, `--zopfli`, `--font-subset`, `--jbig2`, `--jpeg2000` and
  `--mrc`. Every pass follows the image pipeline's discipline — applied
  only when strictly smaller *and* within its measured fidelity budget
  (lossless structural rewrites by construction; lossy candidates gated
  on per-image measurement — palette ≥0.9999 native SSIM, bitonal masks
  on the classifier's reconstruction-error gate), a no-op whenever a
  conservative gate rejects it. The flags fail with an explicit build hint
  when the feature is absent.
- **`--dedup`** (`optimize` feature, off by default) — the duplicate-image
  coalescer extended to every stream: byte-identical FontFile programs,
  ICC profiles, XForms, patterns and arbitrary streams collapse onto one
  canonical object with every reference rewritten (identical stream +
  identical dictionary renders identically, so lossless by construction).
- **`--zopfli`** (`optimize` feature, off by default) — the
  `--recompress-flate` pass with the Zopfli deflate search instead of
  level-9 zlib: a few percent smaller on text/content streams, kept only
  when strictly smaller.
- **`--font-subset`** (`optimize` feature, off by default) — embedded
  TrueType fonts are subset to the glyphs the content streams actually
  show (typst's `subsetter`) and rewritten as CID fonts (`/Identity-H` +
  identity `CIDToGIDMap`) with the text strings remapped in place and a
  rebuilt `/ToUnicode`; fonts whose used-glyph mapping cannot be resolved
  exactly are skipped, CFF (`FontFile3`) programs are not subset, and the
  subset is kept only when strictly smaller
  (194 KB TrueType font-heavy PDF → ~7 KB, pixel-identical rendering,
  text extraction intact).
- **`--jbig2`** (`optimize` feature, off by default) — a lossless JBIG2
  candidate (Rust `jbig2enc-rust`, symbol-dictionary mode) for the same
  1-bit masks the G4 candidate uses; repeated glyph shapes share one
  dictionary entry. The size court keeps the smaller of G4 / JBIG2 /
  original. The encoder's output is oracled against
  poppler/ghostscript/mutool (identity `/Decode` polarity, verified
  pixel-identically).
- **`--jpeg2000`** (`optimize` feature, off by default) — a JPEG2000
  candidate (pure-Rust `j2k` codec) for continuous-tone images, emitted as
  a minimal JP2 file (signature / file-type / image-header + sRGB /
  codestream boxes) so poppler decodes it cleanly, rate-targeted at 85% of
  the JPEG candidate's bytes. The rate target is only a sizing hint: every
  candidate is decoded back and measured against the source pixels on the
  native 512-px window (`CandidateEvidence` — SSIM plus luma/chroma/edge
  error), and admitted to the size court only above a 0.98 native-SSIM
  gate — the first implementation of the generic runtime fidelity court
  every future lossy representation is expected to fill.
- **`--mrc`** (`optimize` feature, off by default) — **flat two-tone
  mixed-raster content**: the composite commercial scan compressors use,
  minus the textured-background layer. A solid paper-color background (the
  median paper color, emitted as a 1×1 image — never a JPEG, since
  near-flat JPEG bitstreams are mis-decoded as full-page gradients by
  poppler and Ghostscript), solid ink-color foreground composited through
  a high-resolution lossless CCITT G4 mask as its
  `/SMask` (a full image XObject — Ghostscript silently drops a soft mask
  without `/Type /XObject` + `/Subtype /Image`), with the content streams
  rewritten to draw background then foreground *without* re-applying the
  placement `cm` (the source image's transform is already current there;
  re-emitting it squares the scale and made poppler's soft-mask allocator
  overflow — the "Bogus memory allocation size" notice, now fixed and
  pixel-verified identical across poppler/mutool/ghostscript). This is the
  intended home for mask compositing (the
  graphics state is under presse's control, so the composite cannot leak
  background or recolor ink — regression-tested pixel-identically under a
  blue rectangle with a red current color). The mask is always G4:
  poppler's JBIG2 decoder inverts its samples, which would make the mask
  polarity viewer-dependent.

- **`--recompress-flate`** (`press --recompress-flate`, off by default) —
  qpdf-style structural recompression: existing `/FlateDecode` streams are
  decoded and re-encoded at the writer's level 9, each kept only when
  smaller. Lossless (no content-byte changes); recovers the level-6-vs-9
  gap form tools leave behind (irs_fw2 corpus: 1.81 → 1.34 MB — the same
  reduction qpdf's `--recompress-flate` achieves; qpdf's default writer
  leaves already-Flate streams alone).
- **Flate-wrapped JPEG** — retained or re-encoded DCT streams that shrink
  under zlib are stored as `[FlateDecode, DCTDecode]` when the full flate
  result is smaller (OCRmyPDF-style trick).
- **NVDEC hardware decode stage** (`--acceleration cuda`) — baseline 4:2:0
  JPEGs decode on the NVDEC hardware engine through the Video Codec SDK
  (`libnvcuvid.so.1`, a driver component; parser-free decode mirroring
  ffmpeg's `nvdec_mjpeg.c`) instead of nvJPEG's entropy-decode kernels;
  the NV12 output is de-interleaved to planar YUV by a tiny embedded-PTX
  kernel the driver JIT-compiles. Progressive / 4:2:2 / 4:4:4 JPEGs keep
  the nvJPEG decode. Optional: any init failure degrades to nvJPEG, and
  `PRESSE_NO_NVDEC=1` forces that path. Verified pixel-equivalent vs
  nvJPEG, with only ≤1-LSB IDCT-rounding differences in the current
  hardware witness (mean |Δ| 0.0014; 0.14% of pixels differ), by
  `examples/nvdec_verify.rs`.
- **Pluggable GPU transcoders** — `--acceleration cuda|rocm` behind a
  unified `ImageTranscoder` trait with graceful per-stream CPU fallback
  (opt-in Cargo features; not linked into default builds).
- **Linear-time object renumbering** — lopdf's O(n²) `renumber_objects`
  replaced with a hash-map pass (67k-object doc: 530 → 62 ms); bounded
  dedup-cache hash (length + first/last 4 KiB).
- **Parallel image re-encoding** — image streams are detached from the
  `Document`, re-encoded concurrently with rayon on owned buffers, and
  written back in a single serial pass (document-lock-free; only the dedup
  cache is shared).
- **`--dpi` resolution cap** (`press -d <dpi>`) — placement-aware image
  downsampling with Ghostscript-style presets (75 screen, 150 ebook,
  300 printer, 600 prepress). Strict cap: never up-samples; images whose
  placement cannot be determined keep source resolution.
- **`--ssim` fidelity target** (`press -s <target>`) — a quality knob
  calibrated to measured SSIM (native 512-px window, worst-case grainy
  scans) instead of an arbitrary quality number. `-s 0.86` ≈ q9 and
  `-s 0.72` ≈ q6 on the calibration content; smoother content always
  exceeds the target. Lower targets compress harder and encode faster
  (photos20: 11.0 → 4.4 MB at `-s 0.86`, same-or-faster wall time).
- **Regression suite** — tests covering structural integrity (qpdf /
  ghostscript / `/Length` gates), visual SSIM, dpi-capping invariants,
  cross-reference reachability, grayscale component preservation,
  serial/parallel determinism, gapped numbering, the stock flag surface
  (`--jpeg-encoder`, `--ssim`, `--dpi`, `--raster-classify`,
  `--recompress-flate`), and GPU fallback.
- **Benchmark + quality harness** — containerized 100-PDF benchmark,
  quality-vs-speed analysis (including MuPDF and the non-compressing Rust
  tools), a dpi sweep, and a multi-tool Pareto-frontier harness.
- `mimalloc` on the musl release artifact; `zlib-rs` deflate backend;
  memory-mapped input reads; deduplicated image re-encodes.

### Fixed

- **Cross-reference correctness** — objects are renumbered before saving,
  closing lopdf's gapped-numbering xref truncation (blank pages on large
  real-world PDFs; upstream tracked at J-F-Liu/lopdf#558).
- **Grayscale JPEG corruption** — `/DeviceGray` streams keep a
  single-component JPEG instead of a 3-component one that rendered as
  washed-out garbage.
- **Transcode cache** — keyed by quality, input kind, dimensions, and
  exact pixel bytes (equality on bytes, never a hash alone); a per-entry
  `OnceLock` guarantees one encode per unique image even under contention.

## [0.3.2] - previous release

Structural compression, image recompression, merging, conversion (see
`git log` for details).
