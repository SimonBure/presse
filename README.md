# presse — compress PDFs, images and documents, offline
<p align="center">
    <img src="./logo.webp" alt="Presse's logo" width=150 height=150/>
</p>

![demo](demo/demo.gif)

<p align="center">
    <img src="https://img.shields.io/crates/v/presse.svg?style=for-the-badge" alt="Crates.io">
    <a href="https://github.com/SimonBure/presse"><img src="https://img.shields.io/github/stars/SimonBure/presse?style=for-the-badge&label=Stars%20&logo=github&logoColor=white" alt="Presse stars" /></a>
</p>

A fast command-line tool written in Rust. Everything runs on your machine:
no upload, no network access.

- **Compress PDFs** — image recompression and structural optimization,
  **~7× faster than Ghostscript** (median per document on a 100-PDF
  corpus, see [docs/benchmark.md](docs/benchmark.md)).
- **Compress JPEG, PNG and WebP images** in their own format (PNG and WebP
  stay lossless).
- **Compress documents** — ODT, ODS, ODP, DOCX, XLSX, PPTX, EPUB, CBZ and
  ZIP — by compressing the images and PDFs inside them.
- **Convert images** (PNG, JPEG, WebP, GIF, TIFF, BMP) to PDF, JPEG, PNG or
  WebP.
- **Merge** PDFs and images into one PDF.

Formats are detected from the file's bytes, not its extension, so PDFs,
images and documents can be mixed freely in one command.

## Installation

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SimonBure/presse/releases/latest/download/presse-installer.sh | sh
```

```powershell
# Windows (PowerShell); an .msi is also on the latest release page
powershell -ExecutionPolicy ByPass -c "irm https://github.com/SimonBure/presse/releases/latest/download/presse-installer.ps1 | iex"
```

```bash
# From source (Rust 1.88+)
cargo install presse
# + the heavier optional passes (see docs/optimize.md)
cargo install presse --features optimize
```

Downloads: [latest release](https://github.com/SimonBure/presse/releases/latest).

## Usage

### Compress — `presse press`

```bash
presse press document.pdf                  # → document_compressed.pdf
presse press document.pdf -o small.pdf     # custom output name
presse press *.pdf *.jpg *.odt -o out/     # batch into a directory, any mix
presse press photo.jpg logo.png            # images keep their format
presse press report.docx                   # documents keep their format
presse press document.pdf -q 60            # lower JPEG quality (default 80)
presse press document.pdf -d 150           # cap PDF images at 150 dpi
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>_compressed.<ext>` | Output file or directory |
| `-q, --quality` | `80` | JPEG quality (0–100) for images in PDFs, JPEG files and JPEGs inside documents; PNG and WebP stay lossless |
| `-d, --dpi` | source resolution | Cap images placed in PDFs to this resolution (75 screen, 150 ebook, 300 printer) |
| `-s, --ssim` | `1.0` | Target fidelity instead of a fixed quality; lower is smaller |
| `--keep-metadata` | `false` | Keep all image metadata (default: only the ICC profile and EXIF orientation) |
| `-v, --verbose` | `false` | Print what was done to each file |

More flags (palettes, chroma subsampling, scan classification, GPU,
`optimize` passes): [docs/pdf.md](docs/pdf.md).

### Merge — `presse merge`

```bash
presse merge a.pdf b.pdf c.pdf             # → merged.pdf
presse merge *.png *.pdf -o result.pdf     # images and PDFs together
presse merge a.pdf b.pdf --compress        # also compress images
```

### Convert — `presse convert`

```bash
presse convert photo.jpg                   # → photo.pdf
presse convert photo.png -o photo.webp     # the -o extension picks the format
presse convert *.png --to jpg -o jpegs/    # --to when -o is a directory
presse convert *.png -m -c -q 50           # all images into one compressed PDF
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output` | `<input>.<ext>` | Output file or directory; a file's extension picks the format |
| `-t, --to` | `pdf` | Output format when `-o` has no extension: `pdf`, `jpg`, `png`, `webp` |
| `-m, --merge` | `false` | Merge all images into one PDF |
| `-c, --compress` | `false` | JPEG-compress the images in the PDF output |
| `-q, --quality` | `80` | JPEG quality, for `--compress` and JPEG output |

## Good to know

- **Images and documents never grow.** When nothing smaller is found, the
  original is copied as is.
- **Output names.** Missing `-o` directories are created. A run never
  overwrites one of its inputs or a file it already wrote: the clashing
  output gets `_1`, `_2`… (`press d1/a.pdf d2/a.pdf -o out/` writes
  `a_compressed.pdf` and `a_compressed_1.pdf`). `-o` naming the input
  itself compresses it in place.
- **Signed files are left untouched.** A digital signature covers the
  file's bytes, so signed PDFs and signed documents are copied unchanged,
  with a message.
- **Limitations.** CMYK images are not re-encoded. GIF, TIFF and BMP files
  are not compressed yet. Lossy WebP files only have their metadata
  removed (no lossy WebP encoder).

## Documentation

| Topic | Page |
|---|---|
| Every `press` flag and the PDF passes in detail | [docs/pdf.md](docs/pdf.md) |
| JPEG, PNG and WebP compression, image conversion | [docs/images.md](docs/images.md) |
| ODT, DOCX, EPUB and other ZIP-based documents | [docs/documents.md](docs/documents.md) |
| The `optimize` feature (JBIG2, JPEG2000, MRC, fonts, Zopfli, dedup) | [docs/optimize.md](docs/optimize.md) |
| GPU acceleration (experimental) | [docs/gpu.md](docs/gpu.md) |
| Benchmark against Ghostscript | [docs/benchmark.md](docs/benchmark.md) |
| Building, testing and dependencies | [docs/development.md](docs/development.md) |
| Release history | [CHANGELOG.md](CHANGELOG.md) |

## Contributions

We are happy to welcome contributions! Pull requests are welcome.

## License

[GPL-3.0](LICENSE)

Made with ❤️ in Paris.
