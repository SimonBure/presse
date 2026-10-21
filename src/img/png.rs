//! Standalone PNG compression. Lossless unless `--palette` is given.
//!
//! Candidates, smallest wins, and the input is kept when none is smaller:
//!
//! 1. **Stripped**: the original chunks minus metadata (lossless, the
//!    image data is copied byte for byte).
//! 2. **Re-encoded**: decoded, reduced losslessly (16-bit samples that are
//!    exact 8-bit values, an all-opaque alpha channel, color that is
//!    really gray), then deflated at level 9 with adaptive filtering.
//! 3. **Exact palette**: at most 256 distinct colors become an indexed
//!    image, packed to 1/2/4 bits per pixel when the palette allows
//!    (lossless).
//! 4. **Lossy palette** (`--palette` only): opaque images with more colors
//!    go through the PDF pipeline's median-cut quantizer, accepted only
//!    above its 0.9999 SSIM gate.
//!
//! Animated PNGs are only stripped, never re-encoded (decoding keeps the
//! first frame only).
//!
//! Metadata policy: color chunks (iCCP, sRGB, gAMA, cHRM, cICP, mDCV, cLLI),
//! pixel size (pHYs) and EXIF orientation are kept; text chunks, time
//! stamps and the rest of EXIF are removed. `keep_metadata` additionally
//! keeps every ancillary chunk the PNG spec marks safe to copy.

use std::collections::HashMap;
use std::ops::Range;

use super::{Outcome, exif};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Color-management and geometry chunks: kept in every candidate.
const KEPT: [&[u8; 4]; 8] = [
    b"iCCP", b"sRGB", b"gAMA", b"cHRM", b"cICP", b"mDCV", b"cLLI", b"pHYs",
];

/// Ancillary chunks the stripped candidate keeps besides [`KEPT`]: they
/// describe the stored pixels (transparency, significant bits, background,
/// palette statistics, animation), so they stay valid while the image data
/// is untouched.
const KEPT_WITH_ORIGINAL_DATA: [&[u8; 4]; 8] = [
    b"tRNS", b"sBIT", b"bKGD", b"hIST", b"sPLT", b"acTL", b"fcTL", b"fdAT",
];

struct Chunk {
    kind: [u8; 4],
    data: Range<usize>,
    /// Length + type + data + CRC.
    whole: Range<usize>,
}

impl Chunk {
    fn is_critical(&self) -> bool {
        self.kind[0] & 0x20 == 0
    }
    fn is_safe_to_copy(&self) -> bool {
        self.kind[3] & 0x20 != 0
    }
}

/// Every chunk up to and including IEND; `None` unless the file is
/// structurally sound and starts with IHDR.
fn parse_chunks(png: &[u8]) -> Option<Vec<Chunk>> {
    if !png.starts_with(SIGNATURE) {
        return None;
    }
    let mut chunks = Vec::new();
    let mut i = SIGNATURE.len();
    loop {
        let len = u32::from_be_bytes(png.get(i..i + 4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = png.get(i + 4..i + 8)?.try_into().ok()?;
        let data = i + 8..(i + 8).checked_add(len)?;
        let end = data.end.checked_add(4)?;
        if end > png.len() {
            return None;
        }
        chunks.push(Chunk {
            kind,
            data,
            whole: i..end,
        });
        i = end;
        if &kind == b"IEND" {
            break;
        }
    }
    (&chunks.first()?.kind == b"IHDR").then_some(chunks)
}

pub(crate) fn chunk_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
    out
}

/// An `iCCP` chunk holding `icc` (zlib-compressed, as the chunk requires).
pub(crate) fn iccp_chunk(icc: &[u8]) -> Vec<u8> {
    let mut data = b"ICC profile\0\0".to_vec(); // name, separator, method 0
    data.extend_from_slice(&crate::pdf::images::zlib_encode(icc));
    chunk_bytes(b"iCCP", &data)
}

/// An `eXIf` chunk holding only the EXIF orientation.
pub(crate) fn orientation_exif_chunk(orientation: u16) -> Vec<u8> {
    chunk_bytes(b"eXIf", &exif::orientation_only(orientation))
}

fn orientation_chunk(png: &[u8], chunks: &[Chunk]) -> Option<Vec<u8>> {
    let tiff = &png[chunks.iter().find(|c| &c.kind == b"eXIf")?.data.clone()];
    exif::keepable(exif::orientation(tiff)).map(orientation_exif_chunk)
}

/// Lossless candidate: the original minus metadata chunks.
fn stripped(png: &[u8], chunks: &[Chunk]) -> Vec<u8> {
    let mut out = Vec::with_capacity(png.len());
    out.extend_from_slice(SIGNATURE);
    for c in chunks {
        if c.is_critical() || KEPT.contains(&&c.kind) || KEPT_WITH_ORIGINAL_DATA.contains(&&c.kind)
        {
            out.extend_from_slice(&png[c.whole.clone()]);
        }
        if &c.kind == b"IHDR"
            && let Some(o) = orientation_chunk(png, chunks)
        {
            out.extend_from_slice(&o);
        }
    }
    out
}

/// Chunks of the source to carry into a re-encoded file, placed right after
/// IHDR (every chunk here may precede PLTE and IDAT).
fn carried_chunks(png: &[u8], chunks: &[Chunk], keep_metadata: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for c in chunks {
        let carry =
            KEPT.contains(&&c.kind) || (keep_metadata && !c.is_critical() && c.is_safe_to_copy());
        if carry {
            out.extend_from_slice(&png[c.whole.clone()]);
        }
    }
    if !keep_metadata && let Some(o) = orientation_chunk(png, chunks) {
        out.extend_from_slice(&o);
    }
    out
}

/// Decoded pixels: `channels` interleaved samples per pixel, 8 or 16 bits
/// each (16-bit samples big-endian, as PNG stores them).
pub(crate) struct Raster {
    width: u32,
    height: u32,
    channels: usize,
    sixteen: bool,
    data: Vec<u8>,
}

impl Raster {
    pub(crate) fn from_image(img: image::DynamicImage) -> Option<Self> {
        use image::DynamicImage as D;
        let (width, height) = (img.width(), img.height());
        let be = |s: &[u16]| s.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>();
        let (channels, sixteen, data) = match img {
            D::ImageLuma8(b) => (1, false, b.into_raw()),
            D::ImageLumaA8(b) => (2, false, b.into_raw()),
            D::ImageRgb8(b) => (3, false, b.into_raw()),
            D::ImageRgba8(b) => (4, false, b.into_raw()),
            D::ImageLuma16(b) => (1, true, be(&b)),
            D::ImageLumaA16(b) => (2, true, be(&b)),
            D::ImageRgb16(b) => (3, true, be(&b)),
            D::ImageRgba16(b) => (4, true, be(&b)),
            _ => return None,
        };
        Some(Self {
            width,
            height,
            channels,
            sixteen,
            data,
        })
    }

    fn has_alpha(&self) -> bool {
        self.channels.is_multiple_of(2)
    }

    fn sample_bytes(&self) -> usize {
        if self.sixteen { 2 } else { 1 }
    }

    /// 16-bit samples whose two bytes are equal are exactly the 8-bit
    /// value ×257: storing the high byte loses nothing.
    fn reduce_depth(&mut self) {
        if self.sixteen && self.data.chunks_exact(2).all(|s| s[0] == s[1]) {
            self.data = self.data.iter().step_by(2).copied().collect();
            self.sixteen = false;
        }
    }

    fn reduce_alpha(&mut self) {
        if !self.has_alpha() {
            return;
        }
        let sb = self.sample_bytes();
        let px = self.channels * sb;
        let opaque = self
            .data
            .chunks_exact(px)
            .all(|p| p[px - sb..].iter().all(|&b| b == 0xFF));
        if opaque {
            let mut data = Vec::with_capacity(self.data.len() / px * (px - sb));
            for p in self.data.chunks_exact(px) {
                data.extend_from_slice(&p[..px - sb]);
            }
            self.data = data;
            self.channels -= 1;
        }
    }

    fn reduce_gray(&mut self) {
        if self.channels < 3 {
            return;
        }
        let sb = self.sample_bytes();
        let px = self.channels * sb;
        let gray = self
            .data
            .chunks_exact(px)
            .all(|p| p[..sb] == p[sb..2 * sb] && p[..sb] == p[2 * sb..3 * sb]);
        if gray {
            let alpha = self.has_alpha();
            let out_px = if alpha { 2 * sb } else { sb };
            let mut data = Vec::with_capacity(self.data.len() / px * out_px);
            for p in self.data.chunks_exact(px) {
                data.extend_from_slice(&p[..sb]);
                if alpha {
                    data.extend_from_slice(&p[3 * sb..]);
                }
            }
            self.data = data;
            self.channels = if alpha { 2 } else { 1 };
        }
    }

    fn color_type(&self) -> png::ColorType {
        match self.channels {
            1 => png::ColorType::Grayscale,
            2 => png::ColorType::GrayscaleAlpha,
            3 => png::ColorType::Rgb,
            _ => png::ColorType::Rgba,
        }
    }
}

/// A palette image: `palette` (RGB triples), optional `trns` (alpha of the
/// first entries) and one index byte per pixel.
struct Indexed {
    palette: Vec<u8>,
    trns: Option<Vec<u8>>,
    indices: Vec<u8>,
}

/// Exact palette of an 8-bit RGB/RGBA raster with at most 256 colors.
/// Transparent entries come first so `tRNS` stays as short as possible.
fn exact_palette(r: &Raster) -> Option<Indexed> {
    if r.sixteen || r.channels < 3 {
        return None;
    }
    let mut index_of: HashMap<[u8; 4], u8> = HashMap::with_capacity(257);
    let mut colors: Vec<[u8; 4]> = Vec::with_capacity(256);
    let mut indices = Vec::with_capacity(r.data.len() / r.channels);
    for p in r.data.chunks_exact(r.channels) {
        let c = [p[0], p[1], p[2], if r.channels == 4 { p[3] } else { 0xFF }];
        let i = match index_of.get(&c) {
            Some(&i) => i,
            None => {
                if colors.len() == 256 {
                    return None;
                }
                let i = colors.len() as u8;
                index_of.insert(c, i);
                colors.push(c);
                i
            }
        };
        indices.push(i);
    }
    // Reorder: transparent entries first (stable), so tRNS covers only them.
    let mut order: Vec<usize> = (0..colors.len()).collect();
    order.sort_by_key(|&i| colors[i][3] == 0xFF);
    let mut remap = vec![0u8; colors.len()];
    for (new, &old) in order.iter().enumerate() {
        remap[old] = new as u8;
    }
    let palette = order
        .iter()
        .flat_map(|&i| colors[i][..3].to_vec())
        .collect();
    let trns: Vec<u8> = order
        .iter()
        .map(|&i| colors[i][3])
        .take_while(|&a| a != 0xFF)
        .collect();
    for i in &mut indices {
        *i = remap[*i as usize];
    }
    Some(Indexed {
        palette,
        trns: (!trns.is_empty()).then_some(trns),
        indices,
    })
}

/// Pack one-byte indices into `bits`-per-pixel rows (each row padded to a
/// whole byte), as PNG stores sub-byte depths.
fn pack(indices: &[u8], width: usize, bits: usize) -> Vec<u8> {
    if bits == 8 {
        return indices.to_vec();
    }
    let per_byte = 8 / bits;
    let mut out = Vec::with_capacity(indices.len().div_ceil(per_byte));
    for row in indices.chunks(width) {
        for group in row.chunks(per_byte) {
            let mut byte = 0u8;
            for (k, &ix) in group.iter().enumerate() {
                byte |= ix << (8 - bits * (k + 1));
            }
            out.push(byte);
        }
    }
    out
}

fn depth_for(entries: usize) -> (usize, png::BitDepth) {
    match entries {
        0..=2 => (1, png::BitDepth::One),
        3..=4 => (2, png::BitDepth::Two),
        5..=16 => (4, png::BitDepth::Four),
        _ => (8, png::BitDepth::Eight),
    }
}

/// Encode at deflate level 9 with the given row filter.
fn encode(
    r: &Raster,
    color: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
    indexed: Option<&Indexed>,
    filter: png::Filter,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, r.width, r.height);
        enc.set_color(color);
        enc.set_depth(depth);
        enc.set_compression(png::Compression::High);
        enc.set_filter(filter);
        if let Some(ix) = indexed {
            enc.set_palette(ix.palette.clone());
            if let Some(t) = &ix.trns {
                enc.set_trns(t.clone());
            }
        }
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(data).ok()?;
        writer.finish().ok()?;
    }
    Some(out)
}

fn encode_indexed(r: &Raster, ix: &Indexed) -> Option<Vec<u8>> {
    let (bits, depth) = depth_for(ix.palette.len() / 3);
    let data = pack(&ix.indices, r.width as usize, bits);
    // libpng's advice for palette images is no filtering; adaptive filtering
    // sometimes still wins, so both are tried.
    [png::Filter::NoFilter, png::Filter::Adaptive]
        .into_iter()
        .filter_map(|f| encode(r, png::ColorType::Indexed, depth, &data, Some(ix), f))
        .min_by_key(Vec::len)
}

/// Insert `extra` (complete chunks) right after IHDR.
pub(crate) fn with_chunks(encoded: Vec<u8>, extra: &[u8]) -> Vec<u8> {
    if extra.is_empty() {
        return encoded;
    }
    // IHDR always comes first and always holds 13 bytes: 8 + 4 + 4 + 13 + 4.
    let at = SIGNATURE.len() + 25;
    let mut out = Vec::with_capacity(encoded.len() + extra.len());
    out.extend_from_slice(&encoded[..at]);
    out.extend_from_slice(extra);
    out.extend_from_slice(&encoded[at..]);
    out
}

/// Every PNG encoding of `r` worth trying, without ancillary chunks:
/// lossless reductions + level-9 deflate, an exact palette when the colors
/// allow, and (with `palette`) the SSIM-gated lossy palette. `gray_ok`
/// false keeps color images in color (an RGB ICC profile must not end up
/// on a grayscale image).
pub(crate) fn encodings(
    mut r: Raster,
    gray_ok: bool,
    palette: bool,
) -> Vec<(Vec<u8>, &'static str)> {
    let mut out = Vec::new();
    r.reduce_depth();
    r.reduce_alpha();
    if gray_ok {
        r.reduce_gray();
    }
    let depth = if r.sixteen {
        png::BitDepth::Sixteen
    } else {
        png::BitDepth::Eight
    };
    if let Some(b) = encode(
        &r,
        r.color_type(),
        depth,
        &r.data,
        None,
        png::Filter::Adaptive,
    ) {
        out.push((b, "re-encoded (lossless)"));
    }
    match exact_palette(&r) {
        Some(ix) => {
            if let Some(b) = encode_indexed(&r, &ix) {
                out.push((b, "exact palette (lossless)"));
            }
        }
        None if palette && !r.sixteen && r.channels == 3 => {
            if let Some(c) = crate::pdf::images::indexed_candidate(&r.data, r.width, r.height) {
                let ix = Indexed {
                    palette: c.palette,
                    trns: None,
                    indices: c.indices,
                };
                if let Some(b) = encode_indexed(&r, &ix) {
                    out.push((b, "quantized palette (SSIM ≥ 0.9999)"));
                }
            }
        }
        None => {}
    }
    out
}

pub(crate) fn press(
    png_bytes: &[u8],
    palette: bool,
    keep_metadata: bool,
    verbose: bool,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let chunks = parse_chunks(png_bytes).ok_or("not a valid PNG")?;
    let mut best: Option<(Vec<u8>, &'static str)> = None;
    let mut offer = |bytes: Vec<u8>, how: &'static str| {
        verbose!(
            verbose,
            "[png] {how}: {} → {} bytes",
            png_bytes.len(),
            bytes.len()
        );
        if best.as_ref().is_none_or(|(b, _)| bytes.len() < b.len()) {
            best = Some((bytes, how));
        }
    };

    if !keep_metadata {
        offer(stripped(png_bytes, &chunks), "metadata stripped (lossless)");
    }

    if chunks.iter().any(|c| &c.kind == b"acTL") {
        verbose!(verbose, "[png] animated PNG: not re-encoded");
    } else {
        let r = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)
            .ok()
            .and_then(Raster::from_image)
            .ok_or("PNG decode failed")?;
        let extra = carried_chunks(png_bytes, &chunks, keep_metadata);
        // An RGB ICC profile is invalid on a grayscale image.
        let gray_ok = !chunks.iter().any(|c| &c.kind == b"iCCP");
        for (b, how) in encodings(r, gray_ok, palette) {
            offer(with_chunks(b, &extra), how);
        }
    }

    Ok(match best {
        Some((bytes, how)) if bytes.len() < png_bytes.len() => Outcome::Smaller { bytes, how },
        _ => Outcome::Unchanged("no smaller encoding found".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png: &[u8]) -> image::DynamicImage {
        image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap()
    }

    fn png_from(img: &image::DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn pack_rows_pad_to_bytes() {
        // 3 px wide, 1 bit: row [1,0,1] -> 0b1010_0000
        assert_eq!(
            pack(&[1, 0, 1, 0, 1, 1], 3, 1),
            vec![0b1010_0000, 0b0110_0000]
        );
        assert_eq!(pack(&[3, 1], 2, 4), vec![0x31]);
    }

    #[test]
    fn flat_image_becomes_lossless_palette() {
        let mut img = image::RgbaImage::new(64, 32);
        for (x, _, p) in img.enumerate_pixels_mut() {
            *p = match x / 16 {
                0 => image::Rgba([255, 0, 0, 255]),
                1 => image::Rgba([0, 255, 0, 255]),
                2 => image::Rgba([0, 0, 255, 128]),
                _ => image::Rgba([0, 0, 0, 0]),
            };
        }
        let src = png_from(&image::DynamicImage::ImageRgba8(img.clone()));
        let Outcome::Smaller { bytes, .. } = press(&src, false, false, false).unwrap() else {
            panic!("flat RGBA image must shrink");
        };
        assert_eq!(
            decode(&bytes).to_rgba8(),
            img,
            "palette conversion must be lossless"
        );
    }

    #[test]
    fn gray_and_opaque_reductions_are_lossless() {
        let mut img = image::RgbaImage::new(40, 40);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = ((x * 7 + y * 13) % 256) as u8;
            *p = image::Rgba([v, v, v, 255]);
        }
        let src = png_from(&image::DynamicImage::ImageRgba8(img.clone()));
        let Outcome::Smaller { bytes, .. } = press(&src, false, false, false).unwrap() else {
            panic!("gray RGBA must shrink");
        };
        assert_eq!(decode(&bytes).to_rgba8(), img);
        assert!(matches!(decode(&bytes).color(), image::ColorType::L8));
    }

    #[test]
    fn text_chunks_removed_color_chunks_kept() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 30) as u8, (y * 30) as u8, 7])
        }));
        let base = png_from(&img);
        let text = chunk_bytes(b"tEXt", b"Comment\0a long secret comment");
        let gama = chunk_bytes(b"gAMA", &45455u32.to_be_bytes());
        let src = with_chunks(base, &[gama.clone(), text].concat());
        let chunks = parse_chunks(&src).unwrap();
        let out = stripped(&src, &chunks);
        let kinds: Vec<[u8; 4]> = parse_chunks(&out).unwrap().iter().map(|c| c.kind).collect();
        assert!(kinds.contains(b"gAMA"));
        assert!(!kinds.contains(b"tEXt"));
        assert!(out.windows(gama.len()).any(|w| w == gama));
        assert_eq!(carried_chunks(&src, &chunks, false), gama);
    }

    #[test]
    fn orientation_survives_as_minimal_exif() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let mut exif_tiff = exif::orientation_only(8);
        exif_tiff.extend_from_slice(b"pretend GPS data");
        let src = with_chunks(png_from(&img), &chunk_bytes(b"eXIf", &exif_tiff));
        let chunks = parse_chunks(&src).unwrap();
        let out = stripped(&src, &chunks);
        let out_chunks = parse_chunks(&out).unwrap();
        let e = out_chunks.iter().find(|c| &c.kind == b"eXIf").unwrap();
        assert_eq!(&out[e.data.clone()], exif::orientation_only(8).as_slice());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_chunks(b"\x89PNG\r\n\x1a\n\0\0").is_none());
        assert!(press(b"nope", false, false, false).is_err());
    }
}
