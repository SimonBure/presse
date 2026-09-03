//! Structural optimization passes behind the `optimize` feature
//! (default-off CLI flags: `--dedup`, `--zopfli`, `--font-subset`, `--mrc`,
//! `--jbig2`, `--jpeg2000`, and the `--compression` presets).
//!
//! # Design rationale
//!
//! Everything here follows the same candidate/court discipline as the image
//! pipeline: a representation is applied only when it is *strictly smaller*
//! (or, for font subsetting, provably rendering-equivalent AND smaller), is
//! lossless where claimed, and degrades to a no-op on anything it cannot
//! prove safe. The passes are deliberately independent of the image pipeline
//! and of each other, so each flag can be used alone or combined.
//!
//! - **`--dedup`** extends the duplicate-image coalescing pattern to *every*
//!   stream in the document: fonts (`FontFile*`), ICC profiles, XForms,
//!   patterns/shadings, and arbitrary identical streams. Two streams are one
//!   when their canonical dictionaries and payloads are byte-identical
//!   (canonical = reference-following, `/Length` and the cosmetic `/Name`
//!   hint ignored, exactly like the image coalescer); the first object stays
//!   and every reference is rewritten. Rendering cannot change: identical
//!   stream + identical dictionary renders identically, and PDF allows any
//!   number of references to one object. The graph rewrite is the same
//!   reference-rewriting machinery the image coalescer already ships.
//!
//! - **`--zopfli`** upgrades Flate recompression from the writer's level-9
//!   deflate to Zopfli — the same standards-compatible zlib streams, but a
//!   deliberately CPU-hungry search that routinely finds smaller encodings
//!   (a few % on text/content streams, more on compressible binary). It only
//!   ever *replaces* a stream when the Zopfli result is strictly smaller, so
//!   it is a pure size win at CPU cost — exactly the trade the
//!   `--compression smallest` preset is for.

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId};
#[cfg(feature = "optimize")]
use lopdf::{Stream, dictionary};

use crate::pdf::images::{canonical_object, rewrite_references};

/// Coalesce byte-identical non-image streams (fonts, ICC profiles, XForms,
/// patterns, arbitrary streams) onto one canonical object, rewriting every
/// reference. Returns the number of duplicate objects removed.
///
/// Images are handled by the image pipeline's own coalescer; running both is
/// idempotent (the image pass already collapsed them).
pub fn dedup_streams(doc: &mut Document) -> usize {
    let objects = &doc.objects;
    let mut groups: HashMap<(Vec<u8>, Vec<u8>), Vec<ObjectId>> = HashMap::new();
    for (id, obj) in objects.iter() {
        let Object::Stream(s) = obj else {
            continue;
        };
        if crate::pdf::images::is_image_stream(s) {
            continue; // the image coalescer owns images
        }
        let mut dict = s.dict.clone();
        dict.remove(b"Length");
        dict.remove(b"Name");
        let mut canon = Vec::new();
        let mut visited = HashSet::new();
        canonical_object(&Object::Dictionary(dict), &mut canon, objects, &mut visited);
        groups
            .entry((canon, s.content.clone()))
            .or_default()
            .push(*id);
    }

    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut removed = 0;
    for group in groups.values() {
        if group.len() < 2 {
            continue;
        }
        let canonical = *group.iter().min().expect("non-empty group");
        for &dup in group.iter().filter(|&&d| d != canonical) {
            replace.insert(dup, canonical);
        }
        removed += group.len() - 1;
    }
    if replace.is_empty() {
        return 0;
    }

    for obj in doc.objects.values_mut() {
        rewrite_references(obj, &replace);
    }
    for (_, v) in doc.trailer.iter_mut() {
        rewrite_references(v, &replace);
    }
    for dup in replace.keys() {
        doc.objects.remove(dup);
    }
    removed
}

/// Recompress existing pure-Flate streams with Zopfli (`--zopfli`), keeping
/// each only when strictly smaller. Mirrors [`crate::pdf::writer::recompress_flate`]
/// exactly (same eligibility walk) but swaps the encoder: the Zopfli search
/// is slower and finds smaller zlib streams for the same decoded bytes.
/// Returns the number of streams recompressed.
#[cfg(feature = "optimize")]
pub fn recompress_flate_zopfli(doc: &mut Document) -> usize {
    let mut recompressed = 0;
    for obj in doc.objects.values_mut() {
        let Object::Stream(stream) = obj else {
            continue;
        };
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
        let zopfli = zopfli_encode(&decoded);
        if zopfli.len() < stream.content.len() {
            stream.content = zopfli;
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            recompressed += 1;
        }
    }
    recompressed
}

/// Zopfli-zlib encode (the same format FlateDecode expects).
#[cfg(feature = "optimize")]
pub(crate) fn zopfli_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    zopfli::compress(
        zopfli::Options::default(),
        zopfli::Format::Zlib,
        data,
        &mut out,
    )
    .expect("zopfli compression of in-memory data cannot fail");
    out
}

/// Stub for builds without the `optimize` feature: `main` guards the flag
/// before reaching here, so this is only to keep one call site.
#[cfg(not(feature = "optimize"))]
pub fn recompress_flate_zopfli(_doc: &mut Document) -> usize {
    0
}

/// Stub for builds without the `optimize` feature: `main` guards the flag
/// before reaching here, so this is only to keep one call site.
#[cfg(not(feature = "optimize"))]
pub fn subset_fonts(_doc: &mut Document) -> usize {
    0
}

/// Subset every embedded TrueType (FontFile2) font to the glyphs the content
/// streams actually show (`--font-subset`), via typst's `subsetter`. CFF
/// (FontFile3) programs are deliberately skipped — see the installer note.
/// Returns the number of fonts subset.
///
/// # Design rationale
///
/// The pass is deliberately conservative — a font is left untouched unless
/// *every* condition holds:
///
/// - the font is a simple (non-CID, non-Type3) TrueType font with an
///   embedded `FontFile2` program (CFF `FontFile3` programs are skipped);
/// - every content stream that shows text with it parsed cleanly, every text
///   op was attributable to a `Tf`, and the font is not used from any
///   unparseable or non-scanned context (unattributable usage blocks the
///   font: the rewrite must never change glyph selection anywhere);
/// - every used character code resolves through the font's encoding to a GID
///   (WinAnsiEncoding / built-in cmap / `/Differences` glyph names via the
///   font's `post` table);
/// - the subset is strictly smaller than the embedded font program.
///
/// When all that holds, the font is rewritten as the CID-keyed form
/// `subsetter` requires: a `Type0` font with `/Encoding /Identity-H`, a
/// `CIDFontType2` descendant with an identity `CIDToGIDMap`, the content
/// strings re-mapped from single-byte codes to the two-byte remapped GIDs
/// (as CIDs), and a rebuilt `/ToUnicode` for the used codes. Text
/// positioning is unaffected: the string *length* in characters never
/// changes, only the byte width per character.
#[cfg(feature = "optimize")]
pub fn subset_fonts(doc: &mut Document) -> usize {
    // ---- Pass 1: collect usage ------------------------------------------
    let mut usage: HashMap<ObjectId, FontUsage> = HashMap::new();
    let mut stream_ops: HashMap<ObjectId, Vec<TextOp>> = HashMap::new();
    let mut blocked: HashSet<ObjectId> = HashSet::new();
    let mut visited: HashSet<ObjectId> = HashSet::new();
    for (_, page_id) in doc.get_pages() {
        let resources = page_resources(doc, page_id);
        for content_id in page_contents(doc, page_id) {
            scan_text_usage(
                doc,
                content_id,
                &resources,
                0,
                true,
                &mut visited,
                &mut usage,
                &mut stream_ops,
                &mut blocked,
            );
        }
        // Annotation appearance streams draw text with the same fonts; a
        // font used there must keep its full glyph set.
        for ap_id in appearance_streams(doc, page_id) {
            scan_text_usage(
                doc,
                ap_id,
                &resources,
                0,
                true,
                &mut visited,
                &mut usage,
                &mut stream_ops,
                &mut blocked,
            );
        }
    }

    // ---- Pass 2: resolve each used font to a subset plan ----------------
    let mut plans: Vec<FontPlan> = Vec::new();
    for (font_id, font_usage) in &usage {
        if blocked.contains(font_id) || font_usage.used_codes.is_empty() {
            continue;
        }
        if let Some(plan) = build_font_plan(doc, *font_id, font_usage) {
            plans.push(plan);
        }
    }
    if plans.is_empty() {
        return 0;
    }

    // ---- Pass 3: rewrite the content streams (codes → CIDs) ------------
    rewrite_text_streams(doc, &stream_ops, &plans);

    // ---- Pass 4: replace the font objects -------------------------------
    let mut subset = 0;
    for plan in &plans {
        if install_font_plan(doc, plan) {
            subset += 1;
        }
    }
    subset
}

/// Per-font usage gathered from the content scan: the set of single-byte
/// character codes shown (the code→width table is derived from the font
/// dict's `/Widths` at plan time, not recorded here).
#[cfg(feature = "optimize")]
#[derive(Default)]
struct FontUsage {
    used_codes: std::collections::BTreeSet<u8>,
}

/// One text-showing op in one content stream: the op index (rewritten in
/// place, so indices stay stable) and the font object id in effect.
#[cfg(feature = "optimize")]
struct TextOp {
    op_index: usize,
    font_id: ObjectId,
}

/// A resolved, strictly-smaller subset plan for one font.
#[cfg(feature = "optimize")]
struct FontPlan {
    font_id: ObjectId,
    /// Original character code → new CID (remapped GID).
    code_to_cid: HashMap<u8, u16>,
    /// Original character code → Unicode (for the rebuilt `/ToUnicode`).
    code_to_unicode: HashMap<u8, u16>,
    /// New CID → glyph width, for the `/W` array.
    cid_to_width: std::collections::BTreeMap<u16, f32>,
    /// Subset-tagged `/BaseFont` name.
    base_font: Vec<u8>,
    /// The subset font program.
    font_file: Vec<u8>,
    /// Original descriptor / font program / ToUnicode ids (removed once
    /// unreferenced).
    old_ids: Vec<ObjectId>,
    /// Original descriptor dict (reused, with the new font program).
    descriptor: Option<lopdf::Dictionary>,
}

/// Scan one content stream for text-showing ops. `inherits` is true when
/// the stream may fall back to the *parent's* resources; such streams make
/// their fonts unresolvable-at-rewrite-time and are handled conservatively
/// (see the module doc of [`subset_fonts`]).
#[cfg(feature = "optimize")]
#[allow(clippy::too_many_arguments)]
fn scan_text_usage(
    doc: &Document,
    content_id: ObjectId,
    inherited: &lopdf::Dictionary,
    depth: usize,
    allow_inherit: bool,
    visited: &mut HashSet<ObjectId>,
    usage: &mut HashMap<ObjectId, FontUsage>,
    stream_ops: &mut HashMap<ObjectId, Vec<TextOp>>,
    blocked: &mut HashSet<ObjectId>,
) {
    if !visited.insert(content_id) || depth > 8 {
        return;
    }
    let own = doc
        .get_object(content_id)
        .ok()
        .and_then(|o| match o {
            Object::Stream(s) => s.dict.get(b"Resources").ok().and_then(|r| r.as_dict().ok()),
            _ => None,
        })
        .cloned();
    let effective = match own {
        Some(r) => r,
        None if allow_inherit => inherited.clone(),
        // A form/appearance without its own resources resolves names through
        // whichever parent drew it; that cannot be re-derived at rewrite
        // time, so every font it could resolve is blocked.
        None => {
            for font_id in resources_font_ids(inherited) {
                blocked.insert(font_id);
            }
            return;
        }
    };
    let Ok(data) = doc.get_object(content_id).and_then(|o| match o {
        Object::Stream(s) => s.decompressed_content(),
        _ => Err(lopdf::Error::ObjectNotFound(content_id)),
    }) else {
        for font_id in resources_font_ids(&effective) {
            blocked.insert(font_id);
        }
        return;
    };
    let Ok(content) = lopdf::content::Content::decode(&data) else {
        for font_id in resources_font_ids(&effective) {
            blocked.insert(font_id);
        }
        return;
    };

    let mut current: Option<ObjectId> = None; // font in effect from `Tf`
    let mut ops: Vec<TextOp> = Vec::new();
    for (i, op) in content.operations.iter().enumerate() {
        match op.operator.as_str() {
            "Tf" => {
                current = op
                    .operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|name| lookup_font(&effective, name));
            }
            "Tj" | "'" | "\"" | "TJ" => {
                let mut bytes: Vec<u8> = Vec::new();
                for operand in &op.operands {
                    if let Object::String(b, _) = operand {
                        bytes.extend_from_slice(b);
                    }
                }
                record_text_op(i, current, &effective, &bytes, usage, &mut ops, blocked);
            }
            "Do" => {
                let Some(Object::Name(name)) = op.operands.first() else {
                    continue;
                };
                let Some(xobj_id) = lookup_xobject(&effective, name) else {
                    continue;
                };
                if xobject_subtype(doc, xobj_id).as_deref() == Some(b"Form")
                    && let Some((form_resources, form_id)) = form_content(doc, xobj_id)
                {
                    let form_owns = form_resources.is_some();
                    let form_res = form_resources.unwrap_or_else(|| effective.clone());
                    scan_text_usage(
                        doc,
                        form_id,
                        &form_res,
                        depth + 1,
                        form_owns,
                        visited,
                        usage,
                        stream_ops,
                        blocked,
                    );
                }
            }
            _ => {}
        }
    }
    if !ops.is_empty() {
        stream_ops.insert(content_id, ops);
    }
}

/// Record a text-showing op: the bytes shown become that font's used codes;
/// an unresolvable `Tf` (or none in effect) blocks every font the stream's
/// resources could resolve — an op whose font cannot be attributed must
/// never be re-mapped.
#[cfg(feature = "optimize")]
fn record_text_op(
    i: usize,
    current: Option<ObjectId>,
    resources: &lopdf::Dictionary,
    bytes: &[u8],
    usage: &mut HashMap<ObjectId, FontUsage>,
    ops: &mut Vec<TextOp>,
    blocked: &mut HashSet<ObjectId>,
) {
    match current {
        Some(font_id) => {
            let entry = usage.entry(font_id).or_default();
            for &b in bytes {
                entry.used_codes.insert(b);
            }
            ops.push(TextOp {
                op_index: i,
                font_id,
            });
        }
        None => {
            for font_id in resources_font_ids(resources) {
                blocked.insert(font_id);
            }
        }
    }
}

/// The font object ids a resources dictionary's `/Font` references.
#[cfg(feature = "optimize")]
fn resources_font_ids(resources: &lopdf::Dictionary) -> Vec<ObjectId> {
    let mut out = Vec::new();
    if let Ok(fonts) = resources.get(b"Font").and_then(|f| f.as_dict()) {
        for (_, obj) in fonts.iter() {
            if let Object::Reference(id) = obj {
                out.push(*id);
            }
        }
    }
    out
}

/// Resolve a `Tf` font resource name to its font object id.
#[cfg(feature = "optimize")]
fn lookup_font(resources: &lopdf::Dictionary, name: &[u8]) -> Option<ObjectId> {
    let fonts = resources.get(b"Font").ok()?.as_dict().ok()?;
    match fonts.get(name).ok()? {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

/// The form streams an annotation's `/AP` references (its `/N`, `/R` and
/// `/D` entries, or a plain form reference).
#[cfg(feature = "optimize")]
fn appearance_streams(doc: &Document, page_id: ObjectId) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let Some(annots) = doc
        .get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Annots").ok())
        .and_then(|a| a.as_array().ok())
    else {
        return out;
    };
    for annot in annots {
        let Some(annot_id) = annot.as_reference().ok() else {
            continue;
        };
        let Some(ap) = doc
            .get_object(annot_id)
            .ok()
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"AP").ok())
            .and_then(|a| a.as_dict().ok())
        else {
            continue;
        };
        for (_, obj) in ap.iter() {
            match obj {
                Object::Reference(id) => out.push(*id),
                Object::Dictionary(d) => {
                    for (_, inner) in d.iter() {
                        if let Object::Reference(id) = inner {
                            out.push(*id);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Try to build a strictly-smaller subset plan for one font; `None` when any
/// conservative gate fails (unsupported font kind, unresolvable encoding,
/// unmappable used code, subset not smaller).
#[cfg(feature = "optimize")]
fn build_font_plan(doc: &Document, font_id: ObjectId, usage: &FontUsage) -> Option<FontPlan> {
    let font = doc.get_object(font_id).ok()?.as_dict().ok()?.clone();
    // Simple fonts only: Type0 (already CID) and Type3 are skipped, as are
    // fonts without an embedded program (never subsetted unembedded).
    if font.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Type3") {
        return None;
    }
    let desc_id = font.get(b"FontDescriptor").ok()?.as_reference().ok()?;
    let descriptor = doc.get_object(desc_id).ok()?.as_dict().ok()?.clone();
    // Only TrueType programs (FontFile2) are subset. A CFF program
    // (FontFile3 — the descriptor value is an indirect reference to the
    // program stream; its `/Subtype`, `Type1C` or `CIDFontType0C`, lives on
    // that stream's dict) is deliberately skipped: the installer below
    // emits the TrueType path (a `CIDFontType2` descendant + identity
    // `CIDToGIDMap`), and a CFF subset needs a `CIDFontType0` descendant
    // with its own charset and width handling, so claiming CFF support
    // would mis-install it.
    let file_id: ObjectId = if let Ok(f) = descriptor.get(b"FontFile2") {
        f.as_reference().ok()?
    } else {
        return None;
    };
    let Object::Stream(file_stream) = doc.get_object(file_id).ok()? else {
        return None;
    };
    let font_file = file_stream.decompressed_content().ok()?;

    // code → (gid, unicode) through the font's encoding + cmap (+ post).
    let (cmap, cmap_is_unicode) = cmap_glyph_map(&font_file)?;
    let post_names = post_name_map(&font_file);
    let mut code_map: HashMap<u8, (u16, Option<u16>)> = HashMap::new();
    let encoding = font.get(b"Encoding").ok();
    match encoding {
        Some(Object::Name(n)) => {
            if n != b"WinAnsiEncoding" {
                return None; // other standard encodings: skip conservatively
            }
            for &code in &usage.used_codes {
                let uni = win_ansi(code);
                let gid = cmap.get(&uni)?;
                code_map.insert(code, (*gid, Some(uni)));
            }
        }
        Some(Object::Dictionary(enc)) => {
            let base_winansi = enc.get(b"BaseEncoding").ok().and_then(|b| b.as_name().ok())
                == Some(b"WinAnsiEncoding".as_slice());
            if enc.get(b"BaseEncoding").ok().is_some() && !base_winansi {
                return None;
            }
            let differences = enc.get(b"Differences").ok().and_then(|d| d.as_array().ok());
            // (code, glyph name) from the /Differences array.
            let mut diffs: HashMap<u8, Vec<u8>> = HashMap::new();
            if let Some(arr) = differences {
                let mut code: i64 = -1;
                for item in arr {
                    match item {
                        Object::Integer(c) => code = *c,
                        Object::Name(name) if code >= 0 => {
                            diffs.insert(code as u8, name.clone());
                            code += 1;
                        }
                        _ => {}
                    }
                }
            }
            for &code in &usage.used_codes {
                let (gid, uni): (u16, Option<u16>) = if let Some(name) = diffs.get(&code) {
                    let gid = post_names.get(name)?;
                    (*gid, glyph_name_unicode(name))
                } else if base_winansi {
                    let uni = win_ansi(code);
                    let gid = cmap.get(&uni)?;
                    (*gid, Some(uni))
                } else {
                    // Built-in base encoding: the cmap maps codes directly.
                    let gid = cmap.get(&(code as u16))?;
                    (*gid, cmap_is_unicode.then_some(code as u16))
                };
                code_map.insert(code, (gid, uni));
            }
        }
        _ => {
            // No /Encoding: the font's built-in encoding (its cmap).
            for &code in &usage.used_codes {
                let gid = cmap.get(&(code as u16))?;
                code_map.insert(code, (*gid, cmap_is_unicode.then_some(code as u16)));
            }
        }
    }
    if code_map.is_empty() {
        return None;
    }

    // Widths: code → width from /Widths + /FirstChar (PDF widths are f32).
    let first_char = font
        .get(b"FirstChar")
        .ok()
        .and_then(|f| f.as_i64().ok())
        .unwrap_or(0) as u8;
    let widths_arr: Vec<f32> = font
        .get(b"Widths")
        .ok()
        .and_then(|w| w.as_array().ok())
        .map(|a| a.iter().filter_map(|o| o.as_float().ok()).collect())
        .unwrap_or_default();
    let missing_width = descriptor
        .get(b"MissingWidth")
        .ok()
        .and_then(|m| m.as_float().ok())
        .unwrap_or(1000.0);
    let width_of = |code: u8| -> f32 {
        let idx = code as usize - first_char as usize;
        widths_arr.get(idx).copied().unwrap_or(missing_width)
    };

    // Glyphs to keep: every used GID plus .notdef.
    let mut gids: Vec<u16> = code_map.values().map(|(g, _)| *g).collect();
    gids.push(0);
    gids.sort_unstable();
    gids.dedup();
    let remapper = subsetter::GlyphRemapper::new_from_glyphs_sorted(&gids);
    let subset_bytes = subsetter::subset(&font_file, 0, &remapper).ok()?;
    if subset_bytes.len() >= font_file.len() {
        return None; // strictly-smaller gate
    }

    // code → new CID (= remapped GID); new CID → width.
    let mut code_to_cid: HashMap<u8, u16> = HashMap::new();
    let mut code_to_unicode: HashMap<u8, u16> = HashMap::new();
    let mut cid_to_width: std::collections::BTreeMap<u16, f32> = std::collections::BTreeMap::new();
    for (&code, &(gid, uni)) in &code_map {
        let new_gid = remapper.get(gid)?;
        code_to_cid.insert(code, new_gid);
        if let Some(u) = uni {
            code_to_unicode.insert(code, u);
        }
        cid_to_width.entry(new_gid).or_insert(width_of(code));
    }

    let old_name = font
        .get(b"BaseFont")
        .ok()
        .and_then(|b| b.as_name().ok())
        .unwrap_or(b"Font");
    let base_font = subset_tag(old_name);
    let mut old_ids = vec![desc_id, file_id];
    if let Ok(tu) = font.get(b"ToUnicode")
        && let Ok(id) = tu.as_reference()
    {
        old_ids.push(id);
    }

    Some(FontPlan {
        font_id,
        code_to_cid,
        code_to_unicode,
        cid_to_width,
        base_font,
        font_file: subset_bytes,
        old_ids,
        descriptor: Some(descriptor),
    })
}

/// Rewrite every recorded content stream, re-mapping the text strings of the
/// recorded ops from single-byte codes to the two-byte CIDs of the plan.
#[cfg(feature = "optimize")]
fn rewrite_text_streams(
    doc: &mut Document,
    stream_ops: &HashMap<ObjectId, Vec<TextOp>>,
    plans: &[FontPlan],
) {
    use lopdf::content::Content;
    let mut by_font: HashMap<ObjectId, &FontPlan> = HashMap::new();
    for plan in plans {
        by_font.insert(plan.font_id, plan);
    }
    for (content_id, ops) in stream_ops {
        let Some(Object::Stream(stream)) = doc.objects.get_mut(content_id) else {
            continue;
        };
        let Ok(data) = stream.decompressed_content() else {
            continue;
        };
        let Ok(mut content) = Content::decode(&data) else {
            continue;
        };
        let ops_by_index: HashMap<usize, ObjectId> =
            ops.iter().map(|o| (o.op_index, o.font_id)).collect();
        // The op indices were recorded by the scan (which tracked `Tf` through
        // the stream's effective resources); matching by index + font id needs
        // no re-derivation of the graphics state here.
        let mut changed = false;
        for (i, op) in content.operations.iter_mut().enumerate() {
            if matches!(op.operator.as_str(), "Tj" | "'" | "\"" | "TJ") {
                let Some(&font_id) = ops_by_index.get(&i) else {
                    continue;
                };
                let Some(plan) = by_font.get(&font_id) else {
                    continue;
                };
                changed |= remap_string_operands(op, plan);
            }
        }
        if !changed {
            continue;
        }
        let encoded = content
            .encode()
            .expect("re-encoding a parsed content stream");
        let compressed = crate::pdf::images::zlib_encode(&encoded);
        let Some(Object::Stream(stream)) = doc.objects.get_mut(content_id) else {
            continue;
        };
        stream.content = compressed;
        stream
            .dict
            .set(b"Length", Object::Integer(stream.content.len() as i64));
        stream
            .dict
            .set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
    }
}

/// Replace every string operand of one text-showing op with the mapped CIDs.
/// All bytes are checked first: if any byte of any operand is unmappable,
/// the op is left untouched (never a partial rewrite).
#[cfg(feature = "optimize")]
fn remap_string_operands(op: &mut lopdf::content::Operation, plan: &FontPlan) -> bool {
    for operand in &op.operands {
        if let Object::String(bytes, _) = operand
            && bytes.iter().any(|b| !plan.code_to_cid.contains_key(b))
        {
            return false;
        }
    }
    let mut changed = false;
    for operand in &mut op.operands {
        let Object::String(bytes, format) = operand else {
            continue;
        };
        let mapped: Vec<u8> = bytes
            .iter()
            .flat_map(|b| plan.code_to_cid[b].to_be_bytes())
            .collect();
        *operand = Object::String(mapped, *format);
        changed = true;
    }
    changed
}

/// Install one plan: new Type0 / CIDFontType2 / descriptor / font program /
/// ToUnicode objects, replacing the font dict in place and removing the old
/// program + descriptor once unreferenced. Returns whether the font was
/// replaced.
#[cfg(feature = "optimize")]
fn install_font_plan(doc: &mut Document, plan: &FontPlan) -> bool {
    let Some(font) = doc
        .get_object(plan.font_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .cloned()
    else {
        return false;
    };
    let old_name = font
        .get(b"BaseFont")
        .ok()
        .and_then(|b| b.as_name().ok())
        .unwrap_or(b"Font");

    // Font program stream.
    let file = Stream::new(
        dictionary! {
            "Length" => plan.font_file.len() as i64,
        },
        plan.font_file.clone(),
    );
    let file_id = doc.add_object(Object::Stream(file));

    // Descriptor (reuses the original dict, new program + name).
    let mut descriptor = plan.descriptor.clone().unwrap_or_default();
    descriptor.set(b"FontName", Object::Name(plan.base_font.clone()));
    descriptor.set(b"FontFile2", Object::Reference(file_id));
    let desc_id = doc.add_object(Object::Dictionary(descriptor));

    // CIDFontType2 descendant.
    let mut w_array: Vec<Object> = Vec::new();
    if let Some((&first, _)) = plan.cid_to_width.iter().next() {
        let mut widths = Vec::with_capacity(plan.cid_to_width.len());
        for w in plan.cid_to_width.values() {
            widths.push(Object::Integer(w.round() as i64));
        }
        w_array.push(Object::Integer(first as i64));
        w_array.push(Object::Array(widths));
    }
    let cid_font = Object::Dictionary(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => Object::Name(old_name.to_vec()),
        "CIDSystemInfo" => Object::Dictionary(dictionary! {
            "Registry" => Object::String(b"Adobe".to_vec(), lopdf::StringFormat::Literal),
            "Ordering" => Object::String(b"Identity".to_vec(), lopdf::StringFormat::Literal),
            "Supplement" => 0,
        }),
        "FontDescriptor" => desc_id,
        "CIDToGIDMap" => "Identity",
        "DW" => 1000,
        "W" => w_array,
    });
    let cid_id = doc.add_object(cid_font);

    // ToUnicode: CID → Unicode for the used codes.
    let mut bf = Vec::new();
    for (&code, &cid) in &plan.code_to_cid {
        if let Some(&uni) = plan.code_to_unicode.get(&code) {
            bf.push(format!("<{:04X}> <{:04X}>", cid, uni));
        }
    }
    let tu_id = if bf.is_empty() {
        None
    } else {
        let mut cmap = Vec::new();
        cmap.extend_from_slice(
            b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n".as_slice(),
        );
        cmap.extend_from_slice(
            b"/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n"
                .as_slice(),
        );
        cmap.extend_from_slice(b"/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n".as_slice());
        cmap.extend_from_slice(format!("{} beginbfchar\n", bf.len()).as_bytes());
        for line in bf {
            cmap.extend_from_slice(line.as_bytes());
            cmap.push(b'\n');
        }
        cmap.extend_from_slice(
            b"endbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n"
                .as_slice(),
        );
        let compressed = crate::pdf::images::zlib_encode(&cmap);
        let tu = Stream::new(
            dictionary! {
                "Filter" => "FlateDecode",
                "Length" => compressed.len() as i64,
            },
            compressed,
        );
        Some(doc.add_object(Object::Stream(tu)))
    };

    // The Type0 font replacing the simple font in place.
    let mut type0 = dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => Object::Name(plan.base_font.clone()),
        "Encoding" => "Identity-H",
        "DescendantFonts" => vec![Object::Reference(cid_id)],
    };
    if let Some(tu_id) = tu_id {
        type0.set("ToUnicode", Object::Reference(tu_id));
    }
    doc.objects.insert(plan.font_id, Object::Dictionary(type0));

    // Remove the old program / descriptor / ToUnicode once unreferenced.
    for old in &plan.old_ids {
        if *old == plan.font_id {
            continue;
        }
        let referenced = doc.objects.values().any(|obj| object_references(obj, *old));
        if !referenced {
            doc.objects.remove(old);
        }
    }
    true
}

/// Whether `obj` (recursively) references `target`.
#[cfg(feature = "optimize")]
fn object_references(obj: &Object, target: ObjectId) -> bool {
    match obj {
        Object::Reference(id) => *id == target,
        Object::Array(items) => items.iter().any(|o| object_references(o, target)),
        Object::Dictionary(d) => d.iter().any(|(_, o)| object_references(o, target)),
        Object::Stream(s) => s.dict.iter().any(|(_, o)| object_references(o, target)),
        _ => false,
    }
}

/// A six-character subset tag (`ABCDEF+`) for a `/BaseFont` name.
#[cfg(feature = "optimize")]
fn subset_tag(name: &[u8]) -> Vec<u8> {
    let tag = b"ABCDEF";
    let mut out = Vec::with_capacity(tag.len() + 1 + name.len());
    out.extend_from_slice(tag);
    out.push(b'+');
    // Strip any existing subset tag (`XXXXXX+`) from the name.
    if name.len() > 7 && name[6] == b'+' {
        out.extend_from_slice(&name[7..]);
    } else {
        out.extend_from_slice(name);
    }
    out
}

/// Windows-1252 (WinAnsi) code → Unicode.
#[cfg(feature = "optimize")]
fn win_ansi(c: u8) -> u16 {
    const SPECIAL: [u16; 32] = [
        0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, // 0x80..
        0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, // 0x88..
        0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, // 0x90..
        0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178, // 0x98..
    ];
    match c {
        0x00..=0x7F => c as u16,
        0x80..=0x9F => SPECIAL[(c - 0x80) as usize],
        // 0xA0..=0xFF is Latin-1 == WinAnsi.
        _ => c as u16,
    }
}

/// Best-effort glyph-name → Unicode (for `/ToUnicode` of `/Differences`
/// entries): `uniXXXX`/`uXXXX` forms, single ASCII letters, and a compact
/// table of common names. `None` skips the `bfchar` entry — rendering is
/// unaffected, only extraction for that exotic glyph.
#[cfg(feature = "optimize")]
fn glyph_name_unicode(name: &[u8]) -> Option<u16> {
    let s = std::str::from_utf8(name).ok()?;
    if s.starts_with("uni") && s.len() == 7 {
        return u16::from_str_radix(&s[3..], 16).ok();
    }
    if s.starts_with('u') && s.len() == 5 && s[1..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return u16::from_str_radix(&s[1..], 16).ok();
    }
    if s.len() == 1 {
        let b = s.as_bytes()[0];
        if b.is_ascii() {
            return Some(b as u16);
        }
    }
    let common: &[(&str, u16)] = &[
        ("space", 0x20),
        ("hyphen", 0x2D),
        ("period", 0x2E),
        ("comma", 0x2C),
        ("colon", 0x3A),
        ("semicolon", 0x3B),
        ("exclam", 0x21),
        ("question", 0x3F),
        ("quotesingle", 0x27),
        ("quotedbl", 0x22),
        ("numbersign", 0x23),
        ("dollar", 0x24),
        ("percent", 0x25),
        ("ampersand", 0x26),
        ("parenleft", 0x28),
        ("parenright", 0x29),
        ("asterisk", 0x2A),
        ("plus", 0x2B),
        ("slash", 0x2F),
        ("at", 0x40),
        ("bracketleft", 0x5B),
        ("backslash", 0x5C),
        ("bracketright", 0x5D),
        ("asciicircum", 0x5E),
        ("underscore", 0x5F),
        ("grave", 0x60),
        ("braceleft", 0x7B),
        ("bar", 0x7C),
        ("braceright", 0x7D),
        ("asciitilde", 0x7E),
        ("endash", 0x2013),
        ("emdash", 0x2014),
        ("quoteleft", 0x2018),
        ("quoteright", 0x2019),
        ("quotedblleft", 0x201C),
        ("quotedblright", 0x201D),
        ("quotesinglbase", 0x201A),
        ("quotedblbase", 0x201E),
        ("bullet", 0x2022),
        ("ellipsis", 0x2026),
        ("dagger", 0x2020),
        ("daggerdbl", 0x2021),
        ("Euro", 0x20AC),
        ("trademark", 0x2122),
        ("registered", 0x00AE),
        ("copyright", 0x00A9),
        ("degree", 0x00B0),
        ("plusminus", 0x00B1),
        ("multiply", 0x00D7),
        ("divide", 0x00F7),
        ("notequal", 0x2260),
        ("lessequal", 0x2264),
        ("greaterequal", 0x2265),
        ("infinity", 0x221E),
        ("partialdiff", 0x2202),
        ("summation", 0x2211),
        ("product", 0x220F),
        ("pi", 0x03C0),
        ("integral", 0x222B),
        ("radical", 0x221A),
        ("approxequal", 0x2248),
        ("Delta", 0x0394),
        ("Omega", 0x03A9),
        ("mu", 0x00B5),
        ("florin", 0x0192),
        ("logicalnot", 0x00AC),
        ("yen", 0x00A5),
        ("cent", 0x00A2),
        ("sterling", 0x00A3),
        ("currency", 0x00A4),
        ("section", 0x00A7),
        ("paragraph", 0x00B6),
        ("germandbls", 0x00DF),
        ("AE", 0x00C6),
        ("ae", 0x00E6),
        ("OE", 0x0152),
        ("oe", 0x0153),
        ("oslash", 0x00F8),
        ("Oslash", 0x00D8),
        ("Eth", 0x00D0),
        ("eth", 0x00F0),
        ("Thorn", 0x00DE),
        ("thorn", 0x00FE),
        ("Yacute", 0x00DD),
        ("yacute", 0x00FD),
        ("Ydieresis", 0x0178),
        ("ydieresis", 0x00FF),
        ("Lslash", 0x0141),
        ("lslash", 0x0142),
        ("Scaron", 0x0160),
        ("scaron", 0x0161),
        ("Zcaron", 0x017D),
        ("zcaron", 0x017E),
        ("brokenbar", 0x00A6),
        ("minus", 0x2212),
        ("onehalf", 0x00BD),
        ("onequarter", 0x00BC),
        ("threequarters", 0x00BE),
        ("onesuperior", 0x00B9),
        ("twosuperior", 0x00B2),
        ("threesuperior", 0x00B3),
        ("franc", 0x20A3),
        ("fraction", 0x2044),
        ("lozenge", 0x25CA),
        ("perthousand", 0x2030),
        ("guilsinglleft", 0x2039),
        ("guilsinglright", 0x203A),
        ("guillemotleft", 0x00AB),
        ("guillemotright", 0x00BB),
        ("fi", 0xFB01),
        ("fl", 0xFB02),
        ("dotlessi", 0x0131),
        ("circumflex", 0x02C6),
        ("tilde", 0x02DC),
        ("macron", 0x00AF),
        ("breve", 0x02D8),
        ("dotaccent", 0x02D9),
        ("ring", 0x02DA),
        ("cedilla", 0x00B8),
        ("hungarumlaut", 0x02DD),
        ("ogonek", 0x02DB),
        ("caron", 0x02C7),
        ("nonbreakingspace", 0x00A0),
        ("ordfeminine", 0x00AA),
        ("ordmasculine", 0x00BA),
        ("questiondown", 0x00BF),
        ("exclamdown", 0x00A1),
        ("dieresis", 0x00A8),
        ("acute", 0x00B4),
        ("aacute", 0x00E1),
        ("agrave", 0x00E0),
        ("acircumflex", 0x00E2),
        ("adieresis", 0x00E4),
        ("atilde", 0x00E3),
        ("aring", 0x00E5),
        ("ccedilla", 0x00E7),
        ("eacute", 0x00E9),
        ("egrave", 0x00E8),
        ("ecircumflex", 0x00EA),
        ("edieresis", 0x00EB),
        ("igrave", 0x00EC),
        ("iacute", 0x00ED),
        ("icircumflex", 0x00EE),
        ("idieresis", 0x00EF),
        ("ntilde", 0x00F1),
        ("ograve", 0x00F2),
        ("oacute", 0x00F3),
        ("ocircumflex", 0x00F4),
        ("odieresis", 0x00F6),
        ("otilde", 0x00F5),
        ("ugrave", 0x00F9),
        ("uacute", 0x00FA),
        ("ucircumflex", 0x00FB),
        ("udieresis", 0x00FC),
        ("Aacute", 0x00C1),
        ("Agrave", 0x00C0),
        ("Acircumflex", 0x00C2),
        ("Adieresis", 0x00C4),
        ("Atilde", 0x00C3),
        ("Aring", 0x00C5),
        ("Ccedilla", 0x00C7),
        ("Eacute", 0x00C9),
        ("Egrave", 0x00C8),
        ("Ecircumflex", 0x00CA),
        ("Edieresis", 0x00CB),
        ("Iacute", 0x00CD),
        ("Igrave", 0x00CC),
        ("Icircumflex", 0x00CE),
        ("Idieresis", 0x00CF),
        ("Ntilde", 0x00D1),
        ("Oacute", 0x00D3),
        ("Ograve", 0x00D2),
        ("Ocircumflex", 0x00D4),
        ("Odieresis", 0x00D6),
        ("Otilde", 0x00D5),
        ("Uacute", 0x00DA),
        ("Ugrave", 0x00D9),
        ("Ucircumflex", 0x00DB),
        ("Udieresis", 0x00DC),
    ];
    common.iter().find(|(n, _)| *n == s).map(|(_, u)| *u)
}

/// Parse the `cmap` table into a code → GID map (format 4 subtables only),
/// plus whether the chosen subtable is a Unicode one (platform 3/1 or 0) —
/// that decides whether the built-in-encoding path can claim the code as
/// its own Unicode value for `/ToUnicode`.
#[cfg(feature = "optimize")]
fn cmap_glyph_map(data: &[u8]) -> Option<(HashMap<u16, u16>, bool)> {
    let cmap = sfnt_table(data, b"cmap")?;
    if cmap.len() < 4 {
        return None;
    }
    let num = u16::from_be_bytes([cmap[2], cmap[3]]) as usize;
    let mut unicode: Option<(bool, &[u8])> = None;
    let mut fallback: Option<(bool, &[u8])> = None;
    for i in 0..num {
        let rec = 4 + i * 8;
        if rec + 8 > cmap.len() {
            break;
        }
        let platform = u16::from_be_bytes([cmap[rec], cmap[rec + 1]]);
        let encoding = u16::from_be_bytes([cmap[rec + 2], cmap[rec + 3]]);
        let offset =
            u32::from_be_bytes([cmap[rec + 4], cmap[rec + 5], cmap[rec + 6], cmap[rec + 7]])
                as usize;
        if offset + 2 > cmap.len() {
            continue;
        }
        let format = u16::from_be_bytes([cmap[offset], cmap[offset + 1]]);
        if format != 4 {
            continue;
        }
        let sub = &cmap[offset..];
        let is_unicode = (platform == 3 && encoding == 1) || platform == 0;
        if is_unicode && unicode.is_none() {
            unicode = Some((true, sub));
        } else if fallback.is_none() {
            fallback = Some((false, sub));
        }
    }
    let (is_unicode, chosen) = unicode.or(fallback)?;
    Some((parse_format4(chosen), is_unicode))
}

/// Parse a format-4 cmap subtable.
#[cfg(feature = "optimize")]
fn parse_format4(sub: &[u8]) -> HashMap<u16, u16> {
    let mut map = HashMap::new();
    if sub.len() < 16 {
        return map;
    }
    let seg_count_x2 = u16::from_be_bytes([sub[6], sub[7]]) as usize;
    let seg_count = seg_count_x2 / 2;
    let end_base = 14;
    let start_base = end_base + seg_count_x2 + 2;
    let delta_base = start_base + seg_count_x2;
    let ro_base = delta_base + seg_count_x2;
    if ro_base + seg_count_x2 + 2 > sub.len() {
        return map;
    }
    for i in 0..seg_count {
        let end = u16::from_be_bytes([sub[end_base + i * 2], sub[end_base + i * 2 + 1]]);
        let start = u16::from_be_bytes([sub[start_base + i * 2], sub[start_base + i * 2 + 1]]);
        let delta = u16::from_be_bytes([sub[delta_base + i * 2], sub[delta_base + i * 2 + 1]]);
        let ro = u16::from_be_bytes([sub[ro_base + i * 2], sub[ro_base + i * 2 + 1]]);
        if end == 0xFFFF && start == 0xFFFF {
            continue;
        }
        for code in start..=end {
            let gid = if ro == 0 {
                code.wrapping_add(delta)
            } else {
                let addr = ro_base + i * 2 + ro as usize + (code - start) as usize * 2;
                if addr + 2 > sub.len() {
                    continue;
                }
                let g = u16::from_be_bytes([sub[addr], sub[addr + 1]]);
                if g == 0 { 0 } else { g.wrapping_add(delta) }
            };
            map.insert(code, gid);
        }
    }
    map
}

/// Parse the `post` table (format 2.0) into a glyph-name → GID map, for
/// `/Differences` name resolution. Format 3.0 fonts (no names) yield an
/// empty map (differences then make the font unsupported).
#[cfg(feature = "optimize")]
fn post_name_map(data: &[u8]) -> HashMap<Vec<u8>, u16> {
    let mut out = HashMap::new();
    let Some(post) = sfnt_table(data, b"post") else {
        return out;
    };
    if post.len() < 32 {
        return out;
    }
    let version = u32::from_be_bytes([post[0], post[1], post[2], post[3]]);
    if version != 0x00020000 {
        return out; // format 1.0/2.5/3.0/4.0: no usable name array
    }
    let num_glyphs = u16::from_be_bytes([post[32], post[33]]) as usize;
    let idx_base = 34;
    if idx_base + num_glyphs * 2 > post.len() {
        return out;
    }
    let mut custom: Vec<(u16, Vec<u8>)> = Vec::new();
    // First pass: collect the custom Pascal-string names.
    let mut cursor = idx_base + num_glyphs * 2;
    let mut seen: std::collections::HashMap<u16, Vec<u8>> = std::collections::HashMap::new();
    for i in 0..num_glyphs {
        let idx = u16::from_be_bytes([post[idx_base + i * 2], post[idx_base + i * 2 + 1]]);
        if idx >= 258 {
            let slot = idx - 258;
            if !seen.contains_key(&slot) && cursor < post.len() {
                let len = post[cursor] as usize;
                if cursor + 1 + len <= post.len() {
                    let name = post[cursor + 1..cursor + 1 + len].to_vec();
                    seen.insert(slot, name);
                }
                cursor += 1 + len;
            }
        }
    }
    for (slot, name) in seen {
        custom.push((slot, name));
    }
    // Second pass: build name → gid (custom names only; standard names
    // cannot be resolved without the 258-entry standard table, so glyphs
    // with standard names are simply absent — differences referencing them
    // make the font unsupported, which is the conservative outcome).
    for i in 0..num_glyphs {
        let idx = u16::from_be_bytes([post[idx_base + i * 2], post[idx_base + i * 2 + 1]]);
        if idx >= 258 {
            let slot = idx - 258;
            if let Some((_, name)) = custom.iter().find(|(s, _)| *s == slot) {
                out.entry(name.clone()).or_insert(i as u16);
            }
        }
    }
    out
}

/// Look up an sfnt table by tag, returning its bytes.
#[cfg(feature = "optimize")]
fn sfnt_table<'a>(data: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() {
            return None;
        }
        if &data[rec..rec + 4] == tag {
            let offset =
                u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                    as usize;
            let length = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            return data.get(offset..offset.saturating_add(length));
        }
    }
    None
}

/// Codec candidates behind the `optimize` feature: lossless JBIG2 encoding of
/// bitonal masks (`--jbig2`) and rate-targeted JPEG2000 encoding of
/// continuous-tone RGB (`--jpeg2000`), plus the MRC composite builder
/// (`--mrc`).
///
/// # Design rationale
///
/// Both codecs are *candidates* in the existing size court, never
/// unconditional replacements: JBIG2 competes against the CCITT G4 mask and
/// the source, JPEG2000 against the JPEG re-encode, and each wins only when
/// strictly smaller. JBIG2 uses symbol-dictionary mode (lossless exact
/// matching) so repeated glyphs share one dictionary entry — the G4 gap the
/// roadmap calls out — and the PDF embedding follows the spec: page stream
/// with `/JBIG2Globals` referencing the dictionary when the encoder emits
/// one. JPEG2000 is rate-targeted at 85% of the JPEG candidate's byte
/// budget, so the court only prefers it when it is genuinely smaller at
/// comparable rate. The encoders are oracled against poppler and ghostscript
/// in the regression suite (both must decode the output pixel-identically to
/// the reference representation).
#[cfg(feature = "optimize")]
pub(crate) mod codecs {
    /// Encode a packed 1-bit mask (MSB-first, 1 = ink) as a lossless JBIG2
    /// PDF fragment with a symbol dictionary. Returns `(page_data,
    /// global_data)`. The oracle: poppler and ghostscript must decode the
    /// embedded result pixel-identically to the same bits stored as G4
    /// (regression suite).
    pub fn jbig2_encode(
        packed: &[u8],
        w: u32,
        h: u32,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
        let mut bytes = Vec::with_capacity((w as usize) * (h as usize));
        let row_bytes = (w as usize).div_ceil(8);
        for row in 0..h as usize {
            let base = row * row_bytes;
            for x in 0..w as usize {
                let bit = (packed[base + x / 8] >> (7 - (x % 8))) & 1;
                bytes.push(bit);
            }
        }
        // Lossless symbol mode: exact matching so repeated glyph shapes share
        // one dictionary entry with no pixel change.
        let mut cfg = jbig2enc_rust::jbig2structs::Jbig2Config::text();
        cfg.is_lossless = true;
        let ctx = jbig2enc_rust::Jbig2Context::with_config(cfg, true);
        let res = jbig2enc_rust::encode_single_image_with_config(&bytes, w, h, ctx)
            .map_err(|e| format!("jbig2: {e}"))?;
        Ok((res.page_data, res.global_data))
    }

    /// Encode interleaved RGB as a lossy JPEG2000 codestream targeting
    /// `target_bytes` of output, wrapped in a minimal JP2 file (signature /
    /// file-type / image-header + sRGB colour / contiguous-codestream boxes).
    ///
    /// # Design rationale
    ///
    /// The JP2 wrapper is not decoration: poppler tries the JP2-file parser
    /// first and falls back to the raw codestream with noisy warnings; the
    /// wrapper also carries the colour space (sRGB), which the raw
    /// codestream cannot. The wrapper is the minimal ISO 15444-1 structure:
    /// signature, file-type (jp2), image header (3×8-bit sRGB) and the
    /// codestream box. JPXDecode accepts the JP2 file form directly.
    pub fn j2k_encode_rgb(
        rgb: &[u8],
        w: u32,
        h: u32,
        target_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let samples =
            j2k::J2kLossySamples::new(rgb, w, h, 3, 8, false).map_err(|e| format!("j2k: {e}"))?;
        let mut options = j2k::J2kLossyEncodeOptions::default();
        options.rate_target = Some(j2k::J2kRateTarget::Bytes(target_bytes));
        let out = j2k::encode_j2k_lossy(samples, &options).map_err(|e| format!("j2k: {e}"))?;
        Ok(wrap_jp2(&out.codestream, w, h))
    }

    /// Wrap a raw J2K codestream in the minimal JP2 file structure.
    fn wrap_jp2(codestream: &[u8], w: u32, h: u32) -> Vec<u8> {
        fn box_(len: u32, tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut b = Vec::with_capacity(8 + body.len());
            b.extend_from_slice(&len.to_be_bytes());
            b.extend_from_slice(tag);
            b.extend_from_slice(body);
            b
        }
        let mut out = Vec::with_capacity(77 + codestream.len());
        // Signature box: the 12-byte magic that identifies a JP2 file.
        out.extend_from_slice(&[0, 0, 0, 12, b'j', b'P', b' ', b' ', 13, 10, 0x87, 10]);
        // File-type box: brand `jp2 `, version 0, compatible brand `jp2 `.
        let mut ftyp = Vec::with_capacity(12);
        ftyp.extend_from_slice(b"jp2 ");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        ftyp.extend_from_slice(b"jp2 ");
        out.extend_from_slice(&box_(20, b"ftyp", &ftyp));
        // Image header: height, width, 3 components, 8-bit unsigned (0x07;
        // 0x87 would mean *signed* 8-bit and every strict JP2 validator
        // rejects the mismatch against our unsigned codestream), JPEG 2000
        // compression (7), unknown colourspace flag 0.
        let mut ihdr = Vec::with_capacity(14);
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&3u16.to_be_bytes());
        ihdr.extend_from_slice(&[0x07, 7, 0, 0]);
        // Colour box: enumerated, sRGB (16).
        let mut colr = Vec::with_capacity(9);
        colr.extend_from_slice(&[1, 0, 0]);
        colr.extend_from_slice(&16u32.to_be_bytes());
        let mut jp2h = Vec::with_capacity(8 + ihdr.len() + colr.len());
        jp2h.extend_from_slice(&box_(8 + ihdr.len() as u32, b"ihdr", &ihdr));
        jp2h.extend_from_slice(&box_(8 + colr.len() as u32, b"colr", &colr));
        out.extend_from_slice(&box_(8 + jp2h.len() as u32, b"jp2h", &jp2h));
        // Contiguous codestream box.
        out.extend_from_slice(&box_(8 + codestream.len() as u32, b"jp2c", codestream));
        out
    }

    /// Median of a byte slice (used for the MRC paper/ink colors).
    fn median(v: &[u8]) -> u8 {
        if v.is_empty() {
            return 0;
        }
        let mut s = v.to_vec();
        s.sort_unstable();
        s[s.len() / 2]
    }

    /// Build the three MRC layers for a classified bitonal scan: a solid
    /// paper-color background (the median paper color, emitted as a 1×1
    /// image), a solid-color foreground (the median ink color), and the
    /// high-res mask bytes.
    ///
    /// Returns `(bg, bg_dims, fg_color, mask_bytes, mask_codec, global)`
    /// where `bg` is the 3-byte paper color, `bg_dims` is always `(1, 1)`,
    /// `mask_codec` is `b"CCITTFaxDecode"` and `global` is `None` (the mask
    /// is never JBIG2 — see below).
    ///
    /// # Design rationale
    ///
    /// The background is deliberately *not* a downsampled JPEG of the
    /// ink-painted paper. A bitonal scan's paper is (by classification)
    /// near-uniform, so the painted image is a flat color plus JPEG noise —
    /// and exactly that content (a near-flat image with tiny noise) is
    /// mis-decoded by poppler's and Ghostscript's JPEG paths as a
    /// full-page gradient (verified against `image`-crate, libjpeg, and
    /// PIL-encoded bitstreams; mutool and libjpeg decode them flat). A
    /// flat 1×1 paper image renders identically in every renderer and
    /// removes the DCT stream from the composite entirely. (The poppler
    /// "Bogus memory allocation size" notice that motivated this was a
    /// separate bug: the content rewrite used to re-emit the placement
    /// `cm` for the foreground, squaring the scale — fixed in
    /// `apply_mrc_rewrites`.)
    #[allow(clippy::type_complexity)]
    pub fn mrc_layers(
        rgb: &[u8],
        w: u32,
        h: u32,
        mask: &[u8],
        jbig2: bool,
    ) -> Result<
        (
            Vec<u8>,
            (u32, u32),
            [u8; 3],
            Vec<u8>,
            &'static [u8],
            Option<Vec<u8>>,
        ),
        String,
    > {
        // Ink / paper colors: the median RGB of ink pixels and the median of
        // paper pixels (bitonal → two dominant colors).
        let mut ink_r = Vec::new();
        let mut ink_g = Vec::new();
        let mut ink_b = Vec::new();
        let mut paper_r = Vec::new();
        let mut paper_g = Vec::new();
        let mut paper_b = Vec::new();
        let row_bytes = (w as usize).div_ceil(8);
        for y in 0..h as usize {
            let mrow = y * row_bytes;
            let prow = y * (w as usize) * 3;
            for x in 0..w as usize {
                let ink = (mask[mrow + x / 8] >> (7 - (x % 8))) & 1 == 1;
                let p = prow + x * 3;
                let (r, g, b) = (rgb[p], rgb[p + 1], rgb[p + 2]);
                if ink {
                    ink_r.push(r);
                    ink_g.push(g);
                    ink_b.push(b);
                } else {
                    paper_r.push(r);
                    paper_g.push(g);
                    paper_b.push(b);
                }
            }
        }
        let fg = [median(&ink_r), median(&ink_g), median(&ink_b)];
        let bg = [median(&paper_r), median(&paper_g), median(&paper_b)];

        // Mask: G4 (1 = ink, MSB-first, same bits as the opaque image
        // candidate). JBIG2 is deliberately *not* offered for the mask:
        // poppler's JBIG2 decoder emits inverted samples, which would make
        // the SMask polarity viewer-dependent (ink transparent in poppler,
        // opaque elsewhere) — the one place the composite cannot afford it.
        let (mask_bytes, codec, global) = (
            crate::pdf::fax::encode_g4(mask, w, h),
            b"CCITTFaxDecode".as_slice(),
            None,
        );
        let _ = jbig2; // the standalone --jbig2 candidate still competes
        Ok((bg.to_vec(), (1, 1), fg, mask_bytes, codec, global))
    }
}

/// The MRC site index computed once per document: every draw site of every
/// image, plus the images *blocked* from MRC because a content stream that
/// references them cannot be parsed (an unparseable stream may draw the
/// image, and the composite must not drop its foreground there).
#[cfg(feature = "optimize")]
#[derive(Default)]
pub(crate) struct MrcIndex {
    pub sites: HashMap<ObjectId, Vec<MrcSite>>,
    pub blocked: HashSet<ObjectId>,
}

/// One place where an image is drawn from a content stream: the content
/// object id (so the stream can be rewritten), the XObject resource name,
/// the *effective* resource dictionary for that content stream (its own
/// `/Resources` when present, else the page's or form's), and the *owner* of
/// that dictionary — the page or form object whose `/Resources` must gain
/// the foreground XObject name. (The full CTM at the `Do` is deliberately
/// *not* stored: the rewrite re-walks the same operation list with the same
/// `q`/`Q`/`cm` stack, so the transform is re-derived at rewrite time and
/// cannot go stale.)
#[cfg(feature = "optimize")]
#[derive(Debug, Clone)]
pub(crate) struct MrcSite {
    pub content_id: ObjectId,
    pub name: Vec<u8>,
    pub resources: lopdf::Dictionary,
    pub owner: ObjectId,
}

/// Scan every page (and, recursively, every drawn form) for `Do` sites of
/// each image. Returns per-image sites plus the set of images that are
/// *blocked*: referenced by a content stream that cannot be parsed (an
/// unparseable stream might draw the image, and the MRC composite must not
/// drop its foreground there). MRC is offered only for images with at least
/// one site and no blocker.
///
/// # Design rationale
///
/// The scan is the same deliberately small content interpreter the
/// placement scan uses (`q`/`Q`/`cm`/`Do`, form recursion), but it does not
/// need to *track* the CTM — the rewrite re-derives the transform by
/// re-walking the same operation list, so the scan records only where each
/// image is drawn and which resources resolve it.
#[cfg(feature = "optimize")]
pub(crate) fn mrc_sites(doc: &Document) -> (HashMap<ObjectId, Vec<MrcSite>>, HashSet<ObjectId>) {
    let mut sites: HashMap<ObjectId, Vec<MrcSite>> = HashMap::new();
    let mut blocked: HashSet<ObjectId> = HashSet::new();
    let mut visited: HashSet<(ObjectId, ObjectId)> = HashSet::new();
    for (_, page_id) in doc.get_pages() {
        let resources = page_resources(doc, page_id);
        for content_id in page_contents(doc, page_id) {
            scan_content(
                doc,
                content_id,
                &resources,
                page_id,
                0,
                &mut visited,
                &mut sites,
                &mut blocked,
            );
        }
    }
    (sites, blocked)
}

#[cfg(feature = "optimize")]
#[allow(clippy::too_many_arguments)]
fn scan_content(
    doc: &Document,
    content_id: ObjectId,
    resources: &lopdf::Dictionary,
    owner: ObjectId,
    depth: usize,
    visited: &mut HashSet<(ObjectId, ObjectId)>,
    sites: &mut HashMap<ObjectId, Vec<MrcSite>>,
    blocked: &mut HashSet<ObjectId>,
) {
    // Visited per (stream, owner): a content stream shared by two pages (or
    // a form drawn from two scopes) must record sites for *each* owner so
    // the foreground name is registered in every resources dict that
    // resolves it.
    if !visited.insert((content_id, owner)) || depth > 8 {
        return;
    }
    // The *effective* resources for this stream: its own `/Resources` when
    // present, else the inherited (page/form) one. A page content stream
    // with its own resources is a corner case the rewrite cannot register
    // the foreground name into reliably (renderers resolve page content
    // through the page's dictionary), so its images are blocked.
    let inherited = resources;
    let is_form = xobject_subtype(doc, content_id).as_deref() == Some(b"Form");
    let own = doc
        .get_object(content_id)
        .ok()
        .and_then(|o| match o {
            Object::Stream(s) => s.dict.get(b"Resources").ok().and_then(|r| r.as_dict().ok()),
            _ => None,
        })
        .cloned();
    if own.is_some() && !is_form {
        for image_id in resources_xobject_images(doc, inherited) {
            blocked.insert(image_id);
        }
        return;
    }
    let effective = own.clone().unwrap_or_else(|| inherited.clone());
    let site_owner = if own.is_some() { content_id } else { owner };
    // An unparseable stream may draw anything its resources reference:
    // mark every image it references as blocked for MRC.
    let Ok(data) = doc.get_object(content_id).and_then(|o| match o {
        Object::Stream(s) => s.decompressed_content(),
        _ => Err(lopdf::Error::ObjectNotFound(content_id)),
    }) else {
        for image_id in resources_xobject_images(doc, &effective) {
            blocked.insert(image_id);
        }
        return;
    };
    let Ok(content) = lopdf::content::Content::decode(&data) else {
        for image_id in resources_xobject_images(doc, &effective) {
            blocked.insert(image_id);
        }
        return;
    };
    let resources = &effective;

    for op in &content.operations {
        if op.operator != "Do" {
            continue;
        }
        let Some(Object::Name(name)) = op.operands.first() else {
            continue;
        };
        let Some(xobj_id) = lookup_xobject(resources, name) else {
            continue;
        };
        match xobject_subtype(doc, xobj_id).as_deref() {
            Some(b"Image") => {
                sites.entry(xobj_id).or_default().push(MrcSite {
                    content_id,
                    name: name.clone(),
                    resources: resources.clone(),
                    owner: site_owner,
                });
            }
            Some(b"Form") => {
                if let Some((form_resources, form_id)) = form_content(doc, xobj_id) {
                    if let Some(form_res) = form_resources {
                        // Self-contained form: its own resources
                        // resolve every draw, so it becomes the
                        // owner of whatever it draws.
                        scan_content(
                            doc,
                            form_id,
                            &form_res,
                            form_id,
                            depth + 1,
                            visited,
                            sites,
                            blocked,
                        );
                    } else {
                        // A form without its own resources resolves
                        // names through whichever parent drew it;
                        // that cannot be re-derived per site, so
                        // every image it might draw is blocked.
                        for image_id in resources_xobject_images(doc, resources) {
                            blocked.insert(image_id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Every image object id a resources dictionary references (its XObjects).
#[cfg(feature = "optimize")]
fn resources_xobject_images(doc: &Document, resources: &lopdf::Dictionary) -> Vec<ObjectId> {
    let mut out = Vec::new();
    if let Ok(xobjects) = resources.get(b"XObject").and_then(|x| x.as_dict()) {
        for (_, obj) in xobjects.iter() {
            if let Object::Reference(id) = obj
                && xobject_subtype(doc, *id).as_deref() == Some(b"Image")
            {
                out.push(*id);
            }
        }
    }
    out
}

#[cfg(feature = "optimize")]
fn cm_operands(operands: &[Object]) -> Option<[f64; 6]> {
    if operands.len() != 6 {
        return None;
    }
    let f = |o: &Object| o.as_float().ok().map(f64::from);
    Some([
        f(&operands[0])?,
        f(&operands[1])?,
        f(&operands[2])?,
        f(&operands[3])?,
        f(&operands[4])?,
        f(&operands[5])?,
    ])
}

#[cfg(feature = "optimize")]
fn mat_mul(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[0] * b[4] + a[1] * b[5] + a[4],
        a[2] * b[4] + a[3] * b[5] + a[5],
    ]
}

#[cfg(feature = "optimize")]
fn lookup_xobject(resources: &lopdf::Dictionary, name: &[u8]) -> Option<ObjectId> {
    let xobjects = resources.get(b"XObject").ok()?.as_dict().ok()?;
    match xobjects.get(name).ok()? {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

#[cfg(feature = "optimize")]
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

#[cfg(feature = "optimize")]
fn form_content(doc: &Document, id: ObjectId) -> Option<(Option<lopdf::Dictionary>, ObjectId)> {
    let Object::Stream(stream) = doc.get_object(id).ok()? else {
        return None;
    };
    let resources = stream
        .dict
        .get(b"Resources")
        .ok()
        .and_then(|r| r.as_dict().ok())
        .cloned();
    Some((resources, id))
}

#[cfg(feature = "optimize")]
fn page_resources(doc: &Document, page_id: ObjectId) -> lopdf::Dictionary {
    doc.get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Resources").ok())
        .and_then(|r| r.as_dict().ok())
        .cloned()
        .unwrap_or_default()
}

/// The content stream object ids of a page's `/Contents` (stream or array).
#[cfg(feature = "optimize")]
fn page_contents(doc: &Document, page_id: ObjectId) -> Vec<ObjectId> {
    let Some(dict) = doc.get_object(page_id).ok().and_then(|o| o.as_dict().ok()) else {
        return Vec::new();
    };
    match dict.get(b"Contents") {
        Ok(Object::Reference(id)) => vec![*id],
        Ok(Object::Array(items)) => items.iter().filter_map(|o| o.as_reference().ok()).collect(),
        _ => Vec::new(),
    }
}

/// Rewrite the content streams recorded in `sites` so that, after the image's
/// `Do`, the MRC foreground layer (`/`FgN``) is drawn with the transform that
/// was in effect. Each affected stream's `/XObject` resources gain the
/// foreground entry; streams that fail to re-parse are left untouched (the
/// scan already excluded unparseable ones, so this only guards against
/// races/corruption and the image simply renders as the background layer).
#[cfg(feature = "optimize")]
pub(crate) fn apply_mrc_rewrites(doc: &mut Document, sites: &[MrcSite], fg_object_id: ObjectId) {
    use lopdf::content::{Content, Operation};
    let mut per_stream: HashMap<ObjectId, Vec<&MrcSite>> = HashMap::new();
    for site in sites {
        per_stream.entry(site.content_id).or_default().push(site);
    }
    for (content_id, stream_sites) in per_stream {
        let Some(Object::Stream(stream)) = doc.objects.get_mut(&content_id) else {
            continue;
        };
        let Ok(data) = stream.decompressed_content() else {
            continue;
        };
        let Ok(mut content) = Content::decode(&data) else {
            continue;
        };
        // Collect (op index, name) per site: the op list indices are
        // processed in reverse so insertions do not shift earlier indices.
        // (The CTM is deliberately *not* recorded: the foreground is drawn
        // at whatever transform is current right after the source `Do`,
        // which is exactly the transform the stack re-derives here — see
        // the injection below.)
        let mut inject: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut stack: Vec<[f64; 6]> = vec![[1.0, 0.0, 0.0, 1.0, 0.0, 0.0]];
        let mut target_names: Vec<Vec<u8>> = Vec::new();
        for site in &stream_sites {
            if !target_names.contains(&site.name) {
                target_names.push(site.name.clone());
            }
        }
        for (i, op) in content.operations.iter().enumerate() {
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
                    if let (Some(&ctm), Some(m)) = (stack.last(), cm_operands(&op.operands)) {
                        let next = mat_mul(ctm, m);
                        stack.pop();
                        stack.push(next);
                    }
                }
                "Do" => {
                    if let Some(Object::Name(name)) = op.operands.first()
                        && target_names.iter().any(|n| n == name)
                    {
                        inject.push((i, name.clone()));
                    }
                }
                _ => {}
            }
        }
        if inject.is_empty() {
            continue;
        }
        // One foreground resource name for this stream, distinct from any
        // existing XObject name in the *effective* resources.
        let fg_name = {
            let mut n = b"FgMrc0".to_vec();
            let xobjs: Vec<Vec<u8>> = stream_sites
                .first()
                .and_then(|s| s.resources.get(b"XObject").ok())
                .and_then(|x| x.as_dict().ok())
                .map(|d| d.iter().map(|(k, _)| k.clone()).collect())
                .unwrap_or_default();
            while xobjs.contains(&n) {
                let d = n.last().copied().unwrap_or(b'0');
                *n.last_mut().unwrap() = if d == b'9' { b'A' } else { d + 1 };
            }
            n
        };
        // Inject the foreground draw immediately after the source image's
        // `Do` — *without* a `cm`. The source image's own `cm` is still
        // active at that point, so the current CTM is already the placement
        // transform recorded by the stack walk above; re-emitting it as a
        // concatenated `cm` would multiply the scale onto itself (a
        // 1600×1200 placement becomes 2,560,000×1,440,000), and poppler's
        // Splash soft-mask path then overflows its `int`-based mask
        // allocation — printing "Bogus memory allocation size" and
        // silently dropping the foreground (mutool/ghostscript degrade
        // differently but just as wrongly). The `q`/`Q` pair keeps the
        // composite from leaking graphics state either way.
        for (i, _name) in inject.into_iter().rev() {
            let fg_draw = Content {
                operations: vec![
                    Operation::new("q", vec![]),
                    Operation::new("Do", vec![Object::Name(fg_name.clone())]),
                    Operation::new("Q", vec![]),
                ],
            };
            content
                .operations
                .splice(i + 1..i + 1, fg_draw.operations.clone());
        }
        let encoded = content
            .encode()
            .expect("re-encoding a parsed content stream");
        let compressed = crate::pdf::images::zlib_encode(&encoded);
        let stream = doc.objects.get_mut(&content_id).and_then(|o| match o {
            Object::Stream(s) => Some(s),
            _ => None,
        });
        if let Some(stream) = stream {
            stream.content = compressed;
            stream
                .dict
                .set(b"Length", Object::Integer(stream.content.len() as i64));
            stream
                .dict
                .set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
            // Register the foreground XObject in the *owner's* resources —
            // the page or form object whose dictionary resolves the stream's
            // names (renderers resolve page content through the page's
            // `/Resources`, and a form's content through the form stream's
            // own `/Resources`). Pages are dictionaries; a self-contained
            // Form XObject is a stream, so both shapes are handled. The scan
            // recorded the effective dictionary so every name the stream
            // already resolves is preserved, and every distinct owner of a
            // shared stream gets the entry.
            let mut owners: Vec<ObjectId> = Vec::new();
            for site in &stream_sites {
                if !owners.contains(&site.owner) {
                    owners.push(site.owner);
                }
            }
            let fg_name = fg_name.clone();
            for owner in owners {
                let Some(obj) = doc.objects.get_mut(&owner) else {
                    continue;
                };
                let dict = match obj {
                    Object::Dictionary(d) => d,
                    Object::Stream(s) => &mut s.dict,
                    _ => continue,
                };
                let mut resources = dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|r| r.as_dict().ok())
                    .cloned()
                    .unwrap_or_default();
                let mut xobjects = resources
                    .get(b"XObject")
                    .ok()
                    .and_then(|x| x.as_dict().ok())
                    .cloned()
                    .unwrap_or_default();
                xobjects.set(fg_name.clone(), Object::Reference(fg_object_id));
                resources.set(b"XObject", Object::Dictionary(xobjects));
                dict.set(b"Resources", Object::Dictionary(resources));
            }
        }
    }
}
