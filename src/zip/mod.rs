//! ZIP-based files: OpenDocument (ODT, ODS, ODP…), Office Open XML (DOCX,
//! XLSX, PPTX), EPUB, CBZ and plain ZIP. Output in the input's format.
//!
//! The archive is rebuilt entry by entry, in the same order with the same
//! names. Each entry's content goes through `press` when it is an image
//! (JPEG, PNG, WebP), a PDF or a nested ZIP; everything else (XML, fonts…)
//! keeps its content byte for byte. Each entry is then stored as the
//! smallest of: its original compressed bytes (when the content did not
//! change), uncompressed, or deflated at level 9. The file is kept as is
//! when the result is not smaller.
//!
//! - **`mimetype` first**: ODF and EPUB require the first entry, when it
//!   is `mimetype`, to stay first and uncompressed; it is never deflated.
//! - **Left untouched**: signed archives (ODF, OOXML, EPUB, JAR/XPI
//!   signatures, APK signing block), since any change invalidates the
//!   signature, and archives this reader cannot rebuild faithfully
//!   (encrypted entries, ZIP64, multi-disk, prefixed data).
//! - **Metadata**: entry names, order, timestamps, attributes and comments
//!   are kept. Extra fields (extended timestamps, Unix owners) are removed
//!   except the Unicode file name and comment records; `keep_metadata`
//!   keeps them all. Images inside follow the image metadata policy.
//! - **Limits**: entries over 256 MiB uncompressed are copied as is,
//!   nested archives are followed 3 levels deep, and at most 2 GiB is
//!   decompressed per input file (nested archives included); past that,
//!   entries are copied as is. Together they bound the work a
//!   decompression bomb can cause.

pub(crate) mod container;

use std::cell::Cell;
use std::io::{Read, Write};

use container::{
    Archive, DEFLATED, Entry, FLAG_DEFLATE_OPTIONS, Parsed, STORED, Writer, extra_fields,
};

use crate::press::{Pressed, Presser};

/// Larger entries are copied without being decompressed.
const MAX_ENTRY: u32 = 256 << 20;
/// Nesting depth of archives inside archives that is still processed.
pub(crate) const MAX_DEPTH: u8 = 3;
/// Bytes that may be decompressed for one input file, nested archives
/// included.
pub(crate) const BUDGET: u64 = 2 << 30;

/// Info-ZIP Unicode path and comment extra fields: they can carry the real
/// name, so they are kept.
const UNICODE_EXTRAS: [u16; 2] = [0x7075, 0x6375];

/// Why a signed archive is left untouched, when it is.
fn signature(zip: &[u8], archive: &Archive) -> Option<&'static str> {
    let at = archive.central_offset;
    if at >= 16 && &zip[at - 16..at] == b"APK Sig Block 42" {
        return Some("signed APK");
    }
    let signed = archive.entries.iter().any(|e| {
        let name = String::from_utf8_lossy(&e.name).to_ascii_lowercase();
        let meta_inf = name.strip_prefix("meta-inf/");
        name.starts_with("_xmlsignatures/")
            || meta_inf.is_some_and(|f| {
                f.contains("signature")
                    || [".sf", ".rsa", ".dsa", ".ec", ".sig", ".p7s"]
                        .iter()
                        .any(|ext| f.ends_with(ext))
            })
    });
    signed.then_some("signed document: changing it would invalidate the signature")
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::best(),
    );
    e.write_all(data).expect("writing to a Vec cannot fail");
    e.finish().expect("writing to a Vec cannot fail")
}

/// The entry's content, checked against its declared size and CRC. `None`
/// for methods other than stored/deflate, for entries over [`MAX_ENTRY`]
/// and once `budget` is spent: those are copied as is.
fn content(zip: &[u8], e: &Entry, budget: &Cell<u64>) -> Result<Option<Vec<u8>>, String> {
    let left = budget.get();
    if e.uncompressed_size > MAX_ENTRY || u64::from(e.uncompressed_size) > left {
        return Ok(None);
    }
    budget.set(left - u64::from(e.uncompressed_size));
    let raw = &zip[e.data.clone()];
    let size = e.uncompressed_size as usize;
    let data = match e.method {
        STORED => raw.to_vec(),
        DEFLATED => {
            let mut out = Vec::with_capacity(size);
            // One byte past the declared size reveals a lying header (and
            // bounds a decompression bomb).
            flate2::read::DeflateDecoder::new(raw)
                .take(size as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            out
        }
        _ => return Ok(None),
    };
    let name = String::from_utf8_lossy(&e.name);
    if data.len() != size {
        return Err(format!("{name}: size does not match the ZIP header"));
    }
    if crc32(&data) != e.crc {
        return Err(format!("{name}: CRC mismatch"));
    }
    Ok(Some(data))
}

/// Extra fields kept under the metadata policy.
fn kept_extra(extra: &[u8], keep_metadata: bool) -> Vec<u8> {
    if keep_metadata {
        return extra.to_vec();
    }
    let mut out = Vec::new();
    for (id, data) in extra_fields(extra) {
        if UNICODE_EXTRAS.contains(&id) {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(data.len() as u16).to_le_bytes());
            out.extend_from_slice(data);
        }
    }
    out
}

pub(crate) fn press(
    zip: &[u8],
    presser: &Presser,
    depth: u8,
    budget: &Cell<u64>,
) -> Result<Pressed, Box<dyn std::error::Error>> {
    let o = presser.options();
    let archive = match container::parse(zip)? {
        Parsed::Archive(a) => a,
        Parsed::Unsupported(why) => return Ok(Pressed::Untouched(format!("{why}: not supported"))),
    };
    if let Some(why) = signature(zip, &archive) {
        return Ok(Pressed::Untouched(why.into()));
    }

    let mut writer = Writer::with_capacity(zip.len());
    let (mut pressed, mut recompressed) = (0, 0);
    for (index, e) in archive.entries.iter().enumerate() {
        let raw = &zip[e.data.clone()];
        let mut out = Entry {
            local_extra: kept_extra(&e.local_extra, o.keep_metadata),
            central_extra: kept_extra(&e.central_extra, o.keep_metadata),
            ..e.clone()
        };
        let must_store = index == 0 && e.name == b"mimetype";

        let Some(data) = content(zip, e, budget)? else {
            writer.add(&out, raw).ok_or("archive too large")?;
            continue;
        };
        let (data, changed) = match presser.press_entry(&data, depth + 1, budget) {
            Some(smaller) => {
                verbose!(
                    o.verbose,
                    "[zip] {}: {} → {} bytes",
                    String::from_utf8_lossy(&e.name),
                    data.len(),
                    smaller.len()
                );
                pressed += 1;
                (smaller, true)
            }
            None => (data, false),
        };

        // Smallest of: original bytes (unchanged content only), stored,
        // deflated. Ties keep the original.
        let deflated = (!must_store && !data.is_empty()).then(|| deflate(&data));
        let original = (!changed && (!must_store || e.method == STORED)).then_some(raw);
        let mut best: (u16, &[u8]) = (STORED, &data);
        if let Some(d) = &deflated
            && d.len() < best.1.len()
        {
            best = (DEFLATED, d);
        }
        let keep_original = original.is_some_and(|r| r.len() <= best.1.len());
        let (method, bytes) = if keep_original { (e.method, raw) } else { best };
        if !keep_original {
            recompressed += 1;
            out.method = method;
            out.flags &= !FLAG_DEFLATE_OPTIONS;
            out.crc = crc32(&data);
            out.uncompressed_size = u32::try_from(data.len())?;
            if method == DEFLATED {
                out.version_needed = out.version_needed.max(20);
            }
        }
        writer.add(&out, bytes).ok_or("archive too large")?;
    }
    let rebuilt = writer.finish(&archive.comment).ok_or("archive too large")?;
    verbose!(
        o.verbose,
        "[zip] {} entries: {pressed} with smaller content, {recompressed} re-stored",
        archive.entries.len()
    );

    // Self-check: the rebuilt archive must parse to the same entry names.
    match container::parse(&rebuilt)? {
        Parsed::Archive(a)
            if a.entries.len() == archive.entries.len()
                && a.entries
                    .iter()
                    .zip(&archive.entries)
                    .all(|(x, y)| x.name == y.name) => {}
        _ => return Err("rebuilt archive failed its self-check".into()),
    }

    Ok(if rebuilt.len() < zip.len() {
        Pressed::Bytes(rebuilt)
    } else {
        Pressed::Unchanged("no smaller encoding found".into())
    })
}

#[cfg(test)]
mod tests {
    use super::container::tests::{build, deflate as deflate_at, entry};
    use super::*;
    use crate::press::{PdfOptions, PressOptions};
    use crate::transcode::Acceleration;

    fn run(zip: &[u8], presser: &Presser) -> Result<Pressed, Box<dyn std::error::Error>> {
        press(zip, presser, 0, &Cell::new(BUDGET))
    }

    fn presser(keep_metadata: bool) -> Presser {
        Presser::new(
            PressOptions {
                quality: 80,
                ssim: None,
                palette: false,
                keep_metadata,
                pdf: PdfOptions::default(),
                verbose: false,
            },
            Acceleration::Cpu,
            false,
        )
        .unwrap()
    }

    fn archive(zip: &[u8]) -> Archive {
        match container::parse(zip).unwrap() {
            Parsed::Archive(a) => a,
            Parsed::Unsupported(why) => panic!("{why}"),
        }
    }

    fn read(zip: &[u8], name: &str) -> Vec<u8> {
        let a = archive(zip);
        let e = a
            .entries
            .iter()
            .find(|e| e.name == name.as_bytes())
            .unwrap();
        content(zip, e, &Cell::new(BUDGET)).unwrap().unwrap()
    }

    /// A fast-deflated, unfiltered PNG: easy to shrink losslessly.
    fn weak_png() -> Vec<u8> {
        let img =
            image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8 * 4, y as u8 * 4, 0]));
        let mut out = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::NoFilter,
            ),
            img.as_raw(),
            64,
            64,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
        out
    }

    fn bytes(p: Pressed) -> Vec<u8> {
        match p {
            Pressed::Bytes(b) => b,
            Pressed::Unchanged(r) | Pressed::Untouched(r) => panic!("not pressed: {r}"),
            Pressed::Pdf(_) => panic!("a PDF"),
        }
    }

    fn odt() -> Vec<u8> {
        let xml = b"<office:document-content>lorem ipsum</office:document-content>".repeat(40);
        build(
            &[
                (
                    "mimetype",
                    b"application/vnd.oasis.opendocument.text",
                    STORED,
                ),
                ("content.xml", &xml, DEFLATED),
                ("Pictures/a.png", &weak_png(), STORED),
                ("META-INF/manifest.xml", b"<manifest/>", DEFLATED),
            ],
            b"",
        )
    }

    #[test]
    fn document_is_rebuilt_smaller_with_mimetype_first_and_stored() {
        let input = odt();
        let out = bytes(run(&input, &presser(false)).unwrap());
        assert!(out.len() < input.len());
        let a = archive(&out);
        let names: Vec<_> = a
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "mimetype",
                "content.xml",
                "Pictures/a.png",
                "META-INF/manifest.xml"
            ]
        );
        assert_eq!(a.entries[0].method, STORED);
        assert!(a.entries[0].local_extra.is_empty());
        assert_eq!(read(&out, "content.xml"), read(&input, "content.xml"));
        assert_eq!(read(&out, "META-INF/manifest.xml"), b"<manifest/>");
        let png = read(&out, "Pictures/a.png");
        assert!(png.len() < weak_png().len());
        let before = image::load_from_memory(&weak_png()).unwrap().to_rgb8();
        assert_eq!(image::load_from_memory(&png).unwrap().to_rgb8(), before);
    }

    #[test]
    fn deflated_mimetype_is_stored() {
        let input = build(
            &[
                ("mimetype", b"application/epub+zip", DEFLATED),
                ("a.xhtml", &b"<p>text</p>".repeat(100), STORED),
            ],
            b"",
        );
        let out = bytes(run(&input, &presser(false)).unwrap());
        let a = archive(&out);
        assert_eq!(a.entries[0].method, STORED);
        assert_eq!(&out[a.entries[0].data.clone()], b"application/epub+zip");
    }

    #[test]
    fn signed_documents_are_untouched() {
        for name in [
            "META-INF/documentsignatures.xml",
            "META-INF/signatures.xml",
            "_xmlsignatures/sig1.xml",
            "META-INF/CERT.RSA",
        ] {
            let input = build(
                &[
                    ("a.xml", &b"<a/>".repeat(200), STORED),
                    (name, b"<sig/>", STORED),
                ],
                b"",
            );
            assert!(
                matches!(run(&input, &presser(false)).unwrap(), Pressed::Untouched(_)),
                "{name}"
            );
        }
    }

    #[test]
    fn optimal_archive_is_unchanged() {
        let input = build(&[("a.bin", &[7u8, 1, 9, 3, 5], STORED)], b"");
        assert!(matches!(
            run(&input, &presser(false)).unwrap(),
            Pressed::Unchanged(_)
        ));
    }

    #[test]
    fn nested_archives_are_followed() {
        let inner = odt();
        let outer = build(&[("inner.odt", &inner, STORED)], b"");
        let out = bytes(run(&outer, &presser(false)).unwrap());
        let nested = read(&out, "inner.odt");
        assert!(nested.len() < inner.len());
        assert_eq!(read(&nested, "content.xml"), read(&inner, "content.xml"));
    }

    #[test]
    fn extra_fields_follow_the_metadata_policy() {
        let unicode = [0x75, 0x70, 3, 0, 1, b'a', b'b'];
        let timestamp = [0x55, 0x54, 5, 0, 1, 1, 2, 3, 4];
        let extra = [unicode.as_slice(), &timestamp].concat();
        let xml = b"<a>x</a>".repeat(300);
        let mut w = Writer::with_capacity(0);
        let mut e = entry("a.xml", STORED, &xml);
        e.local_extra = extra.clone();
        e.central_extra = extra.clone();
        w.add(&e, &xml).unwrap();
        let input = w.finish(b"").unwrap();

        let out = bytes(run(&input, &presser(false)).unwrap());
        assert_eq!(archive(&out).entries[0].central_extra, unicode);
        let kept = bytes(run(&input, &presser(true)).unwrap());
        assert_eq!(archive(&kept).entries[0].central_extra, extra);
    }

    #[test]
    fn spent_budget_copies_entries_as_is() {
        let xml = b"<a>x</a>".repeat(300);
        let input = build(&[("a.xml", &xml, STORED), ("b.xml", &xml, STORED)], b"");
        // Enough for the first entry only: the second is copied stored.
        let out = bytes(press(&input, &presser(false), 0, &Cell::new(xml.len() as u64)).unwrap());
        let a = archive(&out);
        assert_eq!(
            (a.entries[0].method, a.entries[1].method),
            (DEFLATED, STORED)
        );
    }

    #[test]
    fn corrupt_entries_are_errors() {
        let xml = b"<a>x</a>".repeat(50);
        let mut w = Writer::with_capacity(0);
        let mut e = entry("a.xml", DEFLATED, &xml);
        e.crc ^= 1;
        w.add(&e, &deflate_at(&xml, 1)).unwrap();
        let input = w.finish(b"").unwrap();
        assert!(run(&input, &presser(false)).is_err());
    }
}
