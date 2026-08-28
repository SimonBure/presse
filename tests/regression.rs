//! Regression tests for the image re-encoding pipeline (`compress_images`).
//!
//! Three layers of verification:
//! 1. **Structure & syntax** — every output PDF must reload cleanly with
//!    `lopdf`, keep consistent `/Length` entries on every stream, and pass
//!    ghostscript's parser when `gs` is available.
//! 2. **Visual diff** — pages are rasterized with `pdftoppm` before/after
//!    compression and compared with SSIM, so any color-space corruption or
//!    stream damage shows up as a large drop. These tests skip gracefully
//!    when poppler-utils is not installed.
//! 3. **Edge cases** — zero-image PDFs, CMYK streams, inline images, mixed
//!    `FlateDecode` streams, non-canonical 4-byte/px `DeviceRGB` streams, and
//!    serial/parallel determinism.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::GrayImage;
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use presse::pdf::images::{QualityMode, compress_images, compress_images_with};
use presse::pdf::writer::{compress_and_save_pdf, recompress_flate, renumber_objects, save_pdf};
use presse::transcode::CpuTranscoder;

const QUALITY: u8 = 50;

// ---------------------------------------------------------------------------
// Test environment helpers
// ---------------------------------------------------------------------------

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("presse-regression-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// True when the `pdftoppm` binary (poppler-utils) can be executed.
fn pdftoppm_available() -> bool {
    Command::new("pdftoppm").arg("-v").output().is_ok()
}

/// True when ghostscript is installed.
fn gs_available() -> bool {
    Command::new("gs").arg("--version").output().is_ok()
}

/// True when qpdf is installed.
fn qpdf_available() -> bool {
    Command::new("qpdf").arg("--version").output().is_ok()
}

/// CI convention shared with `ci/validate_corpus.py`: when
/// `PRESSE_REQUIRE_PDF_TOOLS` is set, a missing validator is a hard test
/// failure instead of a silent skip, so a broken runner image cannot quietly
/// degrade the suite. Locally (unset) the gates degrade with a loud warning.
fn require_pdf_tools() -> bool {
    std::env::var_os("PRESSE_REQUIRE_PDF_TOOLS").is_some_and(|v| !v.is_empty())
}

/// Assert that an external validator is present, or fail/skip per
/// [`require_pdf_tools`].
fn ensure_tool(name: &str, available: bool) -> bool {
    if available {
        return true;
    }
    let msg = format!("{name} not found — skipping the {name} validity gate");
    if require_pdf_tools() {
        panic!("{msg}; set PRESSE_REQUIRE_PDF_TOOLS=0 to skip locally");
    }
    eprintln!("note: {msg}");
    false
}

// ---------------------------------------------------------------------------
// Synthetic pixel generators
// ---------------------------------------------------------------------------

fn xorshift(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

/// Photo-like RGB: smooth gradient + structure + light grain.
fn photoish_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut next = xorshift(42);
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = (
                (x as f32 / w as f32 * 255.0) as u8,
                (y as f32 / h as f32 * 255.0) as u8,
                (128.0 + 80.0 * ((x as f32 + y as f32) / 32.0).sin()) as u8,
            );
            let n = (next() & 0x1f) as u8;
            v.extend_from_slice(&[r.wrapping_add(n), g.wrapping_add(n), b.wrapping_add(n)]);
        }
    }
    v
}

fn gradient_gray(w: u32, h: u32) -> Vec<u8> {
    let mut next = xorshift(11);
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let base = (((x as f32 / w as f32) + (y as f32 / h as f32)) * 127.5) as u8;
            v.push(base.wrapping_add((next() & 0x0f) as u8));
        }
    }
    v
}

/// Flat-color scientific-paper style figure: a handful of solid regions
/// (few unique colors — the content class where an `/Indexed` palette
/// beats JPEG).
fn flat_figure_rgb(w: u32, h: u32) -> Vec<u8> {
    const REGIONS: [[u8; 3]; 7] = [
        [255, 255, 255], // paper
        [0, 0, 0],       // ink
        [31, 119, 180],  // blue
        [255, 127, 14],  // orange
        [44, 160, 44],   // green
        [214, 39, 40],   // red
        [148, 103, 189], // purple
    ];
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            // Colored bands with a solid top-left quadrant: flat regions,
            // sharp edges, no gradients.
            let c = if x < w / 2 && y < h / 2 {
                REGIONS[1]
            } else {
                REGIONS[(x * 7 / w) as usize % 7]
            };
            v.extend_from_slice(&c);
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Document builders (all synthetic, in-memory, deterministic)
// ---------------------------------------------------------------------------

fn new_doc() -> (Document, lopdf::ObjectId) {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Vec::<Object>::new(),
            "Count" => 0,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    (doc, pages_id)
}

fn push_kid(doc: &mut Document, pages_id: lopdf::ObjectId, kid: lopdf::ObjectId) {
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        let mut kids: Vec<Object> = pages
            .get(b"Kids")
            .and_then(|k| k.as_array().cloned())
            .unwrap_or_default();
        kids.push(Object::Reference(kid));
        let count = kids.len() as u32;
        pages.set("Kids", kids);
        pages.set("Count", count);
    }
}

/// Add a page that draws `pixels` (w×h, `gray` = 1 or 3 bytes/px) full-bleed.
fn add_image_page(
    doc: &mut Document,
    pages_id: lopdf::ObjectId,
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    gray: bool,
) {
    let mut image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => w as i64,
        "Height" => h as i64,
        "BitsPerComponent" => 8,
    };
    image_dict.set("ColorSpace", if gray { "DeviceGray" } else { "DeviceRGB" });
    let mut image_stream = Stream::new(image_dict, pixels);
    image_stream.compress().unwrap(); // FlateDecode → re-encodable
    let image_id = doc.add_object(image_stream);

    let content = Content {
        operations: vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![w.into(), 0.into(), 0.into(), h.into(), 0.into(), 0.into()],
            ),
            Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
            Operation::new("Q", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    });
    push_kid(doc, pages_id, page_id);
}

/// Like [`add_image_page`], but drawn at `placed_w × placed_h` points
/// instead of at pixel size, so the effective resolution of the image can
/// be set independently of its raster (pixels ÷ (points/72) dpi).
fn add_image_page_at(
    doc: &mut Document,
    pages_id: lopdf::ObjectId,
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    gray: bool,
    placed: (f64, f64),
) {
    let mut image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => w as i64,
        "Height" => h as i64,
        "BitsPerComponent" => 8,
    };
    image_dict.set("ColorSpace", if gray { "DeviceGray" } else { "DeviceRGB" });
    let mut image_stream = Stream::new(image_dict, pixels);
    image_stream.compress().unwrap(); // FlateDecode → re-encodable
    let image_id = doc.add_object(image_stream);

    let content = Content {
        operations: vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    placed.0.into(),
                    0.into(),
                    0.into(),
                    placed.1.into(),
                    0.into(),
                    0.into(),
                ],
            ),
            Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
            Operation::new("Q", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), placed.0.into(), placed.1.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    });
    push_kid(doc, pages_id, page_id);
}

fn add_text_page(doc: &mut Document, pages_id: lopdf::ObjectId, page_no: u32) {
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), 12.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
            Operation::new(
                "Tj",
                vec![Object::String(
                    format!("Regression page {page_no}").into_bytes(),
                    lopdf::StringFormat::Literal,
                )],
            ),
            Operation::new("Td", vec![0.into(), (-16).into()]),
            Operation::new(
                "Tj",
                vec![Object::String(
                    b"Structural integrity must survive compression.".to_vec(),
                    lopdf::StringFormat::Literal,
                )],
            ),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        }}},
    });
    push_kid(doc, pages_id, page_id);
}

/// A page whose content stream embeds a 2×2 RGB **inline** image (BI..EI).
/// The stream is hand-crafted: inline image data is raw bytes after `ID`
/// (lopdf's operator encoder would wrap it in parentheses, which is not valid
/// inline-image syntax).
fn inline_image_doc() -> Document {
    let (mut doc, pages_id) = new_doc();
    // 2×2 RGB, no 0x45 ('E') / 0x49 ('I') byte inside so the raw `EI` marker
    // cannot be confused mid-stream.
    let inline: &[u8] = &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
    let mut content = Vec::new();
    content.extend_from_slice(b"q 100 0 0 100 0 0 cm BI /W 2 /H 2 /BPC 8 /CS /RGB ID ");
    content.extend_from_slice(inline);
    content.extend_from_slice(b" EI Q");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        "Contents" => content_id,
        "Resources" => dictionary! {},
    });
    push_kid(&mut doc, pages_id, page_id);
    doc
}

/// A page with a `DeviceCMYK` DCTDecode image stream (must be skipped as-is).
fn cmyk_doc() -> Document {
    let (mut doc, pages_id) = new_doc();
    // Real JPEG payload — the point is that the compressor must skip it
    // (CMYK is not supported) and preserve it byte-for-byte.
    let mut payload = Vec::new();
    let img = image::RgbImage::from_raw(4, 4, vec![128u8; 4 * 4 * 3]).unwrap();
    image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::Cursor::new(&mut payload), 80)
        .encode_image(&img)
        .unwrap();
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 4,
            "Height" => 4,
            "ColorSpace" => "DeviceCMYK",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => payload.len() as i64,
        },
        payload,
    ));
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        Content { operations: vec![] }.encode().unwrap(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 64.into(), 64.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
        "Contents" => content_id,
    });
    push_kid(&mut doc, pages_id, page_id);
    doc
}

/// A page mixing a FlateDecode RGB image, a FlateDecode text stream and a
/// FlateDecode metadata stream.
fn mixed_flate_doc() -> Document {
    let (mut doc, pages_id) = new_doc();
    add_image_page(&mut doc, pages_id, photoish_rgb(96, 96), 96, 96, false);
    add_text_page(&mut doc, pages_id, 1);
    let mut metadata = Stream::new(
        dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
        b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>".to_vec(),
    );
    metadata.compress().unwrap();
    doc.add_object(metadata);
    doc
}

/// A DeviceRGB image stream that illegally carries a 4th (alpha) byte per
/// pixel — the chunked-normalization path in `compress_images`.
fn rgba4_doc(w: u32, h: u32) -> Document {
    let (mut doc, pages_id) = new_doc();
    let mut next = xorshift(7);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        pixels.extend_from_slice(&[
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
            255,
        ]);
    }
    add_image_page(&mut doc, pages_id, pixels, w, h, false);
    doc
}

/// The canonical (3 bytes/px) RGB counterpart of `rgba4_doc` — the ground
/// truth of what the 4-byte stream is supposed to look like.
fn rgb3_doc(w: u32, h: u32) -> Document {
    let (mut doc, pages_id) = new_doc();
    let mut next = xorshift(7);
    let mut pixels = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..w * h {
        pixels.extend_from_slice(&[
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
        ]);
    }
    add_image_page(&mut doc, pages_id, pixels, w, h, false);
    doc
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Assert the PDF at `path` loads cleanly, has ≥1 page, and every stream's
/// `/Length` matches its content exactly. Returns the loaded document.
fn assert_well_formed(path: &Path) -> Document {
    let doc = Document::load(path).unwrap_or_else(|e| panic!("output must load cleanly: {e}"));

    assert!(
        !doc.get_pages().is_empty(),
        "output must have at least one page"
    );

    for (id, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            let len = s.dict.get(b"Length").ok().and_then(|l| l.as_i64().ok());
            if let Some(len) = len {
                assert_eq!(
                    len as usize,
                    s.content.len(),
                    "stream {id:?}: /Length {len} must equal content length {}",
                    s.content.len()
                );
            }
        }
    }

    if ensure_tool("gs", gs_available()) {
        let status = Command::new("gs")
            .args(["-sDEVICE=nullpage", "-dNOPAUSE", "-dBATCH", "-dQUIET"])
            .arg(path)
            .status()
            .expect("ghostscript should run");
        assert!(
            status.success(),
            "ghostscript rejected {} as malformed",
            path.display()
        );
    }

    // qpdf is the strictest syntax gate: it validates every xref entry.
    if ensure_tool("qpdf", qpdf_available()) {
        let output = Command::new("qpdf").args(["--check"]).arg(path).output();
        if let Ok(output) = output {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                output.status.success() && !text.contains("WARNING") && !text.contains("ERROR"),
                "qpdf rejected {}:\n{}",
                path.display(),
                text
            );
        }
    }

    doc
}

/// Rasterize the first page of `pdf` to `<prefix>.png` with `pdftoppm`.
/// Returns `false` when poppler is unavailable or rendering fails.
fn render_first_page(pdf: &Path, prefix: &Path) -> bool {
    let output = Command::new("pdftoppm")
        .args(["-singlefile", "-png", "-r", "72", "-f", "1", "-l", "1"])
        .arg(pdf)
        .arg(prefix)
        .output();
    matches!(output, Ok(out) if out.status.success())
}

/// Structural similarity (mean SSIM over the luminance channel, 64×64).
fn ssim(a: &GrayImage, b: &GrayImage) -> f64 {
    const N: u32 = 64;
    let a = image::imageops::resize(a, N, N, image::imageops::FilterType::Triangle);
    let b = image::imageops::resize(b, N, N, image::imageops::FilterType::Triangle);
    let count = (N * N) as f64;

    let mean = |img: &GrayImage| img.pixels().map(|p| p[0] as f64).sum::<f64>() / count;
    let (ma, mb) = (mean(&a), mean(&b));
    let var = |img: &GrayImage, m: f64| {
        img.pixels().map(|p| (p[0] as f64 - m).powi(2)).sum::<f64>() / count
    };
    let (va, vb) = (var(&a, ma), var(&b, mb));
    let cov = a
        .pixels()
        .zip(b.pixels())
        .map(|(x, y)| (x[0] as f64 - ma) * (y[0] as f64 - mb))
        .sum::<f64>()
        / count;

    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

/// Rasterize pre/post versions of `name` and assert the pages are visually
/// equivalent (SSIM above `threshold`). Returns early when poppler is absent.
fn assert_visual_similarity(dir: &Path, name: &str, threshold: f64) {
    if !ensure_tool("pdftoppm", pdftoppm_available()) {
        return;
    }
    let (pre_pdf, post_pdf) = (
        dir.join(format!("{name}-pre.pdf")),
        dir.join(format!("{name}-post.pdf")),
    );
    let (pre_png, post_png) = (
        dir.join(format!("{name}-pre")),
        dir.join(format!("{name}-post")),
    );

    assert!(
        render_first_page(&pre_pdf, &pre_png),
        "pdftoppm failed on {name}-pre.pdf"
    );
    assert!(
        render_first_page(&post_pdf, &post_png),
        "pdftoppm failed on {name}-post.pdf"
    );

    let pre = image::open(pre_png.with_extension("png")).expect("pre render must decode");
    let post = image::open(post_png.with_extension("png")).expect("post render must decode");
    let score = ssim(&pre.to_luma8(), &post.to_luma8());
    assert!(
        score >= threshold,
        "SSIM {score:.4} below threshold {threshold} for {name}"
    );
}

fn decompressed_non_image_streams(doc: &Document) -> Vec<Vec<u8>> {
    doc.objects
        .values()
        .filter_map(|obj| match obj {
            // Uncompressed streams have no /Filter and fail `decompressed_content`,
            // so fall back to the raw content for those.
            Object::Stream(s) if s.dict.get(b"Subtype").is_err() => Some(
                s.decompressed_content()
                    .unwrap_or_else(|_| s.content.clone()),
            ),
            _ => None,
        })
        .collect()
}

fn count_image_streams(doc: &Document) -> usize {
    doc.objects
        .values()
        .filter(|obj| matches!(obj, Object::Stream(s) if s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok() == Some(b"Image".as_slice())))
        .count()
}

fn find_image_streams(doc: &Document) -> Vec<(lopdf::ObjectId, Vec<u8>, lopdf::Dictionary)> {
    doc.objects
        .iter()
        .filter_map(|(id, obj)| match obj {
            Object::Stream(s)
                if s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok()
                    == Some(b"Image".as_slice()) =>
            {
                Some((*id, s.content.clone(), s.dict.clone()))
            }
            _ => None,
        })
        .collect()
}

/// True when a stream dict describes a DCT image: either a bare
/// `/DCTDecode` name or the flate-wrapped `[FlateDecode, DCTDecode]` chain
/// (the OCRmyPDF-style trick, applied only when the wrapped form is smaller).
fn is_dct_filter(dict: &lopdf::Dictionary) -> bool {
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => n == b"DCTDecode",
        Ok(Object::Array(a)) => {
            let names: Vec<&[u8]> = a.iter().filter_map(|e| e.as_name().ok()).collect();
            names == [b"FlateDecode".as_slice(), b"DCTDecode".as_slice()]
        }
        _ => false,
    }
}

/// Inflate a flate-wrapped JPEG stream back to raw JPEG bytes (lopdf cannot
/// decode DCTDecode, so the test applies the FlateDecode layer itself).
fn unwrap_flate(content: &[u8]) -> Vec<u8> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    ZlibDecoder::new(content)
        .read_to_end(&mut out)
        .expect("flate-wrapped JPEG must inflate");
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Multi-image PDF: structurally valid after compression, every image
/// re-encoded as DCTDecode, pages visually equivalent.
#[test]
fn multi_image_roundtrip_is_well_formed_and_reencoded() {
    let dir = test_dir();

    let images: Vec<(Vec<u8>, u32, u32, bool)> = (0..12)
        .map(|i| {
            let gray = i % 4 == 0; // every 4th image is grayscale
            let (w, h) = (128, 128 + (i % 3) * 16);
            let pixels = if gray {
                gradient_gray(w, h)
            } else {
                photoish_rgb(w, h)
            };
            (pixels, w, h, gray)
        })
        .collect();

    let mut pre = new_doc();
    for (pixels, w, h, gray) in &images {
        add_image_page(&mut pre.0, pre.1, pixels.clone(), *w, *h, *gray);
    }
    save_pdf(&mut pre.0, dir.join("multi-pre.pdf").to_str().unwrap()).unwrap();

    let mut post = new_doc();
    for (pixels, w, h, gray) in &images {
        add_image_page(&mut post.0, post.1, pixels.clone(), *w, *h, *gray);
    }
    compress_images(&mut post.0, QUALITY, false);
    compress_and_save_pdf(
        &mut post.0,
        dir.join("multi-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("multi-post.pdf"));
    assert_eq!(loaded.get_pages().len(), 12, "all pages must survive");

    let mut image_count = 0;
    for obj in loaded.objects.values() {
        if let Object::Stream(s) = obj
            && s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok() == Some(b"Image".as_slice())
        {
            image_count += 1;
            assert!(
                is_dct_filter(&s.dict),
                "re-encodable image must end up as a DCTDecode stream"
            );
            assert_eq!(
                s.dict.get(b"Length").and_then(|l| l.as_i64()).ok(),
                Some(s.content.len() as i64)
            );
        }
    }
    // The 12 pages carry 6 unique pixel sets, each used twice; coalescing
    // collapses the byte-identical duplicates onto one object per unique
    // image (12 pages still render — `get_pages` above proves it).
    assert_eq!(
        image_count, 6,
        "12 pages must collapse to 6 unique image objects"
    );

    assert_visual_similarity(&dir, "multi", 0.90);
}

/// Zero-image (text-only) PDF: nothing to re-encode, content untouched,
/// still renders identically.
#[test]
fn text_only_pdf_is_untouched() {
    let dir = test_dir();
    let (mut doc, pages_id) = new_doc();
    add_text_page(&mut doc, pages_id, 1);
    add_text_page(&mut doc, pages_id, 2);

    let content_before = decompressed_non_image_streams(&doc);
    compress_images(&mut doc, QUALITY, false);
    assert_eq!(
        decompressed_non_image_streams(&doc),
        content_before,
        "content streams must be untouched"
    );

    save_pdf(&mut doc, dir.join("text-pre.pdf").to_str().unwrap()).unwrap();
    compress_and_save_pdf(&mut doc, dir.join("text-post.pdf").to_str().unwrap(), false).unwrap();

    let loaded = assert_well_formed(&dir.join("text-post.pdf"));
    assert_eq!(loaded.get_pages().len(), 2);
    assert_eq!(
        count_image_streams(&loaded),
        0,
        "text-only PDF must stay image-free"
    );

    assert_visual_similarity(&dir, "text", 0.99);
}

/// CMYK streams are not supported by the image crate — they must be skipped
/// and preserved byte-for-byte.
#[test]
fn cmyk_stream_is_preserved() {
    let mut doc = cmyk_doc();
    let before = find_image_streams(&doc);
    assert_eq!(before.len(), 1);

    compress_images(&mut doc, QUALITY, false);

    let after = find_image_streams(&doc);
    assert_eq!(after.len(), 1, "CMYK stream must still be present");
    assert_eq!(
        after[0].1, before[0].1,
        "CMYK payload must be preserved byte-for-byte"
    );
    assert_eq!(after[0].2, before[0].2, "CMYK dict must be preserved");

    // Structural check end-to-end as well.
    let dir = test_dir();
    compress_and_save_pdf(&mut doc, dir.join("cmyk-post.pdf").to_str().unwrap(), false).unwrap();
    assert_well_formed(&dir.join("cmyk-post.pdf"));
}

/// Inline images live inside content streams and must never be touched by the
/// XObject image pass.
#[test]
fn inline_image_is_untouched() {
    let dir = test_dir();
    let mut doc = inline_image_doc();

    let content_before = decompressed_non_image_streams(&doc);
    compress_images(&mut doc, QUALITY, false);
    assert_eq!(
        decompressed_non_image_streams(&doc),
        content_before,
        "inline image content must be untouched"
    );

    save_pdf(&mut doc, dir.join("inline-pre.pdf").to_str().unwrap()).unwrap();
    compress_and_save_pdf(
        &mut doc,
        dir.join("inline-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("inline-post.pdf"));
    assert_eq!(
        count_image_streams(&loaded),
        0,
        "no XObject image streams expected"
    );

    let rendered = decompressed_non_image_streams(&loaded);
    assert!(
        rendered.iter().any(|c| c
            .windows(6)
            .any(|w| w == [0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00])),
        "inline pixel payload must still be present after round-trip"
    );

    assert_visual_similarity(&dir, "inline", 0.99);
}

/// Mixed FlateDecode streams: the image becomes DCTDecode, the text/metadata
/// streams keep their content.
#[test]
fn mixed_flate_streams_are_handled() {
    let dir = test_dir();
    let mut doc = mixed_flate_doc();
    save_pdf(&mut doc, dir.join("mixed-pre.pdf").to_str().unwrap()).unwrap();

    let metadata_before = doc
        .objects
        .values()
        .filter_map(|obj| match obj {
            Object::Stream(s)
                if s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok()
                    == Some(b"XML".as_slice()) =>
            {
                s.decompressed_content().ok()
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    compress_images(&mut doc, QUALITY, false);

    let metadata_after = doc
        .objects
        .values()
        .filter_map(|obj| match obj {
            Object::Stream(s)
                if s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok()
                    == Some(b"XML".as_slice()) =>
            {
                s.decompressed_content().ok()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        metadata_after, metadata_before,
        "metadata stream must be preserved"
    );

    compress_and_save_pdf(
        &mut doc,
        dir.join("mixed-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();
    let loaded = assert_well_formed(&dir.join("mixed-post.pdf"));
    assert_eq!(count_image_streams(&loaded), 1);
    assert_visual_similarity(&dir, "mixed", 0.90);
}

/// A DeviceRGB stream with a 4th (alpha) byte per pixel is normalized by the
/// chunked path and re-encoded without visual drift.
#[test]
fn rgba4_stream_is_normalized_in_chunks() {
    let dir = test_dir();
    // Ground truth: the canonical 3-byte/px interpretation of the same pixels.
    let mut pre = rgb3_doc(128, 96);
    save_pdf(&mut pre, dir.join("rgba-pre.pdf").to_str().unwrap()).unwrap();

    let mut post = rgba4_doc(128, 96);
    compress_images(&mut post, QUALITY, false);
    compress_and_save_pdf(
        &mut post,
        dir.join("rgba-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("rgba-post.pdf"));
    let mut reencoded = 0;
    for obj in loaded.objects.values() {
        if let Object::Stream(s) = obj
            && s.dict.get(b"Subtype").and_then(|x| x.as_name()).ok() == Some(b"Image".as_slice())
        {
            reencoded += 1;
            assert!(
                is_dct_filter(&s.dict),
                "4-byte/px stream must be re-encoded as DCT"
            );
            assert_eq!(
                s.dict.get(b"Width").and_then(|w| w.as_i64()).ok(),
                Some(128)
            );
            assert_eq!(
                s.dict.get(b"Height").and_then(|h| h.as_i64()).ok(),
                Some(96)
            );
        }
    }
    assert_eq!(
        reencoded, 1,
        "4-byte/px stream must be normalized and re-encoded"
    );

    assert_visual_similarity(&dir, "rgba", 0.95);
}

/// Serial (1-thread pool) and parallel (global pool) runs must produce
/// byte-identical documents — parallelism must not change the output.
#[test]
fn parallel_and_serial_outputs_are_identical() {
    let dir = test_dir();
    let build = || {
        let (mut doc, pages_id) = new_doc();
        for _ in 0..8 {
            add_image_page(&mut doc, pages_id, photoish_rgb(96, 96), 96, 96, false);
        }
        doc
    };

    let mut serial = build();
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| compress_images(&mut serial, QUALITY, false));

    let mut parallel = build();
    compress_images(&mut parallel, QUALITY, false);

    let (serial_path, parallel_path) = (
        dir.join("determinism-serial.pdf"),
        dir.join("determinism-parallel.pdf"),
    );
    compress_and_save_pdf(&mut serial, serial_path.to_str().unwrap(), false).unwrap();
    compress_and_save_pdf(&mut parallel, parallel_path.to_str().unwrap(), false).unwrap();

    assert_eq!(
        std::fs::read(&serial_path).unwrap(),
        std::fs::read(&parallel_path).unwrap(),
        "serial and parallel compression must produce identical bytes"
    );
}

/// End-to-end: the actual `presse press` CLI binary produces a loadable,
/// renderable, smaller PDF.
#[test]
fn press_cli_end_to_end() {
    let dir = test_dir();
    let (mut doc, pages_id) = new_doc();
    for _ in 0..6 {
        add_image_page(&mut doc, pages_id, photoish_rgb(160, 160), 160, 160, false);
    }
    save_pdf(&mut doc, dir.join("cli-pre.pdf").to_str().unwrap()).unwrap();

    let presse = env!("CARGO_BIN_EXE_presse");
    let out = dir.join("cli-post.pdf");
    let status = Command::new(presse)
        .args([
            "press",
            dir.join("cli-pre.pdf").to_str().unwrap(),
            "-q",
            "50",
            "-o",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("presse binary should run");
    assert!(status.success(), "presse press must exit successfully");

    assert_well_formed(&out);
    assert!(
        std::fs::metadata(&out).unwrap().len()
            < std::fs::metadata(dir.join("cli-pre.pdf")).unwrap().len(),
        "compressed output must be smaller"
    );
    assert_visual_similarity(&dir, "cli", 0.90);
}

/// Real-world PDFs carry gaps in their object numbering and usually exceed
/// one object stream (>200 objects). Both conditions used to corrupt lopdf's
/// cross-reference writer — shifted sections at every gap plus dropped
/// object-stream entries — which qpdf rejects and poppler can render as
/// blank pages. This test builds a document with both properties and asserts
/// the compressed output stays valid (qpdf gate + render).
///
/// Passes on lopdf >= 0.44 because the save path already calls
/// `renumber_objects()`: compacting the numbering makes the writer's
/// remaining gap/size quirks unreachable (verified: same /Size, same entry
/// count, nothing missing — identical to the previously vendored writer
/// patch).
#[test]
fn gapped_numbering_with_multiple_object_streams_is_valid() {
    let dir = test_dir();

    let build = || {
        let (mut doc, pages_id) = new_doc();
        // Enough pages to span multiple object streams on save (>200 objects).
        for page in 0..90 {
            add_text_page(&mut doc, pages_id, page);
        }
        // An orphaned object that nothing references, then delete it: creates
        // a gap in the object numbering without touching the page tree.
        let orphan = doc.add_object(Object::String(
            b"orphan".to_vec(),
            lopdf::StringFormat::Literal,
        ));
        doc.delete_object(orphan);
        doc
    };

    let mut pre = build();
    save_pdf(&mut pre, dir.join("gapped-pre.pdf").to_str().unwrap()).unwrap();

    let mut post = build();
    compress_and_save_pdf(
        &mut post,
        dir.join("gapped-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("gapped-post.pdf"));
    assert_eq!(loaded.get_pages().len(), 90, "all pages must survive");
    assert_visual_similarity(&dir, "gapped", 0.99);
}

/// `--dpi` downsampling: a 300×300 image drawn at 100×100 pt is 216 dpi
/// effective; at `-d 150` it must be resampled to ≈100·150/72 = 208 px and
/// the `/Width`/`/Height` dictionary entries updated to match, with the
/// output still structurally valid and visually recognizable.
#[test]
fn dpi_downsampling_resizes_placed_images_and_updates_dims() {
    let dir = test_dir();

    let mut pre = new_doc();
    add_image_page_at(
        &mut pre.0,
        pre.1,
        photoish_rgb(300, 300),
        300,
        300,
        false,
        (100.0, 100.0),
    );
    save_pdf(&mut pre.0, dir.join("dpi-pre.pdf").to_str().unwrap()).unwrap();

    let mut post = new_doc();
    add_image_page_at(
        &mut post.0,
        post.1,
        photoish_rgb(300, 300),
        300,
        300,
        false,
        (100.0, 100.0),
    );
    compress_images_with(
        &mut post.0,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        Some(150),
        false,
        false,
    );
    compress_and_save_pdf(
        &mut post.0,
        dir.join("dpi-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("dpi-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    assert_eq!(
        dict.get(b"Width").and_then(|w| w.as_i64()).ok(),
        Some(208),
        "/Width must reflect the resampled raster"
    );
    assert_eq!(
        dict.get(b"Height").and_then(|h| h.as_i64()).ok(),
        Some(208),
        "/Height must reflect the resampled raster"
    );
    assert!(
        is_dct_filter(dict),
        "resampled image must be re-encoded as DCTDecode"
    );

    // Same placement, less detail: still the same page at high SSIM.
    assert_visual_similarity(&dir, "dpi", 0.85);
}

/// `--dpi` above the effective resolution must not bite: a 300×300 image at
/// 216 dpi effective stays 300×300 under a 600 dpi cap.
#[test]
fn dpi_above_effective_resolution_keeps_source_size() {
    let dir = test_dir();

    let mut doc = new_doc();
    add_image_page_at(
        &mut doc.0,
        doc.1,
        photoish_rgb(300, 300),
        300,
        300,
        false,
        (100.0, 100.0),
    );
    compress_images_with(
        &mut doc.0,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        Some(600),
        false,
        false,
    );
    compress_and_save_pdf(
        &mut doc.0,
        dir.join("dpi-noop.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("dpi-noop.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    assert_eq!(dict.get(b"Width").and_then(|w| w.as_i64()).ok(), Some(300));
    assert_eq!(dict.get(b"Height").and_then(|h| h.as_i64()).ok(), Some(300));
}

/// CLI end-to-end: `presse press -d 75` on a high-resolution placed image
/// produces a valid, smaller PDF (qpdf + gs + xref gates in
/// `assert_well_formed`) whose image raster was actually reduced.
#[test]
fn press_cli_dpi_flag_resamples_and_stays_valid() {
    let dir = test_dir();
    let mut doc = new_doc();
    add_image_page_at(
        &mut doc.0,
        doc.1,
        photoish_rgb(600, 600),
        600,
        600,
        false,
        (200.0, 200.0),
    );
    save_pdf(&mut doc.0, dir.join("dpi-cli-pre.pdf").to_str().unwrap()).unwrap();

    let presse = env!("CARGO_BIN_EXE_presse");
    let out = dir.join("dpi-cli-post.pdf");
    let status = Command::new(presse)
        .args([
            "press",
            dir.join("dpi-cli-pre.pdf").to_str().unwrap(),
            "-q",
            "50",
            "-d",
            "75",
            "-o",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("presse binary should run");
    assert!(
        status.success(),
        "presse press -d 75 must exit successfully"
    );

    let loaded = assert_well_formed(&out);
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    // 600 px at 200 pt = 216 dpi effective → 200·75/72 ≈ 208 px.
    assert_eq!(dict.get(b"Width").and_then(|w| w.as_i64()).ok(), Some(208));
    assert_eq!(dict.get(b"Height").and_then(|h| h.as_i64()).ok(), Some(208));
    assert_visual_similarity(&dir, "dpi-cli", 0.85);
}

/// The CPU transcoder is the zero-cost default: `compress_images_with` over
/// [`CpuTranscoder`] must equal the plain `compress_images` path.
#[test]
fn cpu_transcoder_default_parity() {
    let dir = test_dir();
    let build = || {
        let (mut doc, pages_id) = new_doc();
        for i in 0..6 {
            let gray = i % 3 == 0;
            let (w, h) = (96, 96);
            let pixels = if gray {
                gradient_gray(w, h)
            } else {
                photoish_rgb(w, h)
            };
            add_image_page(&mut doc, pages_id, pixels, w, h, gray);
        }
        doc
    };

    let mut plain = build();
    compress_images(&mut plain, QUALITY, false);
    let mut explicit = build();
    compress_images_with(
        &mut explicit,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        None,
        false,
        false,
    );

    let (p_path, e_path) = (
        dir.join("parity-plain.pdf"),
        dir.join("parity-explicit.pdf"),
    );
    compress_and_save_pdf(&mut plain, p_path.to_str().unwrap(), false).unwrap();
    compress_and_save_pdf(&mut explicit, e_path.to_str().unwrap(), false).unwrap();
    assert_eq!(
        std::fs::read(&p_path).unwrap(),
        std::fs::read(&e_path).unwrap(),
        "CpuTranscoder must match the default path byte-for-byte"
    );
}

/// `-d` is a resolution *cap*: output dimensions must never exceed the
/// source, and must match ⌊placed·dpi/72⌋ when that bites. Property-style
/// over a few placements so the "never up-scales" rule is pinned.
#[test]
fn dpi_cap_never_upscales_and_matches_formula() {
    let dir = test_dir();
    let cases = [
        // (source W, source H, placed W, placed H, dpi, expected W, expected H)
        (400, 300, 100.0, 100.0, 150, 208, 208), // 216 dpi effective → 150 cap
        (400, 300, 100.0, 100.0, 300, 400, 300), // 416 > source → capped at source
        (100, 100, 400.0, 400.0, 75, 100, 100),  // placed low-dpi → no change
        (512, 256, 200.0, 100.0, 600, 512, 256), // 1667/833 > source → capped
    ];
    for (i, (w, h, pw, ph, dpi, ew, eh)) in cases.iter().enumerate() {
        let mut doc = new_doc();
        add_image_page_at(
            &mut doc.0,
            doc.1,
            photoish_rgb(*w, *h),
            *w,
            *h,
            false,
            (*pw, *ph),
        );
        compress_images_with(
            &mut doc.0,
            QualityMode::fixed(QUALITY),
            false,
            &CpuTranscoder::default(),
            Some(*dpi),
            false,
            false,
        );
        compress_and_save_pdf(
            &mut doc.0,
            dir.join(format!("dpi-prop-{i}.pdf")).to_str().unwrap(),
            false,
        )
        .unwrap();
        let loaded = assert_well_formed(&dir.join(format!("dpi-prop-{i}.pdf")));
        let images = find_image_streams(&loaded);
        assert_eq!(images.len(), 1);
        let (_, _, dict) = &images[0];
        let ow = dict.get(b"Width").and_then(|x| x.as_i64()).ok().unwrap() as u32;
        let oh = dict.get(b"Height").and_then(|x| x.as_i64()).ok().unwrap() as u32;
        assert!(
            ow <= *w && oh <= *h,
            "case {i}: cap must never up-scale ({ow}x{oh} > {w}x{h})"
        );
        let formula = |placed: f64| ((placed * *dpi as f64 / 72.0).round() as u32).max(1);
        assert!(
            ow <= formula(*pw) && oh <= formula(*ph),
            "case {i}: output {ow}x{oh} exceeds ⌊placed·dpi/72⌋ {}x{}",
            formula(*pw),
            formula(*ph)
        );
        assert_eq!((ow, oh), (*ew, *eh), "case {i}: expected dims");
    }
}

/// After the parallel rewrite, every object in the saved document must be
/// reachable from the trailer's /Root (no orphans from the detach/re-attach
/// phase), and the file must load with a valid cross-reference table.
#[test]
fn xref_is_well_formed_and_all_objects_reachable() {
    let dir = test_dir();

    let (mut doc, pages_id) = new_doc();
    for _ in 0..8 {
        add_image_page(&mut doc, pages_id, photoish_rgb(160, 160), 160, 160, false);
    }
    compress_images_with(
        &mut doc,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        Some(150),
        false,
        false,
    );
    compress_and_save_pdf(
        &mut doc,
        dir.join("reach-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("reach-post.pdf"));

    // BFS from /Root through every reference; every object must be found.
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<lopdf::ObjectId> = loaded
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|r| r.as_reference().ok())
        .map(|id| vec![id])
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(obj) = loaded.get_object(id).ok() else {
            continue;
        };
        match obj {
            Object::Dictionary(d) => {
                for (_, v) in d.iter() {
                    collect_references(v, &mut stack);
                }
            }
            Object::Array(a) => {
                for v in a {
                    collect_references(v, &mut stack);
                }
            }
            Object::Stream(s) => {
                for (_, v) in s.dict.iter() {
                    collect_references(v, &mut stack);
                }
            }
            _ => {}
        }
    }
    let all: std::collections::HashSet<lopdf::ObjectId> = loaded.objects.keys().copied().collect();
    // Structural objects (object-stream container, xref stream) are kept in
    // `doc.objects` by the loader but are not part of the content graph.
    let structural = |obj: &Object| {
        matches!(obj, Object::Stream(s)
            if s.dict.get(b"Type").ok().and_then(|t| t.as_name().ok())
                .is_some_and(|t| t == b"ObjStm" || t == b"XRef"))
    };
    let content_ids: Vec<_> = all
        .iter()
        .copied()
        .filter(|id| !loaded.objects.get(id).is_some_and(structural))
        .collect();
    let missing: Vec<_> = content_ids
        .iter()
        .copied()
        .filter(|id| !seen.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "content objects not reachable from /Root: {missing:?}"
    );
    assert_eq!(
        seen.len(),
        content_ids.len(),
        "reachable set must equal content-object set"
    );
}

/// Push every reference inside `obj` (dictionary values, array elements)
/// onto `stack`.
fn collect_references(obj: &Object, stack: &mut Vec<lopdf::ObjectId>) {
    match obj {
        Object::Reference(id) => stack.push(*id),
        Object::Dictionary(d) => {
            for (_, v) in d.iter() {
                collect_references(v, stack);
            }
        }
        Object::Array(a) => {
            for v in a {
                collect_references(v, stack);
            }
        }
        Object::Stream(s) => {
            for (_, v) in s.dict.iter() {
                collect_references(v, stack);
            }
        }
        _ => {}
    }
}

/// A grayscale source must stay a single-component JPEG after re-encoding —
/// a 3-component JPEG inside a /DeviceGray stream renders as garbage.
#[test]
fn grayscale_jpeg_stays_single_component() {
    let dir = test_dir();

    let (mut doc, pages_id) = new_doc();
    add_image_page(&mut doc, pages_id, gradient_gray(128, 128), 128, 128, true);
    compress_images_with(
        &mut doc,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        None,
        false,
        false,
    );
    compress_and_save_pdf(&mut doc, dir.join("gray-post.pdf").to_str().unwrap(), false).unwrap();

    let loaded = assert_well_formed(&dir.join("gray-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, content, dict) = &images[0];
    assert!(
        is_dct_filter(dict),
        "grayscale image must be re-encoded as DCTDecode"
    );
    let jpeg = match dict.get(b"Filter") {
        Ok(Object::Array(_)) => unwrap_flate(content),
        _ => content.clone(),
    };
    let img = image::load_from_memory(&jpeg).expect("output JPEG must decode");
    assert_eq!(
        img.color(),
        image::ColorType::L8,
        "single-component grayscale must survive re-encoding"
    );
}

/// The rayon + allocator path must be safe under many concurrent
/// compression jobs on the same binary (global thread pool + mimalloc),
/// with every output identical and valid.
#[test]
fn concurrent_compression_is_deterministic_and_valid() {
    let dir = test_dir();
    let build = || {
        let (mut doc, pages_id) = new_doc();
        for _ in 0..6 {
            add_image_page(&mut doc, pages_id, photoish_rgb(160, 160), 160, 160, false);
        }
        doc
    };

    let outputs: Vec<PathBuf> = (0..8)
        .map(|i| {
            let path = dir.join(format!("conc-{i}.pdf"));
            let mut doc = build();
            compress_images_with(
                &mut doc,
                QualityMode::fixed(QUALITY),
                false,
                &CpuTranscoder::default(),
                None,
                false,
                false,
            );
            compress_and_save_pdf(&mut doc, path.to_str().unwrap(), false).unwrap();
            path
        })
        .collect();

    let first = std::fs::read(&outputs[0]).unwrap();
    for path in &outputs {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes, first, "concurrent runs must be byte-identical");
        assert_well_formed(path);
    }
}

/// `-ssim <target>` maps to a lower JPEG quality than the default (smaller
/// output), stays structurally valid, and renders recognizably. The
/// calibration guarantees the target on grainy content; photos exceed it,
/// so the visual gate is lenient by design.
#[test]
fn ssim_target_reduces_size_and_stays_valid() {
    let dir = test_dir();

    let build = |mode: QualityMode, name: &str| {
        let mut doc = new_doc();
        for _ in 0..4 {
            add_image_page(&mut doc.0, doc.1, gradient_gray(256, 256), 256, 256, true);
        }
        compress_images_with(
            &mut doc.0,
            mode,
            false,
            &CpuTranscoder::default(),
            None,
            false,
            false,
        );
        compress_and_save_pdf(&mut doc.0, dir.join(name).to_str().unwrap(), false).unwrap();
        std::fs::metadata(dir.join(name)).unwrap().len()
    };

    let default_size = build(QualityMode::fixed(QUALITY), "ssim-pre.pdf");
    let relaxed = build(QualityMode::press(QUALITY, Some(0.72)), "ssim-post.pdf");
    assert!(
        relaxed < default_size,
        "-ssim 0.72 must compress harder than the default: {relaxed} vs {default_size}"
    );

    let loaded = assert_well_formed(&dir.join("ssim-post.pdf"));
    assert_eq!(loaded.get_pages().len(), 4, "all pages must survive");
    assert_visual_similarity(&dir, "ssim", 0.85);
}

/// `-ssim` ≥ 1.0 is the default behavior: `-q` as given, byte-identical
/// to not passing the flag at all.
#[test]
fn ssim_one_keeps_default_quality() {
    let dir = test_dir();

    let run = |mode: QualityMode, name: &str| {
        let mut doc = new_doc();
        add_image_page(&mut doc.0, doc.1, photoish_rgb(200, 200), 200, 200, false);
        compress_images_with(
            &mut doc.0,
            mode,
            false,
            &CpuTranscoder::default(),
            None,
            false,
            false,
        );
        compress_and_save_pdf(&mut doc.0, dir.join(name).to_str().unwrap(), false).unwrap();
        std::fs::read(dir.join(name)).unwrap()
    };

    let plain = run(QualityMode::fixed(QUALITY), "ssim1-plain.pdf");
    let with_flag = run(QualityMode::press(QUALITY, Some(1.0)), "ssim1-flag.pdf");
    assert_eq!(
        plain, with_flag,
        "-ssim 1.0 must equal the plain -q behavior"
    );
}

/// The two quality knobs compose: `-d 150 -s 0.72` both resamples *and*
/// compresses harder — dimensions shrink to the dpi cap and the output is
/// smaller than either knob alone.
#[test]
fn dpi_and_ssim_compose() {
    let dir = test_dir();

    let run = |mode: QualityMode, dpi: Option<u32>, name: &str| -> (u64, u32, u32) {
        let mut doc = new_doc();
        // 600 px placed at 200 pt = 216 dpi effective.
        add_image_page_at(
            &mut doc.0,
            doc.1,
            photoish_rgb(600, 600),
            600,
            600,
            false,
            (200.0, 200.0),
        );
        compress_images_with(
            &mut doc.0,
            mode,
            false,
            &CpuTranscoder::default(),
            dpi,
            false,
            false,
        );
        compress_and_save_pdf(&mut doc.0, dir.join(name).to_str().unwrap(), false).unwrap();
        let loaded = Document::load(dir.join(name)).unwrap();
        let (_, _, dict) = &find_image_streams(&loaded)[0];
        let w = dict.get(b"Width").and_then(|x| x.as_i64()).ok().unwrap() as u32;
        let h = dict.get(b"Height").and_then(|x| x.as_i64()).ok().unwrap() as u32;
        (std::fs::metadata(dir.join(name)).unwrap().len(), w, h)
    };

    let (base_size, _, _) = run(QualityMode::fixed(QUALITY), None, "compose-base.pdf");
    let (d_size, dw, _) = run(QualityMode::fixed(QUALITY), Some(150), "compose-d.pdf");
    let (s_size, _, _) = run(
        QualityMode::press(QUALITY, Some(0.72)),
        None,
        "compose-s.pdf",
    );
    let (ds_size, dsw, _) = run(
        QualityMode::press(QUALITY, Some(0.72)),
        Some(150),
        "compose-ds.pdf",
    );

    assert_eq!(
        dw, 417,
        "-d 150 on a 216-dpi placement → round(200·150/72) = 417 px"
    );
    assert_eq!(dsw, 417, "dpi cap applies regardless of the ssim target");
    assert!(
        ds_size < d_size && ds_size < s_size && s_size < base_size,
        "composed dpi+ssim must beat each knob alone: base {base_size}, d {d_size}, s {s_size}, ds {ds_size}"
    );
    assert_well_formed(&dir.join("compose-ds.pdf"));
}

/// Three byte-identical image objects must collapse to one canonical object
/// after compression (the *storage* half of duplicate handling; the dedup
/// cache is the *encode* half), and the page must render identically.
#[test]
fn duplicate_image_objects_collapse_to_one() {
    let dir = test_dir();
    let (mut doc, pages_id) = new_doc();
    let pixels = photoish_rgb(96, 96);
    for _ in 0..3 {
        add_image_page(&mut doc, pages_id, pixels.clone(), 96, 96, false);
    }
    save_pdf(&mut doc, dir.join("dedup-pre.pdf").to_str().unwrap()).unwrap();

    compress_images(&mut doc, QUALITY, false);
    compress_and_save_pdf(
        &mut doc,
        dir.join("dedup-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("dedup-post.pdf"));
    assert_eq!(
        count_image_streams(&loaded),
        1,
        "identical image objects must collapse to one canonical object"
    );
    assert_visual_similarity(&dir, "dedup", 0.99);
}

/// `--palette` must convert a flat-color figure to `/Indexed` +
/// `/FlateDecode` (with a palette-stream object in `/ColorSpace`), produce a
/// smaller file than the JPEG-only run, and still render identically.
#[test]
fn flat_figure_with_palette_flag_becomes_indexed() {
    let dir = test_dir();

    let (mut pre, pages_id) = new_doc();
    add_image_page(
        &mut pre,
        pages_id,
        flat_figure_rgb(256, 256),
        256,
        256,
        false,
    );
    save_pdf(&mut pre, dir.join("flat-pre.pdf").to_str().unwrap()).unwrap();

    let build = |palette: bool, name: &str| {
        let (mut doc, pages_id) = new_doc();
        add_image_page(
            &mut doc,
            pages_id,
            flat_figure_rgb(256, 256),
            256,
            256,
            false,
        );
        compress_images_with(
            &mut doc,
            QualityMode::fixed(QUALITY),
            false,
            &CpuTranscoder::default(),
            None,
            palette,
            false,
        );
        compress_and_save_pdf(&mut doc, dir.join(name).to_str().unwrap(), false).unwrap();
    };
    build(false, "flat-jpeg.pdf");
    build(true, "flat-post.pdf");

    let (jpeg_size, pal_size) = (
        std::fs::metadata(dir.join("flat-jpeg.pdf")).unwrap().len(),
        std::fs::metadata(dir.join("flat-post.pdf")).unwrap().len(),
    );
    assert!(
        pal_size < jpeg_size,
        "--palette must beat JPEG on a flat figure: {pal_size} vs {jpeg_size}"
    );

    let loaded = assert_well_formed(&dir.join("flat-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    assert_eq!(
        dict.get(b"Filter").and_then(|f| f.as_name()).ok(),
        Some(b"FlateDecode".as_slice()),
        "indexed image must stay FlateDecode"
    );
    let cs = dict
        .get(b"ColorSpace")
        .and_then(|c| c.as_array())
        .expect("/ColorSpace");
    assert_eq!(
        cs[0].as_name().ok(),
        Some(b"Indexed".as_slice()),
        "ColorSpace must be [/Indexed /DeviceRGB hival <palette stream>]"
    );
    assert_eq!(cs[1].as_name().ok(), Some(b"DeviceRGB".as_slice()));
    assert!(
        cs[3].as_reference().is_ok(),
        "palette must be an indirect stream"
    );

    assert_visual_similarity(&dir, "flat", 0.99);
}

/// `--palette` must not touch photographic content: photos still become
/// JPEG (`DCTDecode`), never `/Indexed`.
#[test]
fn photo_with_palette_flag_stays_jpeg() {
    let dir = test_dir();
    let (mut pre, pages_id) = new_doc();
    add_image_page(&mut pre, pages_id, photoish_rgb(160, 160), 160, 160, false);
    save_pdf(&mut pre, dir.join("photo-pal-pre.pdf").to_str().unwrap()).unwrap();

    let (mut doc, pages_id) = new_doc();
    add_image_page(&mut doc, pages_id, photoish_rgb(160, 160), 160, 160, false);
    compress_images_with(
        &mut doc,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        None,
        true,
        false,
    );
    compress_and_save_pdf(
        &mut doc,
        dir.join("photo-pal-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("photo-pal-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    assert!(
        is_dct_filter(dict),
        "photographic content must keep the JPEG path under --palette"
    );
    assert_eq!(
        dict.get(b"ColorSpace").and_then(|c| c.as_name()).ok(),
        Some(b"DeviceRGB".as_slice()),
        "ColorSpace must stay a plain DeviceRGB name"
    );
    assert_visual_similarity(&dir, "photo-pal", 0.95);
}

/// `--jpeg-encoder` (the 4:2:0 box-averaged codec) must produce smaller RGB
/// output than the default 4:4:4 encoder at the same quality, stay
/// structurally valid, keep grayscale single-component, and still render
/// equivalently. Default (flag off) stays byte-identical to the image-crate
/// path — already covered by `cpu_transcoder_default_parity`.
#[test]
fn jpeg_encoder_420_flag_smaller_valid_and_keeps_gray() {
    let dir = test_dir();

    let build = |native: bool, name: &str| {
        let (mut doc, pages_id) = new_doc();
        add_image_page(&mut doc, pages_id, photoish_rgb(256, 192), 256, 192, false);
        add_image_page(&mut doc, pages_id, gradient_gray(256, 192), 256, 192, true);
        let transcoder = CpuTranscoder::new(native);
        compress_images_with(
            &mut doc,
            QualityMode::fixed(QUALITY),
            false,
            &transcoder,
            None,
            false,
            false,
        );
        compress_and_save_pdf(&mut doc, dir.join(name).to_str().unwrap(), false).unwrap();
    };
    build(false, "je-444.pdf");
    build(true, "je-post.pdf");

    let (a, b) = (
        std::fs::metadata(dir.join("je-444.pdf")).unwrap().len(),
        std::fs::metadata(dir.join("je-post.pdf")).unwrap().len(),
    );
    assert!(
        b < a,
        "4:2:0 must beat 4:4:4 on RGB at the same quality: {b} vs {a}"
    );

    let loaded = assert_well_formed(&dir.join("je-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 2);
    let mut gray_seen = 0;
    for (_, content, dict) in &images {
        assert!(is_dct_filter(dict), "re-encoded images must be DCT streams");
        // The grayscale stream keeps a single-component JPEG payload.
        if dict.get(b"ColorSpace").and_then(|c| c.as_name()).ok() == Some(b"DeviceGray".as_slice())
        {
            gray_seen += 1;
            let jpeg = match dict.get(b"Filter") {
                Ok(Object::Array(_)) => unwrap_flate(content),
                _ => content.clone(),
            };
            let img = image::load_from_memory(&jpeg).expect("gray JPEG must decode");
            assert_eq!(
                img.color(),
                image::ColorType::L8,
                "gray must stay single-component under --jpeg-encoder"
            );
        }
    }
    assert_eq!(gray_seen, 1, "the gray image must be present");

    // Pre-render for the visual gate.
    let mut pre = new_doc();
    add_image_page(&mut pre.0, pre.1, photoish_rgb(256, 192), 256, 192, false);
    add_image_page(&mut pre.0, pre.1, gradient_gray(256, 192), 256, 192, true);
    save_pdf(&mut pre.0, dir.join("je-pre.pdf").to_str().unwrap()).unwrap();
    assert_visual_similarity(&dir, "je", 0.90);
}

/// CLI end-to-end: `presse press --jpeg-encoder` on a photo PDF is valid,
/// smaller than the same run without the flag, and renders equivalently.
#[test]
fn press_cli_jpeg_encoder_flag() {
    let dir = test_dir();
    let (mut doc, pages_id) = new_doc();
    for _ in 0..4 {
        add_image_page(&mut doc, pages_id, photoish_rgb(200, 200), 200, 200, false);
    }
    save_pdf(&mut doc, dir.join("je-cli-pre.pdf").to_str().unwrap()).unwrap();

    let presse = env!("CARGO_BIN_EXE_presse");
    let (plain, flag) = (dir.join("je-cli-plain.pdf"), dir.join("je-cli-post.pdf"));
    let input = dir.join("je-cli-pre.pdf");
    for (out, extra) in [(plain.clone(), false), (flag.clone(), true)] {
        let mut args = vec![
            "press",
            input.to_str().unwrap(),
            "-q",
            "50",
            "-o",
            out.to_str().unwrap(),
        ];
        if extra {
            args.push("--jpeg-encoder");
        }
        let status = Command::new(presse)
            .args(&args)
            .status()
            .expect("presse should run");
        assert!(
            status.success(),
            "press --jpeg-encoder must exit successfully"
        );
        assert_well_formed(&out);
    }
    assert!(
        std::fs::metadata(&flag).unwrap().len() < std::fs::metadata(&plain).unwrap().len(),
        "--jpeg-encoder must shrink a photo PDF"
    );
    assert_visual_similarity(&dir, "je-cli", 0.90);
}

/// CCITT Group 4 encoder correctness, pinned through poppler: the same
/// 1-bit mask stored as raw 1-bit `FlateDecode` and as `CCITTFaxDecode`
/// (K = −1) must render *pixel-identically*. Both `/ImageMask` stencils
/// carry the same bits; a single wrong bit pattern in the G4 stream shows
/// up as a render difference.
#[test]
fn ccitt_g4_mask_decodes_identically_to_raw_1bit() {
    let dir = test_dir();
    if !pdftoppm_available() {
        eprintln!("note: pdftoppm not found — skipping the G4 decode gate");
        return;
    }
    let (w, h) = (600u32, 400u32);
    let row_bytes = (w as usize).div_ceil(8);
    let mut mask = vec![0u8; row_bytes * h as usize];
    // Dense black-on-white "text": column bands of short strokes.
    for y in 0..h {
        for x in 0..w {
            if (x / 40) % 3 == 0 && y % 7 < 4 {
                mask[(y as usize) * row_bytes + (x as usize) / 8] |= 1 << (7 - (x % 8));
            }
        }
    }
    let g4 = presse::pdf::fax::encode_g4(&mask, w, h);
    let flate = {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut out = Vec::new();
        let mut e = ZlibEncoder::new(&mut out, flate2::Compression::best());
        e.write_all(&mask).unwrap();
        e.finish().unwrap();
        out
    };

    let build = |name: &str, filter: &str, parms: Option<lopdf::Dictionary>, data: Vec<u8>| {
        let mut d = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 1,
            "Filter" => filter,
            "Decode" => vec![1.into(), 0.into()],
            "Length" => data.len() as i64,
        };
        if let Some(p) = parms {
            d.set("DecodeParms", p);
        }
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0,
            }),
        );
        let img = doc.add_object(Object::Stream(Stream::new(d, data)));
        let content = doc.add_object(Object::Stream(Stream::new(
            dictionary! {},
            Content {
                operations: vec![
                    Operation::new("q", vec![]),
                    Operation::new(
                        "cm",
                        vec![w.into(), 0.into(), 0.into(), h.into(), 0.into(), 0.into()],
                    ),
                    Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
                    Operation::new("Q", vec![]),
                ],
            }
            .encode()
            .unwrap(),
        )));
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
            "Contents" => content,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img } },
        });
        if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
            pages.set("Kids", vec![Object::Reference(page)]);
            pages.set("Count", 1);
        }
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", cat);
        doc.save(dir.join(name)).unwrap();
    };

    build("g4-flate.pdf", "FlateDecode", None, flate);
    build(
        "g4-ccitt.pdf",
        "CCITTFaxDecode",
        Some(dictionary! {
            "K" => -1,
            "BlackIs1" => true,
            "Columns" => w as i64,
            "Rows" => h as i64,
        }),
        g4,
    );

    for name in ["g4-flate", "g4-ccitt"] {
        assert!(
            render_first_page(&dir.join(format!("{name}.pdf")), &dir.join(name)),
            "pdftoppm failed on {name}"
        );
    }
    let a = image::open(dir.join("g4-flate.png")).unwrap().to_luma8();
    let b = image::open(dir.join("g4-ccitt.png")).unwrap().to_luma8();
    assert_eq!(a.dimensions(), b.dimensions());
    let diff = a
        .pixels()
        .zip(b.pixels())
        .filter(|(x, y)| x[0] != y[0])
        .count();
    assert_eq!(
        diff, 0,
        "G4 mask must decode pixel-identically to raw 1-bit ({diff} px differ)"
    );
}

/// `--raster-classify` end-to-end: a bitonal-text RGB page becomes a 1-bit
/// CCITT G4 opaque `DeviceGray` image (far smaller, still renders as
/// black-on-white), while a photographic image on the same run stays JPEG.
#[test]
fn raster_classify_masks_bitonal_text_and_keeps_photos_jpeg() {
    let dir = test_dir();
    // Bitonal text-like RGB raster: black strokes on white, no anti-aliasing.
    let (w, h) = (320u32, 240u32);
    let mut text = vec![255u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            if (x / 24) % 3 == 0 && y % 9 < 5 {
                let p = ((y * w + x) * 3) as usize;
                text[p] = 0;
                text[p + 1] = 0;
                text[p + 2] = 0;
            }
        }
    }
    let photo = photoish_rgb(200, 160);

    let mut doc = new_doc();
    add_image_page(&mut doc.0, doc.1, text.clone(), w, h, false);
    add_image_page(&mut doc.0, doc.1, photo, 200, 160, false);
    compress_images_with(
        &mut doc.0,
        QualityMode::fixed(QUALITY),
        false,
        &CpuTranscoder::default(),
        None,
        false,
        true,
    );
    compress_and_save_pdf(
        &mut doc.0,
        dir.join("classify-post.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("classify-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 2, "both images must survive");
    let mut masks = 0;
    let mut jpegs = 0;
    for (_, content, dict) in &images {
        let is_g4 = dict.get(b"Filter").and_then(|f| f.as_name()).ok()
            == Some(b"CCITTFaxDecode".as_slice());
        if is_g4 {
            masks += 1;
            assert_eq!(
                dict.get(b"BitsPerComponent").and_then(|b| b.as_i64()).ok(),
                Some(1),
                "bitonal text must become a 1-bit CCITT G4 image"
            );
            // An *opaque* DeviceGray image, never an /ImageMask stencil
            // (a stencil's 0 bits are transparent and its ink inherits the
            // current color — not a substitute for an opaque raster).
            assert_eq!(
                dict.get(b"ColorSpace").and_then(|c| c.as_name()).ok(),
                Some(b"DeviceGray".as_slice()),
                "bitonal G4 must be an opaque DeviceGray image"
            );
            assert!(
                dict.get(b"ImageMask").is_err(),
                "bitonal G4 must not be an /ImageMask stencil"
            );
            assert_eq!(
                dict.get(b"Decode").and_then(|d| d.as_array()).ok(),
                Some(&vec![1.into(), 0.into()]),
                "G4 ink (1, BlackIs1) must decode to black, paper to white"
            );
            assert!(content.len() < 4 * 1024, "G4 image must be tiny");
        } else {
            jpegs += 1;
            assert!(is_dct_filter(dict), "photo must stay a DCT stream");
        }
    }
    assert_eq!(masks, 1, "the bitonal page must be masked");
    assert_eq!(jpegs, 1, "the photo must stay JPEG");

    // The masked page renders as black-on-white text: close to the source.
    let (mut pre, pages_id) = new_doc();
    add_image_page(&mut pre, pages_id, text.clone(), w, h, false);
    save_pdf(&mut pre, dir.join("classify-pre.pdf").to_str().unwrap()).unwrap();
    assert_visual_similarity(&dir, "classify", 0.85);
}

/// The mask representation is an *opaque* 1-bit image, not an `/ImageMask`
/// stencil: a stencil paints ink in the current nonstroking color and treats
/// 0 bits as transparent, so dropping it over a colored background would let
/// the background show through the "white" paper and recolor the text. This
/// brutal fixture puts a blue rectangle beneath the source raster and sets a
/// red nonstroking color before `Do` — with a correct opaque replacement the
/// pre/post renders must be *pixel-identical*.
#[test]
fn raster_classify_mask_is_opaque_over_colored_background() {
    let dir = test_dir();
    if !ensure_tool("pdftoppm", pdftoppm_available()) {
        return;
    }
    // Bitonal text-like RGB raster: black strokes on white (even dims so the
    // even-aligned G4 image has identical geometry).
    let (w, h) = (320u32, 240u32);
    let mut text = vec![255u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            if (x / 24) % 3 == 0 && y % 9 < 5 {
                let p = ((y * w + x) * 3) as usize;
                text[p] = 0;
                text[p + 1] = 0;
                text[p + 2] = 0;
            }
        }
    }

    let build = |dir: &std::path::Path, name: &str, compress: bool| {
        let (mut doc, pages_id) = new_doc();
        let mut image_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "BitsPerComponent" => 8,
        };
        image_dict.set("ColorSpace", "DeviceRGB");
        let mut image_stream = Stream::new(image_dict, text.clone());
        image_stream.compress().unwrap();
        let image_id = doc.add_object(image_stream);

        // Blue page, red current nonstroking color, then the raster on top.
        let content = Content {
            operations: vec![
                Operation::new("rg", vec![0.0.into(), 0.0.into(), 1.0.into()]),
                Operation::new("re", vec![0.into(), 0.into(), 612.into(), 792.into()]),
                Operation::new("f", vec![]),
                Operation::new("rg", vec![1.0.into(), 0.0.into(), 0.0.into()]),
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![w.into(), 0.into(), 0.into(), h.into(), 50.into(), 50.into()],
                ),
                Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
        });
        push_kid(&mut doc, pages_id, page_id);
        if compress {
            compress_images_with(
                &mut doc,
                QualityMode::fixed(QUALITY),
                false,
                &CpuTranscoder::default(),
                None,
                false,
                true,
            );
        }
        save_pdf(&mut doc, dir.join(name).to_str().unwrap()).unwrap();
    };

    build(&dir, "opaque-pre.pdf", false);
    build(&dir, "opaque-post.pdf", true);

    // The compressed image must be the opaque DeviceGray representation.
    let loaded = assert_well_formed(&dir.join("opaque-post.pdf"));
    let images = find_image_streams(&loaded);
    assert_eq!(images.len(), 1);
    let (_, _, dict) = &images[0];
    assert_eq!(
        dict.get(b"Filter").and_then(|f| f.as_name()).ok(),
        Some(b"CCITTFaxDecode".as_slice())
    );
    assert_eq!(
        dict.get(b"ColorSpace").and_then(|c| c.as_name()).ok(),
        Some(b"DeviceGray".as_slice()),
        "bitonal G4 must be an opaque DeviceGray image"
    );
    assert!(
        dict.get(b"ImageMask").is_err(),
        "bitonal G4 must not be an /ImageMask stencil"
    );

    // Pixel-identical: the white paper must stay opaque (covering the blue
    // rectangle) and the ink must stay black (ignoring the red color).
    let (pre, post) = (dir.join("opaque-pre"), dir.join("opaque-post"));
    assert!(
        render_first_page(&dir.join("opaque-pre.pdf"), &pre),
        "pdftoppm failed on opaque-pre.pdf"
    );
    assert!(
        render_first_page(&dir.join("opaque-post.pdf"), &post),
        "pdftoppm failed on opaque-post.pdf"
    );
    let a = image::open(pre.with_extension("png")).unwrap().to_luma8();
    let b = image::open(post.with_extension("png")).unwrap().to_luma8();
    assert_eq!(a.dimensions(), b.dimensions());
    let diff = a
        .pixels()
        .zip(b.pixels())
        .filter(|(x, y)| x[0] != y[0])
        .count();
    assert_eq!(
        diff, 0,
        "opaque G4 image must render pixel-identically to the source raster          ({diff} px differ; an /ImageMask stencil would show the blue through          the white and paint the ink red)"
    );
}

/// The linear renumberer must keep lopdf's `max_id` invariant. lopdf's own
/// `renumber_objects_with` ends with `max_id = new_id - 1`, and its writer
/// sizes the xref and allocates the object-stream / cross-reference-stream
/// ids from `max_id` — a stale value (left behind by deleted objects) would
/// bloat or, worse, *misalign* the xref: a stale-low `max_id` drops objects
/// past it from the xref, the exact corruption this PR eliminates. Both the
/// contiguous early-return path and the compaction path must repair it.
#[test]
fn renumber_repairs_stale_max_id() {
    let dir = test_dir();

    let (mut doc, pages_id) = new_doc();
    for page in 0..30 {
        add_text_page(&mut doc, pages_id, page);
    }
    // Three orphaned objects; the middle one is deleted below to open a gap
    // without touching the page tree.
    for _ in 0..3 {
        doc.add_object(Object::String(
            b"orphan".to_vec(),
            lopdf::StringFormat::Literal,
        ));
    }
    let n = doc.objects.len() as u32;
    assert!(n < 5000, "test document must stay small");
    doc.max_id = 5000; // stale historical max_id (deleted objects)

    // Contiguous ids 1..=n: the early-return path must still repair max_id.
    renumber_objects(&mut doc);
    assert_eq!(
        doc.max_id, n,
        "early-return path must repair max_id to the largest object id"
    );

    // Open a middle gap (delete the second orphan) → compaction path.
    let gap_id = doc.objects.keys().copied().nth(n as usize - 2).unwrap();
    doc.delete_object(gap_id);
    doc.max_id = 5000; // stale again
    renumber_objects(&mut doc);
    let compact = doc.objects.len() as u32;
    assert_eq!(
        doc.max_id, compact,
        "compaction path must repair max_id to the largest object id"
    );
    assert!(
        (1..=compact).all(|id| doc.objects.contains_key(&(id, 0))),
        "renumber must produce contiguous 1..=n ids"
    );

    // Save with object streams (writer allocates stream ids from max_id).
    compress_and_save_pdf(
        &mut doc,
        dir.join("renum-maxid.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let loaded = assert_well_formed(&dir.join("renum-maxid.pdf"));
    // Every original object must appear exactly once in the xref.
    for id in 1..=compact {
        let count = loaded
            .objects
            .keys()
            .filter(|(i, g)| *i == id && *g == 0)
            .count();
        assert_eq!(count, 1, "object {id} must appear exactly once in the xref");
    }
    // /Size must be exactly one past the largest written object id.
    let max_loaded = loaded.objects.keys().map(|(id, _)| *id).max().unwrap();
    let size = loaded
        .trailer
        .get(b"Size")
        .and_then(|s| s.as_i64())
        .ok()
        .unwrap();
    assert_eq!(
        size,
        max_loaded as i64 + 1,
        "/Size ({size}) must equal the largest written object id + 1 ({max_loaded})"
    );
    assert_eq!(
        loaded.get_pages().len(),
        30,
        "all pages must survive renumber + save with object streams"
    );
}

/// `--recompress-flate` end-to-end: a document whose Flate streams sit at a
/// low compression level shrinks (qpdf's structural trick) and stays valid.
#[test]
fn recompress_flate_flag_recompresses_existing_flate_streams() {
    let dir = test_dir();

    // A content stream compressed at level 1 (the "form tool" case).
    let (mut doc, pages_id) = new_doc();
    let content_bytes =
        b"q 0 0 612 792 re W n BT /F1 12 Tf 72 720 Td (Recompress me.) Tj ET Q".repeat(200);
    let level1 = {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut out = Vec::new();
        let mut e = ZlibEncoder::new(&mut out, flate2::Compression::new(1));
        e.write_all(&content_bytes).unwrap();
        e.finish().unwrap();
        out
    };
    let content_stream = Stream::new(dictionary! { "Filter" => "FlateDecode" }, level1);
    let content_id = doc.add_object(Object::Stream(content_stream));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content_id,
        "Resources" => dictionary! {},
    });
    push_kid(&mut doc, pages_id, page_id);

    // Baseline save (no flag): stream kept as level-1.
    let mut plain = Document::load_mem(&{
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    })
    .unwrap();
    compress_and_save_pdf(
        &mut plain,
        dir.join("refl-plain.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    // Flagged save: stream recompressed at level 9.
    let mut flagged = Document::load_mem(&{
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    })
    .unwrap();
    let n = recompress_flate(&mut flagged);
    assert_eq!(n, 1, "exactly the content stream must be recompressed");
    compress_and_save_pdf(
        &mut flagged,
        dir.join("refl-flag.pdf").to_str().unwrap(),
        false,
    )
    .unwrap();

    let (a, b) = (
        std::fs::metadata(dir.join("refl-plain.pdf")).unwrap().len(),
        std::fs::metadata(dir.join("refl-flag.pdf")).unwrap().len(),
    );
    assert!(
        b < a,
        "--recompress-flate must shrink level-1 streams: {b} vs {a}"
    );
    let loaded = assert_well_formed(&dir.join("refl-flag.pdf"));
    assert_eq!(loaded.get_pages().len(), 1);
}
