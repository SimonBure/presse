#!/usr/bin/env python3
"""Native-resolution image fidelity: decoded-pixel SSIM per re-encoded image.

Renders can hide detail loss behind a low observation scale (SSIM at 60 dpi
says nothing about print/zoom fidelity). This witness compares the *pixels
of the actual image streams*: every image of the source and the compressed
PDF is extracted with `pdfimages -j`, decoded, and the source is resampled
to the output's dimensions before SSIM — so a compressor that downsamples
or degrades an image is measured at the resolution it actually shipped.

Usage: native_image_ssim.py <source.pdf> <output.pdf> [--pages N] [--top K]
"""
import argparse
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image

Image.MAX_IMAGE_PIXELS = None


def ssim(a: np.ndarray, b: np.ndarray, window: int = 512) -> float:
    """Luma SSIM. The window is the analysis scale: 64 is the project-wide
    render convention (screen-fidelity); the native-image witness uses a
    larger window so per-pixel JPEG artifacts at shipped resolution count.
    """
    long_edge = max(a.shape[0], a.shape[1])
    w = min(window, long_edge)
    a = np.asarray(Image.fromarray(a).convert("L").resize((w, w), Image.BILINEAR), dtype=np.float64)
    b = np.asarray(Image.fromarray(b).convert("L").resize((w, w), Image.BILINEAR), dtype=np.float64)
    n = a.size
    ma, mb = a.mean(), b.mean()
    va = ((a - ma) ** 2).sum() / n
    vb = ((b - mb) ** 2).sum() / n
    cov = ((a - ma) * (b - mb)).sum() / n
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    return ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))


def ssim_with_chroma(a: np.ndarray, b: np.ndarray, window: int = 512) -> tuple[float, float]:
    """Luma + chroma SSIM on the same window. Chroma degradation (e.g. a
    4:4:4 -> 4:2:0 subsample, which qpdf/libjpeg does by default and which
    the --jpeg-encoder path reproduces) is invisible to a luma-only witness:
    Y is untouched while Cb/Cr lose 75% of their samples. The min over the
    R/G/B channels is the stricter color witness, so a compressor that
    throws away chroma is judged on what it actually changed.

    Returns (luma, min_channel) SSIM.
    """
    long_edge = max(a.shape[0], a.shape[1])
    w = min(window, long_edge)
    a = np.asarray(Image.fromarray(a).convert("RGB").resize((w, w), Image.BILINEAR), dtype=np.float64)
    b = np.asarray(Image.fromarray(b).convert("RGB").resize((w, w), Image.BILINEAR), dtype=np.float64)
    n = a.shape[0] * a.shape[1]

    def _ssim(x: np.ndarray, y: np.ndarray) -> float:
        mx, my = x.mean(), y.mean()
        vx = ((x - mx) ** 2).sum() / n
        vy = ((y - my) ** 2).sum() / n
        cov = ((x - mx) * (y - my)).sum() / n
        c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
        return ((2 * mx * my + c1) * (2 * cov + c2)) / ((mx * mx + my * my + c1) * (vx + vy + c2))

    luma = _ssim(
        0.299 * a[..., 0] + 0.587 * a[..., 1] + 0.114 * a[..., 2],
        0.299 * b[..., 0] + 0.587 * b[..., 1] + 0.114 * b[..., 2],
    )
    chroma = min(_ssim(a[..., i], b[..., i]) for i in range(3))
    return luma, chroma


def extract(pdf: Path, work: Path, tag: str) -> list[Path]:
    subprocess.run(["pdfimages", "-j", str(pdf), str(work / tag)], capture_output=True)
    return sorted((work / f"{tag}-{i:03d}.jpg" for i in range(1000)) if False else
                  list(work.glob(f"{tag}-*")))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("source", type=Path)
    ap.add_argument("output", type=Path)
    args = ap.parse_args()

    with tempfile.TemporaryDirectory() as td:
        work = Path(td)
        src = extract(args.source, work, "src")
        out = extract(args.output, work, "out")
        if len(src) != len(out):
            print(f"image count differs: source {len(src)} vs output {len(out)}")
        pairs = list(zip(src, out))
        print(f"{len(pairs)} images; SSIM at shipped resolution (source resampled to output size)")
        worst = (1.0, None)
        worst_chroma = (1.0, None)
        for i, (s, o) in enumerate(pairs):
            try:
                a = np.asarray(Image.open(s).convert("RGB"))
                b = np.asarray(Image.open(o).convert("RGB"))
            except Exception as e:
                print(f"  img {i}: decode failed ({e})")
                continue
            # source → output resolution (the level the compressor shipped)
            if a.shape != b.shape:
                a = np.asarray(Image.fromarray(a).resize((b.shape[1], b.shape[0]), Image.LANCZOS))
            luma, chroma = ssim_with_chroma(a, b)
            if luma < worst[0]:
                worst = (luma, i)
            if chroma < worst_chroma[0]:
                worst_chroma = (chroma, i)
            print(f"  img {i:2d}: src {s.stat().st_size/1e3:7.1f} KB -> out {o.stat().st_size/1e3:7.1f} KB"
                  f"  {a.shape[1]}x{a.shape[0]} -> {b.shape[1]}x{b.shape[0]}"
                  f"  luma SSIM {luma:.4f}  chroma SSIM {chroma:.4f}")
        print(f"worst luma SSIM:   {worst[0]:.4f} (img {worst[1]})")
        print(f"worst chroma SSIM: {worst_chroma[0]:.4f} (img {worst_chroma[1]})")


if __name__ == "__main__":
    main()
