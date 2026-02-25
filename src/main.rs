mod cli;
mod pdf;

use pdf::reader::{load_pdf, get_pdf_size_in_kilobytes, get_compression_ration_in_percent};
use pdf::writer::compress_and_save_pdf;
use pdf::images::compress_images;

use cli::args::Args;
use clap::Parser;

use indicatif::{ProgressBar, ProgressStyle};


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let bar = ProgressBar::new(args.input.len() as u64);
    bar.set_style(ProgressStyle::default_bar()
        .template("{bar:40.cyan/blue} {pos}/{len} {eta}")
        .unwrap()
    );

    // Fail if multiple files + output are given & output is not a dir 
    if args.input.len() > 1 {
        if let Some(ref path) = args.output {
            if !path.is_dir() && !path.to_str().unwrap().ends_with('/') {
                eprintln!("Error: -o must be a directory when compressing multiple documents");
                std::process::exit(1);
            }
        }
    }

    // Create output dir if needed (once)
    if let Some(ref path) = args.output {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // If -o is a directory itself (ends with /)
        if path.to_str().unwrap().ends_with('/') {
            std::fs::create_dir_all(path)?;
        }
    }
    
    for file_path in &args.input {
        // Loading the document
        let mut doc = match load_pdf(file_path.to_str().unwrap()) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("Skipping {}: {}", file_path.display(), e);
                continue;
            }
        };
    
        compress_images(&mut doc, args.quality);

        // Compressing the document
        let output = args.resolve_output(&file_path);
        compress_and_save_pdf(&mut doc, output.to_str().unwrap())?;

        // Compression summary
        if args.verbose {
            let original_size = get_pdf_size_in_kilobytes(file_path.to_str().unwrap()).unwrap();
            let compressed_size = get_pdf_size_in_kilobytes(output.to_str().unwrap()).unwrap();
            let compression_ratio = get_compression_ration_in_percent(original_size, compressed_size);
            bar.println(format!("{}kB → {}kB ({:.2}% compression)", original_size, compressed_size, compression_ratio));
        }

        bar.inc(1);
    }

    bar.finish_with_message("Done");

    Ok(())
}
