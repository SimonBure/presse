//! Image-to-image conversion (`presse convert --to jpeg|png|webp`).
//!
//! The encoders are the ones `press` uses: the PDF pipeline's JPEG
//! transcoder, the optimized lossless PNG encoder, and lossless WebP
//! (`image-webp`; presse has no lossy WebP encoder). The ICC profile and
//! EXIF orientation follow the image when the output can hold them;
//! nothing else is carried over. Animated inputs keep their first frame.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder};

use super::{exif, jpeg, png, webp};
use crate::format::{Format, detect};
use crate::transcode::{CpuTranscoder, ImageRef, ImageTranscoder, Input};

/// Convert one image file to `target` (JPEG, PNG or WebP). `quality` is the
/// JPEG quality; PNG and WebP output is lossless.
pub fn convert_image(
    input: &[u8],
    target: Format,
    quality: u8,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    match detect(input) {
        Some(Format::Pdf) => return Err("converting a PDF to an image is not supported".into()),
        None => return Err("not a recognized image".into()),
        Some(_) => {}
    }
    let mut decoder = image::ImageReader::new(Cursor::new(input))
        .with_guessed_format()?
        .into_decoder()?;
    let icc = decoder.icc_profile().ok().flatten();
    let orientation = decoder
        .orientation()
        .ok()
        .and_then(|o| exif::keepable(Some(u16::from(o.to_exif()))));
    let img = DynamicImage::from_decoder(decoder)?;

    // A profile is kept only when it describes the output's color model
    // (a CMYK JPEG decodes to RGB, so its CMYK profile must not follow).
    let icc_for = |space: &[u8; 4]| icc.as_deref().filter(|p| p.get(16..20) == Some(space));
    let gray = !img.color().has_color();
    let space = if gray { b"GRAY" } else { b"RGB " };

    match target {
        Format::Jpeg => to_jpeg(img, quality, icc_for(space), orientation),
        Format::Png => to_png(img, icc_for(space), orientation),
        // VP8L stores RGB even for gray input, so only RGB profiles apply.
        Format::WebP => to_webp(img, icc_for(b"RGB "), orientation),
        other => Err(format!("cannot convert an image to {}", other.name()).into()),
    }
}

/// Composite `a` over white.
fn over_white(v: u8, a: u8) -> u8 {
    ((u16::from(v) * u16::from(a) + 255 * (255 - u16::from(a)) + 127) / 255) as u8
}

fn to_jpeg(
    img: DynamicImage,
    quality: u8,
    icc: Option<&[u8]>,
    orientation: Option<u16>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (width, height) = (img.width(), img.height());
    let gray = !img.color().has_color();
    let alpha = img.color().has_alpha();
    // JPEG has no alpha: transparent areas become white.
    let bytes: Vec<u8> = match (gray, alpha) {
        (true, false) => img.to_luma8().into_raw(),
        (false, false) => img.to_rgb8().into_raw(),
        (true, true) => img
            .to_luma_alpha8()
            .pixels()
            .map(|p| over_white(p[0], p[1]))
            .collect(),
        (false, true) => img
            .to_rgba8()
            .pixels()
            .flat_map(|p| [0, 1, 2].map(|c| over_white(p[c], p[3])))
            .collect(),
    };
    let pixels = if gray {
        ImageRef::Luma8 {
            width,
            height,
            bytes: &bytes,
        }
    } else {
        ImageRef::Rgb8 {
            width,
            height,
            bytes: &bytes,
        }
    };
    let encoded = CpuTranscoder::default().transcode_image(&Input::Pixels(pixels), quality)?;
    let mut extra = Vec::new();
    if let Some(o) = orientation {
        extra.extend_from_slice(&jpeg::orientation_segment(o));
    }
    if let Some(segments) = icc.and_then(jpeg::icc_segments) {
        extra.extend_from_slice(&segments);
    }
    jpeg::with_segments(&encoded, &extra).ok_or_else(|| "JPEG encoding failed".into())
}

fn to_png(
    img: DynamicImage,
    icc: Option<&[u8]>,
    orientation: Option<u16>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // Float rasters (TIFF) have no PNG sample type: store them as 16-bit.
    let img = match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            DynamicImage::ImageRgba16(img.to_rgba16())
        }
        other => other,
    };
    let raster = png::Raster::from_image(img).ok_or("unsupported pixel format")?;
    let (best, _) = png::encodings(raster, icc.is_none(), false)
        .into_iter()
        .min_by_key(|(b, _)| b.len())
        .ok_or("PNG encoding failed")?;
    let mut extra = Vec::new();
    if let Some(p) = icc {
        extra.extend_from_slice(&png::iccp_chunk(p));
    }
    if let Some(o) = orientation {
        extra.extend_from_slice(&png::orientation_exif_chunk(o));
    }
    Ok(png::with_chunks(best, &extra))
}

fn to_webp(
    img: DynamicImage,
    icc: Option<&[u8]>,
    orientation: Option<u16>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (width, height) = (img.width(), img.height());
    let (data, color) = webp::lossless_pixels(&img);
    let mut out = Vec::new();
    let mut encoder = image_webp::WebPEncoder::new(&mut out);
    if let Some(p) = icc {
        encoder.set_icc_profile(p.to_vec());
    }
    if let Some(o) = orientation {
        encoder.set_exif_metadata(exif::orientation_only(o));
    }
    encoder.encode(&data, width, height, color)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(img: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn sample() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(48, 32, |x, y| {
            image::Rgb([(x * 5) as u8, (y * 7) as u8, ((x + y) * 3) as u8])
        }))
    }

    #[test]
    fn png_to_webp_and_back_is_lossless() {
        let src = png_of(&sample());
        let webp = convert_image(&src, Format::WebP, 80).unwrap();
        assert_eq!(detect(&webp), Some(Format::WebP));
        let png_again = convert_image(&webp, Format::Png, 80).unwrap();
        assert_eq!(detect(&png_again), Some(Format::Png));
        let back = image::load_from_memory(&png_again).unwrap().to_rgb8();
        assert_eq!(back, sample().to_rgb8());
    }

    #[test]
    fn transparent_png_to_jpeg_is_flattened_on_white() {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([0, 0, 0, 0]),
        ));
        let jpeg = convert_image(&png_of(&img), Format::Jpeg, 90).unwrap();
        let px = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert!(px.pixels().all(|p| p.0.iter().all(|&c| c > 250)));
    }

    #[test]
    fn pdf_and_unknown_inputs_are_rejected() {
        assert!(convert_image(b"%PDF-1.7\n", Format::Png, 80).is_err());
        assert!(convert_image(b"hello", Format::Png, 80).is_err());
        assert!(convert_image(&png_of(&sample()), Format::Pdf, 80).is_err());
    }

    #[test]
    fn over_white_extremes() {
        assert_eq!(over_white(0, 0), 255);
        assert_eq!(over_white(0, 255), 0);
        assert_eq!(over_white(200, 255), 200);
    }
}
