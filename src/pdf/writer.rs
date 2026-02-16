use lopdf::{Document, SaveOptions};
use std::fs::File;

pub fn compress_pdf(doc: &mut Document, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let option: SaveOptions = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(200)
        .compression_level(9)
        .build();

    let mut file = File::create(name)?;

    doc.save_with_options(&mut file, option)?;

    Ok(())
}