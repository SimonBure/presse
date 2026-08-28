use lopdf::{Document, Object, ObjectId, SaveOptions};
use std::collections::HashMap;
use std::fs::File;

use crate::pdf::images::zlib_encode;

/// Structural recompression (`--recompress-flate`).
///
/// **Design rationale.** Form tools and older writers store `FlateDecode`
/// streams at a lower compression level than this writer uses (~6 vs our
/// level 9), so decoding and re-encoding the *same bytes* shrinks the file
/// without touching a single content byte — qpdf's `--recompress-flate`
/// trick, recovered here at the writer level (on the irs_fw2 scan corpus:
/// qpdf's 1.81 → 1.34 MB). Only pure-Flate streams (a `FlateDecode` name or
/// a single-element `[FlateDecode]` array) with no `/DecodeParms` are
/// touched — DCT/LZW/multi-filter chains and predictor-parameterized
/// streams are left alone, as is anything that fails to decompress — and,
/// like every candidate in this codebase, a re-encoded stream is kept only
/// when it is strictly smaller, so the pass is idempotent and can never
/// grow a file. Returns the number of streams recompressed.
///
/// Losslessness is structural: Flate decode + re-encode is bit-exact on
/// the decoded bytes, so pixels, text, metadata and fonts are unchanged;
/// only the compressed representation differs.
pub fn recompress_flate(doc: &mut Document) -> usize {
    let mut recompressed = 0;
    for obj in doc.objects.values_mut() {
        let Object::Stream(stream) = obj else {
            continue;
        };
        // Pure FlateDecode only, and no predictor parameters: re-encoding
        // without them would change the decoded output.
        let is_flate = match stream.dict.get(b"Filter") {
            Ok(Object::Name(n)) => n == b"FlateDecode",
            Ok(Object::Array(a)) => {
                a.len() == 1 && a[0].as_name().is_ok_and(|n| n == b"FlateDecode")
            }
            _ => false,
        };
        if !is_flate || stream.dict.get(b"DecodeParms").is_ok() {
            continue;
        }
        let Ok(decoded) = stream.decompressed_content() else {
            continue;
        };
        let re = zlib_encode(&decoded);
        if re.len() < stream.content.len() {
            stream.content = re;
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            recompressed += 1;
        }
    }
    recompressed
}

/// Compact object ids to 1..=n and rewrite every reference, in linear time.
///
/// Replaces lopdf's `renumber_objects` in the writer paths. lopdf's version
/// walks the reachable graph and tracks visited ids with a `Vec::contains`
/// per reference — O(n²) on object-heavy documents, where it dominates the
/// save (460 ms of a 530 ms save on a 67k-object document). This pass
/// rewrites references in *every* object (reachable or not, so no stale id
/// can survive) in one pass against a hash map, rewrites the trailer's
/// references too, and early-returns when the ids are already contiguous
/// (the common case for freshly built or previously renumbered documents).
///
/// Invariants kept with lopdf's own renumberer:
/// - **`max_id`** is repaired to the largest surviving object id in *both*
///   paths (contiguous ids can still leave a stale high `max_id` behind
///   from deleted objects). lopdf's writer sizes its xref and allocates
///   object-stream / cross-reference-stream ids from `max_id`; a stale low
///   value would collide with existing ids, a stale high one inflate
///   `/Size`.
/// - **bookmarks** are re-pointed when object ids move, matching lopdf's
///   `renumber_bookmarks` (the `Document::bookmarks`/`bookmark_table` the
///   `merge` path populates and `build_outline` reads). Outline *objects*
///   in the tree are rewritten like any other reference; the table keeps
///   `build_outline`'s later reads consistent.
///
/// Page order is untouched: lopdf's variant may reorder the page tree, but
/// the writer must never change document semantics.
pub fn renumber_objects(doc: &mut Document) {
    let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    ids.sort_unstable();
    if ids
        .iter()
        .enumerate()
        .all(|(i, &(n, _))| n as usize == i + 1)
    {
        // Already contiguous — but a stale historical `max_id` (from
        // deleted objects) must still be repaired.
        doc.max_id = ids.last().map(|&(n, _)| n).unwrap_or(0);
        return;
    }

    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(ids.len());
    for (i, &old) in ids.iter().enumerate() {
        replace.insert(old, (i as u32 + 1, old.1));
    }

    fn rewrite(obj: &mut Object, replace: &HashMap<ObjectId, ObjectId>) {
        match obj {
            Object::Reference(id) => {
                if let Some(new) = replace.get(id) {
                    *id = *new;
                }
            }
            Object::Array(items) => {
                for item in items.iter_mut() {
                    rewrite(item, replace);
                }
            }
            Object::Dictionary(dict) => {
                for (_, v) in dict.iter_mut() {
                    rewrite(v, replace);
                }
            }
            Object::Stream(stream) => {
                for (_, v) in stream.dict.iter_mut() {
                    rewrite(v, replace);
                }
            }
            _ => {}
        }
    }

    for obj in doc.objects.values_mut() {
        rewrite(obj, &replace);
    }
    for (_, v) in doc.trailer.iter_mut() {
        rewrite(v, &replace);
    }

    // Keep the outline/bookmark bookkeeping consistent when ids move (the
    // `merge` path populates `bookmarks`/`bookmark_table` and `build_outline`
    // reads them after renumbering).
    for (&old, &new) in &replace {
        if old != new {
            doc.renumber_bookmarks(&old, &new);
        }
    }

    let mut objects = std::collections::BTreeMap::new();
    for (old, new) in &replace {
        if let Some(obj) = doc.objects.remove(old) {
            objects.insert(*new, obj);
        }
    }
    doc.objects = objects;
    doc.max_id = doc.objects.keys().map(|(id, _)| *id).max().unwrap_or(0);
}

pub fn compress_and_save_pdf(
    doc: &mut Document,
    name: &str,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let option: SaveOptions = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(200)
        .compression_level(9)
        .build();

    verbose!(
        verbose,
        "[writer] {} objects before cleanup",
        doc.objects.len()
    );
    Document::delete_zero_length_streams(doc);
    verbose!(
        verbose,
        "[writer] {} objects after cleanup",
        doc.objects.len()
    );

    // Real-world PDFs usually carry gaps in their object numbering (deleted
    // objects, preserved source ids). lopdf's xref writer opens a new xref
    // section at the first *missing* id but appends the next present object's
    // entry to it, shifting every subsequent entry whenever a gap exists — a
    // corruption qpdf rejects and poppler can render as blank pages. Renumber
    // to contiguous ids first so both the xref-table and xref-stream writers
    // stay correct (see [`renumber_objects`] for the linear-time pass).
    renumber_objects(doc);

    doc.compress();

    verbose!(verbose, "[writer] saving to '{}'", name);
    let mut file = File::create(name)?;
    doc.save_with_options(&mut file, option)
        .map_err(|e| format!("save_with_options failed for '{}': {}", name, e))?;

    Ok(())
}

pub fn save_pdf(doc: &mut Document, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Document::delete_zero_length_streams(doc);
    // See compress_and_save_pdf: lopdf's xref writers mis-handle gaps in
    // object numbering, so compact the ids before saving.
    renumber_objects(doc);
    doc.compress();
    doc.save(name)?;
    Ok(())
}
