#!/usr/bin/env python3
"""Built-in cross-reference validator.

Exists because the failure this project actually shipped — objects written to
the file but omitted from the cross-reference table — is invisible to lenient
parsers. Ghostscript and poppler both reconstruct a damaged xref and carry on,
so a document can render perfectly and still be broken.

`qpdf --check` catches it, but only when qpdf is installed. Depending on an
optional external tool for the one check that matters is how the bug survived
this long, so the same validation lives here with no dependencies.

Three assertions:

  1. every type-1 offset resolves to that object's own header
  2. every object physically present in the file is reachable via the xref
  3. every object-stream container named by a type-2 entry has a type-1 entry

Usable as a module (`check(path) -> list[str]`) or standalone.
"""

import re
import sys
import zlib

OBJ_HEADER = re.compile(rb"(?:^|[\r\n>\s])(\d+)\s+(\d+)\s+obj\b")


def _undo_png_predictor(data, columns, colors=1, bpc=8):
    """Reverse PNG row filters, as used by /DecodeParms on xref streams."""
    bpp = max(1, colors * bpc // 8)
    rowlen = columns * bpp
    out = bytearray()
    prev = bytearray(rowlen)
    pos = 0
    while pos + 1 + rowlen <= len(data) + rowlen:
        if pos >= len(data):
            break
        ft = data[pos]
        row = bytearray(data[pos + 1:pos + 1 + rowlen])
        pos += 1 + rowlen
        if len(row) < rowlen:
            break
        if ft == 0:
            pass
        elif ft == 1:
            for i in range(bpp, rowlen):
                row[i] = (row[i] + row[i - bpp]) & 0xFF
        elif ft == 2:
            for i in range(rowlen):
                row[i] = (row[i] + prev[i]) & 0xFF
        elif ft == 3:
            for i in range(rowlen):
                left = row[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ft == 4:
            for i in range(rowlen):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                row[i] = (row[i] + pr) & 0xFF
        else:
            raise ValueError(f"unsupported PNG filter type {ft}")
        out += row
        prev = row
    return bytes(out)


def _int(d, key, default=None):
    m = re.search(rb"/" + key + rb"\s+(\d+)", d)
    return int(m.group(1)) if m else default


def _parse_xref_stream(data, off):
    """-> (entries, size, prev) with entries as (objnum, type, f2)."""
    seg = data[off:]
    dend = seg.find(b"stream")
    d = seg[:dend]

    size = _int(d, b"Size", 0)
    prev = _int(d, b"Prev")

    wm = re.search(rb"/W\s*\[\s*([\d\s]+?)\]", d)
    if not wm:
        raise ValueError("xref stream without /W")
    W = [int(x) for x in wm.group(1).split()]

    im = re.search(rb"/Index\s*\[\s*([\d\s]+?)\]", d)
    if im:
        nums = [int(x) for x in im.group(1).split()]
        index = list(zip(nums[0::2], nums[1::2]))
    else:
        index = [(0, size)]

    s = dend + len(b"stream")
    if seg[s:s + 2] == b"\r\n":
        s += 2
    elif seg[s:s + 1] in (b"\n", b"\r"):
        s += 1
    e = seg.find(b"endstream", s)
    raw = seg[s:e]

    if b"FlateDecode" in d:
        raw = zlib.decompress(raw)
    pred = _int(d, b"Predictor", 1)
    if pred and pred >= 10:
        raw = _undo_png_predictor(raw, columns=sum(W),
                                  colors=1, bpc=8)

    rowlen = sum(W)
    entries, pos = [], 0
    for start, count in index:
        for k in range(count):
            if pos + rowlen > len(raw):
                return entries, size, prev
            row = raw[pos:pos + rowlen]
            pos += rowlen
            vals, o = [], 0
            for width in W:
                vals.append(int.from_bytes(row[o:o + width], "big") if width else None)
                o += width
            etype = vals[0] if W[0] else 1
            entries.append((start + k, etype, vals[1]))
    return entries, size, prev


def _parse_xref_table(data, off):
    seg = data[off:]
    m = re.match(rb"\s*xref\s*", seg)
    if not m:
        raise ValueError("not an xref table")
    pos = m.end()
    entries = []
    while True:
        hm = re.match(rb"(\d+)\s+(\d+)\s*", seg[pos:])
        if not hm:
            break
        start, count = int(hm.group(1)), int(hm.group(2))
        pos += hm.end()
        for k in range(count):
            em = re.match(rb"(\d{10})\s(\d{5})\s([nf])", seg[pos:pos + 20])
            if not em:
                break
            if em.group(3) == b"n":
                entries.append((start + k, 1, int(em.group(1))))
            else:
                entries.append((start + k, 0, int(em.group(1))))
            pos += 20
        if seg[pos:pos + 7] == b"trailer":
            break

    tail = seg[pos:pos + 2048]
    size = _int(tail, b"Size", 0)
    prev = _int(tail, b"Prev")
    return entries, size, prev


def _collect(data):
    """Follow the /Prev chain, newest entry for an id wins."""
    m = re.search(rb"startxref\s+(\d+)", data[data.rfind(b"startxref"):]) \
        if data.rfind(b"startxref") >= 0 else None
    if not m:
        raise ValueError("no startxref")
    off = int(m.group(1))

    merged, size, seen = {}, 0, set()
    while off is not None and off not in seen and 0 <= off < len(data):
        seen.add(off)
        if re.match(rb"\s*xref", data[off:off + 16]):
            entries, sz, prev = _parse_xref_table(data, off)
        else:
            entries, sz, prev = _parse_xref_stream(data, off)
        size = max(size, sz)
        for num, t, f2 in entries:
            merged.setdefault(num, (t, f2))
        off = prev
    return merged, size


def check(path):
    """Return a list of problems; empty means the xref is sound."""
    data = open(path, "rb").read()
    problems = []

    try:
        table, size = _collect(data)
    except Exception as exc:
        return [f"xref: unparseable ({exc})"]

    # 1. type-1 offsets must resolve to their own object header
    misdirected = []
    for num, (t, f2) in table.items():
        if t != 1 or num == 0:
            continue
        hm = re.match(rb"\s*(\d+)\s+(\d+)\s+obj", data[f2:f2 + 48])
        if not hm:
            misdirected.append(f"obj {num} @{f2}: no object header")
        elif int(hm.group(1)) != num:
            misdirected.append(f"obj {num} @{f2}: points at obj {int(hm.group(1))}")
    if misdirected:
        problems.append(f"xref: {len(misdirected)} misdirected "
                        f"({misdirected[0]})")

    # 2. every object written to the file must be reachable
    present = {int(m.group(1)) for m in OBJ_HEADER.finditer(data)}
    listed = {n for n, (t, _) in table.items() if t in (1, 2)}
    orphaned = sorted(n for n in present if n not in listed)
    if orphaned:
        shown = ", ".join(str(n) for n in orphaned[:6])
        more = f" (+{len(orphaned) - 6} more)" if len(orphaned) > 6 else ""
        problems.append(f"xref: {len(orphaned)} object(s) in file with no "
                        f"xref entry: {shown}{more}")

    # 3. object-stream containers must themselves be locatable
    type1 = {n for n, (t, _) in table.items() if t == 1}
    containers = {f2 for n, (t, f2) in table.items() if t == 2}
    unreachable = sorted(c for c in containers if c not in type1)
    if unreachable:
        problems.append(f"xref: object-stream container(s) not locatable: "
                        f"{', '.join(str(c) for c in unreachable)}")

    return problems


if __name__ == "__main__":
    rc = 0
    for p in sys.argv[1:]:
        found = check(p)
        print(f"{p}")
        if found:
            rc = 1
            for f in found:
                print(f"  FAIL {f}")
        else:
            print("  ok   xref is complete and self-consistent")
    sys.exit(rc)
