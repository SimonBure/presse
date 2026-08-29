//! Image placement scan: where each image XObject is drawn, in points.
//!
//! `--dpi` downsampling needs each image's effective resolution, which is
//! pixel size ÷ placed size. The pixel size comes from the image dict; the
//! placed size (points) comes from the page content stream's transform
//! matrix at the `Do` that draws the image.
//!
//! The scan is a deliberately small content interpreter: `q`/`Q` save and
//! restore the transform, `cm` concatenates the linear part of a transform
//! matrix, and `Do` of an image XObject records the bounding box of its
//! unit square under the current transform — `|a| + |b|` by `|c| + |d|`,
//! which is exact for axis-aligned placements and an over-estimate that
//! errs toward *less* downsampling for rotated ones. Form XObjects are
//! followed recursively with the composing transform, so images nested
//! inside forms are found too.
//!
//! Anything that cannot be parsed (a malformed content stream, an
//! unresolvable XObject, an orphan image that is never drawn) simply yields
//! no entry; the caller leaves such images at source resolution rather than
//! guessing a placement.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};
use std::collections::HashMap;

/// Largest placed size (width, height in points) per image object id.
pub(crate) type Placements = HashMap<ObjectId, (f64, f64)>;

/// Maximum form-XObject nesting depth before the scan gives up. Real
/// documents nest a handful of levels; the cap only guards against cycles.
const MAX_DEPTH: usize = 8;

/// Linear part of the current transform: (a, b, c, d) of `[a b 0; c d 0]`.
type Ctm = (f64, f64, f64, f64);

/// Scan every page (and, recursively, every drawn form) for image
/// placements. The recorded size per image is the *largest* it is drawn at
/// anywhere in the document, so a shared image is downsampled for its most
/// demanding placement, never for its smallest.
pub(crate) fn image_placements(doc: &Document) -> Placements {
    let mut out = Placements::new();
    for (_, page_id) in doc.get_pages() {
        let resources = page_resources(doc, page_id);
        if let Ok(content) = doc.get_and_decode_page_content(page_id) {
            scan_operations(
                doc,
                &content.operations,
                &resources,
                (1.0, 0.0, 0.0, 1.0),
                0,
                &mut out,
            );
        }
    }
    out
}

fn scan_operations(
    doc: &Document,
    ops: &[Operation],
    resources: &lopdf::Dictionary,
    initial_ctm: Ctm,
    depth: usize,
    out: &mut Placements,
) {
    let mut stack: Vec<Ctm> = vec![initial_ctm];
    for op in ops {
        match op.operator.as_str() {
            "q" => {
                if let Some(&ctm) = stack.last() {
                    stack.push(ctm);
                }
            }
            "Q" => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            "cm" => {
                if let (Some(&(a, b, c, d)), Some((e, f, g, h, _, _))) =
                    (stack.last(), cm_operands(&op.operands))
                {
                    // CTM ← CTM × M; only the linear part matters for the
                    // bounding box of the image's unit square.
                    let next = (a * e + b * g, a * f + b * h, c * e + d * g, c * f + d * h);
                    stack.pop();
                    stack.push(next);
                }
            }
            "Do" => {
                let Some(&(a, b, c, d)) = stack.last() else {
                    continue;
                };
                let Some(Object::Name(name)) = op.operands.first() else {
                    continue;
                };
                let Some(xobj_id) = lookup_xobject(resources, name) else {
                    continue;
                };
                match xobject_subtype(doc, xobj_id).as_deref() {
                    Some(b"Image") => {
                        let entry = out.entry(xobj_id).or_insert((0.0, 0.0));
                        entry.0 = entry.0.max(a.abs() + b.abs());
                        entry.1 = entry.1.max(c.abs() + d.abs());
                    }
                    Some(b"Form") if depth < MAX_DEPTH => {
                        if let Some((form_resources, form_ops)) = form_content(doc, xobj_id) {
                            scan_operations(
                                doc,
                                &form_ops,
                                &form_resources,
                                (a, b, c, d),
                                depth + 1,
                                out,
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn cm_operands(operands: &[Object]) -> Option<(f64, f64, f64, f64, f64, f64)> {
    if operands.len() != 6 {
        return None;
    }
    let f = |o: &Object| o.as_float().ok().map(f64::from);
    Some((
        f(&operands[0])?,
        f(&operands[1])?,
        f(&operands[2])?,
        f(&operands[3])?,
        f(&operands[4])?,
        f(&operands[5])?,
    ))
}

/// The `/XObject` id a `Do` name resolves to through the given resources.
fn lookup_xobject(resources: &lopdf::Dictionary, name: &[u8]) -> Option<ObjectId> {
    let xobjects = resources.get(b"XObject").ok()?.as_dict().ok()?;
    match xobjects.get(name).ok()? {
        Object::Reference(id) => Some(*id),
        _ => None, // direct (non-referenced) objects have no stable id
    }
}

fn xobject_subtype(doc: &Document, id: ObjectId) -> Option<Vec<u8>> {
    let Object::Stream(stream) = doc.get_object(id).ok()? else {
        return None;
    };
    stream
        .dict
        .get(b"Subtype")
        .ok()?
        .as_name()
        .ok()
        .map(<[u8]>::to_vec)
}

/// A form XObject's resources and decoded content operations.
fn form_content(doc: &Document, id: ObjectId) -> Option<(lopdf::Dictionary, Vec<Operation>)> {
    let Object::Stream(stream) = doc.get_object(id).ok()? else {
        return None;
    };
    let data = stream.decompressed_content().ok()?;
    let content = Content::decode(&data).ok()?;
    let resources = stream
        .dict
        .get(b"Resources")
        .ok()
        .and_then(|r| r.as_dict().ok())
        .cloned()
        .unwrap_or_default();
    Some((resources, content.operations))
}

fn page_resources(doc: &Document, page_id: ObjectId) -> lopdf::Dictionary {
    doc.get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Resources").ok())
        .and_then(|r| r.as_dict().ok())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::image_placements;
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    /// One page drawing a single image at the given placement (points).
    fn placed_image_page(w: u32, h: u32, placed_w: f64, placed_h: f64) -> Document {
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

        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => w as i64,
                "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            vec![0u8; (w * h * 3) as usize],
        ));
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        placed_w.into(),
                        0.into(),
                        0.into(),
                        placed_h.into(),
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
            "MediaBox" => vec![0.into(), 0.into(), placed_w.into(), placed_h.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
        });
        let pages = doc
            .objects
            .get_mut(&pages_id)
            .unwrap()
            .as_dict_mut()
            .unwrap();
        pages.set("Kids", vec![Object::Reference(page_id)]);
        pages.set("Count", 1);
        doc
    }

    #[test]
    fn records_axis_aligned_placement() {
        let doc = placed_image_page(300, 200, 100.0, 50.0);
        let placements = image_placements(&doc);
        assert_eq!(placements.len(), 1);
        let (w, h) = placements.values().next().unwrap();
        assert!((w - 100.0).abs() < 1e-6, "placed width {w}");
        assert!((h - 50.0).abs() < 1e-6, "placed height {h}");
    }

    #[test]
    fn keeps_largest_of_multiple_placements() {
        // Draw the same image twice: small (50 pt) and large (200 pt). The
        // recorded placement must be the large one.
        let mut doc = placed_image_page(300, 200, 200.0, 100.0);
        // append a second, smaller placement to the same content stream
        let page = *doc.get_pages().values().next().unwrap();
        let contents = doc
            .get_object(page)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let stream = doc
            .objects
            .get_mut(&contents)
            .unwrap()
            .as_stream_mut()
            .unwrap();
        let mut content = stream.content.clone();
        content.extend_from_slice(b"\nq 50 0 0 25 0 0 cm /Im0 Do Q");
        stream.content = content;
        let placements = image_placements(&doc);
        let (w, h) = placements.values().next().unwrap();
        assert!((w - 200.0).abs() < 1e-6, "placed width {w}");
        assert!((h - 100.0).abs() < 1e-6, "placed height {h}");
    }

    #[test]
    fn rotated_placement_is_bounding_box() {
        // 90° rotation: [0 -1 1 0] — bbox width |0|+|-1| = 1, height |1|+|0| = 1.
        let mut doc = placed_image_page(300, 200, 100.0, 50.0);
        let page = *doc.get_pages().values().next().unwrap();
        let contents = doc
            .get_object(page)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let stream = doc
            .objects
            .get_mut(&contents)
            .unwrap()
            .as_stream_mut()
            .unwrap();
        stream.content = b"q 0 -100 100 0 0 0 cm /Im0 Do Q".to_vec();
        let placements = image_placements(&doc);
        let (w, h) = placements.values().next().unwrap();
        assert!((w - 100.0).abs() < 1e-6, "bbox width {w}");
        assert!((h - 100.0).abs() < 1e-6, "bbox height {h}");
    }
}
