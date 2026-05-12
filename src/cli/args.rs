use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

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

    Merge {
        /// Input files (>= 2, order matters)
        input: Vec<PathBuf>,

        /// Output file (optional, defaults to <input>_merged.pdf)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Compress the merged file
        #[arg(short, long, default_value_t = false)]
        compress: bool,
    },
}

pub fn resolve_press_path_output(file_path: &Path, output: &Option<PathBuf>) -> PathBuf {
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            path.join(format!("{}_compressed.pdf", stem))
        }
        Some(path) => path.clone(),
        None => {
            let stem = file_path.file_stem().unwrap().to_str().unwrap();
            let mut path = file_path.to_path_buf();
            path.set_file_name(format!("{}_compressed.pdf", stem));
            path
        }
    }
}

pub fn resolve_merge_path_output(output: &Option<PathBuf>, compress: bool) -> PathBuf {
    let default_name = if compress { "compressed_merged.pdf" } else { "merged.pdf" };
    match output {
        Some(path) if path.is_dir() || path.to_str().unwrap().ends_with('/') => {
            path.join(default_name)
        }
        Some(path) => path.clone(),
        None => PathBuf::from(default_name),
    }
}
