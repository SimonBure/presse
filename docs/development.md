# Building, testing and dependencies


**A Rust toolchain (1.88+ — the workspace uses edition 2024, and `image`
and `lopdf` require 1.88) is the only build requirement.** The default CPU pipeline is pure Rust: Cargo compiles
every dependency from source, and no system C library is linked or needed.
The optional GPU backends load vendor libraries at runtime; building them
needs no CUDA toolkit. The `optimize` feature's codec crates compile from
source. No feature needs system packages beyond what its runtime loads:

| To use… | You need… |
|---|---|
| Default CPU build | nothing beyond Rust |
| `--features cuda` (nvJPEG) | an NVIDIA driver and `libnvjpeg` (loaded at runtime — no CUDA toolkit needed to *build*) |
| `--features cuda`, NVDEC stage | the above plus the Video Codec SDK: the driver's `libnvcuvid.so.1` **and** the SDK headers (`ffnvcodec-headers` on Debian/Ubuntu, `/usr/include/ffnvcodec/dynlink_nvcuvid.h`) |
| `--features rocm` | a ROCm installation with `rocjpeg` at runtime |
| `--features optimize` (JBIG2 / JPEG2000 / MRC / font subset / Zopfli / dedup) | nothing beyond Rust — the codec crates (`jbig2enc-rust`, `j2k`, `zopfli`, `subsetter`) compile from source; the build is heavy, not dependent |


The regression suite and the benchmark harness additionally want the usual
PDF tooling (never needed to build, and never needed just to compress a
PDF):

- **poppler-utils** (`pdftoppm`, `pdfinfo`) — the render witnesses and page
  counts used by `tests/regression.rs` and `benches/docker/pareto.py`
- **qpdf** — structural validation (`qpdf --check`) in the tests and in
  `ci/validate_corpus.py`
- **ghostscript** — a third renderer/validator and the `/ebook` benchmark
  opponent
- **mupdf-tools** (`mutool`) — a second renderer/oracle and the
  lossless-repack benchmark opponent
- **pngquant + tesseract-ocr-eng** — the OCRmyPDF leg of the Pareto sweep
  (ocrmypdf's `--optimize` mode hard-requires pngquant)
- **python3 + numpy + Pillow** — the `benches/docker/*.py` harness scripts
  (`pip install -r benches/docker/requirements.txt`)

Debian/Ubuntu one-liner for the full set (driver packages come from
NVIDIA's repository; the rest from distro packages):

```bash
sudo apt install qpdf poppler-utils ghostscript mupdf-tools \
                 pngquant tesseract-ocr-eng ffnvcodec-headers
```

Rust crates this project is built on:

- [lopdf](https://github.com/niclasberg/lopdf) — PDF parsing and manipulation
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [indicatif](https://github.com/console-rs/indicatif) — Progress bars
- [image](https://github.com/image-rs/image) — JPEG decoding and encoding
- [rayon](https://github.com/rayon-rs/rayon) — parallel image re-encoding
