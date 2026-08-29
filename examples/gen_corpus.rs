//! Generate a standardized benchmark corpus for `benches/docker/bench.sh`.
//!
//! Not part of the shipped CLI — built on demand inside the benchmark image
//! with `cargo build --release --example gen_corpus`.
//!
//! Usage: `gen_corpus <output-directory>`

use std::error::Error;
use std::path::Path;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

fn main() -> Result<(), Box<dyn Error>> {
    let out = std::env::args()
        .nth(1)
        .ok_or("usage: gen_corpus <output-directory>")?;
    let out = Path::new(&out);
    std::fs::create_dir_all(out)?;

    gen_text_heavy(out)?;
    gen_image_heavy(out)?;
    gen_scanned(out)?;

    println!("corpus written to {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Synthetic image generators (deterministic, dependency-free)
// ---------------------------------------------------------------------------

/// Photo-like RGB: smooth gradient + structure + light grain.
fn photoish_rgb(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = (
                (x as f32 / w as f32 * 255.0) as u8,
                (y as f32 / h as f32 * 255.0) as u8,
                (128.0 + 80.0 * ((x as f32 + y as f32) / 32.0).sin()) as u8,
            );
            let n = (next() & 0x1f) as u8; // light grain
            v.extend_from_slice(&[r.wrapping_add(n), g.wrapping_add(n), b.wrapping_add(n)]);
        }
    }
    v
}

/// Scanner-like grayscale: near-white paper + grain + darker scan lines.
fn grainy_gray(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..w * h)
        .enumerate()
        .map(|(i, _)| {
            let line = if (i / w as usize).is_multiple_of(17) {
                24
            } else {
                0
            };
            (245u8.wrapping_sub(line)).wrapping_sub((next() & 0x0f) as u8)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Document builders
// ---------------------------------------------------------------------------

fn add_text_page(
    doc: &mut Document,
    pages_id: lopdf::ObjectId,
    page_no: u32,
) -> Result<(), Box<dyn Error>> {
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), 12.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
            Operation::new("Tj", vec![Object::String(format!("Page {page_no} — text-heavy benchmark corpus").into_bytes(), lopdf::StringFormat::Literal)]),
            Operation::new("Td", vec![0.into(), (-16).into()]),
            Operation::new("Tj", vec![Object::String(b"The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs.".to_vec(), lopdf::StringFormat::Literal)]),
            Operation::new("Td", vec![0.into(), (-16).into()]),
            Operation::new("Tj", vec![Object::String(b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.".to_vec(), lopdf::StringFormat::Literal)]),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        }}},
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        let mut kids: Vec<Object> = pages
            .get(b"Kids")
            .and_then(|k| k.as_array().cloned())
            .unwrap_or_default();
        kids.push(Object::Reference(page_id));
        let count = kids.len() as u32;
        pages.set("Kids", kids);
        pages.set("Count", count);
    }
    Ok(())
}

fn add_image_page(
    doc: &mut Document,
    pages_id: lopdf::ObjectId,
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    gray: bool,
) -> Result<(), Box<dyn Error>> {
    let mut image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => w as i64,
        "Height" => h as i64,
        "BitsPerComponent" => 8,
    };
    image_dict.set("ColorSpace", if gray { "DeviceGray" } else { "DeviceRGB" });
    let mut image_stream = Stream::new(image_dict, pixels);
    image_stream.compress()?; // FlateDecode → `presse press` re-encodes these
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
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        let mut kids: Vec<Object> = pages
            .get(b"Kids")
            .and_then(|k| k.as_array().cloned())
            .unwrap_or_default();
        kids.push(Object::Reference(page_id));
        let count = kids.len() as u32;
        pages.set("Kids", kids);
        pages.set("Count", count);
    }
    Ok(())
}

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

fn finish(doc: &mut Document, dir: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    doc.compress();
    doc.save(dir.join(name))?;
    Ok(())
}

fn gen_text_heavy(out: &Path) -> Result<(), Box<dyn Error>> {
    let (mut doc, pages_id) = new_doc();
    for page in 1..=10 {
        add_text_page(&mut doc, pages_id, page)?;
    }
    finish(&mut doc, out, "text-heavy.pdf")
}

fn gen_image_heavy(out: &Path) -> Result<(), Box<dyn Error>> {
    let (mut doc, pages_id) = new_doc();
    for i in 0..24 {
        add_image_page(
            &mut doc,
            pages_id,
            photoish_rgb(512, 512, 1000 + i),
            512,
            512,
            false,
        )?;
    }
    finish(&mut doc, out, "image-heavy.pdf")
}

fn gen_scanned(out: &Path) -> Result<(), Box<dyn Error>> {
    let (mut doc, pages_id) = new_doc();
    for i in 0..3 {
        add_image_page(
            &mut doc,
            pages_id,
            grainy_gray(1600, 1200, 2000 + i),
            1600,
            1200,
            true,
        )?;
    }
    finish(&mut doc, out, "scanned.pdf")
}
