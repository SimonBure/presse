mod cli;
mod pdf;

use pdf::reader::{load_pdf, get_pdf_size_in_kilobytes, get_compression_ration_in_percent};
use pdf::writer::compress_pdf;

use cli::args::Args;
use clap::Parser;

use indicatif::ProgressIterator;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Fail if multiple files + output are given & output is not a dir 
    if args.input.len() > 1 {
        if let Some(ref path) = args.output {
            if !path.is_dir() && !path.to_str().unwrap().ends_with('/') {
                eprintln!("Error: -o must be a directory when compressing multiple documents");
                std::process::exit(1);
            }
        }
    }
    
    for file_path in (args.input).iter().progress() {
        // Loading the document
        let mut doc = match load_pdf(file_path.to_str().unwrap()) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("Skipping {}: {}", file_path.display(), e);
                continue;
            }
        };

        // Compressing the document
        let output = match &args.output {
            Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
                // test.pdf --> outputs/test_compressed.pdf
                let stem = file_path.file_stem().unwrap().to_str().unwrap();
                path.join(format!("{}_compressed.pdf", stem))
            }
            Some(path) => path.clone(),  // test.pdf --> new_name.pdf
            None => {
                // test.pdf -> test_compressed.pdf (in same dir)
                let stem = file_path.file_stem().unwrap().to_str().unwrap();
                let mut path = file_path.clone();
                path.set_file_name(format!("{}_compressed.pdf", stem));
                path
            }
        };

        // Create parent dir if needed
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let output_path = output.to_str().unwrap();
        compress_pdf(&mut doc, output_path)?;

        // Compression summary
        if !args.quiet {
            let original_size = get_pdf_size_in_kilobytes(file_path.to_str().unwrap()).unwrap();
            let compressed_size = get_pdf_size_in_kilobytes(output_path).unwrap();
            let compression_ratio = get_compression_ration_in_percent(original_size, compressed_size);
            println!("{}kB → {}kB ({:.2}% compression)", original_size, compressed_size, compression_ratio);
        }
    }

    Ok(())
}
