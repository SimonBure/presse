//! Standalone JPEG compression.
//!
//! Two candidates, smallest wins, and the input is kept when neither is
//! smaller:
//!
//! 1. **Stripped**: the original bitstream with metadata segments removed
//!    (lossless: the entropy-coded data is copied byte for byte).
//! 2. **Re-encoded**: decoded and re-encoded at the target quality by the
//!    same transcoder the PDF pipeline uses for `/DCTDecode` streams.
//!
//! Metadata policy (both candidates): the ICC profile and EXIF orientation
//! are kept, everything else (EXIF camera/GPS data, thumbnails, XMP,
//! comments, Photoshop blocks) is removed. `keep_metadata` keeps it all.

use std::ops::Range;

use super::{Outcome, exif};
use crate::transcode::{ImageTranscoder, Input, jpeg_components};

const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP14: u8 = 0xEE;
const COM: u8 = 0xFE;
const SOS: u8 = 0xDA;
const EOI: u8 = 0xD9;

/// One marker segment before the scan data.
struct Segment {
    marker: u8,
    /// The whole segment, from its first `0xFF` to the end of its payload.
    whole: Range<usize>,
    /// The payload after the 2-byte length.
    payload: Range<usize>,
}

/// Parsed header of a JPEG: every segment between SOI and the first SOS,
/// and the offset where SOS starts (scan data runs from there to the end).
struct Header {
    segments: Vec<Segment>,
    scan_start: usize,
}

fn parse_header(jpeg: &[u8]) -> Option<Header> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut segments = Vec::new();
    let mut i = 2;
    loop {
        if *jpeg.get(i)? != 0xFF {
            return None;
        }
        // Any number of 0xFF fill bytes may precede a marker.
        let mut m = i + 1;
        while *jpeg.get(m)? == 0xFF {
            m += 1;
        }
        let marker = jpeg[m];
        match marker {
            SOS => {
                return Some(Header {
                    segments,
                    scan_start: i,
                });
            }
            EOI => return None,
            // Standalone markers carry no length (TEM, RSTn); not expected
            // before SOS, but harmless to step over.
            0x01 | 0xD0..=0xD7 => i = m + 1,
            _ => {
                let len = u16::from_be_bytes([*jpeg.get(m + 1)?, *jpeg.get(m + 2)?]) as usize;
                if len < 2 || m + 1 + len > jpeg.len() {
                    return None;
                }
                let end = m + 1 + len;
                segments.push(Segment {
                    marker,
                    whole: i..end,
                    payload: m + 3..end,
                });
                i = end;
            }
        }
    }
}

fn payload<'a>(jpeg: &'a [u8], s: &Segment) -> &'a [u8] {
    &jpeg[s.payload.clone()]
}

fn is_icc(jpeg: &[u8], s: &Segment) -> bool {
    s.marker == APP2 && payload(jpeg, s).starts_with(b"ICC_PROFILE\0")
}

fn is_jfif(jpeg: &[u8], s: &Segment) -> bool {
    s.marker == APP0 && payload(jpeg, s).starts_with(b"JFIF\0")
}

fn is_adobe(jpeg: &[u8], s: &Segment) -> bool {
    s.marker == APP14 && payload(jpeg, s).starts_with(b"Adobe")
}

/// MPF indexes extra images by offset; those offsets are invalid in a
/// re-encoded file, so this segment is never carried over.
fn is_mpf(jpeg: &[u8], s: &Segment) -> bool {
    s.marker == APP2 && payload(jpeg, s).starts_with(b"MPF\0")
}

fn is_metadata(s: &Segment) -> bool {
    (APP0..=0xEF).contains(&s.marker) || s.marker == COM
}

fn orientation(jpeg: &[u8], header: &Header) -> Option<u16> {
    header.segments.iter().find_map(|s| {
        let p = payload(jpeg, s);
        (s.marker == APP1 && p.starts_with(b"Exif\0\0"))
            .then(|| exif::orientation(&p[6..]))
            .flatten()
    })
}

/// An APP1 segment holding only the EXIF orientation.
pub(crate) fn orientation_segment(orientation: u16) -> Vec<u8> {
    let tiff = exif::orientation_only(orientation);
    let len = (2 + 6 + tiff.len()) as u16;
    let mut seg = vec![0xFF, APP1];
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

/// APP2 segments holding `icc`, split into numbered chunks of at most
/// 65519 bytes (the segment limit minus the `ICC_PROFILE\0` + sequence
/// header). `None` when the profile is empty or needs more than 255 chunks.
pub(crate) fn icc_segments(icc: &[u8]) -> Option<Vec<u8>> {
    const MAX_CHUNK: usize = 65535 - 2 - 14;
    let chunks: Vec<&[u8]> = icc.chunks(MAX_CHUNK).collect();
    let count = u8::try_from(chunks.len()).ok().filter(|&n| n > 0)?;
    let mut out = Vec::with_capacity(icc.len() + chunks.len() * 18);
    for (i, chunk) in chunks.iter().enumerate() {
        out.extend_from_slice(&[0xFF, APP2]);
        out.extend_from_slice(&((2 + 14 + chunk.len()) as u16).to_be_bytes());
        out.extend_from_slice(b"ICC_PROFILE\0");
        out.extend_from_slice(&[i as u8 + 1, count]);
        out.extend_from_slice(chunk);
    }
    Some(out)
}

/// Lossless candidate: the original with metadata segments removed. The
/// JFIF header, ICC profile and Adobe color-transform segments stay (they
/// change how pixels decode); orientation is re-added as a minimal EXIF.
fn stripped(jpeg: &[u8], header: &Header) -> Vec<u8> {
    let keep_orientation = exif::keepable(orientation(jpeg, header)).map(orientation_segment);
    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(&jpeg[..2]);
    let mut pending = keep_orientation;
    for s in &header.segments {
        let jfif = is_jfif(jpeg, s);
        if !is_metadata(s) || jfif || is_icc(jpeg, s) || is_adobe(jpeg, s) {
            if !jfif && let Some(seg) = pending.take() {
                out.extend_from_slice(&seg);
            }
            out.extend_from_slice(&jpeg[s.whole.clone()]);
        }
    }
    if let Some(seg) = pending {
        out.extend_from_slice(&seg);
    }
    out.extend_from_slice(&jpeg[header.scan_start..]);
    out
}

/// Segments of the source to carry into a re-encoded file. The encoder
/// writes its own JFIF header and plain YCbCr, so the source's JFIF and
/// Adobe segments never apply.
fn carried_segments(jpeg: &[u8], header: &Header, keep_metadata: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if keep_metadata {
        for s in &header.segments {
            if is_metadata(s) && !is_jfif(jpeg, s) && !is_adobe(jpeg, s) && !is_mpf(jpeg, s) {
                out.extend_from_slice(&jpeg[s.whole.clone()]);
            }
        }
        return out;
    }
    if let Some(o) = exif::keepable(orientation(jpeg, header)) {
        out.extend_from_slice(&orientation_segment(o));
    }
    for s in header.segments.iter().filter(|s| is_icc(jpeg, s)) {
        out.extend_from_slice(&jpeg[s.whole.clone()]);
    }
    out
}

/// Insert `extra` segments after the SOI and JFIF header of `encoded`.
pub(crate) fn with_segments(encoded: &[u8], extra: &[u8]) -> Option<Vec<u8>> {
    let header = parse_header(encoded)?;
    let at = match header.segments.first() {
        Some(s) if is_jfif(encoded, s) => s.whole.end,
        _ => 2,
    };
    let mut out = Vec::with_capacity(encoded.len() + extra.len());
    out.extend_from_slice(&encoded[..at]);
    out.extend_from_slice(extra);
    out.extend_from_slice(&encoded[at..]);
    Some(out)
}

pub(crate) fn press<T: ImageTranscoder>(
    jpeg: &[u8],
    quality: u8,
    keep_metadata: bool,
    transcoder: &T,
    verbose: bool,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let header = parse_header(jpeg).ok_or("not a valid JPEG")?;
    let mut best: Option<(Vec<u8>, &'static str)> = None;
    let mut offer = |bytes: Vec<u8>, how: &'static str| {
        verbose!(
            verbose,
            "[jpeg] {how}: {} → {} bytes",
            jpeg.len(),
            bytes.len()
        );
        if best.as_ref().is_none_or(|(b, _)| bytes.len() < b.len()) {
            best = Some((bytes, how));
        }
    };

    if !keep_metadata {
        offer(stripped(jpeg, &header), "metadata stripped (lossless)");
    }

    // CMYK/YCCK JPEGs (4 components) would come back as RGB: keep their
    // pixels untouched, as the PDF pipeline does for CMYK images.
    if jpeg_components(jpeg) == Some(4) {
        verbose!(verbose, "[jpeg] 4-component (CMYK) image: not re-encoded");
    } else {
        match transcoder.transcode_image(&Input::Jpeg(jpeg), quality) {
            Ok(encoded) => {
                let extra = carried_segments(jpeg, &header, keep_metadata);
                match with_segments(&encoded, &extra) {
                    Some(bytes) => offer(bytes, "re-encoded"),
                    None => verbose!(verbose, "[jpeg] re-encode produced no valid JPEG"),
                }
            }
            Err(e) => verbose!(verbose, "[jpeg] re-encode failed: {e}"),
        }
    }

    Ok(match best {
        Some((bytes, how)) if bytes.len() < jpeg.len() => Outcome::Smaller { bytes, how },
        _ => Outcome::Unchanged("no smaller encoding found".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    /// SOI + the given segments + a fake SOS/scan/EOI tail.
    fn jpeg_with(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        for s in segments {
            j.extend_from_slice(s);
        }
        j.extend_from_slice(&[0xFF, SOS, 0x00, 0x02, 0x12, 0x34, 0xFF, EOI]);
        j
    }

    #[test]
    fn strips_metadata_but_keeps_icc_jfif_adobe_and_orientation() {
        let jfif = segment(APP0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
        let mut exif_payload = b"Exif\0\0".to_vec();
        exif_payload.extend_from_slice(&exif::orientation_only(6));
        let exif = segment(APP1, &exif_payload);
        let xmp = segment(APP1, b"http://ns.adobe.com/xap/1.0/\0<x/>");
        let icc = segment(APP2, b"ICC_PROFILE\0\x01\x01profile");
        let comment = segment(COM, b"secret comment");
        let adobe = segment(APP14, b"Adobe\0\x64\0\0\0\0\x01");
        let dqt = segment(0xDB, &[0; 65]);
        let src = jpeg_with(&[
            jfif.clone(),
            exif,
            xmp,
            icc.clone(),
            comment,
            adobe.clone(),
            dqt.clone(),
        ]);

        let header = parse_header(&src).unwrap();
        let out = stripped(&src, &header);
        let expected = jpeg_with(&[jfif, orientation_segment(6), icc, adobe, dqt]);
        assert_eq!(out, expected);
        assert!(out.len() < src.len());
    }

    #[test]
    fn orientation_one_is_dropped() {
        let mut exif_payload = b"Exif\0\0".to_vec();
        exif_payload.extend_from_slice(&exif::orientation_only(1));
        let src = jpeg_with(&[segment(APP1, &exif_payload)]);
        let header = parse_header(&src).unwrap();
        assert_eq!(stripped(&src, &header), jpeg_with(&[]));
    }

    #[test]
    fn carried_segments_follow_policy() {
        let icc = segment(APP2, b"ICC_PROFILE\0\x01\x01p");
        let mpf = segment(APP2, b"MPF\0data");
        let comment = segment(COM, b"c");
        let src = jpeg_with(&[icc.clone(), mpf, comment.clone()]);
        let header = parse_header(&src).unwrap();
        assert_eq!(carried_segments(&src, &header, false), icc);
        let mut all = icc.clone();
        all.extend_from_slice(&comment);
        assert_eq!(carried_segments(&src, &header, true), all);
    }

    #[test]
    fn segments_go_after_jfif() {
        let jfif = segment(APP0, b"JFIF\0rest");
        let enc = jpeg_with(std::slice::from_ref(&jfif));
        let extra = segment(COM, b"x");
        assert_eq!(
            with_segments(&enc, &extra).unwrap(),
            jpeg_with(&[jfif, extra])
        );
    }

    #[test]
    fn icc_segments_split_and_number() {
        let icc = vec![7u8; 65519 * 2 + 10];
        let src = jpeg_with(&[icc_segments(&icc).unwrap()]);
        let header = parse_header(&src).unwrap();
        let parts: Vec<&[u8]> = header
            .segments
            .iter()
            .filter(|s| is_icc(&src, s))
            .map(|s| payload(&src, s))
            .collect();
        assert_eq!(parts.len(), 3);
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(&p[12..14], &[i as u8 + 1, 3]);
        }
        let joined: Vec<u8> = parts.iter().flat_map(|p| p[14..].to_vec()).collect();
        assert_eq!(joined, icc);
        assert!(icc_segments(&[]).is_none());
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse_header(&[0xFF, 0xD8, 0xFF, APP1, 0x00]).is_none());
        assert!(parse_header(b"not a jpeg").is_none());
    }
}
