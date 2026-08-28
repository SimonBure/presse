//! Image re-encoding pipeline.
//!
//! **Design rationale, in one place:**
//!
//! 1. **Three phases** — (1) detach every eligible image stream from the
//!    `Document` into owned buffers; (2) rayon re-encodes them in parallel;
//!    (3) results are written back in one serial pass. Workers never touch
//!    the `Document`, so nothing in it needs a lock; the only shared
//!    structure is the dedup cache, which synchronizes itself. Detaching
//!    first (instead of locking per stream) is what makes the parallel
//!    phase document-lock-free.
//! 2. **32 KiB chunk size** (`CHUNK_SIZE`) — the one explicit fixed-size
//!    loop is the RGBA→RGB normalization: 32 KiB keeps the working set
//!    inside L1/L2 and, being a multiple of 4 bytes, never lets a pixel
//!    straddle a chunk boundary.
//! 3. **Dedup cache** — keyed by the *encoder input* (quality, kind,
//!    dimensions, exact bytes) so identical images encode once; the map is
//!    a `Mutex` held only for the cheap find-or-insert, and each entry is a
//!    `OnceLock` so competing workers converge on one encode (`get_or_init`
//!    blocks the losers). A hash alone is never treated as equality — the
//!    key compares the actual bytes.
//! 4. **Coalescing** — after the apply pass, image streams that are
//!    byte-identical (same dictionary, same payload) are collapsed onto one
//!    object and every reference is rewritten to it. The dedup cache makes
//!    identical images *encode* once; coalescing makes them *stored* once.
//! 5. **`--palette`** — an opt-in `/Indexed` candidate for plain 8-bit
//!    `DeviceRGB` rasters (default off): flat figures, charts and scans have
//!    low chromatic entropy, so one index byte per pixel plus a ≤256-entry
//!    table Flate-compresses far below what JPEG can reach. Exact palettes
//!    (≤256 unique colors) are lossless; larger rasters go through a
//!    deterministic median-cut quantizer and are accepted only above the
//!    512-px native SSIM gate.
//! 6. **Flate-wrapped JPEG** — OCRmyPDF's cheap trick: DCT bytes usually
//!    don't deflate, but padded/progressive JPEGs occasionally do; the
//!    `[FlateDecode, DCTDecode]` chain is used only when the complete flate
//!    result is smaller.
//! 7. **`--raster-classify`** — opt-in representation selection (default
//!    off): the raster classifier (`pdf::classify`) decides what each
//!    decoded image actually *is* before the size gate runs. Bitonal
//!    text/rules get a 1-bit CCITT G4 opaque grayscale image (`pdf::fax`) —
//!    the representation a document compressor should use instead of
//!    paying photographic cost — flat-color figures get the `/Indexed`
//!    candidate, and photos / mixed pages stay on the JPEG path. The
//!    classifier's routing, not a free-for-all: the indexed candidate is
//!    offered only to `FlatColor` content under this flag, so it can never
//!    turn a photo into a palette. The smallest of original / JPEG /
//!    indexed / mask wins per image.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use lopdf::{Document, Object, ObjectId, Stream, dictionary};
use rayon::prelude::*;

use crate::pdf::classify::{RasterClass, classify as classify_raster, classify_gray};
use crate::pdf::fax::encode_g4;
use crate::pdf::placements::{Placements, image_placements};
use crate::transcode::{
    CpuTranscoder, ImageRef, ImageTranscoder, Input, TranscodeCache, encode_key,
};

/// Fixed-size block used for raw pixel buffer processing.
/// 32 KiB keeps the working set inside the L1/L2 caches (and aligned with
/// 4-byte pixels), so multi-MB scans are walked in cache-resident slices
/// instead of one long linear sweep.
const CHUNK_SIZE: usize = 32 * 1024;

/// The `--dpi` downsampling cap: target output resolution plus the
/// placement scan that maps every image to its drawn size (points).
#[derive(Clone, Copy)]
struct Downsample<'a> {
    /// Target effective resolution in pixels per inch.
    dpi: u32,
    /// Placed size of every image XObject, from the content scan.
    placements: &'a Placements,
}

/// Quality selection for one `press` run.
#[derive(Clone, Copy)]
pub struct QualityMode {
    /// `-q` as given; used when `ssim` is absent or ≥ 1.0 (the default:
    /// byte-identical to the plain `-q` behavior).
    fixed: u8,
    /// `-ssim <target>`: quality derived from the calibration curve below.
    ssim: Option<f64>,
}

impl QualityMode {
    /// The plain `-q` mode (used by `merge --compress` / `convert --compress`).
    pub fn fixed(quality: u8) -> Self {
        Self {
            fixed: quality,
            ssim: None,
        }
    }

    /// `press` construction: `-q` plus the optional `-ssim` target.
    pub fn press(quality: u8, ssim: Option<f64>) -> Self {
        Self {
            fixed: quality,
            ssim: ssim.filter(|t| *t < 1.0),
        }
    }

    /// JPEG quality for one stream. `-ssim <1.0` maps the target through
    /// [`calibrated_quality`]; otherwise `-q` is used as given.
    fn effective(&self) -> u8 {
        match self.ssim {
            Some(target) => calibrated_quality(target),
            None => self.fixed,
        }
    }
}

/// Measured native (512-window luma) SSIM of a JPEG re-encode vs its source
/// on grainy gray scans — the worst-case reference content for JPEG, where
/// artifacts are the most visible. Smooth photos and paper figures score
/// far higher at the same quality, so this curve is conservative: content
/// that is not grainy always *exceeds* the requested target. See
/// `benches/docker/calibrate_ssim.py` and QUALITY.md "SSIM targets".
const SSIM_CALIBRATION: [(u8, f64); 6] = [
    (5, 0.6911),
    (10, 0.8929),
    (15, 0.9182),
    (25, 0.9580),
    (50, 0.9852),
    (75, 0.9934),
];

/// Piecewise-linear inverse of [`SSIM_CALIBRATION`]: the JPEG quality whose
/// measured SSIM is closest to `target`, clamped to [5, 90].
fn calibrated_quality(target: f64) -> u8 {
    let mut q = SSIM_CALIBRATION[0].0;
    for w in SSIM_CALIBRATION.windows(2) {
        let (qa, sa) = w[0];
        let (qb, sb) = w[1];
        if target <= sa {
            q = qa;
            break;
        }
        if target <= sb {
            let t = (target - sa) / (sb - sa);
            q = (qa as f64 + t * (qb as f64 - qa as f64)).round() as u8;
            break;
        }
        q = qb;
    }
    q.clamp(5, 90)
}

/// Replace JPEG images in a document by a compressed version to the given quality.
/// Only JPEG images are replaced, the other are skipped.
///
/// Uses the default CPU backend (see [`compress_images_with`]). `dpi` is
/// `None` (or omitted) for the default behavior: images keep their source
/// resolution.
pub fn compress_images(doc: &mut Document, quality: u8, verbose: bool) {
    compress_images_with(
        doc,
        QualityMode::fixed(quality),
        verbose,
        &CpuTranscoder::default(),
        None,
        false,
        false,
    );
}

/// Options for the image candidate pipeline (`--palette`, `--raster-classify`).
/// [`Default`] is the JPEG-only pipeline.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompressOptions {
    /// `-d/--dpi`: cap placed images to this resolution.
    pub dpi: Option<u32>,
    /// `--palette`: offer the `/Indexed` candidate.
    pub palette: bool,
    /// `--raster-classify`: route bitonal text to 1-bit G4, flat colors to
    /// indexed.
    pub classify: bool,
}

/// Replace JPEG images in a document using the given transcoding backend.
/// Only JPEG images are replaced, the other are skipped.
///
/// Convenience wrapper for the JPEG-only options; see [`compress_images_opt`]
/// for the full candidate set.
pub fn compress_images_with<T: ImageTranscoder>(
    doc: &mut Document,
    mode: QualityMode,
    verbose: bool,
    transcoder: &T,
    dpi: Option<u32>,
    palette: bool,
    classify: bool,
) {
    compress_images_opt(
        doc,
        mode,
        verbose,
        transcoder,
        CompressOptions {
            dpi,
            palette,
            classify,
        },
    );
}

/// Replace JPEG images in a document using the given transcoding backend.
/// Only JPEG images are replaced, the other are skipped.
///
/// `dpi`, when set, caps the effective resolution of every placed image:
/// an image drawn at `w×h` points is downsampled to at most
/// `w·dpi/72 × h·dpi/72` pixels (Ghostscript's `/ebook`-style behavior),
/// and images already below the cap keep their source resolution. `palette`
/// additionally tries an `/Indexed` color-space candidate for eligible flat
/// images and keeps the smallest of original / JPEG / indexed. `classify`
/// (`--raster-classify`) runs the raster classifier first: bitonal content
/// additionally gets a 1-bit CCITT G4 opaque grayscale candidate, and
/// flat-color content gets the indexed candidate. The pipeline is split in
/// three phases:
/// 1. **Extract** — detach every eligible image stream from the `Document`
///    object tree (serial, cheap: just map lookups + moves, no copies).
/// 2. **Re-encode** — transcode all streams concurrently with rayon on owned,
///    detached buffers. No `Document` state is read or written from worker
///    threads, so there is nothing to lock.
/// 3. **Apply** — write the re-encoded streams (and updated `/Filter` +
///    `/Length`, plus `/Width` + `/Height` for downsampled images, and
///    `/ColorSpace` + palette objects for indexed candidates, and the
///    `/ColorSpace`/`Decode` entries for mask candidates) back into the
///    object tree in a
///    single serial pass, right before serialization. The pass ends by
///    coalescing byte-identical image objects.
pub fn compress_images_opt<T: ImageTranscoder>(
    doc: &mut Document,
    mode: QualityMode,
    verbose: bool,
    transcoder: &T,
    opts: CompressOptions,
) {
    let CompressOptions {
        dpi,
        palette,
        classify,
    } = opts;
    // Placement scan is needed only when downsampling is requested; without
    // it (the default) the pipeline is byte-identical to before.
    let placements = dpi.is_some().then(|| image_placements(doc));
    let downsample = dpi
        .zip(placements.as_ref())
        .map(|(d, placements)| Downsample { dpi: d, placements });
    // Phase 1 — extract.
    let image_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| match obj {
            Object::Stream(stream) if is_image_stream(stream) => Some(*id),
            _ => None,
        })
        .collect();

    let mut images: Vec<(ObjectId, Stream)> = Vec::with_capacity(image_ids.len());
    for id in image_ids {
        if let Some(Object::Stream(stream)) = doc.objects.remove(&id) {
            images.push((id, stream));
        }
    }

    verbose!(verbose, "[images] Found {} image object(s)", images.len());

    // Deduplication cache: identical images (logos, watermarks, headers)
    // appear dozens of times in real PDFs; each unique image is encoded
    // exactly once (competing workers converge on one result).
    let cache = TranscodeCache::new();

    // Phase 2 — re-encode in parallel. Workers never touch `Document` state
    // (document-lock-free); the dedup cache is the only shared structure and
    // is internally synchronized.
    let reencoded: Vec<(ObjectId, Reencoded)> = images
        .into_par_iter()
        .map(|(id, stream)| {
            let outcome = reencode_image_stream(
                id,
                stream,
                mode,
                verbose,
                &cache,
                transcoder,
                downsample,
                CompressOptions {
                    palette,
                    classify,
                    ..CompressOptions::default()
                },
            );
            (id, outcome)
        })
        .collect();

    // Phase 3 — single mutation pass back onto the document. Indexed
    // candidates need a fresh palette-stream object; identical palettes
    // (identical images) share one object, so the coalescing pass below can
    // collapse their streams onto one canonical image.
    let mut palette_objects: HashMap<Arc<[u8]>, ObjectId> = HashMap::new();
    for (id, outcome) in reencoded {
        let stream = match outcome {
            Reencoded::Stream(stream) => stream,
            Reencoded::Indexed {
                mut stream,
                palette,
                hival,
                colorspace,
            } => {
                let key: Arc<[u8]> = Arc::from(palette);
                let palette_id = match palette_objects.get(&key) {
                    Some(pid) => *pid,
                    None => {
                        let mut p = Stream::new(dictionary! {}, zlib_encode(&key));
                        p.dict.set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
                        p.dict
                            .set(b"Length", Object::Integer(p.content.len() as i64));
                        let pid = doc.add_object(Object::Stream(p));
                        palette_objects.insert(key, pid);
                        pid
                    }
                };
                stream.dict.set(
                    b"ColorSpace",
                    Object::Array(vec![
                        Object::Name(b"Indexed".to_vec()),
                        Object::Name(colorspace),
                        Object::Integer(hival as i64),
                        Object::Reference(palette_id),
                    ]),
                );
                stream
            }
            // Mask candidates carry their full dictionary (phase 2 wrote
            // /Filter + /DecodeParms + /ColorSpace DeviceGray +
            // /BitsPerComponent and cleared /ImageMask); phase 3 sets the
            // /Decode table — [1 0] maps the G4 ink bit (1, BlackIs1) to
            // decoded 0 = black and paper (0) to 1 = white.
            Reencoded::Mask { mut stream } => {
                stream
                    .dict
                    .set(b"Decode", Object::Array(vec![1.into(), 0.into()]));
                stream
            }
        };
        doc.objects.insert(id, Object::Stream(stream));
    }

    // Byte-identical image objects collapse onto one canonical object (the
    // dedup cache already made them encode once; this makes them stored
    // once). Rendering is unchanged: identical streams render identically,
    // and PDF allows any number of pages to share one XObject.
    let coalesced = coalesce_image_objects(doc);
    verbose!(
        verbose,
        "[images] coalesced {} duplicate image object(s)",
        coalesced
    );
}

/// `--palette` fidelity gate: a lossy (median-cut) palette is accepted only
/// above this native-image SSIM on a 512-px window — the project's stricter
/// witness, not the 64-px render scale.
const PALETTE_SSIM_GATE: f64 = 0.9999;

/// Above this many unique colors the raster is photographic; median-cut
/// quantization costs more than a palette can ever save, so it is skipped.
const MAX_QUANTIZE_COLORS: usize = 65_536;

/// Estimated PDF overhead of the extra palette stream + `/ColorSpace` array
/// (dictionary + object entry), so the indexed candidate is chosen only when
/// it *really* wins against the JPEG and the source.
const PALETTE_OVERHEAD: usize = 96;

/// The final outcome for one image stream, consumed by the apply pass.
/// Workers never touch the `Document`; everything they need to hand back is
/// carried in this value.
enum Reencoded {
    /// The stream is final (unchanged original, or a re-encode already
    /// written into it by phase 2).
    Stream(Stream),
    /// The stream carries an indexed-color payload; phase 3 must create (or
    /// reuse) a palette object and set `/ColorSpace` before insertion.
    Indexed {
        stream: Stream,
        palette: Vec<u8>,
        hival: u8,
        colorspace: Vec<u8>,
    },
    /// The stream carries a 1-bit CCITT G4 opaque grayscale payload; phase
    /// 3 must set `/Decode` (the rest of the dictionary — `/ColorSpace`
    /// `DeviceGray`, `/DecodeParms`, `/BitsPerComponent` — is already
    /// written by phase 2).
    Mask { stream: Stream },
}

/// The best replacement for one stream's content, already chosen by the
/// size gate against the original bytes.
enum Candidate {
    /// Re-encoded JPEG payload, smaller than the source content.
    Jpeg {
        buf: Vec<u8>,
        dims: Option<(u32, u32)>,
    },
    /// Indexed-color payload, smaller than the source content.
    Indexed {
        candidate: IndexedCandidate,
        dims: Option<(u32, u32)>,
    },
    /// 1-bit CCITT G4 mask, smaller than the source content.
    Mask {
        candidate: MaskCandidate,
        dims: Option<(u32, u32)>,
    },
    /// The source content wins; nothing replaced it.
    Unchanged,
}

/// An `/Indexed` color-space candidate: a palette table plus one index byte
/// per pixel. The palette is first-seen-ordered for exact palettes and
/// box-ordered for median-cut palettes — deterministic either way, so
/// serial and parallel runs stay byte-identical.
struct IndexedCandidate {
    /// Palette table bytes, 3 per entry (`DeviceRGB`).
    palette: Vec<u8>,
    /// Per-pixel palette indices, 1 byte each.
    indices: Vec<u8>,
    /// `hival` = palette length − 1 (must fit one byte).
    hival: u8,
    /// Base color space name (always `DeviceRGB` today).
    colorspace: Vec<u8>,
}

impl IndexedCandidate {
    /// Full cost of the indexed representation: indices (flate) + palette
    /// (flate) + the dictionary/array overhead of the extra palette stream.
    fn total_size(&self) -> usize {
        zlib_encode(&self.indices).len() + zlib_encode(&self.palette).len() + PALETTE_OVERHEAD
    }
}

/// A 1-bit CCITT Group 4 mask candidate (`--raster-classify` on bitonal
/// content), written as an *opaque* `DeviceGray` image (not an `/ImageMask`
/// stencil — a stencil's 0 bits are transparent and its ink inherits the
/// current color, so it is not a substitute for an opaque raster). The G4
/// encoding of the 1-bit payload is lossless; the RGB→bitonal conversion
/// itself is lossy by design, which is why the conservative classifier only
/// fires on near-perfect black-and-white content.
struct MaskCandidate {
    /// CCITT G4 fax data (1 = ink).
    g4: Vec<u8>,
    /// Raster dimensions the mask is written at.
    width: u32,
    height: u32,
}

impl MaskCandidate {
    /// Full cost: the G4 payload plus the mask dictionary overhead (the
    /// `/DecodeParms` + `/Decode` entries and the `DeviceGray` color space
    /// replace the source's `DeviceRGB`).
    fn total_size(&self) -> usize {
        self.g4.len() + PALETTE_OVERHEAD
    }
}

/// Re-encode a single image stream. The stream is owned and detached from the
/// document, so this can run on any rayon worker thread.
///
/// Returns the outcome that should be stored back: the original, untouched,
/// when nothing could or should be re-encoded; a modified stream carrying the
/// new JPEG payload (or an indexed candidate, described separately for the
/// apply pass) otherwise.
// The pipeline-wide context (mode/cache/transcoder/downsample/palette/
// classify) is deliberately passed as arguments rather than a bundle struct
// — callers are generic over the transcoder and the function is internal.
#[allow(clippy::too_many_arguments)]
fn reencode_image_stream<T: ImageTranscoder>(
    id: ObjectId,
    mut stream: Stream,
    mode: QualityMode,
    verbose: bool,
    cache: &TranscodeCache,
    transcoder: &T,
    downsample: Option<Downsample<'_>>,
    opts: CompressOptions,
) -> Reencoded {
    let CompressOptions {
        dpi: _,
        palette,
        classify,
    } = opts;
    // Resolve `-q` / `-ssim` to the JPEG quality for this stream. In ssim
    // mode the calibration is deterministic, so the dedup key (which
    // includes the quality) stays valid.
    let quality = mode.effective();
    if let Some(target) = mode.ssim {
        verbose!(
            verbose,
            "[img {:?}] → ssim target {target} → q{quality}",
            id
        );
    }
    let color_space_raw = stream
        .dict
        .get(b"ColorSpace")
        .and_then(|f| f.as_name())
        .ok();
    let width = stream
        .dict
        .get(b"Width")
        .and_then(|w| w.as_i64())
        .unwrap_or(0) as u32;
    let height = stream
        .dict
        .get(b"Height")
        .and_then(|h| h.as_i64())
        .unwrap_or(0) as u32;

    // Target dimensions from the `--dpi` cap (Ghostscript-style presets:
    // 75 screen, 150 ebook, 300 printer, 600 prepress). The cap is a strict
    // resolution *cap* — it never up-samples, and images whose placement is
    // unknown stay at source resolution.
    let resampled: Option<(u32, u32)> = downsample.and_then(|cap| {
        let (placed_w, placed_h) = cap.placements.get(&id).copied()?;
        if width == 0 || height == 0 || placed_w <= 0.0 || placed_h <= 0.0 {
            return None;
        }
        let tw = ((placed_w * f64::from(cap.dpi) / 72.0).round() as u32)
            .min(width)
            .max(1);
        let th = ((placed_h * f64::from(cap.dpi) / 72.0).round() as u32)
            .min(height)
            .max(1);
        (tw != width || th != height).then_some((tw, th))
    });

    // Detect filter: may be a Name or an Array
    let filter_name = stream.dict.get(b"Filter").ok();
    let filter_str = filter_name.map(|f| match f {
        Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
        Object::Array(arr) => {
            let names: Vec<String> = arr
                .iter()
                .filter_map(|e| e.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            format!("[{}]", names.join(", "))
        }
        _ => format!("{:?}", f),
    });
    let color_str = color_space_raw
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .unwrap_or_else(|| "unknown".to_string());

    verbose!(
        verbose,
        "[img {:?}] filter={} colorspace={} size={}x{} raw_content={}B",
        id,
        filter_str.as_deref().unwrap_or("none"),
        color_str,
        width,
        height,
        stream.content.len()
    );

    // CMYK images not yet supported by image crate
    if color_space_raw == Some(b"DeviceCMYK") {
        verbose!(verbose, "[img {:?}] → skipped: CMYK not supported", id);
        return Reencoded::Stream(stream);
    }

    // Resolve the effective image filter.
    // Singleton --> ok.
    // Multi-element Array --> (e.g. [FlateDecode, DCTDecode]) not supported, skip.
    let filter: Option<&[u8]> = match stream.dict.get(b"Filter").ok() {
        Some(Object::Name(n)) => Some(n.as_slice()),
        Some(Object::Array(arr)) if arr.len() == 1 => arr[0].as_name().ok(),
        Some(Object::Array(arr)) => {
            if verbose {
                let names: Vec<String> = arr
                    .iter()
                    .filter_map(|e| e.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .collect();
                eprintln!(
                    "[img {:?}] → skipped: multi-filter pipeline [{}] not supported",
                    id,
                    names.join(", ")
                );
            }
            return Reencoded::Stream(stream);
        }
        _ => None,
    };

    let original_len = stream.content.len();

    // `resized_buf` outlives the borrow in the encode request below, so a
    // resampled raw stream's pixels stay alive across the cache lookup.
    // (Set only in the resample branch; read only right after.)
    let resized_buf: Option<Vec<u8>>;

    // Choose the best replacement for the stream's content: the JPEG, an
    // indexed-color candidate, or the source itself (the size gate decides).
    let candidate = match filter {
        Some(b"DCTDecode") | Some(b"JPXDecode") => {
            if let Some((tw, th)) = resampled {
                verbose!(
                    verbose,
                    "[img {:?}] → resampling {}x{} → {}x{} (dpi cap)",
                    id,
                    width,
                    height,
                    tw,
                    th
                );
                // The backend decodes from concrete pixels (it cannot
                // resize), so decode + downsample here and hand it pixels.
                let Some((bytes, tag)) = resampled_pixels(&stream.content, tw, th) else {
                    verbose!(
                        verbose,
                        "[img {:?}] → skipped: JPEG decode failed during resample",
                        id
                    );
                    return Reencoded::Stream(stream);
                };
                let input = pixel_input(tag, tw, th, &bytes);
                let key = encode_key(quality, tag, tw, th, &bytes);
                let result = cache.get(key, || transcoder.transcode_image(&input, quality));
                match result.as_ref() {
                    Ok(buf) if !buf.is_empty() && buf.len() < original_len => Candidate::Jpeg {
                        buf: buf.clone(),
                        dims: Some((tw, th)),
                    },
                    Ok(buf) if buf.is_empty() => {
                        verbose!(
                            verbose,
                            "[img {:?}] → skipped: backend produced empty output",
                            id
                        );
                        return Reencoded::Stream(stream);
                    }
                    Ok(buf) => {
                        verbose!(
                            verbose,
                            "[img {:?}] → skipped: re-encoded ({}B) not smaller than original ({}B)",
                            id,
                            buf.len(),
                            original_len
                        );
                        Candidate::Unchanged
                    }
                    Err(e) => {
                        verbose!(verbose, "[img {:?}] → skipped: transcode failed: {}", id, e);
                        return Reencoded::Stream(stream);
                    }
                }
            } else {
                verbose!(verbose, "[img {:?}] → processing JPEG/JPX via backend", id);
                // Identical JPEG bytes re-encode identically: encode once,
                // reuse the cached buffer for every duplicate image object.
                let input = Input::Jpeg(&stream.content);
                let key = encode_key(quality, 2, width, height, &stream.content);
                let result = cache.get(key, || transcoder.transcode_image(&input, quality));
                match result.as_ref() {
                    Ok(buf) if !buf.is_empty() && buf.len() < original_len => Candidate::Jpeg {
                        buf: buf.clone(),
                        dims: None,
                    },
                    Ok(buf) if buf.is_empty() => {
                        verbose!(
                            verbose,
                            "[img {:?}] → skipped: backend produced empty output",
                            id
                        );
                        return Reencoded::Stream(stream);
                    }
                    Ok(buf) => {
                        verbose!(
                            verbose,
                            "[img {:?}] → skipped: re-encoded ({}B) not smaller than original ({}B)",
                            id,
                            buf.len(),
                            original_len
                        );
                        Candidate::Unchanged
                    }
                    Err(e) => {
                        verbose!(verbose, "[img {:?}] → skipped: transcode failed: {}", id, e);
                        return Reencoded::Stream(stream);
                    }
                }
            }
        }
        Some(other_filter) => {
            verbose!(
                verbose,
                "[img {:?}] → processing raw pixels (filter={}, colorspace={})",
                id,
                String::from_utf8_lossy(other_filter),
                color_str
            );
            let raw = match stream.decompressed_content() {
                Ok(data) => data,
                Err(e) => {
                    verbose!(
                        verbose,
                        "[img {:?}] → skipped: decompressed_content failed: {}",
                        id,
                        e
                    );
                    return Reencoded::Stream(stream);
                }
            };

            let classified = match classify_raw(&raw, width, height, color_space_raw) {
                Some(c) => c,
                None => {
                    verbose!(
                        verbose,
                        "[img {:?}] → skipped: mismatched dimensions or unsupported format, expected {}x{}x{} bytes",
                        id,
                        width,
                        height,
                        if color_space_raw == Some(b"DeviceGray") {
                            width * height
                        } else {
                            width * height * 3
                        }
                    );
                    return Reencoded::Stream(stream);
                }
            };

            // (encoder input, key bytes, kind, width, height)
            let (input, key_bytes, tag, ew, eh): (Input<'_>, &[u8], u8, u32, u32) =
                if let Some((tw, th)) = resampled {
                    verbose!(
                        verbose,
                        "[img {:?}] → resampling {}x{} → {}x{} (dpi cap)",
                        id,
                        width,
                        height,
                        tw,
                        th
                    );
                    let tag = match &classified {
                        RawPixels::Luma(_) => 1,
                        _ => 3,
                    };
                    let bytes = resize_raw(&classified, width, height, tw, th);
                    resized_buf = Some(bytes);
                    let bytes = resized_buf.as_ref().expect("just set");
                    (
                        pixel_input(tag, tw, th, bytes),
                        bytes.as_slice(),
                        tag,
                        tw,
                        th,
                    )
                } else {
                    match &classified {
                        RawPixels::Luma(bytes) => (
                            Input::Pixels(ImageRef::Luma8 {
                                width,
                                height,
                                bytes,
                            }),
                            bytes,
                            1,
                            width,
                            height,
                        ),
                        RawPixels::Rgb(bytes) => (
                            Input::Pixels(ImageRef::Rgb8 {
                                width,
                                height,
                                bytes,
                            }),
                            bytes,
                            3,
                            width,
                            height,
                        ),
                        RawPixels::RgbaNormalized(rgb) => (
                            Input::Pixels(ImageRef::Rgb8 {
                                width,
                                height,
                                bytes: rgb.as_slice(),
                            }),
                            rgb.as_slice(),
                            3,
                            width,
                            height,
                        ),
                    }
                };

            // Same pixels → same JPEG, regardless of how the source stream
            // happened to be compressed. Width/height are keyed too: two
            // images with identical pixel bytes but different dimensions
            // must not share a cached JPEG.
            let key = encode_key(quality, tag, ew, eh, key_bytes);
            let result = cache.get(key, || transcoder.transcode_image(&input, quality));
            let jpeg: Option<Vec<u8>> = match result.as_ref() {
                Ok(buf) if !buf.is_empty() => Some(buf.clone()),
                Ok(_) => {
                    verbose!(
                        verbose,
                        "[img {:?}] → skipped: backend produced empty output",
                        id
                    );
                    None
                }
                Err(e) => {
                    verbose!(verbose, "[img {:?}] → skipped: transcode failed: {}", id, e);
                    None
                }
            };

            // `--raster-classify`: the classifier runs first — it decides
            // the routing: bitonal text gets a 1-bit CCITT G4 mask candidate,
            // flat-color figures get the `/Indexed` candidate, and photos /
            // mixed pages get neither (the within-image split is future
            // work). `--palette` alone keeps offering the indexed candidate
            // to every eligible raster; the candidate itself still
            // self-gates on the fidelity/size courts. The G4/palette
            // *replacements* are gated on `classify` alone, keeping each
            // flag self-sufficient.
            let decision = if classify {
                Some(if tag == 3 {
                    classify_raster(key_bytes, ew, eh)
                } else {
                    classify_gray(key_bytes, ew, eh)
                })
            } else {
                None
            };

            // A 1-bit CCITT G4 opaque-grayscale candidate for bitonal text,
            // offered only under `--raster-classify` (the flag that opts into
            // the G4 representation).
            let mask: Option<MaskCandidate> = (classify
                && decision
                    .as_ref()
                    .is_some_and(|d| d.class == RasterClass::BitonalText))
            .then(|| MaskCandidate {
                g4: encode_g4(
                    decision
                        .as_ref()
                        .and_then(|d| d.mask.as_ref())
                        .expect("bitonal ⇒ mask"),
                    ew,
                    eh,
                ),
                width: ew,
                height: eh,
            });

            // `/Indexed` palette candidate for plain 8-bit DeviceRGB rasters
            // (see [`indexed_candidate`]). Under `--raster-classify` it is
            // offered only to flat-color figures — photos must stay on the
            // JPEG path; `--palette` offers it to all eligible rasters.
            let indexed: Option<IndexedCandidate> = if palette_eligible(&stream, color_space_raw)
                && (palette
                    || decision
                        .as_ref()
                        .is_some_and(|d| d.class == RasterClass::FlatColor))
            {
                indexed_candidate(key_bytes, ew, eh)
            } else {
                None
            };

            // The size gate: pick the smallest of JPEG / indexed / mask /
            // source. Only a strictly smaller candidate replaces the source.
            let mut best: Option<(usize, Candidate)> = None;
            let mut consider = |size: usize, c: Candidate| {
                if best.as_ref().is_none_or(|(s, _)| size < *s) {
                    best = Some((size, c));
                }
            };
            if let Some(m) = mask {
                consider(
                    m.total_size(),
                    Candidate::Mask {
                        candidate: m,
                        dims: resampled,
                    },
                );
            }
            if let Some(idx) = indexed {
                consider(
                    idx.total_size(),
                    Candidate::Indexed {
                        candidate: idx,
                        dims: resampled,
                    },
                );
            }
            if let Some(j) = jpeg {
                consider(
                    j.len(),
                    Candidate::Jpeg {
                        buf: j,
                        dims: resampled,
                    },
                );
            }
            match best {
                Some((size, c)) if size < original_len => c,
                Some((size, _)) => {
                    verbose!(
                        verbose,
                        "[img {:?}] → skipped: re-encoded ({}B) not smaller than original ({}B)",
                        id,
                        size,
                        original_len
                    );
                    Candidate::Unchanged
                }
                None => Candidate::Unchanged,
            }
        }
        None => {
            verbose!(
                verbose,
                "[img {:?}] → skipped: no Filter entry (uncompressed stream)",
                id
            );
            return Reencoded::Stream(stream);
        }
    };

    match candidate {
        Candidate::Unchanged => {
            // Retained content. A DCT stream can occasionally shrink under
            // zlib (padded or progressive JPEG frames): use the
            // `[FlateDecode, DCTDecode]` chain only when the full flate
            // result is smaller.
            try_flate_wrap(&mut stream);
            Reencoded::Stream(stream)
        }
        Candidate::Jpeg { buf, dims } => {
            verbose!(
                verbose,
                "[img {:?}] → compressed {}B → {}B",
                id,
                original_len,
                buf.len()
            );
            stream.content = buf;
            stream
                .dict
                .set(b"Filter", Object::Name(b"DCTDecode".to_vec()));
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            if let Some((tw, th)) = dims {
                // Downsampled: the raster is smaller, so the dict must say so.
                stream.dict.set(b"Width", Object::Integer(tw as i64));
                stream.dict.set(b"Height", Object::Integer(th as i64));
            }
            try_flate_wrap(&mut stream);
            Reencoded::Stream(stream)
        }
        Candidate::Indexed { candidate, dims } => {
            let payload = zlib_encode(&candidate.indices);
            verbose!(
                verbose,
                "[img {:?}] → compressed {}B → {}B indexed ({} colors)",
                id,
                original_len,
                payload.len(),
                candidate.hival as usize + 1
            );
            stream.content = payload;
            stream
                .dict
                .set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            if let Some((tw, th)) = dims {
                stream.dict.set(b"Width", Object::Integer(tw as i64));
                stream.dict.set(b"Height", Object::Integer(th as i64));
            }
            Reencoded::Indexed {
                stream,
                palette: candidate.palette,
                hival: candidate.hival,
                colorspace: candidate.colorspace,
            }
        }
        Candidate::Mask { candidate, dims } => {
            verbose!(
                verbose,
                "[img {:?}] → compressed {}B → {}B 1-bit CCITT G4 mask",
                id,
                original_len,
                candidate.g4.len()
            );
            stream.content = candidate.g4;
            stream
                .dict
                .set(b"Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            // An *opaque* 1-bit grayscale image, not an /ImageMask stencil:
            // a stencil paints ink in the current nonstroking color and
            // treats 0 bits as transparent, so replacing an opaque
            // black-on-white raster with one would let background content
            // show through the "white" and recolor the text. As a plain
            // DeviceGray 1-bit image both states stay real pixels. /Decode
            // is set by the apply pass: [1 0] maps the G4 ink bit (1,
            // BlackIs1) to decoded 0 = black and paper (0) to 1 = white.
            stream
                .dict
                .set(b"ColorSpace", Object::Name(b"DeviceGray".to_vec()));
            stream.dict.set(b"BitsPerComponent", Object::Integer(1));
            stream.dict.remove(b"ImageMask");
            stream.dict.set(
                b"DecodeParms",
                Object::Dictionary(dictionary! {
                    "K" => -1,
                    "BlackIs1" => true,
                    "Columns" => candidate.width as i64,
                    "Rows" => candidate.height as i64,
                }),
            );
            if let Some((tw, th)) = dims {
                stream.dict.set(b"Width", Object::Integer(tw as i64));
                stream.dict.set(b"Height", Object::Integer(th as i64));
            }
            Reencoded::Mask { stream }
        }
    }
}

/// A raw (decompressed) pixel buffer, classified into a backend-friendly
/// layout.
enum RawPixels<'a> {
    /// 1 byte/pixel grayscale.
    Luma(&'a [u8]),
    /// 3 bytes/pixel interleaved RGB.
    Rgb(&'a [u8]),
    /// A 4-byte/pixel `DeviceRGB` stream normalized to RGB by dropping the
    /// alpha channel in fixed-size chunks (owned result).
    RgbaNormalized(Vec<u8>),
}

/// Classify a decompressed pixel buffer.
///
/// Canonical layouts are wrapped zero-copy: `DeviceGray` → 1 byte/pixel,
/// `DeviceRGB` → 3 bytes/pixel. Streams that illegally carry a fourth (alpha)
/// byte per pixel on a `DeviceRGB` stream are normalized by dropping the
/// alpha channel in fixed-size chunks (each 32 KiB chunk holds a whole number
/// of 4-byte pixels, so a pixel never straddles a chunk boundary). Anything
/// else is rejected (`None` → the stream is skipped by the caller).
fn classify_raw<'a>(
    raw: &'a [u8],
    width: u32,
    height: u32,
    color_space: Option<&[u8]>,
) -> Option<RawPixels<'a>> {
    let (w, h) = (width as usize, height as usize);
    let area = w.checked_mul(h)?;

    match color_space {
        Some(b"DeviceGray") if raw.len() == area => Some(RawPixels::Luma(raw)),
        _ if raw.len() == area.checked_mul(3)? => Some(RawPixels::Rgb(raw)),
        _ if raw.len() == area.checked_mul(4)? => {
            let mut rgb = Vec::with_capacity(area * 3);
            for chunk in raw.chunks(CHUNK_SIZE) {
                for px in chunk.as_chunks::<4>().0 {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            Some(RawPixels::RgbaNormalized(rgb))
        }
        _ => None,
    }
}

/// Check if a stream represents an image.
pub(crate) fn is_image_stream(stream: &Stream) -> bool {
    stream
        .dict
        .get(b"Subtype")
        .and_then(|s| s.as_name())
        .ok()
        .is_some_and(|name| name == b"Image")
}

/// An [`Input`] over pixel bytes at the given size, from a kind tag
/// (1 = luma, 3 = rgb).
fn pixel_input<'a>(tag: u8, width: u32, height: u32, bytes: &'a [u8]) -> Input<'a> {
    match tag {
        1 => Input::Pixels(ImageRef::Luma8 {
            width,
            height,
            bytes,
        }),
        _ => Input::Pixels(ImageRef::Rgb8 {
            width,
            height,
            bytes,
        }),
    }
}

/// Decode a JPEG and downsample it to `(tw, th)`, returning the resized
/// pixel buffer and its kind tag. `None` when the source cannot be decoded.
/// Grayscale sources stay single-component, matching `/DeviceGray` streams.
fn resampled_pixels(jpeg: &[u8], tw: u32, th: u32) -> Option<(Vec<u8>, u8)> {
    let decoded = image::load_from_memory(jpeg).ok()?;
    // `imageops::resize` on a `DynamicImage` yields an RGBA buffer; convert
    // RGB sources to 3-channel first so the encoder sees the right format.
    match decoded {
        image::DynamicImage::ImageLuma8(gray) => {
            let resized =
                image::imageops::resize(&gray, tw, th, image::imageops::FilterType::Triangle);
            Some((resized.into_raw(), 1))
        }
        other => {
            let rgb = other.to_rgb8();
            let resized =
                image::imageops::resize(&rgb, tw, th, image::imageops::FilterType::Triangle);
            Some((resized.into_raw(), 3))
        }
    }
}

/// Downsample a classified pixel buffer to `(tw, th)`, preserving its
/// channel count. `classify_raw` already validated the dimensions, so the
/// `from_raw` calls cannot fail.
fn resize_raw(classified: &RawPixels<'_>, width: u32, height: u32, tw: u32, th: u32) -> Vec<u8> {
    match classified {
        RawPixels::Luma(bytes) => {
            let img =
                image::GrayImage::from_raw(width, height, bytes.to_vec()).expect("validated dims");
            image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle).into_raw()
        }
        RawPixels::Rgb(bytes) => {
            let img =
                image::RgbImage::from_raw(width, height, bytes.to_vec()).expect("validated dims");
            image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle).into_raw()
        }
        RawPixels::RgbaNormalized(rgb) => {
            let img =
                image::RgbImage::from_raw(width, height, rgb.clone()).expect("validated dims");
            image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle).into_raw()
        }
    }
}

/// zlib-compress bytes (PDF `/FlateDecode`) at the writer's compression
/// level. Infallible on a `Vec` sink. Shared with the writer's
/// `--recompress-flate` pass.
pub(crate) fn zlib_encode(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(data)
        .expect("zlib write into a Vec cannot fail");
    enc.finish().expect("zlib finish into a Vec cannot fail")
}

/// OCRmyPDF's cheap trick: DCT byte streams are Huffman-coded and normally
/// incompressible, but JPEGs with zero-padded tails or progressive frames
/// can occasionally shrink under zlib. The `[FlateDecode, DCTDecode]` chain
/// is applied only when the complete flate result is smaller.
fn try_flate_wrap(stream: &mut Stream) {
    let is_plain_jpeg = stream
        .dict
        .get(b"Filter")
        .ok()
        .and_then(|f| f.as_name().ok())
        .is_some_and(|f| f == b"DCTDecode");
    if !is_plain_jpeg {
        return;
    }
    let flate = zlib_encode(&stream.content);
    if flate.len() < stream.content.len() {
        stream.content = flate;
        stream.dict.set(
            b"Filter",
            Object::Array(vec![
                Object::Name(b"FlateDecode".to_vec()),
                Object::Name(b"DCTDecode".to_vec()),
            ]),
        );
        stream
            .dict
            .set(b"Length", Object::Integer(stream.content.len() as i64));
    }
}

/// The `--palette` path is deliberately conservative: only plain 8-bit
/// `DeviceRGB` raster streams without masks, custom decode tables or extra
/// parameters. CMYK and ICCBased are excluded because the `/ColorSpace`
/// must be a plain name; `DeviceGray` is excluded because 8-bit gray *is*
/// its own palette — an indexed gray stream is byte-identical to the source
/// plus palette overhead, so it can never pass the smaller-than-original
/// gate.
fn palette_eligible(stream: &Stream, color_space: Option<&[u8]>) -> bool {
    if color_space != Some(b"DeviceRGB") {
        return false;
    }
    for key in [
        b"Mask".as_slice(),
        b"SMask".as_slice(),
        b"Decode".as_slice(),
        b"DecodeParms".as_slice(),
    ] {
        if stream.dict.get(key).is_ok() {
            return false;
        }
    }
    matches!(
        stream
            .dict
            .get(b"BitsPerComponent")
            .ok()
            .and_then(|b| b.as_i64().ok()),
        None | Some(8)
    )
}

/// Build the `/Indexed` candidate for an RGB raster, or `None` when the
/// palette representation cannot reach the fidelity gate.
///
/// Mechanism (the OCRmyPDF paper win): plots, diagrams, charts and scans
/// carry far less chromatic entropy than photos. A palette raster is one
/// index byte per pixel plus a table of at most 256×3 bytes, so flat
/// regions become runs of identical indices that Flate compresses into
/// almost nothing — JPEG cannot exploit that structure, because it pays
/// per-block overhead even on constant color. Images with at most 256
/// unique colors are converted losslessly (exact palette); larger rasters
/// go through a deterministic median-cut quantizer and are accepted only
/// above the native-image SSIM gate, so a lossy palette can never visibly
/// degrade a figure.
fn indexed_candidate(rgb: &[u8], width: u32, height: u32) -> Option<IndexedCandidate> {
    if rgb.len() < 3 {
        return None;
    }
    let hist = build_histogram(rgb);
    let unique = hist.colors.len();

    if unique <= 256 {
        // Lossless: the palette is the set of distinct colors, in
        // first-seen order (deterministic).
        let mut palette = Vec::with_capacity(unique * 3);
        let mut index_of: HashMap<[u8; 3], u8> = HashMap::with_capacity(unique);
        for (i, c) in hist.colors.iter().enumerate() {
            index_of.insert(*c, i as u8);
            palette.extend_from_slice(c);
        }
        let indices: Vec<u8> = rgb
            .as_chunks::<3>()
            .0
            .iter()
            .map(|px| index_of[px])
            .collect();
        return Some(IndexedCandidate {
            palette,
            indices,
            hival: (unique - 1) as u8,
            colorspace: b"DeviceRGB".to_vec(),
        });
    }

    // Photographic content: median-cut would cost more than a palette can
    // ever save, so skip the candidate entirely.
    if unique > MAX_QUANTIZE_COLORS {
        return None;
    }

    let palette = median_cut(&hist);
    let indices = map_to_palette(rgb, &palette);

    // Fidelity gate: the quantized raster must be visually equivalent to
    // the source at the native-image witness scale (512-px window).
    let mapped: Vec<u8> = indices
        .iter()
        .flat_map(|&i| {
            let c = palette[i as usize];
            [c[0], c[1], c[2]]
        })
        .collect();
    if ssim_window(rgb, &mapped, width, height) < PALETTE_SSIM_GATE {
        return None;
    }

    let mut palette_bytes = Vec::with_capacity(palette.len() * 3);
    for c in &palette {
        palette_bytes.extend_from_slice(c);
    }
    Some(IndexedCandidate {
        palette: palette_bytes,
        indices,
        hival: (palette.len() - 1) as u8,
        colorspace: b"DeviceRGB".to_vec(),
    })
}

/// Distinct 24-bit colors and their occurrence counts, in first-seen order.
struct Histogram {
    colors: Vec<[u8; 3]>,
    counts: Vec<u32>,
}

fn build_histogram(rgb: &[u8]) -> Histogram {
    let mut index: HashMap<[u8; 3], u32> = HashMap::new();
    let mut colors = Vec::new();
    let mut counts = Vec::new();
    for px in rgb.as_chunks::<3>().0 {
        let c = *px;
        match index.entry(c) {
            std::collections::hash_map::Entry::Occupied(e) => {
                counts[*e.get() as usize] += 1;
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(colors.len() as u32);
                colors.push(c);
                counts.push(1);
            }
        }
    }
    Histogram { colors, counts }
}

/// Deterministic median-cut quantization of a histogram to ≤256 colors.
/// Boxes are split along their longest channel at the count-weighted
/// median; every tie is broken on the color bytes, so the output is
/// reproducible run-to-run (serial and parallel runs stay byte-identical).
fn median_cut(hist: &Histogram) -> Vec<[u8; 3]> {
    let n = hist.colors.len();
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut boxes: Vec<(usize, usize)> = vec![(0, n)];

    while boxes.len() < 256 {
        let mut split = None;
        'outer: for b in 0..boxes.len() {
            let (lo, hi) = boxes[b];
            if hi - lo < 2 {
                continue;
            }
            let mut min_c = [255u8; 3];
            let mut max_c = [0u8; 3];
            for &ci in &order[lo..hi] {
                let c = hist.colors[ci as usize];
                for ch in 0..3 {
                    min_c[ch] = min_c[ch].min(c[ch]);
                    max_c[ch] = max_c[ch].max(c[ch]);
                }
            }
            // Longest channel (ties resolve to the highest channel via
            // `max_by_key`, which returns the last maximum).
            let (channel, _) = (0..3)
                .map(|ch| (ch, max_c[ch] as u32 - min_c[ch] as u32))
                .max_by_key(|&(_, span)| span)
                .expect("non-empty box");
            if max_c[channel] == min_c[channel] {
                continue; // all colors identical → unsplittable
            }
            order[lo..hi].sort_by(|&a, &b| {
                let ca = hist.colors[a as usize];
                let cb = hist.colors[b as usize];
                ca[channel].cmp(&cb[channel]).then_with(|| ca.cmp(&cb))
            });
            let total: u64 = order[lo..hi]
                .iter()
                .map(|&ci| hist.counts[ci as usize] as u64)
                .sum();
            let mut acc = 0u64;
            let mut cut = hi;
            for (k, &ci) in order[lo..hi].iter().enumerate() {
                acc += hist.counts[ci as usize] as u64;
                if acc * 2 >= total {
                    cut = lo + k + 1;
                    break;
                }
            }
            if cut <= lo || cut >= hi {
                continue; // weighted median at the edge → unsplittable
            }
            boxes[b] = (lo, cut);
            boxes.push((cut, hi));
            split = Some(b);
            break 'outer;
        }
        if split.is_none() {
            break;
        }
    }

    boxes
        .iter()
        .map(|&(lo, hi)| {
            let mut sum = [0u64; 3];
            let mut cnt = 0u64;
            for &ci in &order[lo..hi] {
                let c = hist.colors[ci as usize];
                let w = hist.counts[ci as usize] as u64;
                for ch in 0..3 {
                    sum[ch] += c[ch] as u64 * w;
                }
                cnt += w;
            }
            [
                (sum[0] / cnt) as u8,
                (sum[1] / cnt) as u8,
                (sum[2] / cnt) as u8,
            ]
        })
        .collect()
}

/// Map every pixel to its nearest palette entry (squared RGB distance),
/// memoized per distinct color so flat regions pay for the search once.
fn map_to_palette(rgb: &[u8], palette: &[[u8; 3]]) -> Vec<u8> {
    let mut memo: HashMap<[u8; 3], u8> = HashMap::new();
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|px| {
            let c = *px;
            *memo.entry(c).or_insert_with(|| {
                palette
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, p)| {
                        let dr = p[0] as i32 - c[0] as i32;
                        let dg = p[1] as i32 - c[1] as i32;
                        let db = p[2] as i32 - c[2] as i32;
                        dr * dr + dg * dg + db * db
                    })
                    .map(|(i, _)| i as u8)
                    .unwrap_or(0)
            })
        })
        .collect()
}

/// Resize two RGB buffers to the project's native comparison window: the
/// long side capped at 512 px with the same Triangle filter the render
/// witnesses use, so every fidelity metric is measured at one consistent
/// resolution. `width`/`height` are the buffers' source dimensions.
fn native_windows(
    a: &[u8],
    b: &[u8],
    width: u32,
    height: u32,
) -> (image::RgbImage, image::RgbImage) {
    let long = width.max(height);
    let n = long.min(512);
    let to_rgb = |pixels: &[u8]| {
        let img =
            image::RgbImage::from_raw(width, height, pixels.to_vec()).expect("validated RGB dims");
        image::imageops::resize(&img, n, n, image::imageops::FilterType::Triangle)
    };
    (to_rgb(a), to_rgb(b))
}

/// Rec. 601 luma reduction of a resized RGB window — the same conversion
/// the render witnesses use.
fn luma_of(img: &image::RgbImage) -> image::GrayImage {
    image::DynamicImage::ImageRgb8(img.clone()).to_luma8()
}

/// SSIM of two equal-size luma images.
fn ssim_luma(a: &image::GrayImage, b: &image::GrayImage) -> f64 {
    let count = (a.width() * a.height()) as f64;
    let mean = |img: &image::GrayImage| img.pixels().map(|p| p[0] as f64).sum::<f64>() / count;
    let (ma, mb) = (mean(a), mean(b));
    let var = |img: &image::GrayImage, m: f64| {
        img.pixels().map(|p| (p[0] as f64 - m).powi(2)).sum::<f64>() / count
    };
    let (va, vb) = (var(a, ma), var(b, mb));
    let cov = a
        .pixels()
        .zip(b.pixels())
        .map(|(x, y)| (x[0] as f64 - ma) * (y[0] as f64 - mb))
        .sum::<f64>()
        / count;
    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

/// Mean luma SSIM between two equal-sized RGB rasters, on a window of at
/// most 512 px on the long edge — the project's native-image witness scale
/// (see `benches/docker/native_image_ssim.py`).
fn ssim_window(a: &[u8], b: &[u8], width: u32, height: u32) -> f64 {
    let (a, b) = native_windows(a, b, width, height);
    ssim_luma(&luma_of(&a), &luma_of(&b))
}

/// Identity of one image stream for coalescing: canonical dict bytes (sans
/// `/Length` and `/Name`) plus the exact payload.
type ImageIdentity = (Arc<[u8]>, Arc<[u8]>);

/// Collapse image streams that are semantically identical — same dictionary
/// (modulo `/Length`, which is recomputed on save, and `/Name`, a cosmetic
/// hint the page's resource key overrides) and same payload — onto one
/// canonical object, and rewrite every reference to point at it. This is the
/// *storage* half of duplicate handling: the dedup cache already made
/// identical images encode once; this pass makes them be stored once.
/// Rendering is unchanged — identical streams render identically, and PDF
/// allows any number of pages to share one XObject.
///
/// Returns the number of redundant objects removed.
fn coalesce_image_objects(doc: &mut Document) -> usize {
    // Group image streams by (canonical dict bytes, payload).
    let objects = &doc.objects;
    let mut groups: HashMap<ImageIdentity, Vec<ObjectId>> = HashMap::new();
    for (id, obj) in objects.iter() {
        let Object::Stream(s) = obj else {
            continue;
        };
        if !is_image_stream(s) {
            continue;
        }
        let mut dict = s.dict.clone();
        dict.remove(b"Length");
        dict.remove(b"Name");
        let mut canon = Vec::new();
        let mut visited = std::collections::HashSet::new();
        canonical_object(&Object::Dictionary(dict), &mut canon, objects, &mut visited);
        groups
            .entry((Arc::from(canon), Arc::from(s.content.as_slice())))
            .or_default()
            .push(*id);
    }

    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut removed = 0;
    for group in groups.values() {
        if group.len() < 2 {
            continue;
        }
        let canonical = *group.iter().min().expect("non-empty group");
        for &dup in group.iter().filter(|&&d| d != canonical) {
            replace.insert(dup, canonical);
        }
        removed += group.len() - 1;
    }
    if replace.is_empty() {
        return 0;
    }

    // Rewrite every reference in the whole object graph and the trailer
    // (not just objects reachable from /Root), so no dangling reference
    // survives.
    for obj in doc.objects.values_mut() {
        rewrite_references(obj, &replace);
    }
    for (_, v) in doc.trailer.iter_mut() {
        rewrite_references(v, &replace);
    }
    for dup in replace.keys() {
        doc.objects.remove(dup);
    }
    removed
}

pub(crate) fn rewrite_references(obj: &mut Object, replace: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if let Some(new_id) = replace.get(id) {
                *id = *new_id;
            }
        }
        Object::Array(a) => {
            for e in a.iter_mut() {
                rewrite_references(e, replace);
            }
        }
        Object::Dictionary(d) => {
            for (_, v) in d.iter_mut() {
                rewrite_references(v, replace);
            }
        }
        Object::Stream(s) => {
            for (_, v) in s.dict.iter_mut() {
                rewrite_references(v, replace);
            }
        }
        _ => {}
    }
}

/// Canonical, order-independent serialization of a PDF object tree, used to
/// group identical images for coalescing. Keys are sorted and every
/// variable-length item is length-prefixed, so two dictionaries that are
/// equal under PDF semantics always produce identical bytes regardless of
/// key insertion order.
///
/// Indirect references are *followed*: two images whose `/ColorSpace` (or
/// `/SMask`, `/DecodeParms`) entries point at separate objects with
/// identical content are semantically identical — the reference ids are a
/// serialization artifact, not a semantic difference. `visited` guards
/// against reference cycles (path-based, so a shared object is
/// canonicalized identically each time it is reached); a cycle or an
/// unresolvable id falls back to the raw id bytes.
pub(crate) fn canonical_object(
    obj: &Object,
    out: &mut Vec<u8>,
    objects: &std::collections::BTreeMap<ObjectId, Object>,
    visited: &mut std::collections::HashSet<ObjectId>,
) {
    match obj {
        Object::Null => out.push(0),
        Object::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Object::Integer(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Object::Real(f) => {
            out.push(3);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Object::Name(n) => {
            out.push(4);
            out.extend_from_slice(&(n.len() as u32).to_le_bytes());
            out.extend_from_slice(n);
        }
        Object::String(s, _) => {
            out.push(5);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s);
        }
        Object::Array(a) => {
            out.push(7);
            for e in a {
                canonical_object(e, out, objects, visited);
            }
        }
        Object::Dictionary(d) => {
            out.push(8);
            let mut entries: Vec<(&Vec<u8>, &Object)> = d.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                out.extend_from_slice(&(k.len() as u32).to_le_bytes());
                out.extend_from_slice(k);
                canonical_object(v, out, objects, visited);
            }
        }
        Object::Stream(s) => {
            out.push(9);
            let mut dict = s.dict.clone();
            dict.remove(b"Length");
            canonical_object(&Object::Dictionary(dict), out, objects, visited);
            out.extend_from_slice(&(s.content.len() as u32).to_le_bytes());
            out.extend_from_slice(&s.content);
        }
        Object::Reference((n, g)) => {
            let id = (*n, *g);
            if visited.insert(id) {
                if let Some(target) = objects.get(&id) {
                    canonical_object(target, out, objects, visited);
                    visited.remove(&id);
                    return;
                }
                visited.remove(&id);
            }
            // Unresolvable or cyclic: fall back to the raw id bytes.
            out.push(10);
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(&g.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_histogram, calibrated_quality, indexed_candidate, median_cut};

    /// A tiny photo-ish gradient with per-pixel noise (photographic content).
    fn photoish(w: u32, h: u32) -> Vec<u8> {
        let mut next: u64 = 42;
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                next ^= next << 13;
                next ^= next >> 7;
                next ^= next << 17;
                let n = (next & 0x1f) as u8;
                let (r, g, b) = (
                    (x as f32 / w as f32 * 255.0) as u8,
                    (y as f32 / h as f32 * 255.0) as u8,
                    (128.0 + 80.0 * ((x as f32 + y as f32) / 32.0).sin()) as u8,
                );
                v.extend_from_slice(&[r.wrapping_add(n), g.wrapping_add(n), b.wrapping_add(n)]);
            }
        }
        v
    }

    #[test]
    fn palette_exact_is_lossless_and_reconstructs() {
        // Flat figure: 4 solid colors -> exact palette, indices reproduce the
        // source byte-for-byte (no fidelity gate needed).
        let mut pixels = Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                let c = match (x < 4, y < 4) {
                    (true, true) => [255, 0, 0],
                    (true, false) => [0, 255, 0],
                    (false, true) => [0, 0, 255],
                    (false, false) => [255, 255, 255],
                };
                pixels.extend_from_slice(&c);
            }
        }
        let cand = indexed_candidate(&pixels, 8, 8).expect("flat figure must qualify");
        assert_eq!(cand.palette.len(), 4 * 3, "4 unique colors");
        assert_eq!(cand.hival, 3);
        let rebuilt: Vec<u8> = cand
            .indices
            .iter()
            .flat_map(|&i| {
                let c = &cand.palette[i as usize * 3..i as usize * 3 + 3];
                [c[0], c[1], c[2]]
            })
            .collect();
        assert_eq!(rebuilt, pixels, "exact palette must be lossless");
    }

    #[test]
    fn palette_median_cut_is_deterministic_and_gated() {
        // A smooth gradient has many unique colors -> median-cut path. Two
        // runs must produce identical palettes and indices (determinism, so
        // serial and parallel runs stay byte-identical).
        let (w, h) = (48u32, 48u32);
        let mut pixels = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let c = [
                    (x as f32 / w as f32 * 255.0) as u8,
                    (y as f32 / h as f32 * 255.0) as u8,
                    ((x as f32 + y as f32) / (w as f32 + h as f32) * 255.0) as u8,
                ];
                pixels.extend_from_slice(&c);
            }
        }
        let hist = build_histogram(&pixels);
        assert!(
            hist.colors.len() > 256,
            "gradient must exceed the exact limit"
        );
        let a = median_cut(&hist);
        let b = median_cut(&hist);
        assert_eq!(a, b, "median cut must be deterministic");
        assert!(a.len() <= 256);

        // The candidate itself may pass or fail the SSIM gate depending on
        // how the palette samples the gradient; either way it must not panic
        // and must produce a well-formed candidate or None.
        let cand = indexed_candidate(&pixels, w, h);
        if let Some(c) = cand {
            assert_eq!(c.indices.len(), (w * h) as usize, "one index per pixel");
        }
    }

    #[test]
    fn palette_rejects_photoish_noise() {
        // Photographic content (gradient + per-pixel noise) quantizes to
        // banding; the 0.9999 native-SSIM gate must reject it, so photos can
        // never be palette-converted.
        let (w, h) = (64u32, 64u32);
        let pixels = photoish(w, h);
        assert!(indexed_candidate(&pixels, w, h).is_none());
    }

    #[test]
    fn calibration_maps_ssim_targets_to_quality() {
        // The two documented presets land in the aggressive range.
        assert_eq!(calibrated_quality(0.86), 9);
        assert_eq!(calibrated_quality(0.72), 6);
        // Targets above the measured curve clamp to the top quality.
        assert!(calibrated_quality(0.9999) >= 75);
        assert_eq!(calibrated_quality(1.0), 75);
        // Targets below the curve clamp to the floor.
        assert_eq!(calibrated_quality(0.01), 5);
        // Monotonic: a stricter target never yields a lower quality.
        let mut prev = 0;
        for t in [0.5, 0.7, 0.86, 0.95, 0.99] {
            let q = calibrated_quality(t);
            assert!(q >= prev, "q({t})={q} < previous {prev}");
            prev = q;
        }
    }
}
