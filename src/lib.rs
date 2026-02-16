// =============================================================================
// PRESSE - PDF Compression Library
// =============================================================================
//
// IMPLEMENTATION ORDER:
// 1. stream/filters.rs    - Understand PDF filter chains
// 2. pdf/reader.rs        - Load PDFs into memory
// 3. image/extractor.rs   - Extract image streams from PDF
// 4. image/decoder.rs     - Decode streams to raw pixels
// 5. image/encoder.rs     - Encode pixels to JPEG
// 6. image/compressor.rs  - Orchestrate the compression pipeline
// 7. stream/builder.rs    - Rebuild PDF streams with compressed data
// 8. pdf/writer.rs        - Save compressed PDF
// 9. pdf/objects.rs       - Helper utilities (optional)
// 10. cli/args.rs         - CLI argument parsing
// 11. cli/batch.rs        - Batch processing logic
//
// =============================================================================

pub mod cli;
pub mod pdf;

// Re-export main types for convenience
pub use cli::args::{Cli, Commands, Preset};
pub use pdf::reader::load_pdf;
