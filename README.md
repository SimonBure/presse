[![Crates.io](https://img.shields.io/crates/v/presse.svg)](https://crates.io/crates/presse)
![demo](demo/demo.gif)

# presse

A fast command-line tool for PDF manipulation written in Rust.

**Compress and merge** PDF files naturally and easily with this ready-to-use command line tool.
**Convert images** of any format into ready-to-use pdfs.

## Features

- **Image recompression** — re-encodes images at a target quality, skipping CMYK images
- **Structural compression** — object stream packing, xref stream compression
- **Batch processing** — compress multiple files in one command via shell wildcards
- **Smart output paths** — sensible defaults, explicit naming, or output to a directory
- **PDF merging** — combine multiple documents into one, with optional compression
- **Image conversion** — transform any image (.png, .jpg) into a .pdf
- **Merge docs and images** in a single command, with smart format detection

## Installation

### Shell installer (macOS/Linux)
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SimonBure/presse/releases/latest/download/presse-installer.sh | sh
```

### PowerShell (Windows)
```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/SimonBure/presse/releases/latest/download/presse-installer.ps1 | iex"
```

### MSI (Windows)
Download the `.msi` from the [latest release](https://github.com/SimonBure/presse/releases/latest).

### Cargo
```bash
cargo install presse
```

## Benchmark

Measured over 19 real-world PDFs, comparing `presse press --quality 50` against Ghostscript `/ebook`.

| | presse | ghostscript |
|---|---|---|
| Mean execution time | **0.135s** | 0.927s |
| Mean size reduction | **+19.2%** | -10.2% |

Presse is **~7× faster** and compresses more effectively on this corpus. Ghostscript's `/ebook` preset can inflate already-optimised documents by downsampling images that are below its target DPI.

## Usage

### Compress — `presse press`

```bash
# Single file — outputs document_compressed.pdf alongside the original
presse press document.pdf

# Custom output name
presse press document.pdf -o small.pdf

# Output to a directory
presse press document.pdf -o compressed/

# Batch — multiple files into a directory
presse press *.pdf -o compressed/

# Set JPEG quality (0–100, default 80)
presse press document.pdf --quality 60

# Show size comparison after each file
presse press document.pdf --verbose
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>_compressed.pdf` | Output file or directory |
| `-q, --quality` | `80` | Image recompression quality (0–100) |
| `-v, --verbose` | `false` | Print size comparison after each file |

### Merge — `presse merge`

```bash
# Merge two or more files — outputs merged.pdf in the current directory
presse merge a.pdf b.pdf c.pdf

# Custom output name
presse merge a.pdf b.pdf -o result.pdf

# Output to a directory
presse merge a.pdf b.pdf -o output/

# Also compress images while merging
presse merge a.pdf b.pdf --compress
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `merged.pdf` | Output file or directory |
| `-c, --compress` | `false` | Compress images in the merged document |

## Limitations

- CMYK images are not compressed (not currently handled by `image` crate)

## Dependencies

- [lopdf](https://github.com/niclasberg/lopdf) — PDF parsing and manipulation
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [indicatif](https://github.com/console-rs/indicatif) — Progress bars
- [image](https://github.com/image-rs/image) — JPEG decoding and encoding

## Contributions
We are happy to welcome contributions! Pull requests are welcome.

## License
[GPL-3.0](LICENSE)
