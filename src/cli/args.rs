use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]

pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Compress one or several PDF documents
    Press {
        /// Input file
        input: Vec<PathBuf>,

        /// Output file (optional, defaults to <input>_compressed.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Target quality for lossy image compression
        #[arg(short, long, default_value_t = 80)]
        quality: u8,

        // Details during the compression process --> sizes comparison before & after
        #[arg(short, long, default_value_t = false)]
        verbose: bool,
    },
}

pub fn resolve_output(file_path: &PathBuf, output: &Option<PathBuf>) -> PathBuf {
    let output = match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            // test.pdf --> outputs/test_compressed.pdf
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}_compressed.pdf", stem))
        }
        Some(path) => path.clone(), // test.pdf --> new_name.pdf
        None => {
            // test.pdf -> test_compressed.pdf (in same dir)
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            let mut path = file_path.clone();
            path.set_file_name(format!("{}_compressed.pdf", stem));
            path
        }
    };
    return output;
}
