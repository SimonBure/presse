use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use crate::format::Format;
use crate::transcode::Acceleration;

/// `convert --to` output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Pdf,
    #[value(alias = "jpg")]
    Jpeg,
    Png,
    /// Lossless WebP.
    Webp,
}

impl OutputFormat {
    pub fn format(self) -> Format {
        match self {
            OutputFormat::Pdf => Format::Pdf,
            OutputFormat::Jpeg => Format::Jpeg,
            OutputFormat::Png => Format::Png,
            OutputFormat::Webp => Format::WebP,
        }
    }
}

/// Compression effort preset (`--compression`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum CompressionMode {
    /// JPEG-only pipeline (the default): nothing beyond the standard passes.
    #[default]
    Fast,
    /// Plus `--dedup` (structural stream coalescing; no new codecs).
    Balanced,
    /// Plus `--dedup`, `--zopfli`, `--recompress-flate`.
    Small,
    /// Everything: the codec candidates (JBIG2/JPEG2000/MRC), font
    /// subsetting, dedup, Zopfli and Flate recompression.
    Smallest,
}

impl CompressionMode {
    // (Presets expand to the individual flags in `main`; the type itself
    // carries no behavior.)
}

/// Fast PDF and image compression tool - easier and faster than ghostscript
#[derive(Parser)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Compress PDF documents and JPEG/PNG/WebP images (format detected from content)
    Press {
        /// Input files: PDF, JPEG, PNG or WebP, mixed freely
        input: Vec<PathBuf>,

        /// Output file or directory (optional, defaults to
        /// <input>_compressed.<ext>, same format as the input)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Target quality for lossy image compression (images inside PDFs
        /// and JPEG files; PNG and WebP files stay lossless)
        #[arg(short, long, default_value_t = 80)]
        quality: u8,

        /// Image transcoding backend: cpu (default), auto, cuda, or rocm.
        /// The gpu backends require a build with --features cuda / rocm.
        #[arg(short, long, value_enum, default_value_t = Acceleration::Cpu)]
        acceleration: Acceleration,

        /// Cap the effective resolution of placed images to DPI pixels per
        /// inch. Omitted: images keep their source resolution.
        #[arg(
            short = 'd',
            long,
            value_name = "DPI",
            value_parser = clap::value_parser!(u32).range(1..),
            long_help = "Cap the effective resolution of placed images to DPI pixels per inch.\n\nAn image drawn at w×h points is downsampled to at most w·DPI/72 × h·DPI/72\npixels (the same placement-aware rule Ghostscript's -dPDFSETTINGS uses). The\ncap is strict: it never up-samples, and images whose on-page placement\ncannot be determined keep their source resolution.\n\nPresets:\n  75   screen     – low resolution\n  150  ebook      – medium resolution\n  300  printer    – high quality\n  600  prepress   – high quality, color preserving\n\nDefault (flag omitted): no downsampling, current behavior.",
        )]
        dpi: Option<u32>,

        /// Target output fidelity as measured SSIM (0–1). Lower = smaller
        /// and faster encodes. Default 1.0: use -q as given.
        #[arg(
            short = 's',
            long,
            value_name = "SSIM",
            value_parser = |s: &str| -> Result<f64, String> {
                let v: f64 = s.parse().map_err(|_| "not a number".to_string())?;
                if (0.01..=1.0).contains(&v) {
                    Ok(v)
                } else {
                    Err("must be between 0.01 and 1.0".to_string())
                }
            },
            long_help = "Target output fidelity, as the SSIM of each re-encoded image vs its\nsource, measured on a 512-px analysis window. The mapping is calibrated on\ngrainy scans — the worst case for JPEG, where artifacts are most visible\n— so smoother content (photos, paper figures) always *exceeds* the\ntarget. Lower targets give smaller and faster encodes.\n\n  -ssim 1.0   – default: use -q as given (no calibration)\n  -ssim 0.86  – ~q9 on the calibration content (aggressive size cut)\n  -ssim 0.72  – ~q6 (very aggressive)\n\nAchieved SSIM varies with content and is reported by the benchmark\n(QUALITY.md \"SSIM targets\").",
        )]
        ssim: Option<f64>,

        /// Also try an indexed-color (/Indexed) palette representation for
        /// eligible flat-color images and keep the smallest candidate.
        #[arg(
            long,
            default_value_t = false,
            long_help = "For each eligible flat-color image (plain 8-bit DeviceRGB raster with
no mask or custom decode table), also build an indexed-color (/Indexed +
/FlateDecode) candidate alongside the JPEG one, and keep the smallest of
original / JPEG / indexed.

Palettes win where JPEG cannot: figures, charts, diagrams, screenshots
and scans have little chromatic entropy, so one index byte per pixel plus
a <=256-entry table Flate-compresses far below a JPEG. Images with at
most 256 unique colors are converted losslessly; larger rasters are
median-cut and accepted only above a 0.9999 native-image SSIM gate, so a
lossy palette can never visibly degrade the figure. Photos effectively
never qualify and keep the JPEG path.

Default (flag omitted): JPEG-only pipeline, current behavior.

PNG files: exact palettes (<=256 colors) are always tried because they are
lossless. This flag also allows a lossy median-cut palette for opaque PNGs
with more colors, under the same 0.9999 SSIM gate."
        )]
        palette: bool,

        /// Use the native Rust `jpeg-encoder` codec (YCbCr 4:2:0, box-
        /// averaged chroma) instead of the `image` crate's 4:4:4 encoder.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Use the pure-Rust `jpeg-encoder` codec on the CPU path instead of the
`image` crate's encoder.

The `image` encoder writes full-resolution Cb/Cr (effectively 4:4:4); the
`jpeg-encoder` codec writes YCbCr 4:2:0 with box-averaged chroma
downsampling — exactly libjpeg/libjpeg-turbo's default RGB pipeline
(jpeg_set_defaults + h2v2_downsample), which is the model Ghostscript and
qpdf use. That is half the DCT chroma blocks of 4:4:4, so RGB output at
the same -q is materially smaller (on the irs_fw2 scan corpus: 2.00 ->
~1.5 MB at q30, matching qpdf). Chroma loss is invisible to a luminance
SSIM witness, so this is opt-in rather than the default; the benchmark's
native-image witness reports per-channel SSIM to keep it honest.

Grayscale images stay single-component on both paths. The flag applies to
the CPU path and to GPU fallback. Default (flag omitted): the `image`
encoder, current behavior."
        )]
        jpeg_encoder: bool,

        /// Recompress existing FlateDecode streams at the writer's level.
        #[arg(
            long,
            default_value_t = false,
            long_help = "qpdf-style structural recompression: decode every existing
/FlateDecode stream and re-encode it at the writer's compression level (9),
keeping it only when smaller.

Form tools usually store streams at a lower level than the writer uses, so
this recovers the difference (on the irs_fw2 scan corpus: 1.81 -> 1.34 MB,
the same reduction qpdf's --recompress-flate achieves — qpdf's default
writer leaves already-Flate streams alone). Pure-Flate streams only;
DCT/LZW/multi-filter chains, streams
with /DecodeParms and anything that fails to decompress are left alone.
Lossless: decoding and re-encoding Flate changes no pixel or content
bytes, only their storage size.

Default (flag omitted): existing Flate streams are kept as-is, current
behavior."
        )]
        recompress_flate: bool,

        /// Classify rasters and store bitonal text as 1-bit CCITT G4 grayscale images.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Run the raster classifier on every decoded image: text/rules/line-art
content stored as a photographic RGB raster is detected (adaptive Otsu
threshold + connected-component density + color statistics) and re-stored
as a 1-bit CCITT Group 4 opaque DeviceGray image — the representation a
document compressor should use for text, instead of paying photographic
cost for it. (Deliberately not an /ImageMask stencil: a stencil's white
is transparent and its ink inherits the current graphics color, so it is
not a substitute for an opaque raster.) Flat-color figures are routed to
the /Indexed palette candidate. Photos and mixed pages are never masked.

The G4 encoding of the 1-bit payload is lossless; the RGB-to-bitonal
conversion itself is lossy (bitonal content is black-on-white by
definition, so this is the point of the flag), which is why the flag is
opt-in rather than the default and only fires on near-perfect
black-and-white content; on real scans the size win is large (an RGB
page -> a few KB of G4). Only the smallest candidate (original / JPEG /
indexed / mask) is kept per image.

Default (flag omitted): JPEG-only pipeline, current behavior."
        )]
        raster_classify: bool,

        /// Coalesce byte-identical non-image streams (fonts, ICC, XForms).
        #[arg(
            long,
            default_value_t = false,
            long_help = "Extend the duplicate-image coalescing to every stream in the
  document: FontFile streams, ICC profiles, XForms, patterns/shadings and
  arbitrary identical streams with byte-identical payloads and dictionaries
  collapse onto one canonical object with every reference rewritten.
  Rendering cannot change (identical stream + identical dictionary renders
  identically). Safe, deterministic, idempotent; requires a build with the
  `optimize` feature."
        )]
        dedup: bool,

        /// Recompress Flate streams with the Zopfli algorithm.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Like --recompress-flate, but the re-encode uses Zopfli — a
  deliberately CPU-hungry deflate search that routinely finds smaller zlib
  streams for the same decoded bytes (a few percent on text/content
  streams). Each stream is kept only when strictly smaller, so it is a
  pure size win at CPU cost; combine with `--compression smallest`.
  Requires a build with the `optimize` feature."
        )]
        zopfli: bool,

        /// Subset embedded TrueType fonts to the glyphs actually used.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Subset every embedded TrueType font to the glyphs the content
  streams actually use (typst's `subsetter`): text-showing operators are
  scanned per font, used glyphs are kept, and the font is rewritten as a
  CID font with an identity GID-to-CID map so the content strings are
  remapped in place. A font is skipped whenever the used-glyph mapping
  cannot be resolved exactly (unusual encodings, Type1/CID fonts) or the
  program is CFF (FontFile3 — CFF subsetting is not implemented; only
  TrueType FontFile2 programs are subset), and the
  subset is kept only when strictly smaller. Text extraction stays valid
  via a rebuilt ToUnicode map. Requires a build with the `optimize`
  feature."
        )]
        font_subset: bool,

        /// Also try a lossless JBIG2 encoding of bitonal masks.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Add a JBIG2 candidate for bitonal content next to the CCITT G4 one
  (rust jbig2enc-rust, lossless symbol/generic-region coding); the size
  court keeps the smaller of G4 / JBIG2 / original per image. Requires a
  build with the `optimize` feature."
        )]
        jbig2: bool,

        /// Also try a JPEG2000 re-encoding of continuous-tone images.
        #[arg(
            long,
            default_value_t = false,
            long_help = "Add a JPEG2000 (JPXDecode) candidate for continuous-tone images
  (pure-Rust j2k codec, rate-targeted lossy encode at 85% of the JPEG
  candidate's bit budget). The rate target is only a sizing hint: every
  candidate is decoded back and measured against the source pixels on the
  native 512-px window, and admitted to the size court only above a 0.98
  native-SSIM gate (candidate evidence: luma/chroma/edge error + SSIM).
  Requires a build with the `optimize` feature."
        )]
        jpeg2000: bool,

        /// Mixed-raster-content composite for classified bitonal scans.
        #[arg(
            long,
            default_value_t = false,
            long_help = "For classified bitonal scans, build a flat two-tone MRC composite
  (solid paper-color background + solid ink-color foreground + a
  high-resolution lossless CCITT G4 mask as its /SMask) instead of a flat
  1-bit image: the background is the median paper color emitted as a 1×1
  image, the foreground is composited through the mask — the
  representation commercial scan compressors (ABBYY/LEADTOOLS/Pdftools)
  use, minus their textured-background layer (future work). The page
  content is rewritten to draw background then foreground, and the
  candidate is kept only when strictly smaller than the source and the
  JPEG candidate. (The mask is always G4: poppler's JBIG2 decoder
  inverts its samples, which would make the SMask polarity
  viewer-dependent.) Requires a build with the `optimize` feature."
        )]
        mrc: bool,

        /// Compression effort preset.
        #[arg(
            long,
            value_enum,
            default_value_t = CompressionMode::Fast,
            long_help = "Preset that enables the default-off optimization flags together:
  fast (default) — the JPEG-only pipeline;
  balanced — plus --dedup (no new codecs);
  small — plus --zopfli and --recompress-flate;
  smallest — plus --jbig2, --jpeg2000, --font-subset, --mrc and every
  structural pass. Individual flags still work on their own. Modes other
  than `fast` require a build with the `optimize` feature."
        )]
        compression: CompressionMode,

        /// Keep all image metadata in JPEG/PNG/WebP files (EXIF, XMP,
        /// comments, text chunks). Default: keep only the ICC color profile and the
        /// EXIF orientation.
        #[arg(long, default_value_t = false)]
        keep_metadata: bool,

        // Details during the compression process --> sizes comparison before & after
        #[arg(short, long, default_value_t = false)]
        verbose: bool,
    },

    Merge {
        /// Input files (>= 2, order matters)
        input: Vec<PathBuf>,

        /// Output file (optional, defaults to <input>_merged.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Compress the merged file
        #[arg(short, long, default_value_t = false)]
        compress: bool,
    },

    /// Convert images to PDF (default), JPEG, PNG or WebP
    Convert {
        /// Input image files
        input: Vec<PathBuf>,

        /// Output file or directory (optional). A file's extension picks the
        /// output format: .pdf, .jpg, .png or .webp
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Output format when -o gives no extension (default: pdf). WebP
        /// output is lossless
        #[arg(short, long, value_enum)]
        to: Option<OutputFormat>,

        /// Merge all images into a single PDF
        #[arg(short, long, default_value_t = false)]
        merge: bool,

        /// JPEG-compress the images embedded in the PDF output
        #[arg(short, long, default_value_t = false)]
        compress: bool,

        /// JPEG quality (1-100): for --compress, and for JPEG output
        #[arg(short, long, default_value_t = 80)]
        quality: u8,

        /// Verbose output
        #[arg(short, long, default_value_t = false)]
        verbose: bool,
    },
}

pub fn resolve_press_path_output(file_path: &Path, output: &Option<PathBuf>) -> PathBuf {
    resolve_press_output(file_path, output, "pdf")
}

/// Like [`resolve_press_path_output`], for an output with the given
/// extension (images keep their own format).
pub fn resolve_press_output(
    file_path: &Path,
    output: &Option<PathBuf>,
    extension: &str,
) -> PathBuf {
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}_compressed.{}", stem, extension))
        }
        Some(path) => path.clone(),
        None => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            let mut path = file_path.to_path_buf();
            path.set_file_name(format!("{}_compressed.{}", stem, extension));
            path
        }
    }
}

pub fn resolve_convert_path_output(file_path: &Path, output: &Option<PathBuf>) -> PathBuf {
    resolve_convert_output(file_path, output, "pdf")
}

/// Like [`resolve_convert_path_output`], for an output with the given
/// extension.
pub fn resolve_convert_output(
    file_path: &Path,
    output: &Option<PathBuf>,
    extension: &str,
) -> PathBuf {
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}.{}", stem, extension))
        }
        Some(path) => path.clone(),
        None => file_path.with_extension(extension),
    }
}

/// The `convert` output format: the `-o` file extension, else `--to`, else
/// PDF. An unknown extension, or one that disagrees with `--to`, is an error.
pub fn resolve_convert_format(
    output: &Option<PathBuf>,
    to: Option<OutputFormat>,
) -> Result<Format, String> {
    let from_extension = match output {
        Some(path) if !(path.is_dir() || path.to_str().unwrap_or("").ends_with('/')) => {
            match path.extension().and_then(|e| e.to_str()) {
                None => None,
                Some(ext) => Some(match ext.to_ascii_lowercase().as_str() {
                    "pdf" => Format::Pdf,
                    "jpg" | "jpeg" | "jpe" | "jfif" => Format::Jpeg,
                    "png" => Format::Png,
                    "webp" => Format::WebP,
                    _ => {
                        return Err(format!(
                            "unsupported output extension .{ext} (use .pdf, .jpg, .png or .webp)"
                        ));
                    }
                }),
            }
        }
        _ => None,
    };
    match (from_extension, to.map(OutputFormat::format)) {
        (Some(a), Some(b)) if a != b => Err(format!(
            "-o asks for {} but --to asks for {}",
            a.name(),
            b.name()
        )),
        (Some(a), _) | (None, Some(a)) => Ok(a),
        (None, None) => Ok(Format::Pdf),
    }
}

pub fn resolve_merge_path_output(output: &Option<PathBuf>, compress: bool) -> PathBuf {
    let default_name = if compress {
        "compressed_merged.pdf"
    } else {
        "merged.pdf"
    };
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            path.join(default_name)
        }
        Some(path) => path.clone(),
        None => PathBuf::from(default_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(p: &str) -> Option<PathBuf> {
        Some(PathBuf::from(p))
    }

    #[test]
    fn convert_format_rules() {
        assert_eq!(resolve_convert_format(&None, None), Ok(Format::Pdf));
        assert_eq!(
            resolve_convert_format(&out("a.WEBP"), None),
            Ok(Format::WebP)
        );
        assert_eq!(
            resolve_convert_format(&out("a.jpeg"), None),
            Ok(Format::Jpeg)
        );
        assert_eq!(
            resolve_convert_format(&out("dir/"), Some(OutputFormat::Png)),
            Ok(Format::Png)
        );
        assert_eq!(
            resolve_convert_format(&out("noext"), Some(OutputFormat::Jpeg)),
            Ok(Format::Jpeg)
        );
        assert_eq!(
            resolve_convert_format(&out("a.png"), Some(OutputFormat::Png)),
            Ok(Format::Png)
        );
        assert!(resolve_convert_format(&out("a.png"), Some(OutputFormat::Webp)).is_err());
        assert!(resolve_convert_format(&out("a.tiff"), None).is_err());
    }

    #[test]
    fn convert_output_names() {
        let p = Path::new("photos/cat.jpg");
        assert_eq!(
            resolve_convert_output(p, &None, "webp"),
            PathBuf::from("photos/cat.webp")
        );
        assert_eq!(
            resolve_convert_output(p, &out("out/"), "png"),
            PathBuf::from("out/cat.png")
        );
        assert_eq!(
            resolve_convert_path_output(p, &None),
            PathBuf::from("photos/cat.pdf")
        );
    }
}
