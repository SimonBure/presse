//! Minimal ZIP reading and writing (PKWARE APPNOTE.TXT): enough to rebuild
//! an archive entry by entry. Archives this cannot rebuild faithfully
//! (ZIP64, multi-disk, encrypted, prefixed by other data) are reported as
//! [`Parsed::Unsupported`] so the caller leaves them untouched.

use std::ops::Range;

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EXTRA: u16 = 0x0001;

const LOCAL_SIZE: usize = 30;
const CENTRAL_SIZE: usize = 46;
const END_SIZE: usize = 22;

pub(crate) const STORED: u16 = 0;
pub(crate) const DEFLATED: u16 = 8;

/// General-purpose flags: encryption (traditional, strong, central
/// directory) and the trailing data descriptor.
const FLAG_ENCRYPTED: u16 = 0x0001 | 0x0040 | 0x2000;
pub(crate) const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
/// Deflate option hint (bits 1–2), informational.
pub(crate) const FLAG_DEFLATE_OPTIONS: u16 = 0x0006;

/// One archive member, as its central directory record describes it.
#[derive(Clone)]
pub(crate) struct Entry {
    pub version_made_by: u16,
    pub version_needed: u16,
    pub flags: u16,
    pub method: u16,
    pub time: u16,
    pub date: u16,
    pub crc: u32,
    pub uncompressed_size: u32,
    pub name: Vec<u8>,
    pub local_extra: Vec<u8>,
    pub central_extra: Vec<u8>,
    pub comment: Vec<u8>,
    pub internal_attributes: u16,
    pub external_attributes: u32,
    /// Compressed bytes, as a range of the archive.
    pub data: Range<usize>,
    /// Offset of the local header (physical order).
    pub offset: usize,
}

pub(crate) struct Archive {
    /// In physical order (local header offsets).
    pub entries: Vec<Entry>,
    pub comment: Vec<u8>,
    /// Where the central directory starts.
    pub central_offset: usize,
}

pub(crate) enum Parsed {
    Archive(Archive),
    Unsupported(&'static str),
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn slice(b: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    b.get(at..at.checked_add(len)?)
}

/// Whether an extra field block holds a ZIP64 record.
fn has_zip64_extra(extra: &[u8]) -> bool {
    extra_fields(extra).any(|(id, _)| id == ZIP64_EXTRA)
}

/// The `(id, data)` records of an extra field block; stops at the first
/// malformed record.
pub(crate) fn extra_fields(extra: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let id = u16_at(extra, at)?;
        let len = u16_at(extra, at + 2)? as usize;
        let data = slice(extra, at + 4, len)?;
        at += 4 + len;
        Some((id, data))
    })
}

/// Locate the end-of-central-directory record: the last signature whose
/// comment fits in the file.
fn find_end(zip: &[u8]) -> Option<usize> {
    let last = zip.len().checked_sub(END_SIZE)?;
    let first = last.saturating_sub(u16::MAX as usize);
    (first..=last).rev().find(|&at| {
        u32_at(zip, at) == Some(END)
            && u16_at(zip, at + 20).is_some_and(|n| at + END_SIZE + n as usize <= zip.len())
    })
}

/// Parse an archive. `Err` means malformed (truncated, inconsistent).
pub(crate) fn parse(zip: &[u8]) -> Result<Parsed, String> {
    let malformed = |what: &str| format!("malformed ZIP: {what}");
    let end = find_end(zip).ok_or_else(|| malformed("no end of central directory"))?;
    let field16 = |at| u16_at(zip, end + at).unwrap_or(0);
    let (disk, central_disk) = (field16(4), field16(6));
    let (here, total) = (field16(8), field16(10));
    let central_size = u32_at(zip, end + 12).unwrap_or(0);
    let central_offset = u32_at(zip, end + 16).unwrap_or(0);
    let comment_len = field16(20) as usize;
    let comment = zip[end + END_SIZE..end + END_SIZE + comment_len].to_vec();

    let zip64_locator = end >= 20 && u32_at(zip, end - 20) == Some(ZIP64_LOCATOR);
    if zip64_locator
        || here == u16::MAX
        || total == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
    {
        return Ok(Parsed::Unsupported("ZIP64 archive"));
    }
    if disk != 0 || central_disk != 0 || here != total {
        return Ok(Parsed::Unsupported("multi-disk archive"));
    }
    let (central_offset, central_size) = (central_offset as usize, central_size as usize);
    if central_offset + central_size != end {
        // Self-extracting stubs and other prefixed data shift every offset.
        return Ok(Parsed::Unsupported(
            "data around the archive (self-extracting?)",
        ));
    }

    let mut entries = Vec::with_capacity(total as usize);
    let mut at = central_offset;
    for _ in 0..total {
        if u32_at(zip, at) != Some(CENTRAL) {
            return Err(malformed("bad central directory record"));
        }
        let f16 = |o| u16_at(zip, at + o).ok_or_else(|| malformed("truncated record"));
        let f32 = |o| u32_at(zip, at + o).ok_or_else(|| malformed("truncated record"));
        let (name_len, extra_len, comment_len) =
            (f16(28)? as usize, f16(30)? as usize, f16(32)? as usize);
        let var = at + CENTRAL_SIZE;
        let name = slice(zip, var, name_len).ok_or_else(|| malformed("truncated name"))?;
        let central_extra = slice(zip, var + name_len, extra_len)
            .ok_or_else(|| malformed("truncated extra field"))?;
        let entry_comment = slice(zip, var + name_len + extra_len, comment_len)
            .ok_or_else(|| malformed("truncated comment"))?;
        let flags = f16(8)?;
        let (compressed, uncompressed, offset) = (f32(20)?, f32(24)?, f32(42)?);
        if flags & FLAG_ENCRYPTED != 0 {
            return Ok(Parsed::Unsupported("encrypted entries"));
        }
        if f16(34)? != 0 {
            return Ok(Parsed::Unsupported("multi-disk archive"));
        }
        if [compressed, uncompressed, offset].contains(&u32::MAX) || has_zip64_extra(central_extra)
        {
            return Ok(Parsed::Unsupported("ZIP64 archive"));
        }

        let offset = offset as usize;
        if u32_at(zip, offset) != Some(LOCAL) {
            return Err(malformed("bad local header"));
        }
        let local_name_len = u16_at(zip, offset + 26).unwrap_or(0) as usize;
        let local_extra_len = u16_at(zip, offset + 28).unwrap_or(0) as usize;
        let start = offset + LOCAL_SIZE + local_name_len + local_extra_len;
        let local_extra = slice(zip, offset + LOCAL_SIZE + local_name_len, local_extra_len)
            .ok_or_else(|| malformed("truncated local header"))?;
        let stop = start
            .checked_add(compressed as usize)
            .filter(|&s| s <= central_offset)
            .ok_or_else(|| malformed("entry data overlaps the central directory"))?;

        entries.push(Entry {
            version_made_by: f16(4)?,
            version_needed: f16(6)?,
            flags,
            method: f16(10)?,
            time: f16(12)?,
            date: f16(14)?,
            crc: f32(16)?,
            uncompressed_size: uncompressed,
            name: name.to_vec(),
            local_extra: local_extra.to_vec(),
            central_extra: central_extra.to_vec(),
            comment: entry_comment.to_vec(),
            internal_attributes: f16(36)?,
            external_attributes: f32(38)?,
            data: start..stop,
            offset,
        });
        at = var + name_len + extra_len + comment_len;
    }
    if at != end {
        return Err(malformed("central directory size mismatch"));
    }
    entries.sort_by_key(|e| e.offset);
    if entries.windows(2).any(|w| w[0].data.end > w[1].offset) {
        return Err(malformed("overlapping entries"));
    }
    Ok(Parsed::Archive(Archive {
        entries,
        comment,
        central_offset,
    }))
}

/// Writes a fresh archive: every local header carries the final sizes, so
/// no data descriptors are needed.
pub(crate) struct Writer {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

impl Writer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            central: Vec::new(),
            count: 0,
        }
    }

    /// Append `entry` (its `data` range is ignored) with `compressed` as its
    /// stored bytes. `None` when the archive would need ZIP64.
    pub fn add(&mut self, entry: &Entry, compressed: &[u8]) -> Option<()> {
        let offset = u32::try_from(self.out.len()).ok()?;
        let size = u32::try_from(compressed.len()).ok()?;
        let name_len = u16::try_from(entry.name.len()).ok()?;
        let flags = entry.flags & !FLAG_DATA_DESCRIPTOR;
        self.count = self.count.checked_add(1).filter(|&c| c < u16::MAX)?;

        let o = &mut self.out;
        o.extend_from_slice(&LOCAL.to_le_bytes());
        o.extend_from_slice(&entry.version_needed.to_le_bytes());
        o.extend_from_slice(&flags.to_le_bytes());
        o.extend_from_slice(&entry.method.to_le_bytes());
        o.extend_from_slice(&entry.time.to_le_bytes());
        o.extend_from_slice(&entry.date.to_le_bytes());
        o.extend_from_slice(&entry.crc.to_le_bytes());
        o.extend_from_slice(&size.to_le_bytes());
        o.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
        o.extend_from_slice(&name_len.to_le_bytes());
        o.extend_from_slice(&u16::try_from(entry.local_extra.len()).ok()?.to_le_bytes());
        o.extend_from_slice(&entry.name);
        o.extend_from_slice(&entry.local_extra);
        o.extend_from_slice(compressed);

        let c = &mut self.central;
        c.extend_from_slice(&CENTRAL.to_le_bytes());
        c.extend_from_slice(&entry.version_made_by.to_le_bytes());
        c.extend_from_slice(&entry.version_needed.to_le_bytes());
        c.extend_from_slice(&flags.to_le_bytes());
        c.extend_from_slice(&entry.method.to_le_bytes());
        c.extend_from_slice(&entry.time.to_le_bytes());
        c.extend_from_slice(&entry.date.to_le_bytes());
        c.extend_from_slice(&entry.crc.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
        c.extend_from_slice(&name_len.to_le_bytes());
        c.extend_from_slice(&u16::try_from(entry.central_extra.len()).ok()?.to_le_bytes());
        c.extend_from_slice(&u16::try_from(entry.comment.len()).ok()?.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes()); // disk number
        c.extend_from_slice(&entry.internal_attributes.to_le_bytes());
        c.extend_from_slice(&entry.external_attributes.to_le_bytes());
        c.extend_from_slice(&offset.to_le_bytes());
        c.extend_from_slice(&entry.name);
        c.extend_from_slice(&entry.central_extra);
        c.extend_from_slice(&entry.comment);
        Some(())
    }

    pub fn finish(mut self, comment: &[u8]) -> Option<Vec<u8>> {
        let central_offset = u32::try_from(self.out.len()).ok()?;
        let central_size = u32::try_from(self.central.len()).ok()?;
        central_offset.checked_add(central_size)?;
        self.out.extend_from_slice(&self.central);
        let o = &mut self.out;
        o.extend_from_slice(&END.to_le_bytes());
        o.extend_from_slice(&[0, 0, 0, 0]); // this disk, central directory disk
        o.extend_from_slice(&self.count.to_le_bytes());
        o.extend_from_slice(&self.count.to_le_bytes());
        o.extend_from_slice(&central_size.to_le_bytes());
        o.extend_from_slice(&central_offset.to_le_bytes());
        o.extend_from_slice(&u16::try_from(comment.len()).ok()?.to_le_bytes());
        o.extend_from_slice(comment);
        Some(self.out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn entry(name: &str, method: u16, content: &[u8]) -> Entry {
        let mut crc = flate2::Crc::new();
        crc.update(content);
        Entry {
            version_made_by: 20,
            version_needed: 20,
            flags: 0,
            method,
            time: 0x6000,
            date: 0x5a21,
            crc: crc.sum(),
            uncompressed_size: content.len() as u32,
            name: name.as_bytes().to_vec(),
            local_extra: Vec::new(),
            central_extra: Vec::new(),
            comment: Vec::new(),
            internal_attributes: 0,
            external_attributes: 0,
            data: 0..0,
            offset: 0,
        }
    }

    pub(crate) fn deflate(data: &[u8], level: u32) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// An archive of `(name, content, method)` built with [`Writer`].
    pub(crate) fn build(files: &[(&str, &[u8], u16)], comment: &[u8]) -> Vec<u8> {
        let mut w = Writer::with_capacity(0);
        for (name, content, method) in files {
            let stored = if *method == DEFLATED {
                deflate(content, 1)
            } else {
                content.to_vec()
            };
            w.add(&entry(name, *method, content), &stored).unwrap();
        }
        w.finish(comment).unwrap()
    }

    fn archive(zip: &[u8]) -> Archive {
        match parse(zip).unwrap() {
            Parsed::Archive(a) => a,
            Parsed::Unsupported(why) => panic!("unsupported: {why}"),
        }
    }

    #[test]
    fn write_then_parse_round_trips() {
        let zip = build(
            &[
                ("mimetype", b"application/x-test", STORED),
                ("dir/", b"", STORED),
                ("dir/é.xml", &b"<a>hello</a>".repeat(50), DEFLATED),
            ],
            b"archive comment",
        );
        let a = archive(&zip);
        assert_eq!(a.comment, b"archive comment");
        let names: Vec<_> = a.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            [b"mimetype".to_vec(), b"dir/".to_vec(), "dir/é.xml".into()]
        );
        assert_eq!(&zip[a.entries[0].data.clone()], b"application/x-test");
        assert_eq!(a.entries[1].uncompressed_size, 0);
        assert_eq!(a.entries[2].method, DEFLATED);
        assert_eq!(a.entries[2].uncompressed_size, 600);
    }

    #[test]
    fn data_descriptors_are_read_from_the_central_directory() {
        // Local header with zero sizes + bit 3, sizes only in the central
        // directory and the trailing descriptor (streamed archives).
        let content = b"streamed content";
        let mut zip = build(&[("a.txt", content, STORED)], b"");
        zip[6] |= FLAG_DATA_DESCRIPTOR as u8;
        zip[18..26].fill(0);
        let a = archive(&zip);
        assert_eq!(&zip[a.entries[0].data.clone()], content);
    }

    #[test]
    fn unsupported_archives_are_reported() {
        let zip = build(&[("a", b"x", STORED)], b"");
        let reason = |z: &[u8]| match parse(z).unwrap() {
            Parsed::Unsupported(why) => why,
            Parsed::Archive(_) => "supported",
        };
        let mut encrypted = zip.clone();
        let central = archive(&zip).central_offset;
        encrypted[central + 8] |= 1;
        assert_eq!(reason(&encrypted), "encrypted entries");
        let prefixed = [b"MZ stub".as_slice(), &zip].concat();
        assert_eq!(
            reason(&prefixed),
            "data around the archive (self-extracting?)"
        );
        let mut zip64 = zip.clone();
        let end = zip64.len() - END_SIZE;
        zip64[end + 16..end + 20].fill(0xFF);
        assert_eq!(reason(&zip64), "ZIP64 archive");
    }

    #[test]
    fn malformed_archives_are_errors() {
        let zip = build(&[("a", b"x", STORED)], b"");
        assert!(parse(&zip[..zip.len() - 3]).is_err(), "truncated");
        assert!(parse(b"PK\x03\x04 not a zip").is_err());
        let mut lying = zip.clone();
        let central = archive(&zip).central_offset;
        lying[central + 20] = 0xF0; // compressed size runs into the directory
        assert!(parse(&lying).is_err());
    }
}
