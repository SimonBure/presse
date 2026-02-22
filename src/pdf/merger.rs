use lopdf::{Object, ObjectId, Stream, Bookmark};
use lopdf::content::{Content, Operation};
use std::collections::BTreeMap;

pub fn merge(doc1: &mut Document, doc2: &mut Document) -> Result<Document, Box<dyn std::error::Error>> {
    // Collect all Documents Objects grouped by a map
    let mut documents_pages = BTreeMap::new();
    let mut documents_objects = BTreeMap::new();
    let mut document = Document::with_version("1.5");


}
