# Images — JPEG, PNG and WebP


`presse press` detects each input from its first bytes: a JPEG, PNG or
WebP signature means an image, anything else goes to the PDF pipeline (as
before). Images come back in their own format, named
`<input>_compressed.<ext>`. For each image, presse builds candidates and
keeps the smallest; when none is smaller than the input, the input is
copied unchanged.

- **JPEG** — two candidates: the original with metadata removed
  (lossless, the compressed data is copied as is), and a re-encode at
  `-q` (or `-s`) through the same encoder the PDF pipeline uses
  (`--jpeg-encoder` and `--acceleration` apply). CMYK JPEGs are only
  stripped, never re-encoded.
- **PNG** — lossless by default: metadata removed; 16-bit samples that
  are exact 8-bit values, all-opaque alpha and gray stored as RGB are
  reduced; images with at most 256 colors become palette images (1/2/4/8
  bits per pixel); everything is deflated at level 9. `--palette` also
  allows a lossy palette for opaque images with more colors, under the
  same 0.9999 SSIM gate as in PDFs. Animated PNGs are only stripped.
- **WebP** — lossless only (presse has no lossy WebP encoder, so `-q`
  does not apply): metadata removed, the image data copied as is; lossless
  still images are also re-encoded with `image-webp` and the smaller file
  wins. That encoder has fewer coding tools than libwebp, so the re-encode
  only beats files from fast encoders (libwebp method 0); files from
  libwebp's default settings are only stripped. Lossy and animated WebPs
  are only stripped.
- **Metadata** — kept: ICC profile, EXIF orientation (rewritten as a
  minimal EXIF block), and PNG color/pixel-size chunks. Removed: the rest
  of EXIF (camera, GPS, thumbnail), XMP, comments, Photoshop blocks, PNG
  text and time chunks, unknown WebP chunks. `--keep-metadata` keeps
  everything.
- **Not yet** — GIF, TIFF and BMP files are skipped with a message.
  Flags that only apply to PDFs (`-d`, `--raster-classify`,
  `--recompress-flate`, the `optimize` passes, `--compression`) are
  ignored for images, with one warning per run, so a mixed batch never
  fails because of them.

Measured at the defaults on 4 JPEG photos and 9 PNGs (scikit-image sample
data and the `webp` crate's test photo): JPEGs −37% to −62% (DSSIM
0.0005–0.0017); PNGs pixel-identical, −2% to −43% on six files and
unchanged on three that were already optimal. A poorly deflated synthetic
PNG (the CI fixture) shrinks 98%.

WebP, measured on 48 files made with libwebp from 12 of those images
(lossless at methods 0, 4 and 6, lossy at q80), no metadata:
pixel-identical; the four method-0 files that a re-encode beats shrink
14% to 66%, the 44 others are copied unchanged. The same files carrying
an ICC profile, camera EXIF and a 3.3 kB XMP packet shrink by the
metadata (−3.4 kB on a 14 kB lossy photo), animated ones included.

## Converting images — `presse convert`

Image output uses the same encoders as `presse press`: JPEG through the
PDF pipeline's encoder, PNG through the lossless optimizer (palettes,
reductions, level-9 deflate), WebP lossless (presse has no lossy WebP
encoder). The ICC profile and EXIF orientation follow the image when the
output can hold them (a profile is dropped when it doesn't match the
output's color model, e.g. a CMYK profile on RGB pixels); other metadata
is not carried. Transparent areas become white in JPEG. Animated inputs
keep their first frame. When the result is larger than the input (a photo
JPEG as lossless WebP or PNG usually is), it is written anyway with a
warning. `-o` with an unknown extension, or one that disagrees with
`--to`, is an error.

Output names follow the `press` rules (missing `-o` directories are
created; a clash with an input or an earlier output gets `_1`, `_2`…),
except that an output never replaces even its own input: `convert a.png
a.bmp` writes `a.pdf` and `a_1.pdf`; `convert a.png a.webp --to webp`
keeps `a.webp` and writes `a_1.webp` and `a_2.webp`.
