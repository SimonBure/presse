#!/usr/bin/env python3
"""Pareto-frontier benchmark: size × fidelity × time across PDF compressors.

Compares compressors at *equal measured fidelity* (SSIM of pdftoppm renders
at multiple DPIs), not at equal arbitrary quality numbers:

  presse          press -q <q> [-d <dpi>] [--palette] [--raster-classify]
                  [--recompress-flate]
  ghostscript     pdfwrite -dPDFSETTINGS=/<screen|ebook|printer|prepress>
  qpdf            --optimize-images --jpeg-quality=<q> --object-streams=generate
  mutool          clean -i -gggg -z --{color,gray}{,-lossless}-image-recompress-method jpeg:<q>
                  ("recompress when smaller" is the default; no subsampling)
  pdf-optimizer   -q <q> --dpi <0|150>
  ocrmypdf        python3 -m ocrmypdf -m skip -l eng --output-type pdf --optimize 3
                  --jpeg-quality <q> (needs pngquant + tesseract-ocr-eng + the
                  hocr config at runtime — installable via the bench image)

For every file × tool × setting: output size, wall time, and SSIM of
renders vs the source at each DPI in --render-dpis (screen 72, ebook 150,
print 300, zoom 600). SSIM at the *strictest* render DPI is the fidelity
witness, so a compressor can't hide detail loss behind a low render scale.

Summary: for each threshold in --thresholds, the smallest output per tool
and the fastest per tool among settings that meet the threshold.

Usage: pareto.py <in-dir> --presse BIN --pdf-optimize BIN [--render-dpis 72,300]
       [--thresholds 0.9999,0.999,0.995,0.99] [--pages N] [--csv FILE]
"""
import argparse
import csv
import subprocess
import time
from pathlib import Path

import numpy as np
from PIL import Image

Image.MAX_IMAGE_PIXELS = None

QUALITIES = [30, 50, 75]


def ssim(a: np.ndarray, b: np.ndarray) -> float:
    """The fidelity witness for one render pair: min over luma and the R/G/B
    channels. Luma alone hides chroma degradation (a 4:4:4 -> 4:2:0 subsample
    leaves Y untouched), so the stricter color witness is included — a
    compressor that throws away chroma is judged on what it changed.
    """
    n = 64 * 64

    def _one(a: np.ndarray, b: np.ndarray) -> float:
        a = np.asarray(Image.fromarray(a).resize((64, 64), Image.BILINEAR), dtype=np.float64)
        b = np.asarray(Image.fromarray(b).resize((64, 64), Image.BILINEAR), dtype=np.float64)
        ma, mb = a.mean(), b.mean()
        va = ((a - ma) ** 2).sum() / n
        vb = ((b - mb) ** 2).sum() / n
        cov = ((a - ma) * (b - mb)).sum() / n
        c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
        return ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))

    rgb_a = np.asarray(Image.fromarray(a).convert("RGB"), dtype=np.float64)
    rgb_b = np.asarray(Image.fromarray(b).convert("RGB"), dtype=np.float64)
    luma = _one(
        0.299 * rgb_a[..., 0] + 0.587 * rgb_a[..., 1] + 0.114 * rgb_a[..., 2],
        0.299 * rgb_b[..., 0] + 0.587 * rgb_b[..., 1] + 0.114 * rgb_b[..., 2],
    )
    chroma = min(_one(rgb_a[..., i], rgb_b[..., i]) for i in range(3))
    return min(luma, chroma)


def page_count(pdf: Path) -> int:
    out = subprocess.run(["pdfinfo", str(pdf)], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if line.startswith("Pages:"):
            return int(line.split()[1])
    return 0


def render(pdf: Path, prefix: Path, pages: list[int], dpi: int) -> dict[int, np.ndarray]:
    prefix.parent.mkdir(parents=True, exist_ok=True)
    for stale in prefix.parent.glob(f"{prefix.name}-{dpi}-*.png"):
        stale.unlink()
    try:
        r = subprocess.run(
            ["pdftoppm", "-png", "-r", str(dpi), str(pdf), f"{prefix}-{dpi}"],
            capture_output=True,
            timeout=300,
        )
        if r.returncode != 0:
            return {}
    except subprocess.TimeoutExpired:
        # e.g. a 41-inch page at 300 dpi is a ~100 MP render; skip rather
        # than kill the whole sweep (its SSIM cell reads as missing/nan).
        return {}
    out = {}
    for p in sorted(prefix.parent.glob(f"{prefix.name}-{dpi}-*.png")):
        num = int(p.stem.rsplit("-", 1)[1])
        if num in pages:
            out[num] = np.asarray(Image.open(p).convert("RGB"))
    return out


def tool_settings(
    tools: list[str], optimize: bool = False
) -> list[tuple[str, str, list[str]]]:
    """(tool, setting-label, command template with {in} {out})"""
    out = []
    for q in QUALITIES:
        out.append(("presse", f"q{q}", ["{presse}", "press", "-q", str(q), "{in}", "-o", "{out}"]))
        out.append(("presse", f"q{q}-palette", ["{presse}", "press", "-q", str(q), "--palette",
                                                  "{in}", "-o", "{out}"]))
        # Representation-selection flags: the classifier (bitonal text ->
        # 1-bit CCITT G4 /ImageMask, flat-color -> /Indexed) and the
        # structural Flate recompression, alone and combined.
        out.append(("presse", f"q{q}-classify", ["{presse}", "press", "-q", str(q), "--raster-classify",
                                                  "{in}", "-o", "{out}"]))
        out.append(("presse", f"q{q}-reflate", ["{presse}", "press", "-q", str(q), "--recompress-flate",
                                                  "{in}", "-o", "{out}"]))
        out.append(("presse", f"q{q}-classify-reflate", ["{presse}", "press", "-q", str(q),
                                                           "--raster-classify", "--recompress-flate",
                                                           "{in}", "-o", "{out}"]))
        out.append(("presse", f"q{q}-palette-classify", ["{presse}", "press", "-q", str(q),
                                                           "--palette", "--raster-classify",
                                                           "{in}", "-o", "{out}"]))
        # optimize-feature candidates (need a `--features optimize` build; gated
        # behind --optimize so a plain build's sweep doesn't run them). The
        # default-off codec candidates (JBIG2/JPEG2000/MRC), the structural
        # passes (dedup/zopfli/font-subset) and the effort presets, at one
        # quality each. They enter the same size court as everything else, so
        # this is where the Pareto matrix sees them.
        if optimize and q == 50:
            for label, extra in [
                ("jbig2", ["--jbig2"]),
                ("jpeg2000", ["--jpeg2000"]),
                ("mrc", ["--mrc"]),
                ("font-subset", ["--font-subset"]),
                ("dedup", ["--dedup"]),
                ("zopfli", ["--zopfli"]),
                ("comp-small", ["--compression", "small"]),
                ("comp-smallest", ["--compression", "smallest"]),
            ]:
                out.append(("presse", f"q{q}-{label}",
                            ["{presse}", "press", "-q", str(q), *extra, "{in}", "-o", "{out}"]))
        out.append(("qpdf", f"q{q}", ["qpdf", "--optimize-images", f"--jpeg-quality={q}",
                                      "--object-streams=generate", "{in}", "{out}"]))
        out.append(("mutool", f"jpeg:{q}", ["mutool", "clean", "-i", "-gggg", "-z",
                                            f"--color-image-recompress-method", f"jpeg:{q}",
                                            f"--color-lossless-image-recompress-method", f"jpeg:{q}",
                                            f"--gray-image-recompress-method", f"jpeg:{q}",
                                            f"--gray-lossless-image-recompress-method", f"jpeg:{q}",
                                            "{in}", "{out}"]))
        out.append(("pdf-optimizer", f"q{q}", ["node", "{pdfopt}", "-q", str(q), "--dpi", "0",
                                               "{in}", "-o", "{out}"]))
        out.append(("ocrmypdf", f"q{q}", ["python3", "-m", "ocrmypdf", "-m", "skip", "-l", "eng",
                                           "--output-type", "pdf", "--optimize", "3",
                                           f"--jpeg-quality={q}", "{in}", "{out}"]))
    # The dpi × ssim cross matrix (q50 base): every combination of the two
    # quality knobs, so a cell like d150-s0.86 is `press -q 50 -d 150 -s 0.86`.
    for d in [None, 75, 150, 300, 600]:
        for s in [None, 0.86, 0.72]:
            label = f"d{d or 0}-s{s if s else 1.0}"
            cmd = ["{presse}", "press", "-q", "50"]
            if d:
                cmd += ["-d", str(d)]
            if s:
                cmd += ["-s", f"{s:.2f}"]
            cmd += ["{in}", "-o", "{out}"]
            out.append(("presse", label, cmd))
    out.append(("pdf-optimizer", "q50-d150", ["node", "{pdfopt}", "-q", "50", "--dpi", "150",
                                              "{in}", "-o", "{out}"]))
    for s in ["screen", "ebook", "printer", "prepress"]:
        out.append(("ghostscript", s, ["gs", "-q", "-sDEVICE=pdfwrite", f"-dPDFSETTINGS=/{s}",
                                       "-dNOPAUSE", "-dBATCH", "-sOutputFile={out}", "{in}"]))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("in_dir", type=Path)
    ap.add_argument("--presse", required=True)
    ap.add_argument("--pdf-optimize", required=True)
    ap.add_argument("--render-dpis", default="72,300")
    ap.add_argument("--thresholds", default="0.9999,0.999,0.995,0.99")
    ap.add_argument("--pages", type=int, default=8)
    ap.add_argument("--tools", default="presse,qpdf,mutool,pdf-optimizer,ghostscript,ocrmypdf")
    ap.add_argument("--optimize", action="store_true",
                    help="also sweep the optimize-feature candidates (--jbig2/--jpeg2000/"
                         "--mrc/--font-subset/--dedup/--zopfli and --compression presets; "
                         "the presse binary must be a --features optimize build)")
    ap.add_argument("--csv", type=Path)
    args = ap.parse_args()

    render_dpis = [int(d) for d in args.render_dpis.split(",")]
    thresholds = [float(t) for t in args.thresholds.split(",")]
    tools = args.tools.split(",")
    settings = [s for s in tool_settings(tools, args.optimize) if s[0] in tools]
    strict = max(render_dpis)  # the fidelity witness

    writer = None
    if args.csv:
        f = open(args.csv, "w", newline="")
        writer = csv.writer(f)
        writer.writerow(["pdf", "tool", "setting", "out_bytes", "wall_s"] +
                        [f"ssim@{d}" for d in render_dpis])

    rows = []
    for pdf in sorted(args.in_dir.glob("*.pdf")):
        total = page_count(pdf)
        if total == 0:
            print(f"--- {pdf.name}: pdfinfo failed")
            continue
        pages = list(range(1, min(total, args.pages) + 1))
        src_size = pdf.stat().st_size
        src_renders = {d: render(pdf, pdf.parent / f"_po_src_{pdf.stem}", pages, d) for d in render_dpis}
        print(f"--- {pdf.name} ({total}p, sampled {len(pages)}, {src_size/1e6:.1f} MB)")
        for tool, label, tmpl in settings:
            out = pdf.parent / f"_po_{pdf.stem}_{tool}_{label.replace(':','_')}.pdf"
            cmd = [a.replace("{presse}", args.presse).replace("{pdfopt}", args.pdf_optimize)
                   .replace("{in}", str(pdf)).replace("{out}", str(out)) for a in tmpl]
            t0 = time.perf_counter()
            try:
                r = subprocess.run(cmd, capture_output=True, timeout=600)
                wall = time.perf_counter() - t0
            except subprocess.TimeoutExpired:
                print(f"  {tool:13s} {label:10s}: TIMEOUT")
                continue
            if r.returncode != 0 or not out.exists():
                print(f"  {tool:13s} {label:10s}: FAILED rc={r.returncode}")
                continue
            out_size = out.stat().st_size
            scores = {}
            for d in render_dpis:
                out_render = render(out, out.parent / f"_po_out_{pdf.stem}_{tool}_{label.replace(':','_')}", pages, d)
                common = [p for p in pages if p in src_renders[d] and p in out_render]
                vals = [ssim(src_renders[d][p], out_render[p]) for p in common]
                scores[d] = float(np.mean(vals)) if vals else float("nan")
            rows.append({"pdf": pdf.name, "tool": tool, "setting": label, "bytes": out_size,
                         "wall": wall, "scores": scores})
            print(f"  {tool:13s} {label:10s}: {out_size/1e6:7.3f} MB  {wall:6.2f} s  " +
                  "  ".join(f"{d}:{scores[d]:.4f}" for d in render_dpis))
            if writer:
                writer.writerow([pdf.name, tool, label, out_size, round(wall, 3)] +
                                [round(scores[d], 4) for d in render_dpis])
        for d in render_dpis:
            for stale in pdf.parent.glob(f"_po_src_{pdf.stem}-{d}-*.png"):
                stale.unlink()
    if writer:
        f.close()

    # ---- Pareto summary at each threshold (fidelity witness = strict DPI) ----
    print("\n=== Pareto: smallest output meeting SSIM >= T (at", f"{strict} dpi render) ===")
    for t in thresholds:
        print(f"\n-- threshold {t}")
        by_file = {}
        for r in rows:
            by_file.setdefault(r["pdf"], []).append(r)
        for pdf in sorted(by_file):
            meets = [r for r in by_file[pdf] if r["scores"].get(strict, 0) >= t]
            if not meets:
                print(f"  {pdf}: no tool meets {t}")
                continue
            best = min(meets, key=lambda r: r["bytes"])
            fastest = min(meets, key=lambda r: r["wall"])
            print(f"  {pdf}: smallest {best['tool']}/{best['setting']} "
                  f"({best['bytes']/1e6:.2f} MB, ssim@{strict} {best['scores'][strict]:.4f}, {best['wall']:.2f} s)   "
                  f"fastest {fastest['tool']}/{fastest['setting']} ({fastest['wall']:.2f} s, "
                  f"{fastest['bytes']/1e6:.2f} MB)")


if __name__ == "__main__":
    main()
