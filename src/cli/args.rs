use clap::Parser;
use std::path::PathBuf;

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser, Debug)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]

pub struct Args {
    /// Input file
    pub input: PathBuf,

    /// Output file (optional, defaults to <input>_compressed.pdf)
    pub output: Option<PathBuf>,

    // Details during the compression process --> sizes comparison before & after
    #[arg(short, long, default_value_t = false)]
    pub quiet: bool
}