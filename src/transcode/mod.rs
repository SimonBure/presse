//! The CPU JPEG re-encoding backend.
//!
//! [`CpuTranscoder`] is a rayon-parallel JPEG re-encoder — the only explicit
//! fixed-size block loop is the RGBA→RGB normalization pass; there is no
//! hand-written SIMD, the codecs are `image`-crate based and may use SIMD
//! internally, and `-C target-cpu=native` lets LLVM auto-vectorize the rest.
//! Callers are generic over [`ImageTranscoder`], so the CPU backend
//! monomorphizes — zero dynamic dispatch on the default path.

use std::fmt;
use std::sync::{Arc, OnceLock};

/// Decoded pixels ready for re-encoding.
#[derive(Debug, Clone, Copy)]
pub enum ImageRef<'a> {
    /// Single-channel (grayscale) pixels.
    Luma8 {
        width: u32,
        height: u32,
        bytes: &'a [u8],
    },
    /// Interleaved RGB pixels.
    Rgb8 {
        width: u32,
        height: u32,
        bytes: &'a [u8],
    },
}

/// The input to one transcode call.
#[derive(Debug, Clone, Copy)]
pub enum Input<'a> {
    /// JPEG/JPX stream bytes (a `/DCTDecode` image stream).
    Jpeg(&'a [u8]),
    /// Decoded pixels of a raw stream, after any channel normalization.
    Pixels(ImageRef<'a>),
}

/// Transcoding failure. Safe to surface to the user; the caller decides
/// whether to skip the stream.
#[derive(Debug, Clone)]
pub enum TranscodeError {
    /// The input could not be decoded.
    Decode(String),
    /// Encoding produced no usable output.
    Encode(String),
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranscodeError::Decode(m) => write!(f, "decode failed: {m}"),
            TranscodeError::Encode(m) => write!(f, "encode failed: {m}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

/// Pluggable JPEG re-encoding backend.
pub trait ImageTranscoder: Send + Sync {
    /// Re-encode one image stream at the target quality, returning the new
    /// JPEG payload. Implementations must be deterministic for a given input.
    fn transcode_image(&self, input: &Input, quality: u8) -> Result<Vec<u8>, TranscodeError>;
}

/// The default backend: CPU JPEG decode/encode. Two encoder paths are
/// available:
///
/// - **`image` crate (default, [`CpuTranscoder::default`])** — 4:4:4 chroma
///   (its `JpegEncoder` writes full-resolution Cb/Cr). Byte-identical to the
///   pre-`--jpeg-encoder` behavior.
/// - **`jpeg-encoder` crate (`--jpeg-encoder`)** — YCbCr **4:2:0 with
///   box-averaged chroma downsampling**, matching libjpeg/libjpeg-turbo's
///   default RGB pipeline (`jpeg_set_defaults` → 2×2 luminance, 1×1 chroma;
///   `h2v2_downsample`). That is the model Ghostscript and qpdf actually
///   use, and it encodes half the DCT chroma blocks of 4:4:4 — the dominant
///   size difference behind qpdf's lead on scan corpora (irs_fw2: 2.00 →
///   ~1.5 MB at q30). The `simd` feature gives it runtime-detected AVX2
///   DCT/quantization under `-C target-cpu=native`.
///
/// Grayscale payloads stay single-component JPEGs on both paths (see
/// [`encode_jpeg`] / [`encode_jpeg_420`]), matching `/DeviceGray` streams.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTranscoder {
    /// Use the `jpeg-encoder` codec (4:2:0 box-averaged) instead of the
    /// `image` crate's 4:4:4 encoder. Default `false` keeps the output
    /// byte-identical to the pre-flag behavior.
    native_jpeg: bool,
}

impl CpuTranscoder {
    /// `native_jpeg: true` selects the `--jpeg-encoder` 4:2:0 codec.
    pub fn new(native_jpeg: bool) -> Self {
        Self { native_jpeg }
    }
}

impl ImageTranscoder for CpuTranscoder {
    fn transcode_image(&self, input: &Input, quality: u8) -> Result<Vec<u8>, TranscodeError> {
        let img = match input {
            Input::Jpeg(bytes) => image::load_from_memory(bytes)
                .map_err(|e| TranscodeError::Decode(format!("jpeg decode: {e}")))?,
            Input::Pixels(ImageRef::Luma8 {
                width,
                height,
                bytes,
            }) => image::GrayImage::from_raw(*width, *height, bytes.to_vec())
                .map(image::DynamicImage::ImageLuma8)
                .ok_or_else(|| TranscodeError::Decode("bad grayscale dimensions".into()))?,
            Input::Pixels(ImageRef::Rgb8 {
                width,
                height,
                bytes,
            }) => image::RgbImage::from_raw(*width, *height, bytes.to_vec())
                .map(image::DynamicImage::ImageRgb8)
                .ok_or_else(|| TranscodeError::Decode("bad RGB dimensions".into()))?,
        };
        let mut out = Vec::new();
        if self.native_jpeg {
            // The jpeg-encoder API is u16-addressed; a raster wider or taller
            // than 65535 px falls back to the image encoder rather than
            // wrapping (impossible in practice — a 65k×65k image is 12 GB —
            // but a pathological PDF must not panic or emit a corrupt JPEG).
            if img.width() <= u16::MAX as u32 && img.height() <= u16::MAX as u32 {
                encode_jpeg_420(&mut out, &img, quality)?;
                return Ok(out);
            }
        }
        encode_jpeg(&mut out, &img, quality);
        Ok(out)
    }
}

/// Encode `img` as JPEG at the given quality, preserving grayscale payloads.
///
/// `DynamicImage` always presents an RGB pixel type to the encoder, so a
/// Luma8 payload would be written as a 3-component JPEG — invalid inside a
/// `/DeviceGray` stream and rendered as washed-out garbage by viewers.
/// Encoding the concrete `GrayImage` keeps the JPEG at a single component.
pub(crate) fn encode_jpeg(out: &mut Vec<u8>, img: &image::DynamicImage, quality: u8) {
    use image::codecs::jpeg::JpegEncoder;
    let mut encoder = JpegEncoder::new_with_quality(std::io::Cursor::new(out), quality);
    match img {
        image::DynamicImage::ImageLuma8(gray) => {
            let _ = encoder.encode_image(gray);
        }
        other => {
            let _ = encoder.encode_image(other);
        }
    }
}

/// Encode `img` as JPEG with libjpeg-compatible **4:2:0** chroma
/// subsampling (box-averaged, `h2v2_downsample`) — the `--jpeg-encoder`
/// path. Grayscale stays a single-component JPEG, matching `/DeviceGray`
/// streams; RGB goes through YCbCr 4:2:0 exactly like libjpeg's default
/// `jpeg_set_defaults` pipeline that qpdf and Ghostscript use.
///
/// The AVX2 `simd` feature is compiled in and dispatch is runtime-detected,
/// so under `-C target-cpu=native` the DCT/quantization loops use the same
/// instruction set as the rest of the pipeline.
pub(crate) fn encode_jpeg_420(
    out: &mut Vec<u8>,
    img: &image::DynamicImage,
    quality: u8,
) -> Result<(), TranscodeError> {
    use jpeg_encoder::{ChromaSubsamplingMethod, ColorType, Encoder, SamplingFactor};
    let (w, h) = (img.width() as u16, img.height() as u16);
    let mut encoder = Encoder::new(out, quality);
    encoder.set_sampling_factor(SamplingFactor::R_4_2_0);
    encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
    match img {
        image::DynamicImage::ImageLuma8(gray) => encoder
            .encode(gray.as_raw(), w, h, ColorType::Luma)
            .map_err(|e| TranscodeError::Encode(format!("jpeg-encoder (luma): {e}"))),
        other => {
            let rgb = other.to_rgb8();
            encoder
                .encode(&rgb, w, h, ColorType::Rgb)
                .map_err(|e| TranscodeError::Encode(format!("jpeg-encoder (rgb): {e}")))
        }
    }
}

/// Semantic identity of one transcode request: everything that determines
/// the output JPEG, independent of which object happened to carry the
/// pixels.
///
/// The identity is the *encoder input*, not the source stream — two streams
/// that decode to identical pixels at the same size share one entry and one
/// encode (e.g. a `/FlateDecode` and an `/LZWDecode` twin, or a 4-byte
/// `DeviceRGB` stream normalized to 3 bytes). Width and height are part of
/// the key because two images can carry identical pixel bytes at different
/// dimensions (RGB 10×10 and RGB 20×5 are both 300 bytes) and must still
/// emit JPEGs with their own headers. Equality is decided on the exact
/// content bytes, never on a hash alone: the map may hash the key for
/// placement, but a hash collision can never substitute one image's JPEG for
/// another's.
///
/// The `Hash` impl is deliberately bounded ([`HASH_PREFIX_BYTES`] +
/// [`HASH_SUFFIX_BYTES`] of the content, plus the length) so a cache lookup
/// never hashes a whole multi-MB scan: the common case (a unique image)
/// pays a few KiB of hashing instead of the full payload, and equality
/// short-circuits on the length before touching bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheKey {
    /// Target quality.
    quality: u8,
    /// Input kind: 1 = luma pixels, 2 = JPEG bytes, 3 = RGB pixels. Pins
    /// the encoder-input pixel format together with the content bytes.
    kind: u8,
    /// Raster width of the encoder input.
    width: u32,
    /// Raster height of the encoder input.
    height: u32,
    /// Exact encoder-input bytes (content identity).
    content: Arc<[u8]>,
}

/// Bytes of the content slice fed to the key hash — the fast-reject bound.
/// Both ends are hashed because JPEG streams share identical SOI / marker /
/// table prefixes (and EOI suffixes) for same-encoder, same-quality output,
/// so a single prefix would cluster every such image into one bucket.
const HASH_PREFIX_BYTES: usize = 4096;
const HASH_SUFFIX_BYTES: usize = 4096;

impl std::hash::Hash for CacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.quality.hash(state);
        self.kind.hash(state);
        self.width.hash(state);
        self.height.hash(state);
        let bytes = &*self.content;
        let n = bytes.len();
        n.hash(state);
        if n <= HASH_PREFIX_BYTES + HASH_SUFFIX_BYTES {
            bytes.hash(state);
        } else {
            bytes[..HASH_PREFIX_BYTES].hash(state);
            bytes[n - HASH_SUFFIX_BYTES..].hash(state);
        }
    }
}

/// Build a [`CacheKey`] for one transcode request.
pub(crate) fn encode_key(quality: u8, kind: u8, width: u32, height: u32, bytes: &[u8]) -> CacheKey {
    CacheKey {
        quality,
        kind,
        width,
        height,
        content: Arc::from(bytes),
    }
}

/// Shared handle for caching transcode results across duplicate streams.
/// Empty buffers and errors are cached too, so every duplicate image object
/// reproduces the same decision without re-running the backend.
///
/// The map lock is held only for the find-or-insert of the (cheap) entry;
/// the encode itself runs outside it, and the per-entry [`OnceLock`] makes
/// the first producer's result visible to every worker that raced on the
/// same key — `get_or_init` blocks the losers until the winner finishes, so
/// a given key is computed exactly once even under full contention.
pub(crate) struct TranscodeCache {
    map: std::sync::Mutex<CacheMap>,
}

type CacheMap = std::collections::HashMap<CacheKey, Entry>;

/// One cached transcode result, shared by every worker that hit the same key.
type CachedResult = Arc<Result<Vec<u8>, TranscodeError>>;

struct Entry {
    /// `OnceLock` for once-only production; the outer `Arc` is the handle we
    /// can take out of the map lock so callers never hold it while encoding.
    value: Arc<OnceLock<CachedResult>>,
}

impl Entry {
    fn new() -> Self {
        Self {
            value: Arc::new(OnceLock::new()),
        }
    }
}

impl TranscodeCache {
    pub(crate) fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub(crate) fn get(
        &self,
        key: CacheKey,
        produce: impl FnOnce() -> Result<Vec<u8>, TranscodeError>,
    ) -> Arc<Result<Vec<u8>, TranscodeError>> {
        let value = {
            let mut guard = self.map.lock().unwrap();
            Arc::clone(&guard.entry(key).or_insert_with(Entry::new).value)
        };
        Arc::clone(value.get_or_init(|| Arc::new(produce())))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CacheKey, TranscodeCache, TranscodeError, encode_jpeg, encode_jpeg_420, encode_key,
    };
    use std::hash::Hash;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Photo-ish RGB pixels (smooth gradient + grain).
    fn photoish_rgb(w: u32, h: u32) -> Vec<u8> {
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

    /// Parse the SOF0 frame header of a JPEG and return each component's
    /// (horizontal, vertical) sampling factors.
    fn sampling_factors(jpeg: &[u8]) -> Vec<(u8, u8)> {
        let mut i = 2;
        while i + 4 <= jpeg.len() {
            if jpeg[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = jpeg[i + 1];
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
            {
                let components = jpeg[i + 9];
                let mut out = Vec::with_capacity(components as usize);
                let mut j = i + 10;
                for _ in 0..components {
                    let s = jpeg[j + 1];
                    out.push((s >> 4, s & 0x0F));
                    j += 3;
                }
                return out;
            }
            i += 2 + u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        }
        Vec::new()
    }

    #[test]
    fn cache_key_distinguishes_dimensions_with_identical_bytes() {
        // RGB 10×10 and RGB 20×5 both hold 300 identical bytes; their keys
        // must differ so a cached 10×10 JPEG is never handed to the 20×5
        // image (each would carry the wrong /Width /Height header).
        let bytes = vec![7u8; 300];
        let a = encode_key(50, 3, 10, 10, &bytes);
        let b = encode_key(50, 3, 20, 5, &bytes);
        assert_ne!(a, b);

        // and the cache keeps them as distinct entries
        let cache = TranscodeCache::new();
        let ra = cache.get(a, || Ok(vec![1]));
        let rb = cache.get(b, || Ok(vec![2]));
        assert_eq!(ra.as_ref().as_deref().unwrap(), &[1][..]);
        assert_eq!(rb.as_ref().as_deref().unwrap(), &[2][..]);
    }

    #[test]
    fn cache_key_distinguishes_quality_kind_and_content() {
        let bytes = vec![7u8; 12];
        let other = vec![8u8; 12];
        assert_ne!(
            encode_key(30, 3, 4, 3, &bytes),
            encode_key(50, 3, 4, 3, &bytes)
        );
        assert_ne!(
            encode_key(50, 1, 4, 3, &bytes),
            encode_key(50, 3, 4, 3, &bytes)
        );
        assert_ne!(
            encode_key(50, 3, 4, 3, &bytes),
            encode_key(50, 3, 4, 3, &other)
        );
    }

    /// A `Hasher` that counts the bytes fed to it, to pin the bounded-hash
    /// fast-reject: hashing a large key must not touch the whole payload.
    #[derive(Default)]
    struct CountingHasher {
        bytes: usize,
    }

    impl std::hash::Hasher for CountingHasher {
        fn finish(&self) -> u64 {
            self.bytes as u64
        }
        fn write(&mut self, bytes: &[u8]) {
            self.bytes += bytes.len();
        }
    }

    fn hash_cost(key: &CacheKey) -> usize {
        let mut h = CountingHasher::default();
        key.hash(&mut h);
        h.bytes
    }

    #[test]
    fn cache_key_hash_is_bounded_to_prefix_and_suffix() {
        use super::{HASH_PREFIX_BYTES, HASH_SUFFIX_BYTES};
        // A 28 MB payload must hash only the bounded window (both ends), not
        // the whole scan.
        let big = vec![0xA5u8; 28 * 1024 * 1024];
        let cost = hash_cost(&encode_key(50, 2, 4000, 2800, &big));
        assert!(
            cost <= HASH_PREFIX_BYTES + HASH_SUFFIX_BYTES + 64,
            "hashing a 28 MB key must be bounded: {cost} bytes hashed"
        );

        // A small payload is hashed in full (nothing to bound).
        let small = vec![0xA5u8; 100];
        assert!(hash_cost(&encode_key(50, 3, 10, 10, &small)) >= 100);
    }

    #[test]
    fn cache_key_middle_bytes_still_distinguish_keys() {
        // The bounded hash must not create false dedup: two keys that share
        // the same 4 KiB prefix and suffix but differ in the middle are
        // different images and must produce different results through the
        // cache (equality is on the exact bytes, so they land in distinct
        // entries even if the hash collides).
        let mut a = vec![0x11u8; 100_000];
        let mut b = vec![0x11u8; 100_000];
        a[50_000] = 0x01;
        b[50_000] = 0x02;
        let ka = encode_key(50, 2, 400, 250, &a);
        let kb = encode_key(50, 2, 400, 250, &b);
        assert_ne!(ka, kb, "different middle bytes must not be equal");

        let cache = TranscodeCache::new();
        let ra = cache.get(ka, || Ok(vec![1]));
        let rb = cache.get(kb, || Ok(vec![2]));
        assert_eq!(ra.as_ref().as_deref().unwrap(), &[1][..]);
        assert_eq!(rb.as_ref().as_deref().unwrap(), &[2][..]);
    }

    #[test]
    fn cache_produces_once_for_repeated_keys() {
        let cache = TranscodeCache::new();
        let produced = AtomicUsize::new(0);
        let key = encode_key(50, 3, 4, 3, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        for _ in 0..4 {
            let r = cache.get(key.clone(), || {
                produced.fetch_add(1, Ordering::SeqCst);
                Ok(b"jpeg".to_vec())
            });
            assert_eq!(r.as_ref().as_deref().unwrap(), b"jpeg");
        }
        assert_eq!(produced.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_converges_on_one_computation_under_contention() {
        // Workers that race on the same key must all converge on the single
        // encode the winner performs, not each run their own.
        let cache = TranscodeCache::new();
        let produced = Arc::new(AtomicUsize::new(0));
        let key = encode_key(50, 2, 64, 64, &[9; 128]);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let cache = &cache;
                let key = key.clone();
                let produced = Arc::clone(&produced);
                s.spawn(move || {
                    let r = cache.get(key, || {
                        produced.fetch_add(1, Ordering::SeqCst);
                        Ok(vec![4; 16])
                    });
                    assert_eq!(r.as_ref().as_deref().unwrap(), &[4; 16][..]);
                });
            }
        });
        assert_eq!(produced.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_caches_errors_and_empty_outputs() {
        let cache = TranscodeCache::new();
        let produced = AtomicUsize::new(0);
        let err_key = encode_key(50, 3, 2, 2, &[0; 12]);
        let empty_key = encode_key(50, 3, 2, 2, &[1; 12]);

        // errors are cached: the second lookup does not re-run the backend
        let r1 = cache.get(err_key.clone(), || {
            produced.fetch_add(1, Ordering::SeqCst);
            Err(TranscodeError::Decode("boom".into()))
        });
        assert!(r1.is_err());
        let r2 = cache.get(err_key, || {
            produced.fetch_add(1, Ordering::SeqCst);
            Err(TranscodeError::Decode("boom".into()))
        });
        assert!(r2.is_err());
        assert_eq!(produced.load(Ordering::SeqCst), 1);

        // empty buffers are cached too, so duplicates make the same decision
        let e1 = cache.get(empty_key.clone(), || {
            produced.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        });
        assert!(e1.as_ref().as_deref().unwrap().is_empty());
        let e2 = cache.get(empty_key, || {
            produced.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        });
        assert!(e2.as_ref().as_deref().unwrap().is_empty());
        assert_eq!(produced.load(Ordering::SeqCst), 2);
    }

    /// The 4:2:0 codec must write YCbCr 2×2/1×1/1×1 sampling factors —
    /// libjpeg's default — while the `image` encoder writes 4:4:4 (1×1/1×1/1×1).
    #[test]
    fn jpeg_encoder_420_writes_libjpeg_sampling_factors() {
        let img = image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(128, 96, photoish_rgb(128, 96)).unwrap(),
        );
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode_jpeg(&mut a, &img, 50);
        encode_jpeg_420(&mut b, &img, 50).unwrap();
        assert_eq!(
            sampling_factors(&a),
            vec![(1, 1), (1, 1), (1, 1)],
            "image crate = 4:4:4"
        );
        assert_eq!(
            sampling_factors(&b),
            vec![(2, 2), (1, 1), (1, 1)],
            "jpeg-encoder = 4:2:0"
        );
    }

    /// Same quality, same pixels: 4:2:0 encodes half the chroma DCT blocks of
    /// 4:4:4 and must be materially smaller on RGB content.
    #[test]
    fn jpeg_encoder_420_is_smaller_than_444_on_rgb() {
        let img = image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(256, 192, photoish_rgb(256, 192)).unwrap(),
        );
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode_jpeg(&mut a, &img, 50);
        encode_jpeg_420(&mut b, &img, 50).unwrap();
        assert!(
            b.len() < a.len(),
            "4:2:0 must beat 4:4:4 at the same quality: {} vs {}",
            b.len(),
            a.len()
        );
        // Both decode back to the same raster size (a wrapped/shrunk chroma
        // plane must not change geometry).
        let (da, db) = (
            image::load_from_memory(&a).unwrap(),
            image::load_from_memory(&b).unwrap(),
        );
        assert_eq!((da.width(), da.height()), (db.width(), db.height()));
    }

    /// Grayscale stays a single-component JPEG on the 4:2:0 path — a
    /// 3-component JPEG inside a /DeviceGray stream renders as garbage.
    #[test]
    fn jpeg_encoder_420_keeps_grayscale_single_component() {
        let mut gray = Vec::with_capacity(128 * 128);
        for y in 0..128u32 {
            for x in 0..128u32 {
                gray.push(((x as f32 / 128.0 + y as f32 / 128.0) * 127.5) as u8);
            }
        }
        let img =
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(128, 128, gray).unwrap());
        let mut out = Vec::new();
        encode_jpeg_420(&mut out, &img, 50).unwrap();
        assert_eq!(
            sampling_factors(&out),
            vec![(1, 1)],
            "luma stays 1-component"
        );
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.color(), image::ColorType::L8);
    }
}
