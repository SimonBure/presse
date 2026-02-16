mod cli;
mod pdf;

use pdf::reader::load_pdf;
use pdf::writer::compress_pdf;

use cli::args::Args;
use clap::Parser;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== PRESSE PDF Compression Tool ===\n");

    let args = Args::parse();

    // Test 1: Try loading an existing PDF
    println!("Test 1: Loading PDF...\n");
    let mut doc = match load_pdf(args.input.to_str().unwrap()) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("Failed to load PDF: {}", e);
            return Err(e.into());
        }
    };
    println!("PDF loaded successfully!");

    println!("Test 1: Compressing PDF...\n");
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
    // let name = "test_compressed.pdf";
    compress_pdf(&mut doc, output.to_str().unwrap())?;

    Ok(())
}
