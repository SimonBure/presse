# presse

A fast command-line tool for PDF compression, written in Rust.

## Features

- **Image recompression** — re-encodes images at a target quality, skipping CMYK images
- **Structural compression** — object stream packing, xref stream compression, unused object removal
- **Batch processing** — compress multiple files in one command via shell wildcards
- **Smart output paths** — sensible defaults, explicit naming, or output to a directory

## Installation

```bash
cargo install presse
```

## Usage

```bash
# Single file — outputs document_compressed.pdf alongside the original
presse document.pdf

# Custom output name
presse document.pdf -o small.pdf

# Output to a directory
presse document.pdf -o compressed/

# Batch — multiple files into a directory
presse *.pdf -o compressed/

# Set JPEG quality (0–100, default 80)
presse document.pdf --quality 60

# Suppress output
presse document.pdf --verbose false
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>_compressed.pdf` | Output file or directory |
| `-q, --quality` | `80` | Image recompression quality (0–100) |
| `-v, --verbose` | `true` | Print size comparison after each file |

## Limitations

- CMYK images are not compressed (not currently handled by `image` crate)

## Dependencies

- [lopdf](https://github.com/niclasberg/lopdf) — PDF parsing and manipulation
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [indicatif](https://github.com/console-rs/indicatif) — Progress bars
- [image](https://github.com/image-rs/image) — JPEG decoding and encoding

## Contributions
We are happy to welcome contributions! The next step we have in mind is to migrate to subcommands to implement document merging or splitting in a single CLI! Pull requests are welcome.

## License
[GPL-3.0](LICENSE)
