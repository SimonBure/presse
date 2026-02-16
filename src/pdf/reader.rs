use lopdf::Document;


pub fn load_pdf(doc_path: &str) -> Result<Document, Box<dyn std::error::Error>> {
    let doc: Document = Document::load(doc_path)?;

    return Ok(doc);
}