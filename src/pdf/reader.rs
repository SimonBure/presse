use std::fs::{File, metadata};
use std::io::Read;
use std::path::Path;
use lopdf::Document;


pub fn load_pdf(doc_path: &str) -> Result<Document, Box<dyn std::error::Error>> {
    Ok(Document::load(doc_path)?)
}

/// Load any input into a PDF Document: real PDFs via lopdf, images via image_to_pdf.
/// Detection is content-based (PDF magic), not extension-based.
pub fn load_input_as_pdf(path: &Path, verbose: bool) -> Result<Document, Box<dyn std::error::Error>> {
    if is_pdf(path)? {
        load_pdf(path.to_str().ok_or("non-UTF8 path")?)
    } else {
        super::builder::image_to_pdf(path, verbose)
    }
}

/// True if the file begins with the PDF marker `%PDF-` (scanned in the first 1 KiB,
/// as the spec permits leading bytes before the header).
fn is_pdf(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let mut buf = [0u8; 1024];
    let n = File::open(path)?.read(&mut buf)?;
    Ok(buf[..n].windows(5).any(|w| w == b"%PDF-"))
}

pub fn get_pdf_size_in_kilobytes(doc_path: &str) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(metadata(doc_path)?.len() / 1000)
}

pub fn get_compression_ratio_in_percent(original_size: u64, compressed_size: u64) -> f64 {
    100.0 * (1.0 - (compressed_size as f64) / (original_size as f64))
}
