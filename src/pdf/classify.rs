//! Raster classification for the `--raster-classify` path.
//!
//! **Design rationale.** A scanned page stored as one RGB raster is usually
//! not a photograph — it is text and rules on paper, and the JPEG/4:4:4
//! representation pays photographic cost for document content. The OCRmyPDF
//! comparison showed the value of *representation selection*: plots, charts
//! and scans want different codecs than photos. This module is the small
//! `RasterClassifier` from the design note — no OCR, no tesseract, just the
//! signal machinery the classifier needs:
//!
//! 1. **adaptive threshold** — global Otsu on the sampled luma separates
//!    ink from paper without a fixed cutoff;
//! 2. **connected-component density** — 4-connected labeling of the
//!    binarized sample; text pages are hundreds of small glyph components,
//!    photos are few large blobs;
//! 3. **color statistics** — sampled unique-color count and neutrality
//!    (max−min < ε), distinguishing flat-color figures from photos;
//! 4. **ink / paper coverage** — the fraction of dark and near-white pixels.
//!
//! The decision is deliberately conservative: an image is `BitonalText`
//! only when it is *mostly* black-and-white with glyph-sized components —
//! a photograph or a colored figure is never masked, because the mask
//! representation drops color and anti-aliasing. The heuristic rules are
//! backed by one *measured* gate: the bitonal reconstruction (each pixel
//! replaced by its class mean) must fit the source luma within a small
//! mean error, so the RGB→bitonal conversion is verified per image rather
//! than assumed safe (see [`reconstruction_error`]). The routing is:
//!
//! ```text
//! BitonalText   -> 1-bit CCITT G4 opaque DeviceGray image
//! FlatColor     -> /Indexed palette (reuses the --palette candidate)
//! Photo/Mixed   -> JPEG (current path; the within-image split of
//!                  MixedDocument into mask + continuous-tone layers is
//!                  future work, documented in QUALITY.md)
//! ```
//!
//! All statistics run on a bounded sample window (≤1024 px on the long
//! edge) so a 28 MB scan classifies in a single pass; the output mask, when
//! produced, is full-resolution.

use std::collections::HashMap;

/// The classification of one decoded raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterClass {
    /// Photographic/continuous-tone content — JPEG.
    Photo,
    /// Few colors, low chromatic entropy — indexed palette.
    FlatColor,
    /// Black-and-white text/rules on paper — 1-bit mask.
    BitonalText,
    /// A mix (text + photos on one page) — JPEG today, split is future work.
    MixedDocument,
}

/// The result of classifying one raster.
#[derive(Debug)]
pub struct Classification {
    pub class: RasterClass,
    /// Full-resolution 1-bit mask (1 = ink, MSB-first, rows packed to
    /// bytes) — `Some` exactly when `class == BitonalText`.
    pub mask: Option<Vec<u8>>,
}

/// Classification sample window (long edge, px).
const SAMPLE_EDGE: u32 = 1024;
/// A pixel is "neutral" (not chromatically informative) when max−min < ε.
const NEUTRAL_EPS: u8 = 24;
/// A pixel counts as near-white paper above this luma.
const NEAR_WHITE: u8 = 200;
/// A pixel counts as near-black ink below this luma.
const NEAR_BLACK: u8 = 55;
/// Minimum component count for text-like content.
const MIN_COMPONENTS: usize = 20;
/// A component larger than this share of the page is a photo/rule, not a
/// glyph.
const MAX_LARGEST_FRACTION: f64 = 0.3;
/// Median glyph area above this is not text at the sample scale.
const MAX_MEDIAN_AREA: u32 = 256;
/// Maximum mean luma error of the bitonal reconstruction (each pixel
/// replaced by its class mean) before an image is *not* bitonal. Measured
/// on the corpus: a clean grainy scan scores ≈ 4.0 (paper grain dominates),
/// a photograph the heuristic rules would otherwise accept scores ≈ 21 —
/// the gate separates them with a wide margin.
const MAX_RECON_MEAN_ERR: f64 = 10.0;

/// Mean absolute difference between the sampled luma and its bitonal
/// reconstruction — every pixel replaced by the mean of its own class
/// (ink ≤ threshold, paper > threshold). This *measures* the variation the
/// RGB→bitonal conversion would discard: a genuine bitonal scan has tiny
/// in-class variance (the Otsu split is clean, only anti-aliased glyph
/// edges deviate), while a photograph or gradient that the heuristic rules
/// happened to pass still fails here. The mask candidates (G4, JBIG2, MRC)
/// are gated on this, turning "the classifier is conservative" into a
/// per-image measurement.
pub fn reconstruction_error(luma: &[u8], mask: &[u8]) -> f64 {
    let n = luma.len() as f64;
    if n == 0.0 {
        return f64::MAX;
    }
    let (mut sum_ink, mut n_ink, mut sum_paper, mut n_paper) = (0u64, 0u64, 0u64, 0u64);
    for (&v, &m) in luma.iter().zip(mask) {
        if m == 1 {
            sum_ink += v as u64;
            n_ink += 1;
        } else {
            sum_paper += v as u64;
            n_paper += 1;
        }
    }
    if n_ink == 0 || n_paper == 0 {
        return f64::MAX;
    }
    let mean_ink = sum_ink as f64 / n_ink as f64;
    let mean_paper = sum_paper as f64 / n_paper as f64;
    let err: f64 = luma
        .iter()
        .zip(mask)
        .map(|(&v, &m)| {
            let mean = if m == 1 { mean_ink } else { mean_paper };
            (v as f64 - mean).abs()
        })
        .sum();
    err / n
}

/// Classify an RGB raster. `rgb` is `width × height × 3` interleaved bytes.
pub fn classify(rgb: &[u8], width: u32, height: u32) -> Classification {
    if rgb.len() < 3 || width == 0 || height == 0 {
        return Classification {
            class: RasterClass::Photo,
            mask: None,
        };
    }

    // Sample the raster down to the bounded window for the statistics.
    let (sw, sh) = bounded_dims(width, height, SAMPLE_EDGE);
    let mut luma = Vec::with_capacity((sw * sh) as usize);
    let mut unique = std::collections::HashSet::with_capacity(4096);
    let mut neutral = 0u64;
    let mut near_white = 0u64;
    let mut near_black = 0u64;
    for sy in 0..sh {
        let y = (sy * height / sh) as usize;
        for sx in 0..sw {
            let x = (sx * width / sw) as usize;
            let p = (y * width as usize + x) * 3;
            let (r, g, b) = (rgb[p], rgb[p + 1], rgb[p + 2]);
            luma.push(luma8(r, g, b));
            unique.insert([r, g, b]);
            let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
            if mx - mn < NEUTRAL_EPS {
                neutral += 1;
            }
            let v = luma8(r, g, b);
            if v > NEAR_WHITE {
                near_white += 1;
            } else if v < NEAR_BLACK {
                near_black += 1;
            }
        }
    }
    let n = (sw * sh) as f64;
    let neutral_frac = neutral as f64 / n;
    let near_white_frac = near_white as f64 / n;
    let near_black_frac = near_black as f64 / n;
    let unique_colors = unique.len();

    // Adaptive (Otsu) threshold on the sample, then component analysis.
    let thr = otsu_threshold(&luma);
    let mask_sample: Vec<u8> = luma.iter().map(|&v| u8::from(v <= thr)).collect();
    let recon_err = reconstruction_error(&luma, &mask_sample);
    let areas = connected_components(&mask_sample, sw as usize, sh as usize);
    let ink_frac = areas.iter().sum::<u32>() as f64 / n;
    let (components, median_area, largest_frac) = component_stats(&areas, n);

    // Conservative rules — see the module doc for why. The reconstruction
    // error is the measured gate: it rejects content whose grayscale
    // variation the mask would actually discard.
    let bitonal = (0.001..=0.7).contains(&ink_frac)
        && near_white_frac + near_black_frac >= 0.6
        && neutral_frac >= 0.85
        && components >= MIN_COMPONENTS
        && (median_area <= MAX_MEDIAN_AREA || largest_frac <= MAX_LARGEST_FRACTION)
        && recon_err <= MAX_RECON_MEAN_ERR;

    let class = if bitonal {
        RasterClass::BitonalText
    } else if unique_colors <= 256 {
        RasterClass::FlatColor
    } else if ink_frac > 0.2 {
        RasterClass::MixedDocument
    } else {
        RasterClass::Photo
    };

    let mask = if bitonal {
        // Full-resolution mask from the full-resolution luma: one pass.
        // Rows are packed to whole bytes (each row's partial byte is
        // pushed at the row end), matching the `encode_g4` contract — a
        // flat pack would only agree with it when the width is a multiple
        // of 8.
        let mut full = Vec::with_capacity((width as usize).div_ceil(8) * height as usize);
        let mut acc = 0u8;
        let mut nbits = 0u8;
        let mut p = 0usize;
        for _ in 0..height {
            for _ in 0..width {
                let (r, g, b) = (rgb[p], rgb[p + 1], rgb[p + 2]);
                p += 3;
                acc = (acc << 1) | u8::from(luma8(r, g, b) <= thr);
                nbits += 1;
                if nbits == 8 {
                    full.push(acc);
                    acc = 0;
                    nbits = 0;
                }
            }
            if nbits > 0 {
                full.push(acc << (8 - nbits));
                acc = 0;
                nbits = 0;
            }
        }
        Some(full)
    } else {
        None
    };

    Classification { class, mask }
}

/// Classify a grayscale raster (1 byte/pixel). Saturation is trivially
/// neutral and the unique-color count is the number of distinct gray
/// values, so the shared decision machinery applies unchanged — gray scans
/// are the classic bitonal-text case.
pub fn classify_gray(gray: &[u8], width: u32, height: u32) -> Classification {
    if gray.is_empty() || width == 0 || height == 0 {
        return Classification {
            class: RasterClass::Photo,
            mask: None,
        };
    }
    let (sw, sh) = bounded_dims(width, height, SAMPLE_EDGE);
    let mut luma = Vec::with_capacity((sw * sh) as usize);
    let mut unique = std::collections::HashSet::new();
    let mut near_white = 0u64;
    let mut near_black = 0u64;
    for sy in 0..sh {
        let y = (sy * height / sh) as usize;
        for sx in 0..sw {
            let x = (sx * width / sw) as usize;
            let v = gray[y * width as usize + x];
            luma.push(v);
            unique.insert(v);
            if v > NEAR_WHITE {
                near_white += 1;
            } else if v < NEAR_BLACK {
                near_black += 1;
            }
        }
    }
    let n = (sw * sh) as f64;
    let thr = otsu_threshold(&luma);
    let mask_sample: Vec<u8> = luma.iter().map(|&v| u8::from(v <= thr)).collect();
    let recon_err = reconstruction_error(&luma, &mask_sample);
    let areas = connected_components(&mask_sample, sw as usize, sh as usize);
    let ink_frac = areas.iter().sum::<u32>() as f64 / n;
    let (components, median_area, largest_frac) = component_stats(&areas, n);

    let bitonal = (0.001..=0.7).contains(&ink_frac)
        && near_white as f64 / n + near_black as f64 / n >= 0.6
        && components >= MIN_COMPONENTS
        && (median_area <= MAX_MEDIAN_AREA || largest_frac <= MAX_LARGEST_FRACTION)
        && recon_err <= MAX_RECON_MEAN_ERR;

    let class = if bitonal {
        RasterClass::BitonalText
    } else if unique.len() <= 256 {
        RasterClass::FlatColor
    } else if ink_frac > 0.2 {
        RasterClass::MixedDocument
    } else {
        RasterClass::Photo
    };

    let mask = if bitonal {
        // Row-packed like `classify` (see there): each row's partial byte
        // is pushed at the row end, matching the G4/JBIG2/MRC consumers.
        let row_bytes = (width as usize).div_ceil(8);
        let mut full = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let base = row * width as usize;
            let mut acc = 0u8;
            let mut nbits = 0u8;
            for x in 0..width as usize {
                acc = (acc << 1) | u8::from(gray[base + x] <= thr);
                nbits += 1;
                if nbits == 8 {
                    full.push(acc);
                    acc = 0;
                    nbits = 0;
                }
            }
            if nbits > 0 {
                full.push(acc << (8 - nbits));
            }
        }
        Some(full)
    } else {
        None
    };

    Classification { class, mask }
}

/// Largest `(w, h)` window with the same aspect that fits `edge` on its
/// long side (≥1×1).
fn bounded_dims(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= edge {
        return (width.max(1), height.max(1));
    }
    let (sw, sh) = (
        ((width as u64 * edge as u64) / long as u64).max(1) as u32,
        ((height as u64 * edge as u64) / long as u64).max(1) as u32,
    );
    (sw, sh)
}

pub(crate) fn luma8(r: u8, g: u8, b: u8) -> u8 {
    // Rec. 601 luma, same weights the render witnesses use.
    ((299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000) as u8
}

/// Global Otsu threshold: the bin that maximizes between-class variance,
/// separating ink (≤ threshold) from paper.
fn otsu_threshold(luma: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in luma {
        hist[v as usize] += 1;
    }
    let total = luma.len() as u64;
    if total == 0 {
        return 128;
    }
    let sum_all: u64 = hist.iter().enumerate().map(|(i, &c)| i as u64 * c).sum();
    let (mut w_b, mut sum_b) = (0u64, 0u64);
    let (mut best_t, mut best_var) = (128u8, -1f64);
    for t in 0..256u32 {
        w_b += hist[t as usize];
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as u64 * hist[t as usize];
        let m_b = sum_b as f64 / w_b as f64;
        let m_f = (sum_all - sum_b) as f64 / w_f as f64;
        let var = w_b as f64 * w_f as f64 * (m_b - m_f).powi(2);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

/// 4-connected components of a 1bpp mask (1 = foreground), via two-pass
/// labeling with union-find. Returns the area of every component.
fn connected_components(mask: &[u8], w: usize, h: usize) -> Vec<u32> {
    if mask.len() < w * h || w == 0 || h == 0 {
        return Vec::new();
    }
    let at = |x: usize, y: usize| y * w + x;
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0]; // root table, 1-based label ids
    let mut next = 1u32;

    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }

    for y in 0..h {
        for x in 0..w {
            if mask[at(x, y)] == 0 {
                continue;
            }
            let l = if x > 0 { labels[at(x - 1, y)] } else { 0 };
            let u = if y > 0 { labels[at(x, y - 1)] } else { 0 };
            match (l, u) {
                (0, 0) => {
                    parent.push(next);
                    labels[at(x, y)] = next;
                    next += 1;
                }
                (a, b) if a != 0 && b != 0 && a != b => {
                    let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                    parent[ra as usize] = rb;
                    labels[at(x, y)] = a;
                }
                (a, b) => labels[at(x, y)] = if a != 0 { a } else { b },
            }
        }
    }

    let mut areas: HashMap<u32, u32> = HashMap::new();
    for &lab in labels.iter() {
        if lab == 0 {
            continue;
        }
        let r = find(&mut parent, lab);
        *areas.entry(r).or_insert(0) += 1;
    }
    areas.into_values().collect()
}

/// (component count, median area, largest-area fraction of the window).
fn component_stats(areas: &[u32], n: f64) -> (usize, u32, f64) {
    if areas.is_empty() {
        return (0, 0, 0.0);
    }
    let mut sorted: Vec<u32> = areas.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    let median = sorted[count / 2];
    let largest = *sorted.last().unwrap();
    (count, median, largest as f64 / n)
}
