//! The one EXIF field presse keeps by default: orientation (tag 0x0112).
//!
//! Re-encoding drops all metadata. Dropping orientation would show a
//! camera photo rotated, so it is read from the source EXIF and written
//! back as a minimal EXIF block holding nothing else.

const ORIENTATION_TAG: u16 = 0x0112;
const TYPE_SHORT: u16 = 3;

/// EXIF orientation from a TIFF-structured EXIF payload (the bytes after
/// JPEG's `Exif\0\0` prefix, or a PNG `eXIf` chunk). `None` when absent or
/// malformed.
pub(crate) fn orientation(tiff: &[u8]) -> Option<u16> {
    let big_endian = match tiff.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count).find_map(|i| {
        let entry = ifd.checked_add(2 + i * 12)?;
        (u16_at(entry)? == ORIENTATION_TAG && u16_at(entry + 2)? == TYPE_SHORT)
            .then(|| u16_at(entry + 8))
            .flatten()
    })
}

/// A TIFF-structured EXIF payload holding only `orientation`.
pub(crate) fn orientation_only(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::with_capacity(26);
    tiff.extend_from_slice(b"MM\0*");
    tiff.extend_from_slice(&8u32.to_be_bytes()); // IFD0 right after the header
    tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
    tiff.extend_from_slice(&ORIENTATION_TAG.to_be_bytes());
    tiff.extend_from_slice(&TYPE_SHORT.to_be_bytes());
    tiff.extend_from_slice(&1u32.to_be_bytes()); // one value
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]); // pad the 4-byte value field
    tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    tiff
}

/// The orientation worth keeping: anything but "absent" or 1 (the default,
/// which needs no tag).
pub(crate) fn keepable(orientation: Option<u16>) -> Option<u16> {
    orientation.filter(|&o| o != 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for o in 1..=8 {
            assert_eq!(orientation(&orientation_only(o)), Some(o));
        }
    }

    #[test]
    fn little_endian_with_other_tags() {
        // II header, IFD0 at 8 with two entries: Make (0x010F) then Orientation = 6.
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, b'A', b'B', b'C', 0]);
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        t.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(orientation(&t), Some(6));
    }

    #[test]
    fn malformed_is_none() {
        assert_eq!(orientation(b""), None);
        assert_eq!(orientation(b"MM\0*\xFF\xFF\xFF\xFF"), None);
        assert_eq!(orientation(b"XX\0*\0\0\0\x08"), None);
    }
}
