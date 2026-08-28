#!/usr/bin/env python3
"""DPI sweep: file size and visual quality across `--dpi` levels.

For every PDF in <in-dir>, runs `presse press -q 50 [-d <dpi>]` at each dpi
level in the sweep (including the default, no-dpi run) and reports, per
file and level: output size, reduction %, wall time, and whole-image SSIM
of 60 dpi pdftoppm renders vs the source (same 64x64-luma formula as
`visual_sweep.py`). Long documents are sampled to the first N pages.

Usage: dpi_sweep.py <in-dir> [--presse BIN] [--dpis 75,150,300,600] [--pages N] [--csv FILE]
"""
import argparse
import csv
import subprocess
import time
from pathlib import Path

import numpy as np
from PIL import Image

Image.MAX_IMAGE_PIXELS = None

DPIS = [None, 600, 300, 150, 75]
PAGE_CAP = 20


def ssim(a: np.ndarray, b: np.ndarray) -> float:
    a = np.asarray(Image.fromarray(a).convert("L").resize((64, 64), Image.BILINEAR), dtype=np.float64)
    b = np.asarray(Image.fromarray(b).convert("L").resize((64, 64), Image.BILINEAR), dtype=np.float64)
    n = a.size
    ma, mb = a.mean(), b.mean()
    va = ((a - ma) ** 2).sum() / n
    vb = ((b - mb) ** 2).sum() / n
    cov = ((a - ma) * (b - mb)).sum() / n
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    return ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))


def page_count(pdf: Path) -> int:
    out = subprocess.run(["pdfinfo", str(pdf)], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if line.startswith("Pages:"):
            return int(line.split()[1])
    return 0


def render(pdf: Path, prefix: Path, pages: list[int]) -> dict[int, np.ndarray]:
    prefix.parent.mkdir(parents=True, exist_ok=True)
    for stale in prefix.parent.glob(f"{prefix.name}-*.png"):
        stale.unlink()
    subprocess.run(["pdftoppm", "-png", "-r", "60", str(pdf), str(prefix)], capture_output=True)
    out = {}
    for p in sorted(prefix.parent.glob(f"{prefix.name}-*.png")):
        num = int(p.stem.rsplit("-", 1)[1])
        if num in pages:
            out[num] = np.asarray(Image.open(p).convert("RGB"))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("in_dir", type=Path)
    ap.add_argument("--presse", required=True, help="path to the presse binary")
    ap.add_argument("--dpis", default="75,150,300,600")
    ap.add_argument("--pages", type=int, default=PAGE_CAP)
    ap.add_argument("--csv", type=Path)
    args = ap.parse_args()

    dpis = [int(d) for d in args.dpis.split(",")]
    levels = [None] + dpis
    writer = None
    if args.csv:
        f = open(args.csv, "w", newline="")
        writer = csv.writer(f)
        writer.writerow(["pdf", "in_bytes", "dpi", "out_bytes", "reduction_pct", "wall_s", "ssim"])

    for pdf in sorted(args.in_dir.glob("*.pdf")):
        total = page_count(pdf)
        if total == 0:
            print(f"--- {pdf.name}: pdfinfo failed")
            continue
        pages = list(range(1, min(total, args.pages) + 1))
        print(f"--- {pdf.name} ({total}p, sampled {len(pages)})")
        src_render = render(pdf, pdf.parent / f"_dpi_src_{pdf.stem}", pages)
        src_size = pdf.stat().st_size
        for dpi in levels:
            label = f"none" if dpi is None else str(dpi)
            out = pdf.parent / f"_dpi_{pdf.stem}_d{dpi or 0}.pdf"
            t0 = time.perf_counter()
            cmd = [args.presse, "press", "-q", "50", str(pdf), "-o", str(out)]
            if dpi:
                cmd[2:2] = ["-d", str(dpi)]
            r = subprocess.run(cmd, capture_output=True, timeout=600)
            wall = time.perf_counter() - t0
            if r.returncode != 0:
                print(f"  d={label:>4}: FAILED rc={r.returncode}")
                continue
            out_size = out.stat().st_size
            out_render = render(out, out.parent / f"_dpi_out_{pdf.stem}_d{dpi or 0}", pages)
            common = [p for p in pages if p in src_render and p in out_render]
            scores = [ssim(src_render[p], out_render[p]) for p in common]
            mean = float(np.mean(scores)) if scores else float("nan")
            print(f"  d={label:>4}: {out_size/1e6:8.3f} MB  {(1-out_size/src_size)*100:6.2f}%  {wall:6.3f} s  SSIM {mean:.4f}")
            if writer:
                writer.writerow([pdf.name, src_size, label, out_size,
                                 round((1 - out_size / src_size) * 100, 2), round(wall, 4), round(mean, 4)])
        for stale in pdf.parent.glob(f"_dpi_src_{pdf.stem}-*.png"):
            stale.unlink()
    if writer:
        f.close()
        print("== wrote", args.csv)


if __name__ == "__main__":
    main()
