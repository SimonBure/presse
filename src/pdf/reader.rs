use lopdf::Document;
use memmap2::Mmap;
use std::fs::{File, metadata};
use std::io::Read;
use std::path::Path;

use crate::format::{Format, PDF_HEADER_WINDOW, detect};

/// Load a PDF by memory-mapping the file instead of copying it into a heap
/// buffer. The mapping lives only for the duration of the parse; `lopdf`
/// copies everything it needs into its own structures, so the map can be
/// dropped right after.
pub fn load_pdf(doc_path: &str) -> Result<Document, Box<dyn std::error::Error>> {
    let mmap = map_file(Path::new(doc_path))?;
    Ok(Document::load_mem(&mmap)?)
}

/// Memory-map an input file for reading.
///
/// Drop the map before writing to the same path: truncating a mapped file
/// invalidates the mapping.
pub fn map_file(path: &Path) -> Result<Mmap, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    // SAFETY: the mapping is read-only, and callers drop it before writing
    // to the same path (input files are not otherwise written to).
    Ok(unsafe { Mmap::map(&file)? })
}

/// Load any input into a PDF Document: real PDFs via lopdf, images via image_to_pdf.
/// Detection is content-based (PDF magic), not extension-based.
pub fn load_input_as_pdf(
    path: &Path,
    verbose: bool,
) -> Result<Document, Box<dyn std::error::Error>> {
    if is_pdf(path)? {
        load_pdf(path.to_str().ok_or("non-UTF8 path")?)
    } else {
        super::builder::image_to_pdf(path, verbose)
    }
}

/// True if the file is a PDF by content: the `%PDF-` marker in the first
/// KiB, unless an image signature at byte 0 says otherwise (see
/// [`crate::format::detect`]).
fn is_pdf(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let mut buf = [0u8; PDF_HEADER_WINDOW];
    let n = File::open(path)?.read(&mut buf)?;
    Ok(detect(&buf[..n]) == Some(Format::Pdf))
}

pub fn get_pdf_size_in_kilobytes(doc_path: &str) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(metadata(doc_path)?.len() / 1000)
}

pub fn get_compression_ratio_in_percent(original_size: u64, compressed_size: u64) -> f64 {
    100.0 * (1.0 - (compressed_size as f64) / (original_size as f64))
}
