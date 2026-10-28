//! Signed PDFs. A signature covers the file's exact bytes (its
//! `/ByteRange`), so any rewrite, even a lossless one, invalidates it.

use lopdf::{Dictionary, Document, Object};

/// Deepest direct-object nesting searched: catalog → `/AcroForm` →
/// `/Fields` → field → `/Kids` → widget → `/V` fits with room to spare.
const MAX_DEPTH: u8 = 8;

/// Whether `doc` holds a digital signature: an approval or certification
/// signature, a document timestamp or a usage-rights signature. An empty
/// signature field (nothing signed yet) does not count.
pub fn is_signed(doc: &Document) -> bool {
    doc.objects.values().any(|o| holds_signature(o, 0))
}

fn holds_signature(obj: &Object, depth: u8) -> bool {
    let deeper = |c: &Object| depth < MAX_DEPTH && holds_signature(c, depth + 1);
    match obj {
        Object::Dictionary(d) => is_signature_dictionary(d) || d.iter().any(|(_, v)| deeper(v)),
        Object::Array(a) => a.iter().any(deeper),
        _ => false,
    }
}

/// `/ByteRange` exists only in signature dictionaries (ISO 32000-1
/// §12.8.1, Table 252), alongside the `/Contents` it excludes.
fn is_signature_dictionary(d: &Dictionary) -> bool {
    matches!(d.get(b"ByteRange"), Ok(Object::Array(_))) && d.has(b"Contents")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{StringFormat, dictionary};

    fn signature() -> Dictionary {
        dictionary! {
            "Type" => "Sig",
            "Filter" => "Adobe.PPKLite",
            "SubFilter" => "adbe.pkcs7.detached",
            "ByteRange" => vec![0.into(), 10.into(), 20.into(), 30.into()],
            "Contents" => Object::String(vec![0; 16], StringFormat::Hexadecimal),
        }
    }

    #[test]
    fn signature_object_is_found() {
        let mut doc = Document::with_version("1.7");
        let sig = doc.add_object(signature());
        doc.add_object(
            dictionary! { "FT" => "Sig", "T" => Object::string_literal("S"), "V" => sig },
        );
        assert!(is_signed(&doc));
    }

    #[test]
    fn direct_signature_inside_a_field_is_found() {
        let mut doc = Document::with_version("1.7");
        let field = dictionary! { "FT" => "Sig", "V" => signature() };
        doc.add_object(dictionary! { "Fields" => vec![Object::Dictionary(field)] });
        assert!(is_signed(&doc));
    }

    #[test]
    fn empty_signature_field_is_not_signed() {
        let mut doc = Document::with_version("1.7");
        doc.add_object(dictionary! { "FT" => "Sig", "T" => Object::string_literal("S") });
        assert!(!is_signed(&doc));
    }
}
