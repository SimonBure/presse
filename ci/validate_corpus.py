#!/usr/bin/env python3
"""Validity gate — every PDF presse produces must be structurally sound.

presse transforms files, so the unit under test is the output document, not
the return value. A run that exits 0 while emitting a PDF with a truncated
cross-reference table is a failure, and this gate is what says so.

Three independent validators, because they disagree usefully:

  qpdf --check   strictest; validates every xref entry. Exit 2 = errors,
                 3 = warnings; both are failures here.
  pdfinfo        poppler's parser — the one that rejected presse output
                 before the lopdf 0.44 bump.
  ghostscript    a third, more lenient parser; catches content-stream damage
                 the other two tolerate.

Inputs are validated before use: a corrupt fixture would silently weaken
every assertion built on it (see `compressed_test.pdf` in the repo root).

Tool availability is fatal when PRESSE_REQUIRE_PDF_TOOLS=1. Absent that,
missing tools downgrade the gate with a loud warning — never silently.

Usage: validate_corpus.py <presse-binary> [fixture-dir]
"""

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pdf_xref  # noqa: E402  (local module, path set above)

REQUIRE = os.environ.get("PRESSE_REQUIRE_PDF_TOOLS") == "1"
QUALITY = "50"
TOOLS = ("qpdf", "pdfinfo", "gs")

GREEN, RED, YELLOW, DIM, RESET = (
    ("\033[32m", "\033[31m", "\033[33m", "\033[2m", "\033[0m")
    if sys.stdout.isatty() else ("", "", "", "", "")
)


def have(tool):
    return shutil.which(tool) is not None


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=300, **kw)


def page_count(path):
    if not have("pdfinfo"):
        return None
    r = run(["pdfinfo", str(path)])
    for line in r.stdout.splitlines():
        if line.startswith("Pages:"):
            return int(line.split()[1])
    return None


def check_pdf(path):
    """Return a list of problems; empty means the document is sound."""
    problems = []

    if not path.exists() or path.stat().st_size == 0:
        return ["no output produced"]

    # Built-in, always runs. This is the check that catches the failure
    # lenient parsers hide by silently reconstructing the xref.
    try:
        problems.extend(pdf_xref.check(path))
    except Exception as exc:  # a crash here is itself a finding
        problems.append(f"xref: validator error ({exc})")

    if have("qpdf"):
        r = run(["qpdf", "--check", str(path)])
        text = r.stdout + r.stderr
        if r.returncode != 0:
            first = next(
                (ln.strip() for ln in text.splitlines()
                 if "WARNING" in ln or "ERROR" in ln),
                f"qpdf exit {r.returncode}",
            )
            problems.append(f"qpdf: {first}")

    if have("pdfinfo"):
        r = run(["pdfinfo", str(path)])
        if r.returncode != 0:
            first = next((ln.strip() for ln in r.stderr.splitlines() if ln.strip()),
                         f"exit {r.returncode}")
            problems.append(f"pdfinfo: {first}")

    if have("gs"):
        r = run(["gs", "-sDEVICE=nullpage", "-dNOPAUSE", "-dBATCH", "-dQUIET",
                 str(path)])
        if r.returncode != 0:
            problems.append(f"ghostscript: rejected (exit {r.returncode})")

    return problems


class Report:
    def __init__(self):
        self.failures = []
        self.checked = 0

    def case(self, name, problems, note=""):
        self.checked += 1
        if problems:
            self.failures.append((name, problems))
            print(f"  {RED}FAIL{RESET}  {name}")
            for p in problems:
                print(f"        {RED}{p}{RESET}")
        else:
            suffix = f"  {DIM}{note}{RESET}" if note else ""
            print(f"  {GREEN}ok{RESET}    {name}{suffix}")


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: validate_corpus.py <presse-binary> [fixture-dir]")

    presse = Path(sys.argv[1]).resolve()
    fixtures = Path(sys.argv[2] if len(sys.argv) > 2 else "tests/fixtures").resolve()

    if not presse.exists():
        sys.exit(f"presse binary not found: {presse}")

    missing = [t for t in TOOLS if not have(t)]
    if missing:
        msg = f"missing PDF validators: {', '.join(missing)}"
        if REQUIRE:
            print(f"{RED}FATAL{RESET} {msg}")
            print("PRESSE_REQUIRE_PDF_TOOLS=1 is set, so this is an error.")
            return 2
        print(f"{YELLOW}WARNING{RESET} {msg} — the gate is running degraded.")
        print(f"{YELLOW}        The built-in xref check still runs; the "
              f"external validators above do not.{RESET}")
        print(f"{YELLOW}        Set PRESSE_REQUIRE_PDF_TOOLS=1 to make this "
              f"fatal (CI does).{RESET}\n")
    else:
        print(f"{DIM}validators: built-in xref, qpdf, pdfinfo, "
              f"ghostscript{RESET}\n")

    pdfs = sorted(fixtures.glob("*.pdf"))
    images = sorted(fixtures.glob("*.png"))
    if not pdfs:
        sys.exit(f"no fixtures found in {fixtures} — run ci/make_fixtures.py first")

    rep = Report()
    tmp = Path(tempfile.mkdtemp(prefix="presse-gate-"))

    # ---- 0. the fixtures themselves -------------------------------------
    print("fixtures (inputs must be sound before anything is built on them)")
    for f in pdfs:
        rep.case(f"input {f.name}", check_pdf(f))
    print()

    # ---- 1. press --------------------------------------------------------
    print(f"press -q {QUALITY}")
    for f in pdfs:
        out = tmp / f"press-{f.name}"
        r = run([str(presse), "press", str(f), "-q", QUALITY, "-o", str(out)])
        if r.returncode != 0:
            rep.case(f"press {f.name}", [f"exit {r.returncode}: {r.stderr.strip()[:120]}"])
            continue

        problems = check_pdf(out)

        before, after = page_count(f), page_count(out)
        if before is not None and after is not None and before != after:
            problems.append(f"page count changed: {before} -> {after}")

        note = ""
        if out.exists() and f.stat().st_size:
            delta = (out.stat().st_size - f.stat().st_size) / f.stat().st_size * 100
            note = f"{f.stat().st_size:>6} -> {out.stat().st_size:>6} B ({delta:+.0f}%)"
        rep.case(f"press {f.name}", problems, note)
    print()

    # ---- 2. merge --------------------------------------------------------
    print("merge")
    if len(pdfs) >= 2:
        pair = pdfs[:2]
        out = tmp / "merged.pdf"
        r = run([str(presse), "merge", *[str(p) for p in pair], "-o", str(out)])
        if r.returncode != 0:
            rep.case("merge pair", [f"exit {r.returncode}: {r.stderr.strip()[:120]}"])
        else:
            problems = check_pdf(out)
            want = sum(page_count(p) or 0 for p in pair)
            got = page_count(out)
            if want and got is not None and want != got:
                problems.append(f"page count wrong: expected {want}, got {got}")
            rep.case(f"merge {pair[0].name} + {pair[1].name}", problems,
                     f"{want} pages")
    print()

    # ---- 3. convert ------------------------------------------------------
    print("convert")
    for img in images:
        out = tmp / f"convert-{img.stem}.pdf"
        r = run([str(presse), "convert", str(img), "-o", str(out)])
        if r.returncode != 0:
            rep.case(f"convert {img.name}",
                     [f"exit {r.returncode}: {r.stderr.strip()[:120]}"])
            continue
        rep.case(f"convert {img.name}", check_pdf(out))
    if not images:
        print(f"  {DIM}no image fixtures{RESET}")
    print()

    # ---- 4. idempotence --------------------------------------------------
    print("idempotence (compressing an output again must stay sound)")
    for f in pdfs[:3]:
        once, twice = tmp / f"i1-{f.name}", tmp / f"i2-{f.name}"
        r1 = run([str(presse), "press", str(f), "-q", QUALITY, "-o", str(once)])
        if r1.returncode != 0 or not once.exists():
            rep.case(f"idempotence {f.name}", ["first pass failed"])
            continue
        r2 = run([str(presse), "press", str(once), "-q", QUALITY, "-o", str(twice)])
        if r2.returncode != 0:
            rep.case(f"idempotence {f.name}", [f"second pass exit {r2.returncode}"])
            continue
        rep.case(f"idempotence {f.name}", check_pdf(twice))
    print()

    # ---- summary ---------------------------------------------------------
    shutil.rmtree(tmp, ignore_errors=True)
    bar = "-" * 58
    print(bar)
    if rep.failures:
        print(f"{RED}{len(rep.failures)} of {rep.checked} checks failed{RESET}")
        for name, problems in rep.failures:
            print(f"  {name}: {problems[0]}")
        return 1
    degraded = " (degraded — validators missing)" if missing else ""
    print(f"{GREEN}all {rep.checked} checks passed{RESET}{degraded}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
