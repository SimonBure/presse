use clap::Parser;
use std::path::PathBuf;

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser, Debug)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]

pub struct Args {
    /// Input file
    pub input: Vec<PathBuf>,

    /// Output file (optional, defaults to <input>_compressed.pdf)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Target quality for lossy image compression
    #[arg(short, long, default_value_t = 80)]
    pub quality: u8,

    // Details during the compression process --> sizes comparison before & after
    #[arg(short, long, default_value_t = true)]
    pub verbose: bool
}

impl Args {
    pub fn resolve_output(&self, file_path: &PathBuf) -> PathBuf {
        let output = match &self.output {
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
        return output;
    }
}