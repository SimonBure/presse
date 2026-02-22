use clap::{Subcommand, Parser};
use std::path::PathBuf;

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser, Debug)]
#[command(name = "presse", author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Compress one or several PDF documents
    Compress {
        /// Positional
        input: Vec<PathBuf>,

        /// Output file (optional, defaults to <input>_compressed.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Details during the compression process --> sizes comparison before & after
        #[arg(short, long, default_value_t = false)]
        quiet: bool
    },

    /// Merge two or plus PDF documents together
    Merge {
        /// Position, 2+ pathes
        inputs: Vec<PathBuf>,

        /// Output file (optional, defaults to merged_document.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,
        
        /// Should Merge also compress the processed documents ? (quality to be added later)
        #[arg(short, long)]
        compress: bool,
    },
}

pub fn resolve_compress_output(input_path: &PathBuf, output_path: &Option<PathBuf>) -> PathBuf {
    let output = match output_path {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            // test.pdf --> outputs/test_compressed.pdf
            let stem = input_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}_compressed.pdf", stem))
        }
        Some(path) => path.clone(),  // test.pdf --> new_name.pdf
        None => {
            // test.pdf -> test_compressed.pdf (in same dir)
            let stem = input_path.file_stem().unwrap().to_str().unwrap();
            let mut path = input_path.clone();
            path.set_file_name(format!("{}_compressed.pdf", stem));
            path
        }
    };
    return output;
}

pub fn resolve_merge_output(output_path: &Option<PathBuf>) -> PathBuf {
    output_path.clone().unwrap_or_else(|| PathBuf::from("merged_document.pdf"))
}