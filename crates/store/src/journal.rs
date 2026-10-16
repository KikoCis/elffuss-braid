// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Journals: one fixed-size entry per record appended to an open segment, so
//! readers find new objects without reading payloads (format spec, section 7).
//!
//! ```text
//! [32] id | u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
//! u64 raw_len | u32 record crc | u32 entry crc
//! ```
//!
//! Each entry is 64 bytes, starts at a multiple of 64, and carries a CRC of
//! its first 60 bytes. The only writer is the segment's lease holder, which
//! writes an entry after the record it describes. An entry that fails its CRC
//! was cut short by a crash (or damaged) and is skipped. A new lease holder
//! resumes right after the last valid entry, overwriting any torn tail.
//!
//! An entry whose CRC holds but that names an object kind or a codec this
//! version does not know, or sets its reserved field, is not torn: a newer
//! version of braid wrote it. Reading such a journal fails, so that this
//! version never takes the segment over and writes over what it does not
//! understand.

use std::fs::File;
use std::path::Path;

use braid_format::{Kind, ObjectId};

use crate::error::{Error, io_error};
use crate::fsutil::read_range;
use crate::record::{CODEC_CHUNKED, CODEC_MAX, CODEC_RAW, Header, array};

/// Why a journal slot or an index location holds no entry this version
/// can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unusable {
    /// Torn by a crash, or damaged: it fails its CRC, or holds values no
    /// writer produces.
    Invalid,
    /// Whole, but it names an object kind or a codec this version does not
    /// know, or sets its reserved field: a newer version of braid wrote it.
    Newer,
}

/// Bytes per journal entry.
pub(crate) const ENTRY_LEN: u64 = 64;

/// Where a record is and what it holds. Journals and indexes store the same
/// fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The object's id.
    pub id: ObjectId,
    /// Where the record starts in its segment.
    pub offset: u64,
    /// Bytes of payload in the record.
    pub stored_len: u32,
    /// What the object is.
    pub kind: Kind,
    /// How the payload is stored.
    pub codec: u8,
    /// Bytes of the object once decoded.
    pub raw_len: u64,
    /// The record's CRC.
    pub crc: u32,
}

impl Entry {
    /// The entry for a record at `offset`.
    pub fn new(offset: u64, header: &Header, crc: u32) -> Self {
        Self {
            id: header.id,
            offset,
            stored_len: header.stored_len,
            kind: header.kind,
            codec: header.codec,
            raw_len: header.raw_len,
            crc,
        }
    }

    /// The header the record must have.
    pub fn header(&self) -> Header {
        Header {
            kind: self.kind,
            codec: self.codec,
            stored_len: self.stored_len,
            raw_len: self.raw_len,
            id: self.id,
        }
    }

    /// Where the record ends, padding included.
    pub fn end(&self) -> u64 {
        self.offset + self.header().record_len()
    }

    /// The 64 bytes of a journal entry.
    pub fn encode(&self) -> [u8; ENTRY_LEN as usize] {
        self.encode_in(0)
    }

    /// The 64 bytes of an entry of a version 2 index: a journal entry whose
    /// reserved field holds the number of its segment in the index.
    pub fn encode_in(&self, segment: u16) -> [u8; ENTRY_LEN as usize] {
        let mut out = [0u8; ENTRY_LEN as usize];
        out[0..32].copy_from_slice(self.id.as_bytes());
        out[32..40].copy_from_slice(&self.offset.to_le_bytes());
        out[40..44].copy_from_slice(&self.stored_len.to_le_bytes());
        out[44] = self.kind.code();
        out[45] = self.codec;
        out[46..48].copy_from_slice(&segment.to_le_bytes());
        out[48..56].copy_from_slice(&self.raw_len.to_le_bytes());
        out[56..60].copy_from_slice(&self.crc.to_le_bytes());
        let entry_crc = crc32c::crc32c(&out[..60]);
        out[60..64].copy_from_slice(&entry_crc.to_le_bytes());
        out
    }

    /// Decodes a journal entry.
    pub fn decode(bytes: &[u8]) -> Result<Self, Unusable> {
        Self::check_crc(bytes)?;
        Self::from_fields(&bytes[0..32], &bytes[32..60])
    }

    /// Decodes an entry of a version 2 index, and the number of its
    /// segment in the index.
    pub fn decode_in(bytes: &[u8]) -> Result<(Self, u16), Unusable> {
        Self::check_crc(bytes)?;
        let segment = u16::from_le_bytes(array(&bytes[46..48]));
        let mut fields: [u8; 28] = array(&bytes[32..60]);
        fields[14..16].fill(0);
        Ok((Self::from_fields(&bytes[0..32], &fields)?, segment))
    }

    fn check_crc(bytes: &[u8]) -> Result<(), Unusable> {
        if bytes.len() != ENTRY_LEN as usize
            || crc32c::crc32c(&bytes[..60]) != u32::from_le_bytes(array(&bytes[60..64]))
        {
            return Err(Unusable::Invalid);
        }
        Ok(())
    }

    /// Builds an entry from an id and the 28 bytes that follow it in a journal
    /// entry (`offset` to `record crc`), checking the values.
    pub fn from_fields(id: &[u8], fields: &[u8]) -> Result<Self, Unusable> {
        let codec = fields[13];
        if codec > CODEC_MAX || u16::from_le_bytes(array(&fields[14..16])) != 0 {
            return Err(Unusable::Newer);
        }
        let entry = Self {
            id: ObjectId::from_bytes(array(id)),
            offset: u64::from_le_bytes(array(&fields[0..8])),
            stored_len: u32::from_le_bytes(array(&fields[8..12])),
            kind: Kind::from_code(fields[12]).ok_or(Unusable::Newer)?,
            codec,
            raw_len: u64::from_le_bytes(array(&fields[16..24])),
            crc: u32::from_le_bytes(array(&fields[24..28])),
        };
        let valid = (entry.codec != CODEC_RAW || entry.raw_len == u64::from(entry.stored_len))
            && (entry.codec != CODEC_CHUNKED || entry.kind == Kind::Blob)
            && entry.offset.is_multiple_of(crate::record::ALIGN)
            && entry
                .offset
                .checked_add(entry.header().record_len())
                .is_some();
        if valid {
            Ok(entry)
        } else {
            Err(Unusable::Invalid)
        }
    }
}

/// The valid entries of a journal from some offset on.
#[derive(Debug, Default)]
pub(crate) struct Read {
    /// Valid entries, in the order they were written.
    pub entries: Vec<Entry>,
    /// Where the entry after the last valid one starts: where reading resumes
    /// next time, and where a new lease holder writes.
    pub end: u64,
    /// Slots that failed their CRC, partial slots included.
    pub invalid: u64,
}

/// Reads the entries of `file`, the journal at `path`, from `from`, a
/// multiple of 64, to its end. Fails with [`Error::NewerStore`] at the
/// first entry a newer version of braid wrote.
pub(crate) fn read(file: &File, from: u64, path: &Path) -> Result<Read, Error> {
    let len = file.metadata().map_err(io_error("read", path))?.len();
    let mut read = Read {
        end: from,
        ..Read::default()
    };
    if len <= from {
        return Ok(read);
    }
    let bytes = read_range(file, from, len - from).map_err(io_error("read", path))?;
    let mut pending_invalid = 0;
    for (i, slot) in bytes.chunks(ENTRY_LEN as usize).enumerate() {
        match Entry::decode(slot) {
            Ok(entry) => {
                read.entries.push(entry);
                read.end = from + (i as u64 + 1) * ENTRY_LEN;
                read.invalid += pending_invalid;
                pending_invalid = 0;
            }
            Err(Unusable::Invalid) => pending_invalid += 1,
            Err(Unusable::Newer) => {
                return Err(Error::NewerStore {
                    path: path.to_path_buf(),
                    detail: format!(
                        "its entry at byte {} names an object kind or codec, or sets a \
                         field, that this version does not know",
                        from + i as u64 * ENTRY_LEN
                    ),
                });
            }
        }
    }
    read.invalid += pending_invalid;
    Ok(read)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;
    use crate::fsutil::write_all_at;
    use crate::record;

    fn entry(n: u8) -> Entry {
        let payload = vec![n; usize::from(n) * 10];
        let id = ObjectId::hash(Kind::Blob, &payload);
        let encoded = record::encode(Kind::Blob, id, &payload).unwrap();
        Entry::new(u64::from(n) * 1024, &encoded.header, encoded.crc)
    }

    #[test]
    fn round_trip() {
        for n in 0..20 {
            let entry = entry(n);
            assert_eq!(Entry::decode(&entry.encode()), Ok(entry));
        }
    }

    /// Sets a byte of an entry and fixes its CRC, as a newer writer would
    /// write it.
    fn newer(at: usize, value: u8) -> [u8; 64] {
        let mut bytes = entry(5).encode();
        bytes[at] = value;
        let crc = crc32c::crc32c(&bytes[..60]);
        bytes[60..64].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn entries_from_a_newer_version_stop_the_read() {
        // An unknown codec, an unknown kind, the reserved field set.
        for (at, value) in [(45, 5u8), (45, 0xff), (44, 9), (46, 1), (47, 0x80)] {
            assert_eq!(
                Entry::decode(&newer(at, value)),
                Err(Unusable::Newer),
                "byte {at}"
            );
        }
        // A whole entry with values no writer produces is damage, not news.
        let mut misaligned = entry(5).encode();
        misaligned[32] = 3;
        let crc = crc32c::crc32c(&misaligned[..60]);
        misaligned[60..64].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Entry::decode(&misaligned), Err(Unusable::Invalid));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        write_all_at(&file, &entry(0).encode(), 0).unwrap();
        write_all_at(&file, &newer(45, 5), ENTRY_LEN).unwrap();
        let err = read(&file, 0, &path).unwrap_err();
        assert!(matches!(err, Error::NewerStore { .. }), "{err}");
        assert!(err.to_string().contains("byte 64"), "{err}");
        // From after it, the rest reads as usual.
        assert!(
            read(&file, 2 * ENTRY_LEN, &path)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn layout_matches_the_spec() {
        let entry = entry(3);
        let bytes = entry.encode();
        assert_eq!(&bytes[0..32], entry.id.as_bytes());
        assert_eq!(&bytes[32..40], &3072u64.to_le_bytes());
        assert_eq!(&bytes[40..44], &entry.stored_len.to_le_bytes());
        assert_eq!(bytes[44], 1);
        assert_eq!(bytes[45], entry.codec);
        assert_eq!(&bytes[46..48], &[0, 0]);
        assert_eq!(&bytes[48..56], &30u64.to_le_bytes());
        assert_eq!(&bytes[56..60], &entry.crc.to_le_bytes());
        assert_eq!(&bytes[60..64], &crc32c::crc32c(&bytes[..60]).to_le_bytes());
    }

    #[test]
    fn index_entries_carry_their_segment() {
        let entry = entry(4);
        let bytes = entry.encode_in(0x0102);
        assert_eq!(&bytes[46..48], &[0x02, 0x01]);
        assert_eq!(Entry::decode_in(&bytes), Ok((entry, 0x0102)));
        // In a journal the field is reserved.
        assert_eq!(Entry::decode(&bytes), Err(Unusable::Newer));
        assert_eq!(Entry::decode_in(&entry.encode()), Ok((entry, 0)));
        let mut damaged = bytes;
        damaged[47] ^= 1;
        assert_eq!(Entry::decode_in(&damaged), Err(Unusable::Invalid));
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        let bytes = entry(7).encode();
        for i in 0..bytes.len() {
            let mut damaged = bytes;
            damaged[i] ^= 0x40;
            assert_eq!(Entry::decode(&damaged), Err(Unusable::Invalid), "byte {i}");
        }
        assert_eq!(Entry::decode(&[0u8; 64]), Err(Unusable::Invalid));
        assert_eq!(Entry::decode(&bytes[..63]), Err(Unusable::Invalid));
    }

    #[test]
    fn reading_skips_torn_and_damaged_slots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        for n in 0..4u8 {
            write_all_at(&file, &entry(n).encode(), u64::from(n) * ENTRY_LEN).unwrap();
        }
        // Damage the second entry, and leave half an entry at the end.
        write_all_at(&file, &[0xff], ENTRY_LEN + 5).unwrap();
        write_all_at(&file, &entry(9).encode()[..30], 4 * ENTRY_LEN).unwrap();

        let all = read(&file, 0, &path).unwrap();
        assert_eq!(all.entries, vec![entry(0), entry(2), entry(3)]);
        assert_eq!(all.end, 4 * ENTRY_LEN);
        assert_eq!(all.invalid, 2);

        let tail = read(&file, 3 * ENTRY_LEN, &path).unwrap();
        assert_eq!(tail.entries, vec![entry(3)]);
        assert_eq!(tail.end, 4 * ENTRY_LEN);

        let nothing = read(&file, 4 * ENTRY_LEN, &path).unwrap();
        assert!(nothing.entries.is_empty());
        assert_eq!(nothing.end, 4 * ENTRY_LEN);
        assert_eq!(nothing.invalid, 1);
    }
}
