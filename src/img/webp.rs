//! Standalone WebP compression. Lossless only: presse has no lossy WebP
//! encoder, so `-q` does not apply.
//!
//! Candidates, smallest wins, and the input is kept when none is smaller:
//!
//! 1. **Stripped**: the original chunks minus metadata (lossless: the
//!    VP8/VP8L/ALPH/ANMF payloads are copied byte for byte).
//! 2. **Re-encoded** (lossless still images only): decoded and re-encoded
//!    by `image-webp`'s VP8L encoder. It has fewer coding tools than
//!    libwebp, so it only wins on files from fast encoders (libwebp
//!    method 0 and similar).
//!
//! Lossy (VP8) and animated files are only stripped.
//!
//! Metadata policy: the ICC profile and EXIF orientation are kept; the rest
//! of EXIF, XMP and unknown chunks are removed. `keep_metadata` keeps every
//! chunk.
//!
//! The VP8X header is rebuilt to match the chunks that remain, as libwebp's
//! muxer does: a still image left with nothing but its bitstream becomes a
//! simple-format file without VP8X.

use std::ops::Range;

use image::DynamicImage;
use image_webp::ColorType;

use super::{Outcome, exif};

// VP8X flag bits (libwebp `mux_types.h`).
const ICC: u8 = 0x20;
const ALPHA: u8 = 0x10;
const EXIF: u8 = 0x08;
const XMP: u8 = 0x04;
const ANIMATION: u8 = 0x02;

const VP8X_SIZE: usize = 10;

struct Chunk {
    tag: [u8; 4],
    data: Range<usize>,
}

/// Top-level chunks of a RIFF/WebP file. `None` unless the header is valid,
/// every chunk lies inside the RIFF size, the first chunk is VP8, VP8L or a
/// full VP8X, and an image (VP8, VP8L or animation frame) is present. Bytes
/// after the RIFF size are ignored.
fn parse_chunks(webp: &[u8]) -> Option<Vec<Chunk>> {
    if webp.get(..4)? != b"RIFF" || webp.get(8..12)? != b"WEBP" {
        return None;
    }
    let riff_size = u32::from_le_bytes(webp[4..8].try_into().ok()?) as usize;
    let end = riff_size.checked_add(8)?;
    if end > webp.len() {
        return None;
    }
    let mut chunks = Vec::new();
    let mut at = 12;
    while at < end {
        let header = webp.get(at..at.checked_add(8)?.min(end))?;
        let tag: [u8; 4] = header.get(..4)?.try_into().ok()?;
        let size = u32::from_le_bytes(header.get(4..8)?.try_into().ok()?) as usize;
        let start = at + 8;
        let stop = start.checked_add(size)?;
        if stop > end {
            return None;
        }
        chunks.push(Chunk {
            tag,
            data: start..stop,
        });
        at = stop + (size & 1);
    }
    let first = chunks.first()?;
    let valid_first = match &first.tag {
        b"VP8 " | b"VP8L" => true,
        b"VP8X" => first.data.len() >= VP8X_SIZE,
        _ => false,
    };
    let has_image = chunks
        .iter()
        .any(|c| matches!(&c.tag, b"VP8 " | b"VP8L" | b"ANMF"));
    (valid_first && has_image).then_some(chunks)
}

fn put_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) -> Option<()> {
    out.extend_from_slice(tag);
    out.extend_from_slice(&u32::try_from(data.len()).ok()?.to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    Some(())
}

/// Whether a VP8L bitstream declares an alpha channel.
fn vp8l_has_alpha(vp8l: &[u8]) -> bool {
    vp8l.get(1..5)
        .and_then(|b| b.try_into().ok())
        .is_some_and(|b: [u8; 4]| (u32::from_le_bytes(b) >> 28) & 1 == 1)
}

/// The file rebuilt with the metadata policy applied. `vp8l` replaces the
/// VP8L payload (the re-encoded candidate).
///
/// A metadata chunk counts only when its VP8X flag is set: libwebp ignores
/// it otherwise, so it is treated like an unknown chunk.
fn assemble(
    webp: &[u8],
    chunks: &[Chunk],
    keep_metadata: bool,
    vp8l: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let vp8x = chunks
        .first()
        .filter(|c| &c.tag == b"VP8X")
        .map(|c| &webp[c.data.clone()]);
    let flags = vp8x.map_or(0, |d| d[0]);
    let flagged = |bit: u8| flags & bit != 0;

    let orientation = if keep_metadata || !flagged(EXIF) {
        None
    } else {
        chunks
            .iter()
            .find(|c| &c.tag == b"EXIF")
            .and_then(|c| {
                let payload = &webp[c.data.clone()];
                // Some writers keep JPEG's `Exif\0\0` prefix.
                let tiff = payload.strip_prefix(b"Exif\0\0").unwrap_or(payload);
                exif::keepable(exif::orientation(tiff))
            })
            .map(exif::orientation_only)
    };

    let mut parts: Vec<(&[u8; 4], &[u8])> = Vec::new();
    let mut meta = 0u8;
    // Chunks only the extended format can hold.
    let mut extended = false;
    for c in &chunks[usize::from(vp8x.is_some())..] {
        let data = &webp[c.data.clone()];
        let keep = match &c.tag {
            b"VP8L" => {
                parts.push((&c.tag, vp8l.unwrap_or(data)));
                continue;
            }
            b"VP8 " => true,
            b"ALPH" | b"ANIM" | b"ANMF" => {
                extended = true;
                true
            }
            b"ICCP" if flagged(ICC) => {
                meta |= ICC;
                true
            }
            b"EXIF" if flagged(EXIF) => {
                if keep_metadata {
                    meta |= EXIF;
                    true
                } else {
                    if let Some(o) = orientation.as_deref()
                        && meta & EXIF == 0
                    {
                        meta |= EXIF;
                        parts.push((b"EXIF", o));
                    }
                    false
                }
            }
            b"XMP " if flagged(XMP) && keep_metadata => {
                meta |= XMP;
                true
            }
            // Unknown chunks, and metadata chunks readers ignore. A
            // simple-format file has no place for them.
            _ => {
                let keep = keep_metadata && vp8x.is_some();
                extended |= keep;
                keep
            }
        };
        if keep {
            parts.push((&c.tag, data));
        }
    }

    let mut out = Vec::with_capacity(webp.len());
    out.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    if let Some(vp8x) = vp8x
        && (meta != 0 || extended)
    {
        let alpha = match vp8l {
            Some(v) => vp8l_has_alpha(v),
            None => flagged(ALPHA),
        } || parts.iter().any(|(tag, _)| *tag == b"ALPH");
        let mut header = [0u8; VP8X_SIZE];
        header[0] = (flags & ANIMATION) | meta | if alpha { ALPHA } else { 0 };
        header[4..].copy_from_slice(&vp8x[4..VP8X_SIZE]); // canvas size
        put_chunk(&mut out, b"VP8X", &header)?;
    }
    for (tag, data) in parts {
        put_chunk(&mut out, tag, data)?;
    }
    let riff_size = u32::try_from(out.len() - 8).ok()?;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Some(out)
}

/// Pixels in the layout `image-webp`'s lossless encoder takes.
pub(crate) fn lossless_pixels(img: &DynamicImage) -> (Vec<u8>, ColorType) {
    match (img.color().has_color(), img.color().has_alpha()) {
        (false, false) => (img.to_luma8().into_raw(), ColorType::L8),
        (false, true) => (img.to_luma_alpha8().into_raw(), ColorType::La8),
        (true, false) => (img.to_rgb8().into_raw(), ColorType::Rgb8),
        (true, true) => (img.to_rgba8().into_raw(), ColorType::Rgba8),
    }
}

/// A new VP8L payload for a lossless still image.
fn reencoded_vp8l(webp: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let img = image::load_from_memory_with_format(webp, image::ImageFormat::WebP)?;
    let (data, color) = lossless_pixels(&img);
    let mut encoded = Vec::new();
    image_webp::WebPEncoder::new(&mut encoded).encode(&data, img.width(), img.height(), color)?;
    let chunks = parse_chunks(&encoded).ok_or("encoder produced no valid WebP")?;
    let c = chunks
        .iter()
        .find(|c| &c.tag == b"VP8L")
        .ok_or("encoder produced no VP8L chunk")?;
    Ok(encoded[c.data.clone()].to_vec())
}

pub(crate) fn press(
    webp: &[u8],
    keep_metadata: bool,
    verbose: bool,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let chunks = parse_chunks(webp).ok_or("not a valid WebP")?;
    let mut best: Option<(Vec<u8>, &'static str)> = None;
    let mut offer = |bytes: Vec<u8>, how: &'static str| {
        verbose!(
            verbose,
            "[webp] {how}: {} → {} bytes",
            webp.len(),
            bytes.len()
        );
        if best.as_ref().is_none_or(|(b, _)| bytes.len() < b.len()) {
            best = Some((bytes, how));
        }
    };

    if !keep_metadata && let Some(bytes) = assemble(webp, &chunks, false, None) {
        offer(bytes, "metadata stripped (lossless)");
    }

    let has = |tag: &[u8; 4]| chunks.iter().any(|c| &c.tag == tag);
    let flags = match &chunks[0].tag {
        b"VP8X" => webp[chunks[0].data.start],
        _ => 0,
    };
    let animated = flags & ANIMATION != 0 || has(b"ANIM") || has(b"ANMF");
    let kind = if animated {
        Some("animated")
    } else if has(b"VP8 ") || has(b"ALPH") || !has(b"VP8L") {
        Some("lossy")
    } else {
        None
    };
    match kind {
        Some(kind) => verbose!(verbose, "[webp] {kind} WebP: not re-encoded"),
        None => match reencoded_vp8l(webp) {
            Ok(vp8l) => {
                if let Some(bytes) = assemble(webp, &chunks, keep_metadata, Some(&vp8l)) {
                    offer(bytes, "re-encoded (lossless)");
                }
            }
            Err(e) => verbose!(verbose, "[webp] re-encode failed: {e}"),
        },
    }

    Ok(match best {
        Some((bytes, how)) if bytes.len() < webp.len() => Outcome::Smaller { bytes, how },
        _ => Outcome::Unchanged(match kind {
            Some(kind) => format!("no smaller encoding found ({kind} WebP is never re-encoded)"),
            None => "no smaller encoding found".into(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16×16 lossy WebP (libwebp, simple format).
    const LOSSY: [u8; 82] = [
        0x52, 0x49, 0x46, 0x46, 0x4a, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x3e, 0x00, 0x00, 0x00, 0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x10, 0x00,
        0x02, 0xc0, 0x4c, 0x25, 0xb0, 0x02, 0x74, 0x7f, 0x00, 0x18, 0x1c, 0x00, 0x5a, 0x56, 0xd8,
        0x00, 0xfe, 0xf7, 0xc8, 0x45, 0x1b, 0x12, 0xe9, 0xdc, 0xdd, 0x47, 0x34, 0x7e, 0x3f, 0xfe,
        0xa3, 0xaf, 0x03, 0xfd, 0xf2, 0x6d, 0x78, 0xb5, 0x9f, 0x57, 0x9f, 0xf1, 0xfc, 0xd4, 0xbf,
        0xd3, 0xb2, 0x96, 0x33, 0x98, 0x00, 0x00,
    ];

    fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_chunk(&mut out, tag, data).unwrap();
        out
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&body);
        out
    }

    fn vp8x(flags: u8, w: u32, h: u32) -> Vec<u8> {
        let mut d = vec![flags, 0, 0, 0];
        d.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        d.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        chunk(b"VP8X", &d)
    }

    /// TIFF EXIF with Make = "CamX" and an orientation.
    fn exif_make_and_orientation(o: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', b'X']);
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0]);
        t.extend_from_slice(&o.to_le_bytes());
        t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        t
    }

    fn tags(webp: &[u8]) -> Vec<[u8; 4]> {
        parse_chunks(webp).unwrap().iter().map(|c| c.tag).collect()
    }

    fn payload<'a>(webp: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
        let c = parse_chunks(webp).unwrap();
        let c = c.iter().find(|c| &c.tag == tag).unwrap();
        &webp[c.data.clone()]
    }

    fn smaller(outcome: Outcome) -> Vec<u8> {
        match outcome {
            Outcome::Smaller { bytes, .. } => bytes,
            Outcome::Unchanged(r) => panic!("expected a smaller file: {r}"),
        }
    }

    /// An odd-length "RGB " profile, so padding is exercised.
    fn icc() -> Vec<u8> {
        let mut p = vec![0x11; 131];
        p[16..20].copy_from_slice(b"RGB ");
        p
    }

    fn lossy_with_metadata() -> Vec<u8> {
        let vp8 = &LOSSY[20..];
        riff(&[
            vp8x(ICC | EXIF | XMP, 16, 16),
            chunk(b"ICCP", &icc()),
            chunk(b"VP8 ", vp8),
            chunk(b"EXIF", &exif_make_and_orientation(6)),
            chunk(b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"),
            chunk(b"ABCD", b"unknown"),
        ])
    }

    #[test]
    fn strip_keeps_icc_and_orientation_and_rebuilds_vp8x() {
        let input = lossy_with_metadata();
        let out = smaller(press(&input, false, false).unwrap());
        assert_eq!(tags(&out), [*b"VP8X", *b"ICCP", *b"VP8 ", *b"EXIF"]);
        assert_eq!(payload(&out, b"VP8X")[0], ICC | EXIF);
        assert_eq!(payload(&out, b"ICCP"), icc());
        assert_eq!(payload(&out, b"VP8 "), &LOSSY[20..]);
        assert_eq!(exif::orientation(payload(&out, b"EXIF")), Some(6));
        assert!(!out.windows(4).any(|w| w == b"CamX"));
        let img = image::load_from_memory_with_format(&out, image::ImageFormat::WebP).unwrap();
        assert_eq!((img.width(), img.height()), (16, 16));
    }

    #[test]
    fn strip_falls_back_to_simple_format_when_nothing_is_left() {
        let input = riff(&[
            vp8x(EXIF | XMP, 16, 16),
            chunk(b"VP8 ", &LOSSY[20..]),
            chunk(b"EXIF", &exif_make_and_orientation(1)),
            chunk(b"XMP ", b"<x/>"),
        ]);
        let out = smaller(press(&input, false, false).unwrap());
        assert_eq!(out, LOSSY, "only the original simple file is left");
    }

    #[test]
    fn unflagged_metadata_is_dropped_and_flags_are_not_invented() {
        // ICCP present but its flag clear: libwebp ignores it.
        let input = riff(&[
            vp8x(EXIF, 16, 16),
            chunk(b"ICCP", &icc()),
            chunk(b"VP8 ", &LOSSY[20..]),
            chunk(b"EXIF", &exif_make_and_orientation(1)),
        ]);
        let out = smaller(press(&input, false, false).unwrap());
        assert_eq!(out, LOSSY);
    }

    #[test]
    fn orientation_is_read_behind_a_jpeg_style_exif_prefix() {
        let prefixed = [b"Exif\0\0".as_slice(), &exif_make_and_orientation(8)].concat();
        let input = riff(&[
            vp8x(EXIF, 16, 16),
            chunk(b"VP8 ", &LOSSY[20..]),
            chunk(b"EXIF", &prefixed),
        ]);
        let out = smaller(press(&input, false, false).unwrap());
        assert_eq!(exif::orientation(payload(&out, b"EXIF")), Some(8));
    }

    #[test]
    fn lossy_without_metadata_is_unchanged() {
        match press(&LOSSY, false, false).unwrap() {
            Outcome::Unchanged(reason) => assert!(reason.contains("lossy"), "{reason}"),
            Outcome::Smaller { .. } => panic!("nothing to remove"),
        }
    }

    fn gradient() -> image::RgbaImage {
        image::RgbaImage::from_fn(64, 48, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 200])
        })
    }

    /// A weakly encoded lossless WebP (no predictor transform).
    fn weak_lossless(img: &image::RgbaImage) -> Vec<u8> {
        let mut out = Vec::new();
        let mut params = image_webp::EncoderParams::default();
        params.use_predictor_transform = false;
        let mut e = image_webp::WebPEncoder::new(&mut out);
        e.set_params(params);
        e.encode(img.as_raw(), img.width(), img.height(), ColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn weak_lossless_is_reencoded_pixel_identically() {
        let img = gradient();
        let input = weak_lossless(&img);
        let out = smaller(press(&input, false, false).unwrap());
        assert_eq!(tags(&out), [*b"VP8L"]);
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::WebP)
            .unwrap()
            .to_rgba8();
        assert_eq!(back, img);
    }

    #[test]
    fn keep_metadata_keeps_every_chunk_around_the_new_bitstream() {
        let img = gradient();
        let weak = weak_lossless(&img);
        let vp8l = payload(&weak, b"VP8L").to_vec();
        let exif = exif_make_and_orientation(3);
        let input = riff(&[
            vp8x(ICC | EXIF | XMP | ALPHA, 64, 48),
            chunk(b"ICCP", &icc()),
            chunk(b"VP8L", &vp8l),
            chunk(b"EXIF", &exif),
            chunk(b"XMP ", b"<x/>"),
            chunk(b"ABCD", b"unknown"),
        ]);
        let out = smaller(press(&input, true, false).unwrap());
        assert_eq!(
            tags(&out),
            [*b"VP8X", *b"ICCP", *b"VP8L", *b"EXIF", *b"XMP ", *b"ABCD"]
        );
        assert_eq!(payload(&out, b"VP8X")[0], ICC | EXIF | XMP | ALPHA);
        assert_eq!(payload(&out, b"EXIF"), exif);
        assert_ne!(payload(&out, b"VP8L"), vp8l);
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::WebP)
            .unwrap()
            .to_rgba8();
        assert_eq!(back, img);
    }

    #[test]
    fn invalid_files_are_rejected() {
        assert!(parse_chunks(b"RIFF\x04\0\0\0WEBP").is_none(), "no chunks");
        assert!(parse_chunks(&LOSSY[..60]).is_none(), "truncated");
        let mut bad = LOSSY;
        bad[16] = 0xFF; // chunk larger than the file
        assert!(parse_chunks(&bad).is_none());
        assert!(parse_chunks(&riff(&[chunk(b"EXIF", b"x")])).is_none());
        assert!(press(b"RIFF\0\0\0\0WEBPVP8 ", false, false).is_err());
    }
}
