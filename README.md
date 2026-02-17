# presse

A fast and friendly command-line tool for PDF compression, written in Rust.

> **Status:** Early development. Not yet published to crates.io.

## Features

- **PDF compression** -- Object stream packing, xref stream compression, unused object removal, and zero-length stream cleanup. Image recompression is planned.
- **Batch processing** -- Compress multiple files in one command. Supports shell wildcards.
- **Smart output paths** -- Default naming (`file_compressed.pdf`), explicit output (`-o name.pdf`), or output to a directory (`-o dir/`).

## Installation

Clone and build from source:

```bash
git clone https://github.com/your-username/presse.git
cd presse
cargo build --release
```

The binary will be at `target/release/presse`.

## Usage (planned)

```bash
# Single file (outputs test_compressed.pdf)
presse test.pdf

# Single file with custom output name
presse test.pdf -o small.pdf

# Output to a directory
presse test.pdf -o output/

# Multiple files
presse file1.pdf file2.pdf file3.pdfWrite the readme.md 

# Wildcard
presse *.pdf

# Batch into a directory
presse *.pdf -o compressed/

# Quiet mode (no output)
presse test.pdf -q
```

## Roadmap

- [ ] Image extraction and JPEG recompression
- [ ] Quality presets (screen, ebook, print, prepress)
- [ ] Verbose mode with per-image compression details
- [ ] Font subsetting for oversized embedded fonts
- [ ] Parallel processing with rayon

## Dependencies

- [lopdf](https://github.com/niclasberg/lopdf) -- PDF parsing and manipulation
- [clap](https://github.com/clap-rs/clap) -- CLI argument parsing
- [indicatif](https://github.com/console-rs/indicatif) -- Progress bars

## License

MIT
