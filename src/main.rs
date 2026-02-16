// =============================================================================
// PRESSE - PDF Compression Tool
// =============================================================================
//
// IMPLEMENTATION ORDER (follow this sequence):
//
// Phase 1: Understanding PDF Streams
//   #1  stream/filters.rs    - Understand PDF filter chains (READ THIS FIRST)
//
// Phase 2: Loading PDFs
//   #2  pdf/reader.rs        - Load PDFs into memory
//
// Phase 3: Image Pipeline
//   #3  image/extractor.rs   - Extract image streams from PDF
//   #4  image/decoder.rs     - Decode streams to raw pixels
//   #5  image/encoder.rs     - Encode pixels to JPEG
//   #6  image/compressor.rs  - Orchestrate the compression pipeline
//
// Phase 4: Saving PDFs
//   #7  stream/builder.rs    - Rebuild PDF streams with compressed data
//   #8  pdf/writer.rs        - Save compressed PDF
//   #9  pdf/objects.rs       - Helper utilities (optional, as needed)
//
// Phase 5: CLI
//   #10 cli/args.rs          - CLI argument parsing
//   #11 cli/batch.rs         - Batch processing logic
//
// =============================================================================

// Library modules (all the actual implementation)
mod cli;
mod pdf;

use pdf::reader::load_pdf;
use pdf::writer::compress_pdf;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== PRESSE PDF Compression Tool ===\n");

    // Test 1: Try loading an existing PDF
    println!("Test 1: Loading PDF...\n");
    let mut doc = match load_pdf("test.pdf") {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("Failed to load PDF: {}", e);
            return Err(e.into());
        }
    };
    println!("PDF loaded successfully!");

    println!("Test 1: Compressing PDF...\n");
    let name = "test_compressed.pdf";
    compress_pdf(&mut doc, name)?;

    Ok(())
}
