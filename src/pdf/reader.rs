use std::fs::metadata;
use lopdf::Document;


pub fn load_pdf(doc_path: &str) -> Result<Document, Box<dyn std::error::Error>> {
    let doc: Document = Document::load(doc_path)?;

    return Ok(doc);
}

pub fn get_pdf_size_in_kilobytes(doc_path: &str) -> Result<u64, Box<dyn std::error::Error>> {
    return Ok(metadata(doc_path)?.len() / 1000);
}

pub fn get_compression_ration_in_percent(original_size: u64, compressed_size: u64) -> f64 {
    return  100.0 * (1.0 - (compressed_size as f64) / (original_size as f64));
}