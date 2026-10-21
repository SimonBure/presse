//! File format detection from content bytes, never from the file name.

/// A file format presse recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Jpeg,
    Png,
    WebP,
    Gif,
    Tiff,
    Bmp,
}

impl Format {
    /// Conventional file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::WebP => "webp",
            Format::Gif => "gif",
            Format::Tiff => "tiff",
            Format::Bmp => "bmp",
        }
    }

    /// Display name used in messages.
    pub fn name(self) -> &'static str {
        match self {
            Format::Pdf => "PDF",
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::WebP => "WebP",
            Format::Gif => "GIF",
            Format::Tiff => "TIFF",
            Format::Bmp => "BMP",
        }
    }
}

/// How far into a file the `%PDF-` header may start. The PDF reference
/// (Appendix H, implementation note) lets readers accept a header anywhere
/// in the first 1024 bytes.
pub const PDF_HEADER_WINDOW: usize = 1024;

/// Detect the format of `bytes` (the whole file, or at least its first
/// [`PDF_HEADER_WINDOW`] bytes).
///
/// Image signatures are checked at offset 0 first: they are exact, while
/// the PDF header may sit anywhere in the first KiB, so an image whose
/// metadata happens to contain `%PDF-` must still be read as an image.
pub fn detect(bytes: &[u8]) -> Option<Format> {
    let image = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Jpeg) => Some(Format::Jpeg),
        Ok(image::ImageFormat::Png) => Some(Format::Png),
        Ok(image::ImageFormat::WebP) => Some(Format::WebP),
        Ok(image::ImageFormat::Gif) => Some(Format::Gif),
        Ok(image::ImageFormat::Tiff) => Some(Format::Tiff),
        Ok(image::ImageFormat::Bmp) => Some(Format::Bmp),
        _ => None,
    };
    if image.is_some() {
        return image;
    }
    let window = &bytes[..bytes.len().min(PDF_HEADER_WINDOW)];
    window
        .windows(5)
        .any(|w| w == b"%PDF-")
        .then_some(Format::Pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_signature_not_name() {
        assert_eq!(detect(b"%PDF-1.7\n"), Some(Format::Pdf));
        assert_eq!(detect(b"junk before %PDF-1.4"), Some(Format::Pdf));
        assert_eq!(detect(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n...."), Some(Format::Png));
        assert_eq!(detect(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::WebP));
        assert_eq!(detect(b"plain text"), None);
        assert_eq!(detect(b""), None);
    }

    #[test]
    fn image_signature_wins_over_pdf_marker_in_metadata() {
        // A JPEG whose first KiB (e.g. an EXIF comment) contains "%PDF-".
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x09];
        jpeg.extend_from_slice(b"%PDF-1.4");
        assert_eq!(detect(&jpeg), Some(Format::Jpeg));
    }

    #[test]
    fn pdf_header_beyond_window_is_not_detected() {
        let mut bytes = vec![b' '; PDF_HEADER_WINDOW];
        bytes.extend_from_slice(b"%PDF-1.7");
        assert_eq!(detect(&bytes), None);
    }
}
