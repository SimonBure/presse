#!/usr/bin/env python3
"""4-way benchmark over N real-world PDFs:
  presse parallel (rayon, all cores) vs presse serial (RAYON_NUM_THREADS=1)
  vs ghostscript /ebook vs mutool clean -gggg -z (lossless repack). Appends
  one CSV row per PDF so progress survives interruption.

Usage: bench3.py <in-dir> <out-csv> [presse-bin] [gs-args...]
"""
import csv
import os
import subprocess
import sys
import time
from pathlib import Path

IN = Path(sys.argv[1])
CSV = Path(sys.argv[2])
PRESSE = sys.argv[3] if len(sys.argv) > 3 else "/run/media/one/1tb_kingston1/presse/target/release/presse"
QUALITY = "50"

pdfs = sorted(IN.glob("*.pdf"))
new = not CSV.exists()
f = open(CSV, "a", newline="")
w = csv.writer(f)
if new:
    w.writerow(["pdf", "in_bytes", "p_parallel_s", "p_serial_s", "gs_s", "mutool_s",
                "out_parallel", "out_serial", "out_gs", "out_mutool", "imgs"])


def timed(cmd, timeout=600):
    t0 = time.perf_counter()
    try:
        r = subprocess.run(cmd, capture_output=True, timeout=timeout)
        return (time.perf_counter() - t0), r.returncode
    except subprocess.TimeoutExpired:
        return None, -1


done = set()
if CSV.exists():
    with open(CSV) as fh:
        done = {r[0] for r in csv.reader(fh) if r}

for i, pdf in enumerate(pdfs, 1):
    if pdf.name in done:
        print(f"[{i}/{len(pdfs)}] skip {pdf.name} (done)")
        continue
    size = pdf.stat().st_size

    t_par, rc = timed([PRESSE, "press", str(pdf), "-q", QUALITY, "-o", "/tmp/b3-par.pdf"])
    t_ser, rc2 = timed(["env", "RAYON_NUM_THREADS=1", PRESSE, "press", str(pdf), "-q", QUALITY, "-o", "/tmp/b3-ser.pdf"])
    t_gs, rc3 = timed(["gs", "-sDEVICE=pdfwrite", "-dPDFSETTINGS=/ebook", "-dNOPAUSE", "-dQUIET",
                       "-dBATCH", "-sOutputFile=/tmp/b3-gs.pdf", str(pdf)])
    t_mu, rc4 = timed(["mutool", "clean", "-gggg", "-z", str(pdf), "/tmp/b3-mu.pdf"])

    o_par = Path("/tmp/b3-par.pdf").stat().st_size if rc == 0 else 0
    o_ser = Path("/tmp/b3-ser.pdf").stat().st_size if rc2 == 0 else 0
    o_gs = Path("/tmp/b3-gs.pdf").stat().st_size if rc3 == 0 else 0
    o_mu = Path("/tmp/b3-mu.pdf").stat().st_size if rc4 == 0 else 0
    imgs = subprocess.run([PRESSE, "press", str(pdf), "-q", QUALITY, "-o", "/tmp/b3-chk.pdf", "-v"],
                          capture_output=True, text=True).stdout
    n_imgs = 0
    for line in imgs.splitlines():
        if "Found " in line:
            try:
                n_imgs = int(line.split("Found ")[1].split()[0])
            except Exception:
                pass
    w.writerow([pdf.name, size, f"{t_par:.4f}" if t_par else "NA", f"{t_ser:.4f}" if t_ser else "NA",
                f"{t_gs:.4f}" if t_gs else "NA", f"{t_mu:.4f}" if t_mu else "NA",
                o_par, o_ser, o_gs, o_mu, n_imgs])
    f.flush()
    print(f"[{i}/{len(pdfs)}] {pdf.name}: par={t_par and round(t_par,3)} ser={t_ser and round(t_ser,3)} gs={t_gs and round(t_gs,3)} mu={t_mu and round(t_mu,3)}")
f.close()
print("done ->", CSV)
