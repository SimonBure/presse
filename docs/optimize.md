# The `optimize` feature


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
  to the size court only above a 0.98 native-SSIM gate, so byte savings
  alone are never sufficient for admission. On the 18 MB image-heavy
  corpus
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
