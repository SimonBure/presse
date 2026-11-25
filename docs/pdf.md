# PDF compression in depth

Every `presse press` flag, then the PDF-specific passes in detail. The
README covers everyday use; this page is the reference.

## How presse compresses a PDF

- **Image recompression** — re-encodes images at a target quality, skipping CMYK images
- **Structural compression** — object stream packing, xref stream compression
- **Engine optimizations** — rayon-parallel re-encoding, mimalloc allocator, zlib-rs deflate backend, memory-mapped reads, deduplicated identical image streams

## All `press` flags

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>_compressed.<ext>` | Output file or directory (same format as the input) |
| `-q, --quality` | `80` | Image recompression quality (0–100), for images inside PDFs and for JPEG files; PNG and WebP files stay lossless |
| `-d, --dpi` | source resolution | Cap placed image resolution to DPI pixels/inch (75 screen, 150 ebook, 300 printer, 600 prepress); omitted = keep source resolution. PDFs only: image files ignore it, with one warning per run |
| `-s, --ssim` | `1.0` | Target output fidelity as measured SSIM (calibrated on grainy scans — the worst case for JPEG — so smoother content exceeds the target); lower = smaller and faster. `1.0` = use `-q` as given |
| `--palette` | `false` | Also build an `/Indexed` palette candidate for eligible flat-color images (figures, charts, scans) and keep the smallest of original / JPEG / indexed; exact palettes are lossless, lossy ones are gated on 0.9999 native SSIM. For PNG files: allows the lossy palette (exact palettes are always tried) |
| `--keep-metadata` | `false` | JPEG/PNG/WebP files: keep all metadata. Default keeps only the ICC color profile and the EXIF orientation |
| `--jpeg-encoder` | `false` | Use the pure-Rust `jpeg-encoder` codec (YCbCr 4:2:0, box-averaged chroma — libjpeg's default model, which Ghostscript/qpdf use) instead of the `image` crate's 4:4:4 encoder; smaller RGB output at the same `-q`, faster, but opt-in because luminance-SSIM courts don't see chroma loss |
| `--raster-classify` | `false` | Run the raster classifier on every decoded image: bitonal text/rules stored as a photographic RGB raster are re-stored as a 1-bit CCITT G4 opaque `DeviceGray` image (an RGB page → a few KB of G4). The G4 encoding of the 1-bit payload is lossless; the RGB→bitonal conversion itself is lossy, which is why only near-perfect black-and-white content is masked. Flat-color figures get the `/Indexed` palette candidate; photos and mixed pages are never masked. The smallest of original / JPEG / indexed / mask wins per image |
| `--recompress-flate` | `false` | qpdf-style structural recompression: decode existing `/FlateDecode` streams and re-encode them at the writer's level 9, keeping each only when smaller. Lossless (no content-byte changes); recovers the compression-level gap form tools leave behind |
| `-v, --verbose` | `false` | Print size comparison after each file |

More flags: `-a, --acceleration` (GPU JPEG backends, see [gpu.md](gpu.md)) and
the `optimize` feature's `--compression`, `--dedup`, `--zopfli`,
`--font-subset`, `--jbig2`, `--jpeg2000`, `--mrc` (see [optimize.md](optimize.md)).
`presse press --help` prints them all.

## Resolution capping (`--dpi`)

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

## Chroma subsampling (`--jpeg-encoder`)

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

## Duplicate-image collapsing and palette quantization

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

## Raster classification (`--raster-classify`)

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

## Structural recompression (`--recompress-flate`)

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

## Output names

Missing `-o` directories are created. A run never writes
over another of its inputs or over a file it already wrote: the clashing
output gets `_1`, `_2`… before its extension, with a warning
(`presse press d1/a.pdf d2/a.pdf -o out/` writes `a_compressed.pdf` and
`a_compressed_1.pdf`). `-o` naming the input itself compresses it in
place. Other existing files are overwritten as before, so running the
same command twice gives the same file names.

## Signed PDFs

A digital signature covers the file's exact bytes, so
any rewrite, even a lossless one, breaks it. `press` detects signatures
(approval, certification, document timestamps, usage rights) and copies
those files unchanged, with a message. Empty signature fields do not count.
