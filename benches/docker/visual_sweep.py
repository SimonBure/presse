#!/usr/bin/env python3
"""Visual regression sweep over real-world PDFs.

Rasterizes every page (sampled for very long docs) of the original and the
`presse press -q 50` output with pdftoppm, then compares per page:
  - SSIM (global, 64x64 luma — same formula as tests/regression.rs)
  - luminance mean / variance ratios (catches washed-out or color-shifted
    renders, e.g. component-count mismatches)

Flags:
  [FAIL] SSIM < 0.80  or  |mean ratio-1| > 0.15  or  var ratio < 0.5 or > 1.5
  [warn] SSIM < 0.90
"""
import os
import re
import subprocess
import sys
from pathlib import Path

import numpy as np
from PIL import Image

# Some pdf.js stress-test PDFs have gigantic page sizes; they are trusted
# local files, so disable PIL's decompression-bomb guard.
Image.MAX_IMAGE_PIXELS = None

DIR = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/presse19")
OUT_SUBDIR = sys.argv[2] if len(sys.argv) > 2 else "out"
PAGE_CAP = int(sys.argv[3]) if len(sys.argv) > 3 else 80  # docs longer than this are sampled evenly
SSIM_WARN, SSIM_FAIL = 0.90, 0.80

pdftoppm = "pdftoppm"


def page_count(pdf: Path) -> int:
    out = subprocess.run(["pdfinfo", str(pdf)], capture_output=True, text=True).stdout
    m = re.search(r"Pages:\s+(\d+)", out)
    return int(m.group(1)) if m else 0


def render_all(pdf: Path, prefix: Path) -> int:
    # unique per-doc prefix in its own directory: stale renders from other
    # docs can never collide, even when pdftoppm zero-pads page numbers
    # differently (e.g. `-01.png` for 27 pages vs `-001.png` for 126).
    prefix.parent.mkdir(parents=True, exist_ok=True)
    for stale in prefix.parent.glob(f"{prefix.name}-*.png"):
        stale.unlink()
    r = subprocess.run([pdftoppm, "-png", "-r", "72", str(pdf), str(prefix)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return -1
    return 0


def load_page(prefix: Path, page: int) -> np.ndarray | None:
    for p in sorted(prefix.parent.glob(f"{prefix.name}-*.png")):
        m = re.search(r"-(\d+)\.png$", p.name)
        if m and int(m.group(1)) == page:
            return np.asarray(Image.open(p).convert("RGB"))
    return None


def ssim_and_stats(pre: np.ndarray, post: np.ndarray):
    a = np.asarray(Image.fromarray(pre).convert("L").resize((64, 64), Image.BILINEAR), dtype=np.float64)
    b = np.asarray(Image.fromarray(post).convert("L").resize((64, 64), Image.BILINEAR), dtype=np.float64)
    n = a.size
    ma, mb = a.mean(), b.mean()
    va = ((a - ma) ** 2).sum() / n
    vb = ((b - mb) ** 2).sum() / n
    cov = ((a - ma) * (b - mb)).sum() / n
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    s = ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
    return s, mb / ma if ma > 0 else 1.0, vb / va if va > 0 else 1.0


def main():
    docs = sorted(DIR.glob("*.pdf"))
    summary = []
    for pdf in docs:
        name = pdf.stem
        post = DIR / OUT_SUBDIR / f"{name}.pdf"
        if not post.exists():
            print(f"--- {name}: no output")
            continue
        total = page_count(pdf)
        if total == 0:
            print(f"--- {name}: pdfinfo failed")
            continue
        # sample pages
        if total <= PAGE_CAP:
            pages = list(range(1, total + 1))
        else:
            pages = sorted({int(round(x)) for x in np.linspace(1, total, PAGE_CAP)})
        pre_pfx = DIR / "_renders" / f"pre-{name}"
        post_pfx = DIR / "_renders" / f"post-{name}"
        if render_all(pdf, pre_pfx) != 0 or render_all(post, post_pfx) != 0:
            print(f"--- {name}: pdftoppm failed")
            continue
        worst, worst_page = 1.0, 0
        flags = []
        for pg in pages:
            a = load_page(pre_pfx, pg)
            b = load_page(post_pfx, pg)
            if a is None or b is None:
                continue
            s, mratio, vratio = ssim_and_stats(a, b)
            if s < worst:
                worst, worst_page = s, pg
            if s < SSIM_FAIL or abs(mratio - 1) > 0.15 or vratio < 0.5 or vratio > 1.5:
                flags.append((pg, f"FAIL s={s:.3f} m={mratio:.3f} v={vratio:.3f}"))
            elif s < SSIM_WARN:
                flags.append((pg, f"warn s={s:.3f} m={mratio:.3f} v={vratio:.3f}"))
        summary.append((name, total, len(pages), worst, worst_page, len(flags)))
        if flags:
            print(f"--- {name} ({total}p, sampled {len(pages)}): worst {worst:.3f} on p{worst_page}")
            for pg, msg in flags[:8]:
                print(f"      p{pg}: {msg}")
            if len(flags) > 8:
                print(f"      ... +{len(flags)-8} more")
        else:
            print(f"--- {name} ({total}p, sampled {len(pages)}): worst {worst:.3f} on p{worst_page} — clean")
    print("\n=== summary ===")
    print(f"{'doc':<16} {'pages':>5} {'samp':>4} {'worstSSIM':>9} {'page':>4} {'flags':>5}")
    for name, total, samp, worst, wp, nf in summary:
        print(f"{name:<16} {total:>5} {samp:>4} {worst:>9.3f} {wp:>4} {nf:>5}")


if __name__ == "__main__":
    main()
