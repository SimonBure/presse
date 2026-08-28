//! CCITT Group 4 (T.6 / MMR) fax encoding for 1-bit image masks.
//!
//! **Design rationale.** A scanned page stored as one RGB raster is usually
//! not a photograph — it is text and rules on paper. The `--raster-classify`
//! path detects genuinely bitonal content and stores it as a 1-bit
//! `DeviceGray` CCITT G4 image instead of a photographic JPEG. (Not an
//! `/ImageMask` stencil: a stencil's 0 bits are transparent and its ink
//! inherits the current color, so it cannot substitute for an opaque
//! black-on-white raster — see the opaque-image regression in
//! `tests/regression.rs`.) The 1-bit image needs a codec that exploits
//! bilevel structure; CCITT Group 4 (T.6) is the standard document codec —
//! every PDF viewer and ghostscript decode it natively, it needs no
//! dependency (unlike JBIG2, which would want a symbol-dictionary codec),
//! and for text masks it compresses several times smaller than Flate of
//! the packed bits.
//!
//! The implementation follows the ITU-T T.4/T.6 run-length and 2D mode
//! tables (transcribed from the public spec data; the `fax` crate's
//! published tables were used to verify each bit pattern) and the standard
//! two-dimensional coding: each line is coded against the previous line as
//! reference, using pass / vertical / horizontal modes with changing-element
//! tracking. Output is a plain G4 stream terminated by EOFB (two EOLs), the
//! exact form the `/CCITTFaxDecode /K -1` decoder expects.
//!
//! Losslessness is the court — of the G4 encoding: the encoder is
//! deterministic and the 1-bit payload is decoded bit-exactly by viewers
//! (the regression twin-render test pins this by rasterizing the same mask
//! stored as both raw 1-bit Flate and G4 and asserting pixel-identical
//! output). The earlier RGB→bitonal conversion performed by the classifier
//! is itself lossy, which is why the classifier only fires on near-perfect
//! black-and-white content.

/// A bit pattern: `bits` holds `len` bits, MSB-first.
type Code = (u32, u8);

// Run-length code tables, indexed exactly as the T.4 spec: terminating
// codes at indices 0–63 (run lengths 0–63), make-up codes at indices
// 64–90 (64…1728 in 64-pixel steps) and 91–103 (1792…2560). Runs longer
// than 2560 repeat the 2560 make-up code. White and black have separate
// terminating/make-up codes; the 1792+ extended codes are shared.
#[rustfmt::skip]
const WHITE_ENTRIES: [(u32, u8); 104] = [
    (0b00110101, 8), // 0
    (0b00000111, 6), // 1
    (0b00000111, 4), // 2
    (0b00001000, 4), // 3
    (0b00001011, 4), // 4
    (0b00001100, 4), // 5
    (0b00001110, 4), // 6
    (0b00001111, 4), // 7
    (0b00010011, 5), // 8
    (0b00010100, 5), // 9
    (0b00000111, 5), // 10
    (0b00001000, 5), // 11
    (0b00001000, 6), // 12
    (0b00000011, 6), // 13
    (0b00110100, 6), // 14
    (0b00110101, 6), // 15
    (0b00101010, 6), // 16
    (0b00101011, 6), // 17
    (0b00100111, 7), // 18
    (0b00001100, 7), // 19
    (0b00001000, 7), // 20
    (0b00010111, 7), // 21
    (0b00000011, 7), // 22
    (0b00000100, 7), // 23
    (0b00101000, 7), // 24
    (0b00101011, 7), // 25
    (0b00010011, 7), // 26
    (0b00100100, 7), // 27
    (0b00011000, 7), // 28
    (0b00000010, 8), // 29
    (0b00000011, 8), // 30
    (0b00011010, 8), // 31
    (0b00011011, 8), // 32
    (0b00010010, 8), // 33
    (0b00010011, 8), // 34
    (0b00010100, 8), // 35
    (0b00010101, 8), // 36
    (0b00010110, 8), // 37
    (0b00010111, 8), // 38
    (0b00101000, 8), // 39
    (0b00101001, 8), // 40
    (0b00101010, 8), // 41
    (0b00101011, 8), // 42
    (0b00101100, 8), // 43
    (0b00101101, 8), // 44
    (0b00000100, 8), // 45
    (0b00000101, 8), // 46
    (0b00001010, 8), // 47
    (0b00001011, 8), // 48
    (0b01010010, 8), // 49
    (0b01010011, 8), // 50
    (0b01010100, 8), // 51
    (0b01010101, 8), // 52
    (0b00100100, 8), // 53
    (0b00100101, 8), // 54
    (0b01011000, 8), // 55
    (0b01011001, 8), // 56
    (0b01011010, 8), // 57
    (0b01011011, 8), // 58
    (0b01001010, 8), // 59
    (0b01001011, 8), // 60
    (0b00110010, 8), // 61
    (0b00110011, 8), // 62
    (0b00110100, 8), // 63
    (0b00011011, 5), // makeup 64
    (0b00010010, 5), // makeup 128
    (0b00010111, 6), // makeup 192
    (0b00110111, 7), // makeup 256
    (0b00110110, 8), // makeup 320
    (0b00110111, 8), // makeup 384
    (0b01100100, 8), // makeup 448
    (0b01100101, 8), // makeup 512
    (0b01101000, 8), // makeup 576
    (0b01100111, 8), // makeup 640
    (0b11001100, 9), // makeup 704
    (0b11001101, 9), // makeup 768
    (0b11010010, 9), // makeup 832
    (0b11010011, 9), // makeup 896
    (0b11010100, 9), // makeup 960
    (0b11010101, 9), // makeup 1024
    (0b11010110, 9), // makeup 1088
    (0b11010111, 9), // makeup 1152
    (0b11011000, 9), // makeup 1216
    (0b11011001, 9), // makeup 1280
    (0b11011010, 9), // makeup 1344
    (0b11011011, 9), // makeup 1408
    (0b10011000, 9), // makeup 1472
    (0b10011001, 9), // makeup 1536
    (0b10011010, 9), // makeup 1600
    (0b00011000, 6), // makeup 1664
    (0b10011011, 9), // makeup 1728
    (0b00001000, 11), // makeup 1792
    (0b00001100, 11), // makeup 1856
    (0b00001101, 11), // makeup 1920
    (0b00010010, 12), // makeup 1984
    (0b00010011, 12), // makeup 2048
    (0b00010100, 12), // makeup 2112
    (0b00010101, 12), // makeup 2176
    (0b00010110, 12), // makeup 2240
    (0b00010111, 12), // makeup 2304
    (0b00011100, 12), // makeup 2368
    (0b00011101, 12), // makeup 2432
    (0b00011110, 12), // makeup 2496
    (0b00011111, 12), // makeup 2560
];
#[rustfmt::skip]
const BLACK_ENTRIES: [(u32, u8); 104] = [
    (0b00110111, 10), // 0
    (0b00000010, 3), // 1
    (0b00000011, 2), // 2
    (0b00000010, 2), // 3
    (0b00000011, 3), // 4
    (0b00000011, 4), // 5
    (0b00000010, 4), // 6
    (0b00000011, 5), // 7
    (0b00000101, 6), // 8
    (0b00000100, 6), // 9
    (0b00000100, 7), // 10
    (0b00000101, 7), // 11
    (0b00000111, 7), // 12
    (0b00000100, 8), // 13
    (0b00000111, 8), // 14
    (0b00011000, 9), // 15
    (0b00010111, 10), // 16
    (0b00011000, 10), // 17
    (0b00001000, 10), // 18
    (0b01100111, 11), // 19
    (0b01101000, 11), // 20
    (0b01101100, 11), // 21
    (0b00110111, 11), // 22
    (0b00101000, 11), // 23
    (0b00010111, 11), // 24
    (0b00011000, 11), // 25
    (0b11001010, 12), // 26
    (0b11001011, 12), // 27
    (0b11001100, 12), // 28
    (0b11001101, 12), // 29
    (0b01101000, 12), // 30
    (0b01101001, 12), // 31
    (0b01101010, 12), // 32
    (0b01101011, 12), // 33
    (0b11010010, 12), // 34
    (0b11010011, 12), // 35
    (0b11010100, 12), // 36
    (0b11010101, 12), // 37
    (0b11010110, 12), // 38
    (0b11010111, 12), // 39
    (0b01101100, 12), // 40
    (0b01101101, 12), // 41
    (0b11011010, 12), // 42
    (0b11011011, 12), // 43
    (0b01010100, 12), // 44
    (0b01010101, 12), // 45
    (0b01010110, 12), // 46
    (0b01010111, 12), // 47
    (0b01100100, 12), // 48
    (0b01100101, 12), // 49
    (0b01010010, 12), // 50
    (0b01010011, 12), // 51
    (0b00100100, 12), // 52
    (0b00110111, 12), // 53
    (0b00111000, 12), // 54
    (0b00100111, 12), // 55
    (0b00101000, 12), // 56
    (0b01011000, 12), // 57
    (0b01011001, 12), // 58
    (0b00101011, 12), // 59
    (0b00101100, 12), // 60
    (0b01011010, 12), // 61
    (0b01100110, 12), // 62
    (0b01100111, 12), // 63
    (0b00001111, 10), // makeup 64
    (0b11001000, 12), // makeup 128
    (0b11001001, 12), // makeup 192
    (0b01011011, 12), // makeup 256
    (0b00110011, 12), // makeup 320
    (0b00110100, 12), // makeup 384
    (0b00110101, 12), // makeup 448
    (0b01101100, 13), // makeup 512
    (0b01101101, 13), // makeup 576
    (0b01001010, 13), // makeup 640
    (0b01001011, 13), // makeup 704
    (0b01001100, 13), // makeup 768
    (0b01001101, 13), // makeup 832
    (0b01110010, 13), // makeup 896
    (0b01110011, 13), // makeup 960
    (0b01110100, 13), // makeup 1024
    (0b01110101, 13), // makeup 1088
    (0b01110110, 13), // makeup 1152
    (0b01110111, 13), // makeup 1216
    (0b01010010, 13), // makeup 1280
    (0b01010011, 13), // makeup 1344
    (0b01010100, 13), // makeup 1408
    (0b01010101, 13), // makeup 1472
    (0b01011010, 13), // makeup 1536
    (0b01011011, 13), // makeup 1600
    (0b01100100, 13), // makeup 1664
    (0b01100101, 13), // makeup 1728
    (0b00001000, 11), // makeup 1792
    (0b00001100, 11), // makeup 1856
    (0b00001101, 11), // makeup 1920
    (0b00010010, 12), // makeup 1984
    (0b00010011, 12), // makeup 2048
    (0b00010100, 12), // makeup 2112
    (0b00010101, 12), // makeup 2176
    (0b00010110, 12), // makeup 2240
    (0b00010111, 12), // makeup 2304
    (0b00011100, 12), // makeup 2368
    (0b00011101, 12), // makeup 2432
    (0b00011110, 12), // makeup 2496
    (0b00011111, 12), // makeup 2560
];

const EOL: Code = (0b000000000001, 12);
/// Pass mode: `0001` + P (P is encoded as a run length of the reference
/// segment b1→b2, using the white table — both colors share the code here,
/// and T.4 specifies the pass count against the white table).
const MODE_PASS: Code = (0b0001, 4);
const MODE_HORIZONTAL: Code = (0b001, 3);
/// Vertical mode codes indexed by `delta + 3` (delta −3…+3).
const MODE_VERTICAL: [Code; 7] = [
    (0b0000010, 7), // −3
    (0b000010, 6),  // −2
    (0b010, 3),     // −1
    (0b1, 1),       // 0
    (0b011, 3),     // +1
    (0b000011, 6),  // +2
    (0b0000011, 7), // +3
];

/// Write bits MSB-first into a byte vector.
struct BitWriter {
    out: Vec<u8>,
    partial: u32,
    len: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            partial: 0,
            len: 0,
        }
    }
    fn put(&mut self, (bits, len): Code) {
        self.partial |= (bits & ((1u32 << len) - 1)) << (32 - self.len - len);
        self.len += len;
        while self.len >= 8 {
            self.out.push((self.partial >> 24) as u8);
            self.partial <<= 8;
            self.len -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.len > 0 {
            self.out.push((self.partial >> 24) as u8);
        }
        self.out
    }
}

/// Encode one run of `n` pixels of `color` (0 = white, 1 = black): one or
/// more make-up codes (64…2560 in 64-pixel steps) followed by a terminating
/// code (0…63). Runs above 2560 repeat the 2560 make-up. The make-up count
/// must be *subtracted* before emitting the terminating code — a common
/// first-cut bug that produces streams decoders read as garbage.
fn encode_run(out: &mut BitWriter, color: u8, mut n: u32) {
    let table = if color == 0 {
        &WHITE_ENTRIES
    } else {
        &BLACK_ENTRIES
    };
    while n >= 2560 {
        out.put(table[63 + 2560 / 64]); // the 2560 make-up code
        n -= 2560;
    }
    if n >= 64 {
        out.put(table[63 + (n / 64) as usize]);
        n -= n & !63;
    }
    debug_assert!(n < 64);
    out.put(table[n as usize]);
}

/// The color of the run containing pixel `x` (false = white, true = black).
/// Runs alternate starting white, so the color flips at every edge; an
/// ink-starting line has its first edge at 0.
fn color_at(edges: &[u32], x: u32) -> bool {
    edges.partition_point(|&e| e <= x) % 2 == 1
}

/// libtiff's `finddiff`: the position where the run of `color` starting at
/// `x` ends — `x` itself when the pixel at `x` has the opposite color,
/// otherwise the first edge after `x` (or `width` when the run reaches the
/// end of the line).
fn next_change(edges: &[u32], x: u32, color: bool, width: u32) -> u32 {
    if color_at(edges, x) != color {
        return x;
    }
    let idx = edges.partition_point(|&e| e <= x);
    edges.get(idx).copied().unwrap_or(width)
}

/// libtiff's `finddiff2`: the `x < width` guard so the line end is never
/// read past.
fn next_change2(edges: &[u32], x: u32, color: bool, width: u32) -> u32 {
    if x >= width {
        return width;
    }
    next_change(edges, x, color, width)
}

/// Encode one line against the previous line's edges, per T.6 2D coding.
/// This mirrors libtiff's `Fax3Encode2DRow` exactly (the reference
/// implementation both poppler and libtiff decode against): the key subtlety
/// is that the initial `b1` is the reference line's *first* changing element
/// — for an ink-starting reference line that element sits at position 0 and
/// *is* a valid `b1` when `a0 = 0`, so the first pass can cover the initial
/// black run. Skipping it (requiring `b1 > a0`) makes the encoder's pass
/// path longer than the decoder's, which then runs out of modes before the
/// end of the line and consumes the next line's bits.
fn encode_line(out: &mut BitWriter, reference: &[u32], current: &[u32], width: u32) {
    let mut a0 = 0u32;
    // a1 = first changing element of the coding line (0 when it starts with
    // ink); b1 = first changing element of the reference line (0 when the
    // reference starts with ink).
    let mut a1 = if color_at(current, 0) {
        0
    } else {
        next_change(current, 0, false, width)
    };
    let mut b1 = if color_at(reference, 0) {
        0
    } else {
        next_change(reference, 0, false, width)
    };

    loop {
        // b2 = the changing element after b1 on the reference line.
        let b2 = next_change2(reference, b1, color_at(reference, b1), width);
        if b2 >= a1 {
            // Vertical / horizontal modes: the vertical delta is a1 − b1
            // (libtiff codes b1 − a1; the mirror-image table index below is
            // equivalent).
            let delta = a1 as i64 - b1 as i64;
            if (-3..=3).contains(&delta) {
                out.put(MODE_VERTICAL[(delta + 3) as usize]);
                a0 = a1;
            } else {
                // Horizontal mode: two run lengths, a0 moves to a2.
                let a2 = next_change(current, a1, color_at(current, a1), width);
                out.put(MODE_HORIZONTAL);
                // The first run is white when a0 is the imaginary position
                // before an ink-starting line, or when the pixel at a0 is
                // white (libtiff's `PIXEL(bp, a0)` test).
                if a0 + a1 == 0 || !color_at(current, a0) {
                    encode_run(out, 0, a1 - a0);
                    encode_run(out, 1, a2 - a1);
                } else {
                    encode_run(out, 1, a1 - a0);
                    encode_run(out, 0, a2 - a1);
                }
                a0 = a2;
            }
        } else {
            // Pass mode: b2 lies left of a1; a0 moves to b2, the color
            // unchanged, and the reference cursor moves past b2.
            out.put(MODE_PASS);
            a0 = b2;
        }
        if a0 >= width {
            break;
        }
        // Re-derive a1 and b1 from the new a0 (libtiff's `finddiff` pair:
        // b1 is the first reference changing element right of a0 whose
        // following run has the color opposite to the coding line's at a0).
        a1 = next_change(current, a0, color_at(current, a0), width);
        b1 = next_change(reference, a0, !color_at(current, a0), width);
        b1 = next_change(reference, b1, color_at(current, a0), width);
    }
}

/// Encode a 1-bit raster (1 = black, MSB-first, rows packed to bytes) as
/// CCITT Group 4 fax data: each line 2D-coded against the previous line,
/// terminated by EOFB (two EOLs).
pub fn encode_g4(rows: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let row_bytes = w.div_ceil(8);
    let mut out = BitWriter::new();
    let mut reference: Vec<u32> = Vec::new();
    let mut current: Vec<u32> = Vec::new();

    for y in 0..height as usize {
        let row = &rows[y * row_bytes..(y + 1) * row_bytes];
        current.clear();
        let mut prev = 0u8; // virtual white before the line
        for x in 0..w {
            let bit = (row[x >> 3] >> (7 - (x & 7))) & 1;
            if bit != prev {
                current.push(x as u32);
                prev = bit;
            }
        }
        encode_line(&mut out, &reference, &current, width);
        std::mem::swap(&mut reference, &mut current);
    }

    out.put(EOL);
    out.put(EOL);
    out.finish()
}
