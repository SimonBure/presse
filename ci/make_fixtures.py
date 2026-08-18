#!/usr/bin/env python3
"""Generate the structural fixture corpus used by the validity gate.

Deterministic, dependency-free (stdlib only), and independent of lopdf — the
inputs must not be produced by the library under test, or a writer bug would
be invisible to the gate that exists to catch it.

Each fixture targets one structure presse handles differently:

  text-only      no image streams at all
  gray-flate     /DeviceGray 8bpc, FlateDecode  (single-component JPEG path)
  rgb-flate      /DeviceRGB 8bpc, FlateDecode   (three-component path)
  rgba-flate     /DeviceRGB carrying 4 bytes/px (the misread-as-RGB case)
  cmyk-flate     /DeviceCMYK                    (must be skipped, not mangled)
  predictor      FlateDecode + /DecodeParms PNG predictor  (see issue #6)
  gapped         gapped object numbering        (xref section handling)
  multipage      several pages, mixed content

Usage: make_fixtures.py <out-dir>
"""

import sys
import zlib
from pathlib import Path

MEDIA = b"[0 0 300 300]"


class Pdf:
    """Minimal PDF writer producing a classic xref table."""

    def __init__(self):
        self.objects = {}  # num -> bytes (object body, without "N 0 obj")

    def add(self, num, body):
        self.objects[num] = body
        return num

    def stream(self, num, dict_entries, data):
        d = b"<< " + dict_entries + b" /Length " + str(len(data)).encode() + b" >>"
        self.objects[num] = d + b"\nstream\n" + data + b"\nendstream"
        return num

    def build(self, root_num):
        out = bytearray(b"%PDF-1.5\n%\xe2\xe3\xcf\xd3\n")
        offsets = {}
        for num in sorted(self.objects):
            offsets[num] = len(out)
            out += str(num).encode() + b" 0 obj\n" + self.objects[num] + b"\nendobj\n"

        max_id = max(self.objects)
        size = max_id + 1
        xref_at = len(out)

        # One subsection per contiguous run of present ids, so gapped
        # numbering produces a genuinely multi-section table.
        runs = []
        present = sorted(self.objects)
        run = [present[0]]
        for n in present[1:]:
            if n == run[-1] + 1:
                run.append(n)
            else:
                runs.append(run)
                run = [n]
        runs.append(run)

        out += b"xref\n"
        out += b"0 1\n" + b"0000000000 65535 f \n"
        for run in runs:
            out += f"{run[0]} {len(run)}\n".encode()
            for n in run:
                out += f"{offsets[n]:010d} 00000 n \n".encode()

        out += b"trailer\n<< /Size " + str(size).encode()
        out += b" /Root " + str(root_num).encode() + b" 0 R >>\n"
        out += b"startxref\n" + str(xref_at).encode() + b"\n%%EOF\n"
        return bytes(out)


def gray_pixels(w, h):
    """Smooth gradient plus structure — compresses, but not to nothing."""
    buf = bytearray()
    for y in range(h):
        for x in range(w):
            v = (x * 255 // max(1, w - 1) + y * 97) % 256
            buf.append(v)
    return bytes(buf)


def rgb_pixels(w, h, channels=3):
    buf = bytearray()
    for y in range(h):
        for x in range(w):
            r = x * 255 // max(1, w - 1)
            g = y * 255 // max(1, h - 1)
            b = (x * 7 + y * 13) % 256
            px = [r, g, b, 255][:channels]
            buf.extend(px)
    return bytes(buf)


def cmyk_pixels(w, h):
    buf = bytearray()
    for y in range(h):
        for x in range(w):
            buf.extend([(x * 3) % 256, (y * 5) % 256, (x + y) % 256, 8])
    return bytes(buf)


def png_up_predict(raw, row_len):
    """Encode rows with the PNG 'Up' filter (predictor 12)."""
    out = bytearray()
    prev = bytes(row_len)
    for i in range(0, len(raw), row_len):
        row = raw[i:i + row_len]
        out.append(2)
        out.extend(((b - p) & 0xFF) for b, p in zip(row, prev))
        prev = row
    return bytes(out)


def image_doc(colorspace, raw, w, h, *, decode_parms=None, start_id=1):
    """A one-page document displaying a single image XObject."""
    p = Pdf()
    n = start_id
    cat, pages, page, content, img = n, n + 1, n + 2, n + 3, n + 4

    p.add(cat, b"<< /Type /Catalog /Pages " + str(pages).encode() + b" 0 R >>")
    p.add(pages, b"<< /Type /Pages /Kids [" + str(page).encode()
          + b" 0 R] /Count 1 >>")
    p.add(page,
          b"<< /Type /Page /Parent " + str(pages).encode() + b" 0 R"
          b" /MediaBox " + MEDIA +
          b" /Resources << /XObject << /Im0 " + str(img).encode() + b" 0 R >> >>"
          b" /Contents " + str(content).encode() + b" 0 R >>")

    ops = b"q 300 0 0 300 0 0 cm /Im0 Do Q"
    p.stream(content, b"", ops)

    entries = (b"/Type /XObject /Subtype /Image"
               b" /Width " + str(w).encode() +
               b" /Height " + str(h).encode() +
               b" /ColorSpace /" + colorspace +
               b" /BitsPerComponent 8 /Filter /FlateDecode")
    if decode_parms:
        entries += b" /DecodeParms " + decode_parms
    p.stream(img, entries, zlib.compress(raw, 6))
    return p, cat


def text_doc(pages_count):
    p = Pdf()
    cat, pages, font = 1, 2, 3
    kids, n = [], 4
    for i in range(pages_count):
        page, content = n, n + 1
        n += 2
        kids.append(page)
        p.add(page,
              b"<< /Type /Page /Parent " + str(pages).encode() + b" 0 R"
              b" /MediaBox " + MEDIA +
              b" /Resources << /Font << /F1 " + str(font).encode() + b" 0 R >> >>"
              b" /Contents " + str(content).encode() + b" 0 R >>")
        body = (b"BT /F1 18 Tf 30 200 Td (Fixture page "
                + str(i + 1).encode() + b") Tj ET")
        p.stream(content, b"", body)

    p.add(cat, b"<< /Type /Catalog /Pages " + str(pages).encode() + b" 0 R >>")
    p.add(pages, b"<< /Type /Pages /Kids ["
          + b" ".join(str(k).encode() + b" 0 R" for k in kids)
          + b"] /Count " + str(pages_count).encode() + b" >>")
    p.add(font, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    return p, cat


def gapped_doc():
    """Valid document whose object ids deliberately skip values."""
    p = Pdf()
    cat, pages, font = 1, 5, 9
    kids = []
    n = 20
    for i in range(3):
        page, content = n, n + 3   # leave holes between ids
        n += 10
        kids.append(page)
        p.add(page,
              b"<< /Type /Page /Parent " + str(pages).encode() + b" 0 R"
              b" /MediaBox " + MEDIA +
              b" /Resources << /Font << /F1 " + str(font).encode() + b" 0 R >> >>"
              b" /Contents " + str(content).encode() + b" 0 R >>")
        p.stream(content, b"",
                 b"BT /F1 16 Tf 30 200 Td (Gapped " + str(i + 1).encode()
                 + b") Tj ET")

    p.add(cat, b"<< /Type /Catalog /Pages " + str(pages).encode() + b" 0 R >>")
    p.add(pages, b"<< /Type /Pages /Kids ["
          + b" ".join(str(k).encode() + b" 0 R" for k in kids)
          + b"] /Count 3 >>")
    p.add(font, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    return p, cat


def png(w, h, raw_rgb):
    """Minimal RGB8 PNG — input for the `convert` subcommand."""
    def chunk(tag, data):
        c = tag + data
        return (len(data).to_bytes(4, "big") + c
                + zlib.crc32(c).to_bytes(4, "big"))

    scan = bytearray()
    for y in range(h):
        scan.append(0)  # filter: None
        scan.extend(raw_rgb[y * w * 3:(y + 1) * w * 3])

    ihdr = (w.to_bytes(4, "big") + h.to_bytes(4, "big")
            + bytes([8, 2, 0, 0, 0]))  # 8bpc, truecolour
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(bytes(scan), 6))
            + chunk(b"IEND", b""))


def main(outdir):
    out = Path(outdir)
    out.mkdir(parents=True, exist_ok=True)
    W = H = 96

    fixtures = {}

    p, root = text_doc(1)
    fixtures["text-only.pdf"] = p.build(root)

    p, root = text_doc(4)
    fixtures["multipage.pdf"] = p.build(root)

    # Large enough that saving packs objects into object streams (>200
    # objects). Below that threshold the xref-truncation bug cannot occur,
    # so a corpus of only small documents would let it through.
    p, root = text_doc(120)
    fixtures["many-objects.pdf"] = p.build(root)

    p, root = image_doc(b"DeviceGray", gray_pixels(W, H), W, H)
    fixtures["gray-flate.pdf"] = p.build(root)

    p, root = image_doc(b"DeviceRGB", rgb_pixels(W, H), W, H)
    fixtures["rgb-flate.pdf"] = p.build(root)

    # Non-canonical: 4 bytes per pixel on a /DeviceRGB stream.
    p, root = image_doc(b"DeviceRGB", rgb_pixels(W, H, 4), W, H)
    fixtures["rgba-flate.pdf"] = p.build(root)

    p, root = image_doc(b"DeviceCMYK", cmyk_pixels(W, H), W, H)
    fixtures["cmyk-flate.pdf"] = p.build(root)

    raw = rgb_pixels(W, H)
    parms = (b"<< /Predictor 12 /Colors 3 /Columns " + str(W).encode()
             + b" /BitsPerComponent 8 >>")
    p, root = image_doc(b"DeviceRGB", png_up_predict(raw, W * 3), W, H,
                        decode_parms=parms)
    fixtures["predictor.pdf"] = p.build(root)

    p, root = gapped_doc()
    fixtures["gapped.pdf"] = p.build(root)

    fixtures["source.png"] = png(W, H, rgb_pixels(W, H))

    for name, data in sorted(fixtures.items()):
        (out / name).write_bytes(data)
        print(f"  {name:<20} {len(data):>7} B")

    print(f"{len(fixtures)} fixtures -> {out}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures")
