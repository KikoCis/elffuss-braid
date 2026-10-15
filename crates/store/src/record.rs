// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Object records, the contents of segment files (format spec, section 7).
//!
//! ```text
//! u32 sync = 0xB7A1D5E9 | u8 kind | u8 codec | u16 flags = 0
//! u32 stored_len | u64 raw_len | [32] id
//! codec 2–3 only: [32] dictionary or reference object id
//! payload: stored_len bytes
//! u32 crc32c of everything from the sync word to the end of the payload
//! zero padding to a multiple of 8 bytes
//! ```
//!
//! Integers are little-endian. Records start at multiples of 8 bytes, so a
//! reader that meets the remains of a write cut short can find the next
//! record by looking for the sync word 8 bytes at a time.
//!
//! Payloads of up to 64 bytes are stored raw (codec 0). Larger ones are
//! compressed with zstd at level 3 (codec 1) when that makes them smaller.
//! Codecs 2 and 3 (a dictionary or a delta base) are reserved: their header
//! is parsed, so a scan can step over them, but this version cannot decode
//! them.

use std::io::Read;

use braid_format::{Kind, ObjectId};

/// The first four bytes of every record.
pub(crate) const SYNC: u32 = 0xB7A1_D5E9;

/// Bytes from the sync word to the payload, for codecs 0 and 1.
pub(crate) const HEADER_LEN: usize = 52;

/// Records start at multiples of this.
pub(crate) const ALIGN: u64 = 8;

/// Payloads up to this size are always stored raw.
pub(crate) const RAW_MAX: usize = 64;

/// The zstd level for compressed payloads.
const ZSTD_LEVEL: i32 = 3;

/// The payload is stored as it is.
pub(crate) const CODEC_RAW: u8 = 0;
/// The payload is one zstd frame.
pub(crate) const CODEC_ZSTD: u8 = 1;
/// The highest codec number the format defines (2 and 3 are reserved).
const CODEC_MAX: u8 = 3;

/// Bytes the CRC adds after the payload.
const CRC_LEN: usize = 4;

/// The fixed fields of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    /// What the object is.
    pub kind: Kind,
    /// How the payload is stored.
    pub codec: u8,
    /// Bytes of payload in the record.
    pub stored_len: u32,
    /// Bytes of the object once decoded.
    pub raw_len: u64,
    /// The object's id.
    pub id: ObjectId,
}

impl Header {
    /// Bytes from the sync word to the start of the payload.
    pub fn header_len(&self) -> u64 {
        header_len(self.codec) as u64
    }

    /// Bytes the CRC covers: the header and the payload.
    pub fn checked_len(&self) -> u64 {
        self.header_len() + u64::from(self.stored_len)
    }

    /// Bytes to read to check the record: the header, the payload and the CRC.
    pub fn unpadded_len(&self) -> u64 {
        self.checked_len() + CRC_LEN as u64
    }

    /// Bytes the record takes in its segment, padding included.
    pub fn record_len(&self) -> u64 {
        align(self.unpadded_len())
    }
}

/// Bytes from the sync word to the payload for a codec.
pub(crate) fn header_len(codec: u8) -> usize {
    if codec >= 2 {
        HEADER_LEN + 32
    } else {
        HEADER_LEN
    }
}

/// Rounds up to the next record boundary.
pub(crate) fn align(offset: u64) -> u64 {
    offset.next_multiple_of(ALIGN)
}

/// A record ready to append.
#[derive(Debug)]
pub(crate) struct Encoded {
    /// Its fixed fields.
    pub header: Header,
    /// Its CRC.
    pub crc: u32,
    /// All of its bytes, padding included.
    pub bytes: Vec<u8>,
}

/// Encodes the object `id`, which must be the hash of `payload` as `kind`.
/// Fails with the payload size if the stored form does not fit in a record.
pub(crate) fn encode(kind: Kind, id: ObjectId, payload: &[u8]) -> Result<Encoded, u64> {
    let compressed = if payload.len() > RAW_MAX {
        zstd::bulk::compress(payload, ZSTD_LEVEL)
            .ok()
            .filter(|compressed| compressed.len() < payload.len())
    } else {
        None
    };
    let (codec, stored) = match &compressed {
        Some(compressed) => (CODEC_ZSTD, compressed.as_slice()),
        None => (CODEC_RAW, payload),
    };
    let stored_len = u32::try_from(stored.len()).map_err(|_| payload.len() as u64)?;
    let header = Header {
        kind,
        codec,
        stored_len,
        raw_len: payload.len() as u64,
        id,
    };
    let record_len = usize::try_from(header.record_len()).map_err(|_| payload.len() as u64)?;
    let mut bytes = Vec::with_capacity(record_len);
    bytes.extend_from_slice(&SYNC.to_le_bytes());
    bytes.push(kind.code());
    bytes.push(codec);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&stored_len.to_le_bytes());
    bytes.extend_from_slice(&header.raw_len.to_le_bytes());
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(stored);
    let crc = crc32c::crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.resize(record_len, 0);
    Ok(Encoded { header, crc, bytes })
}

/// Parses the fixed fields at the start of `bytes`, rejecting anything a
/// writer would never produce.
pub(crate) fn parse_header(bytes: &[u8]) -> Result<Header, &'static str> {
    if bytes.len() < HEADER_LEN {
        return Err("record header cut short");
    }
    if u32::from_le_bytes(array(&bytes[0..4])) != SYNC {
        return Err("no sync word");
    }
    let kind = Kind::from_code(bytes[4]).ok_or("unknown object kind")?;
    let codec = bytes[5];
    if codec > CODEC_MAX {
        return Err("unknown codec");
    }
    if u16::from_le_bytes(array(&bytes[6..8])) != 0 {
        return Err("unknown flags");
    }
    let stored_len = u32::from_le_bytes(array(&bytes[8..12]));
    let raw_len = u64::from_le_bytes(array(&bytes[12..20]));
    if codec == CODEC_RAW && raw_len != u64::from(stored_len) {
        return Err("raw payload with two different lengths");
    }
    let id = ObjectId::from_bytes(array(&bytes[20..52]));
    Ok(Header {
        kind,
        codec,
        stored_len,
        raw_len,
        id,
    })
}

/// Checks the record at the start of `bytes`, which must hold at least its
/// unpadded length: the header and the CRC. Returns the header, the CRC and
/// the stored payload.
pub(crate) fn check(bytes: &[u8]) -> Result<(Header, u32, &[u8]), &'static str> {
    let header = parse_header(bytes)?;
    let checked = usize::try_from(header.checked_len()).map_err(|_| "record too large")?;
    if bytes.len() < checked + CRC_LEN {
        return Err("record cut short");
    }
    let crc = u32::from_le_bytes(array(&bytes[checked..checked + CRC_LEN]));
    if crc32c::crc32c(&bytes[..checked]) != crc {
        return Err("checksum mismatch");
    }
    let start = header_len(header.codec);
    Ok((header, crc, &bytes[start..checked]))
}

/// Restores the payload of a checked record and verifies that it hashes to
/// the record's id.
pub(crate) fn decode(header: &Header, stored: &[u8]) -> Result<Vec<u8>, &'static str> {
    let payload = match header.codec {
        CODEC_RAW => stored.to_vec(),
        CODEC_ZSTD => decompress(stored, header.raw_len)?,
        _ => return Err("codec not supported by this version"),
    };
    if ObjectId::hash(header.kind, &payload) != header.id {
        return Err("content does not hash to its id");
    }
    Ok(payload)
}

fn decompress(stored: &[u8], raw_len: u64) -> Result<Vec<u8>, &'static str> {
    // Stream with a cap instead of trusting raw_len for one allocation: a
    // damaged length then costs at most the data actually there.
    let decoder = zstd::stream::read::Decoder::with_buffer(stored).map_err(|_| "bad zstd data")?;
    let mut payload = Vec::with_capacity(usize::try_from(raw_len.min(1 << 24)).unwrap_or(0));
    decoder
        .take(raw_len.saturating_add(1))
        .read_to_end(&mut payload)
        .map_err(|_| "bad zstd data")?;
    if payload.len() as u64 != raw_len {
        return Err("decoded length does not match the record");
    }
    Ok(payload)
}

/// Copies a slice of known length into an array.
pub(crate) fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(kind: Kind, payload: &[u8]) -> Encoded {
        let id = ObjectId::hash(kind, payload);
        let encoded = encode(kind, id, payload).unwrap();
        assert_eq!(encoded.bytes.len() as u64, encoded.header.record_len());
        assert_eq!(encoded.bytes.len() % ALIGN as usize, 0);
        let (header, crc, stored) = check(&encoded.bytes).unwrap();
        assert_eq!(header, encoded.header);
        assert_eq!(crc, encoded.crc);
        assert_eq!(decode(&header, stored).unwrap(), payload);
        encoded
    }

    #[test]
    fn small_payloads_are_raw() {
        for len in [0, 1, 63, 64] {
            let payload = vec![b'a'; len];
            let encoded = round_trip(Kind::Blob, &payload);
            assert_eq!(encoded.header.codec, CODEC_RAW, "{len}");
            assert_eq!(encoded.header.stored_len as usize, len);
        }
    }

    #[test]
    fn compressible_payloads_use_zstd() {
        let payload = b"the same line again and again\n".repeat(100);
        let encoded = round_trip(Kind::Tree, &payload);
        assert_eq!(encoded.header.codec, CODEC_ZSTD);
        assert!((encoded.header.stored_len as usize) < payload.len());
        assert_eq!(encoded.header.raw_len as usize, payload.len());
    }

    #[test]
    fn incompressible_payloads_stay_raw() {
        let payload: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let id = ObjectId::hash(Kind::Blob, &payload);
        if zstd::bulk::compress(&payload, ZSTD_LEVEL).unwrap().len() >= payload.len() {
            assert_eq!(
                encode(Kind::Blob, id, &payload).unwrap().header.codec,
                CODEC_RAW
            );
        }
        round_trip(Kind::Blob, &payload);
    }

    #[test]
    fn layout_matches_the_spec() {
        let encoded = round_trip(Kind::Blob, b"hello\n");
        let bytes = &encoded.bytes;
        assert_eq!(&bytes[0..4], &[0xE9, 0xD5, 0xA1, 0xB7]);
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], CODEC_RAW);
        assert_eq!(&bytes[6..8], &[0, 0]);
        assert_eq!(&bytes[8..12], &6u32.to_le_bytes());
        assert_eq!(&bytes[12..20], &6u64.to_le_bytes());
        assert_eq!(
            &bytes[20..52],
            ObjectId::hash(Kind::Blob, b"hello\n").as_bytes()
        );
        assert_eq!(&bytes[52..58], b"hello\n");
        let crc = crc32c::crc32c(&bytes[..58]);
        assert_eq!(&bytes[58..62], &crc.to_le_bytes());
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[62..], &[0, 0]);
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        for payload in [&b"hello\n"[..], &b"compress me please ".repeat(20)] {
            let encoded = round_trip(Kind::Blob, payload);
            let checked = encoded.header.unpadded_len() as usize;
            for i in 0..checked {
                let mut damaged = encoded.bytes.clone();
                damaged[i] ^= 0x01;
                let caught = match check(&damaged) {
                    Err(_) => true,
                    Ok((header, _, stored)) => decode(&header, stored).is_err(),
                };
                assert!(caught, "flip at byte {i} went unnoticed");
            }
        }
    }

    #[test]
    fn cut_records_are_rejected() {
        let encoded = round_trip(Kind::Blob, b"hello\n");
        for len in 0..encoded.header.unpadded_len() as usize {
            assert!(check(&encoded.bytes[..len]).is_err(), "{len}");
        }
    }

    #[test]
    fn wrong_id_is_rejected_after_decoding() {
        let payload = b"some content";
        let wrong = ObjectId::hash(Kind::Blob, b"other content");
        let encoded = encode(Kind::Blob, wrong, payload).unwrap();
        let (header, _, stored) = check(&encoded.bytes).unwrap();
        assert_eq!(
            decode(&header, stored),
            Err("content does not hash to its id")
        );
    }

    #[test]
    fn reserved_codecs_parse_but_do_not_decode() {
        let mut encoded = round_trip(Kind::Blob, b"x").bytes;
        encoded[5] = 2;
        let header = parse_header(&encoded).unwrap();
        assert_eq!(header.header_len(), HEADER_LEN as u64 + 32);
        encoded[5] = 4;
        assert_eq!(parse_header(&encoded), Err("unknown codec"));
    }
}
