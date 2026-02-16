use clap::Parser;
use std::path::PathBuf;

/// Fast PDF compression tool - easier and faster than ghostscript
#[derive(Parser, Debug)]
#[command(name = "presse")]
#[command(author, version, about, long_about = None)]

pub struct Args {
    /// Input file
    #[arg(short, long)]
    pub input: PathBuf,

    /// Output file
    #[arg(short, long)]
    pub output: PathBuf,
}