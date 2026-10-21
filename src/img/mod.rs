//! Standalone image compression (JPEG, PNG, WebP), output in the input's
//! format.

pub(crate) mod convert;
mod exif;
pub(crate) mod jpeg;
pub(crate) mod png;
pub(crate) mod webp;

/// Result of compressing one image.
pub(crate) enum Outcome {
    /// A smaller file, and which candidate produced it.
    Smaller { bytes: Vec<u8>, how: &'static str },
    /// Nothing smaller was found; the input is already the best version.
    Unchanged(String),
}
