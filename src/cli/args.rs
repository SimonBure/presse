use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Compress one or several PDF documents
    Press {
        /// Input file
        input: Vec<PathBuf>,

        /// Output file (optional, defaults to <input>_compressed.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Target quality for lossy image compression
        #[arg(short, long, default_value_t = 80)]
        quality: u8,

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

Default (flag omitted): JPEG-only pipeline, current behavior."
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

    /// Convert one or several images to PDF
    Convert {
        /// Input image files
        input: Vec<PathBuf>,

        /// Output file or directory (optional)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Merge all images into a single PDF
        #[arg(short, long, default_value_t = false)]
        merge: bool,

        /// JPEG-compress the embedded images
        #[arg(short, long, default_value_t = false)]
        compress: bool,

        /// Target quality for --compress (1-100)
        #[arg(short, long, default_value_t = 80)]
        quality: u8,

        /// Verbose output
        #[arg(short, long, default_value_t = false)]
        verbose: bool,
    },
}

pub fn resolve_press_path_output(file_path: &Path, output: &Option<PathBuf>) -> PathBuf {
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}_compressed.pdf", stem))
        }
        Some(path) => path.clone(),
        None => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            let mut path = file_path.to_path_buf();
            path.set_file_name(format!("{}_compressed.pdf", stem));
            path
        }
    }
}

pub fn resolve_convert_path_output(file_path: &Path, output: &Option<PathBuf>) -> PathBuf {
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}.pdf", stem))
        }
        Some(path) => path.clone(),
        None => file_path.with_extension("pdf"),
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
