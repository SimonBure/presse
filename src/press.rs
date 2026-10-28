//! The single entry point: give it a file's bytes, get the file back
//! compressed in its own format.
//!
//! The format is detected from the content ([`crate::format::detect`]).
//! JPEG, PNG and WebP are compressed in their own format; GIF, TIFF and BMP
//! are rejected; anything else goes down the PDF pipeline, exactly as
//! `presse press` handled every input before images were supported.

use lopdf::Document;

use crate::format::{Format, detect};
use crate::img::{self, Outcome};
use crate::pdf::images::{CompressOptions, QualityMode, compress_images_opt};
use crate::transcode::{Acceleration, RuntimeTranscoder, resolve};

/// Passes that only apply to PDFs.
#[derive(Debug, Default, Clone, Copy)]
pub struct PdfOptions {
    /// `-d/--dpi`: cap placed images to this resolution.
    pub dpi: Option<u32>,
    /// `--raster-classify`.
    pub raster_classify: bool,
    /// `--recompress-flate`.
    pub recompress_flate: bool,
    /// `--dedup` (`optimize` feature).
    pub dedup: bool,
    /// `--zopfli` (`optimize` feature).
    pub zopfli: bool,
    /// `--font-subset` (`optimize` feature).
    pub font_subset: bool,
    /// `--jbig2` (`optimize` feature).
    pub jbig2: bool,
    /// `--jpeg2000` (`optimize` feature).
    pub jpeg2000: bool,
    /// `--mrc` (`optimize` feature).
    pub mrc: bool,
}

/// Options for one run, shared by every file.
#[derive(Debug, Clone, Copy)]
pub struct PressOptions {
    /// JPEG quality (PDF images and JPEG files; PNG and WebP stay lossless).
    pub quality: u8,
    /// `-s/--ssim` target; overrides `quality` when below 1.0.
    pub ssim: Option<f64>,
    /// `--palette`: indexed candidates for PDF images; lossy palettes for PNG.
    pub palette: bool,
    /// Keep all image metadata instead of only ICC profile + orientation.
    pub keep_metadata: bool,
    /// PDF-only passes.
    pub pdf: PdfOptions,
    /// Print per-image decisions to stderr.
    pub verbose: bool,
}

/// The result for one file.
pub enum Pressed {
    /// A processed PDF, to be written with
    /// [`crate::pdf::writer::compress_and_save_pdf`].
    Pdf(Box<Document>),
    /// A smaller image, in the input's format.
    Image(Vec<u8>),
    /// No smaller version was found: the input is the result.
    Unchanged(String),
    /// Deliberately not processed (a signed PDF): the input is the result,
    /// and the reason should be shown to the user.
    Untouched(String),
}

/// Compresses files. Build once per run: it resolves the transcoding
/// backend (and warns about a missing GPU) once, not per file.
pub struct Presser {
    options: PressOptions,
    transcoder: RuntimeTranscoder,
}

impl Presser {
    /// `acceleration` and `jpeg_encoder` select the JPEG backend, as for the
    /// PDF pipeline.
    pub fn new(
        options: PressOptions,
        acceleration: Acceleration,
        jpeg_encoder: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            options,
            transcoder: resolve(acceleration, jpeg_encoder)?,
        })
    }

    /// Compress one file given its full content.
    pub fn press(&self, input: &[u8]) -> Result<Pressed, Box<dyn std::error::Error>> {
        let o = &self.options;
        let image = |outcome: Outcome| match outcome {
            Outcome::Smaller { bytes, how } => {
                verbose!(o.verbose, "[image] kept candidate: {how}");
                Pressed::Image(bytes)
            }
            Outcome::Unchanged(reason) => Pressed::Unchanged(reason),
        };
        match detect(input) {
            Some(Format::Jpeg) => {
                let quality = QualityMode::press(o.quality, o.ssim).effective();
                img::jpeg::press(input, quality, o.keep_metadata, &self.transcoder, o.verbose)
                    .map(image)
            }
            Some(Format::Png) => {
                img::png::press(input, o.palette, o.keep_metadata, o.verbose).map(image)
            }
            Some(Format::WebP) => img::webp::press(input, o.keep_metadata, o.verbose).map(image),
            Some(f @ (Format::Gif | Format::Tiff | Format::Bmp)) => {
                Err(format!("{} compression is not supported yet", f.name()).into())
            }
            Some(Format::Pdf) | None => Ok(self.press_document(Document::load_mem(input)?)),
        }
    }

    /// Compress an already-parsed PDF, unless it is signed: rewriting a
    /// signed PDF, even losslessly, invalidates its signature, so it is
    /// left [`Pressed::Untouched`]. Lets a caller release the source bytes
    /// right after parsing (`lopdf` copies what it needs), which keeps peak
    /// memory down on large documents.
    pub fn press_document(&self, doc: Document) -> Pressed {
        if crate::pdf::signature::is_signed(&doc) {
            return Pressed::Untouched(
                "signed PDF: rewriting it would invalidate the signature".into(),
            );
        }
        Pressed::Pdf(Box::new(self.press_pdf(doc)))
    }

    /// Compress an already-parsed PDF, signed or not (see
    /// [`Presser::press_document`]).
    pub fn press_pdf(&self, mut doc: Document) -> Document {
        let o = &self.options;
        let p = &o.pdf;

        compress_images_opt(
            &mut doc,
            QualityMode::press(o.quality, o.ssim),
            o.verbose,
            &self.transcoder,
            CompressOptions {
                dpi: p.dpi,
                palette: o.palette,
                classify: p.raster_classify,
                jbig2: p.jbig2,
                jpeg2000: p.jpeg2000,
                mrc: p.mrc,
            },
        );

        // `--dedup` (optimize feature): extend the duplicate-image
        // coalescing to every byte-identical non-image stream.
        if p.dedup {
            let n = crate::pdf::optimize::dedup_streams(&mut doc);
            verbose!(
                o.verbose,
                "[dedup] coalesced {n} duplicate stream object(s)"
            );
        }

        // `--recompress-flate` (qpdf-style): re-encode existing
        // Flate streams at the writer's level before saving.
        if p.recompress_flate {
            let n = crate::pdf::writer::recompress_flate(&mut doc);
            verbose!(
                o.verbose,
                "[writer] recompressed {n} existing FlateDecode stream(s)"
            );
        }

        // `--zopfli` (optimize feature): the same recompression with
        // the deliberately slower Zopfli search (strictly-smaller
        // gate, pure size win at CPU cost).
        if p.zopfli {
            let n = crate::pdf::optimize::recompress_flate_zopfli(&mut doc);
            verbose!(o.verbose, "[zopfli] recompressed {n} FlateDecode stream(s)");
        }

        // `--font-subset` (optimize feature): subset embedded
        // TrueType fonts to the glyphs the content actually uses;
        // CFF (FontFile3) programs are skipped, and every font is
        // left untouched unless the used-glyph mapping resolves
        // exactly and the subset is strictly smaller.
        if p.font_subset {
            let n = crate::pdf::optimize::subset_fonts(&mut doc);
            verbose!(o.verbose, "[fonts] subset {n} font(s)");
        }

        doc
    }
}
