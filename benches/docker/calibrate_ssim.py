#!/usr/bin/env python3
"""Calibrate the `-ssim` target → JPEG quality mapping.

`presse press -s <target>` derives the JPEG quality from a committed
curve (`SSIM_CALIBRATION` in src/pdf/images.rs). This script reproduces
that curve: it re-encodes representative sources (photos, scans) at
several qualities and measures the native 512-window luma SSIM vs the
source — the same metric `native_image_ssim.py` uses as the strict
witness. The reference content is *grainy gray scans*: the worst case for
JPEG, where artifacts are most visible, so the curve is conservative —
smoother content always exceeds the requested target.

Usage: calibrate_ssim.py <images...>
Prints the (quality, mean SSIM) table to paste into SSIM_CALIBRATION.
"""
import io
import sys

import numpy as np
from PIL import Image

Image.MAX_IMAGE_PIXELS = None

QUALITIES = (5, 10, 15, 25, 50, 75)


def ssim512(a: np.ndarray, b: np.ndarray) -> float:
    w = min(512, max(a.shape[0], a.shape[1]))
    a = np.asarray(Image.fromarray(a).convert("L").resize((w, w), Image.BILINEAR), dtype=np.float64)
    b = np.asarray(Image.fromarray(b).convert("L").resize((w, w), Image.BILINEAR), dtype=np.float64)
    n = a.size
    ma, mb = a.mean(), b.mean()
    va = ((a - ma) ** 2).sum() / n
    vb = ((b - mb) ** 2).sum() / n
    cov = ((a - ma) * (b - mb)).sum() / n
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    return ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))


def main():
    rows = {q: [] for q in QUALITIES}
    for f in sys.argv[1:]:
        try:
            src = np.asarray(Image.open(f).convert("RGB"))
        except Exception:
            continue
        for q in QUALITIES:
            buf = io.BytesIO()
            Image.fromarray(src).save(buf, "JPEG", quality=q)
            out = np.asarray(Image.open(io.BytesIO(buf.getvalue())).convert("RGB"))
            if out.shape != src.shape:
                out = np.asarray(Image.fromarray(out).resize((src.shape[1], src.shape[0])))
            rows[q].append(ssim512(src, out))
    print("const SSIM_CALIBRATION: [(u8, f64); %d] = [" % len(QUALITIES))
    for q in QUALITIES:
        vals = rows[q]
        mean = float(np.mean(vals)) if vals else float("nan")
        print(f"    ({q}, {mean:.4f}),")
    print("];")


if __name__ == "__main__":
    main()
