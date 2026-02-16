mod cli;
mod pdf;

use pdf::reader::{load_pdf, get_pdf_size_in_kilobytes, get_compression_ration_in_percent};
use pdf::writer::compress_pdf;

use cli::args::Args;
use clap::Parser;

use indicatif::ProgressBar;
use std::time::Duration;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== PRESSE PDF Compression Tool ===\n");

    let bar = ProgressBar::new(100);
    bar.enable_steady_tick(Duration::from_millis(100));

    let args = Args::parse();

    // Loading the document
    let input_file = args.input.to_str().unwrap();
    let mut doc = match load_pdf(input_file) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("Failed to load PDF: {}", e);
            return Err(e.into());
        }
    };

    // Compressing the document
    let output = match args.output {
        Some(path) => path,
        None => {
            // test.pdf -> test_compressed.pdf
            let stem = args.input.file_stem().unwrap().to_str().unwrap();
            let mut path = args.input.clone();
            path.set_file_name(format!("{}_compressed.pdf", stem));
            path
        }
    };
    let output_path = output.to_str().unwrap();
    compress_pdf(&mut doc, output_path)?;

    // Summary
    let original_size = get_pdf_size_in_kilobytes(input_file).unwrap();
    let compressed_size = get_pdf_size_in_kilobytes(output_path).unwrap();
    let compression_ratio = get_compression_ration_in_percent(original_size, compressed_size);
    println!("{}kB --> {}kB ({:.2}% compression)", original_size, compressed_size, compression_ratio);

    bar.finish();
    Ok(())
}
