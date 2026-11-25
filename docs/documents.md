# Documents — ODT, DOCX, EPUB and other ZIP-based files


OpenDocument (ODT, ODS, ODP…), Office Open XML (DOCX, XLSX, PPTX), EPUB,
CBZ and plain ZIP files are detected by their bytes and come back in
their own format, `<input>_compressed.<ext>`. The archive is rebuilt with
the same entries in the same order:

- **Contents** — JPEG, PNG and WebP images go through the image pipeline
  above (same `-q`, `-s`, `--palette`, `--keep-metadata`: JPEGs are
  re-encoded at q80 by default), PDFs through the PDF pipeline (with its
  flags, `-d` included), nested archives recursively (3 levels). Text,
  XML, fonts and everything else keep their content byte for byte.
- **Storage** — each entry keeps the smallest of its original compressed
  bytes, uncompressed, or deflate level 9. ODF and EPUB's `mimetype` entry
  stays first and uncompressed, as both formats require.
- **Metadata** — names, timestamps, attributes and comments are kept;
  ZIP extra fields (extended timestamps, Unix owners) are removed except
  Unicode names; `--keep-metadata` keeps them.
- **Left untouched**, with a message — signed documents (ODF, OOXML, EPUB
  and JAR signatures, APK signing block), since any change breaks the
  signature; encrypted, ZIP64, multi-disk and self-extracting archives.
  Entries over 256 MiB, and anything past 2 GiB of decompressed content
  per file (nested archives included), are copied as is.

Measured on documents written by LibreOffice (ODT, DOCX, ODS, XLSX) and
pandoc (EPUB) with 7 photos and screenshots: ODT −43.0%, DOCX −43.4%,
EPUB −43.5% — almost all from the JPEG re-encode; deflating the entries
alone gives 0–1%. Spreadsheets without images: −1.7% (ODS), −3.4% (XLSX).
A CBZ of 3 JPEG pages: −51%; a ZIP of 3 PDFs: −78%. Every output reads
back with Python's `zipfile` and `unzip -t`; LibreOffice opens the
documents with the same page count (spreadsheets render pixel-identical).
