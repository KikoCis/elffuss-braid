// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reading segments front to back: for fsck, for checking records before a
//! segment is sealed, and for rebuilding a damaged index.

use std::fs::File;
use std::io;

use crate::fsutil::read_exact_at;
use crate::journal::Entry;
use crate::record::{ALIGN, Checked, HEADER_LEN, check, parse_header};

/// Bytes read at a time when walking a file in order.
const CHUNK: u64 = 1 << 20;

/// A buffered view of a file for reads that mostly move forward.
#[derive(Debug)]
pub(crate) struct Window<'a> {
    file: &'a File,
    len: u64,
    start: u64,
    buf: Vec<u8>,
}

impl<'a> Window<'a> {
    /// A view of the first `len` bytes of `file`.
    pub fn new(file: &'a File, len: u64) -> Self {
        Self {
            file,
            len,
            start: 0,
            buf: Vec::new(),
        }
    }

    /// The `n` bytes at `offset`, or `None` if they run past the end.
    pub fn bytes(&mut self, offset: u64, n: u64) -> io::Result<Option<&[u8]>> {
        let Some(end) = offset.checked_add(n).filter(|end| *end <= self.len) else {
            return Ok(None);
        };
        if offset < self.start || end > self.start + self.buf.len() as u64 {
            let want = n.max(CHUNK).min(self.len - offset);
            let want =
                usize::try_from(want).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            self.buf.resize(want, 0);
            read_exact_at(self.file, &mut self.buf, offset)?;
            self.start = offset;
        }
        let at = (offset - self.start) as usize;
        Ok(Some(&self.buf[at..at + n as usize]))
    }
}

/// What a scan found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Records whose header and CRC are valid.
    pub records: u64,
    /// Bytes that are not part of any valid record.
    pub junk: u64,
}

/// Calls `visit(offset, record)` for every record of the first `len` bytes
/// of a segment whose header and CRC are valid, in order.
///
/// Bytes that do not start a valid record are the remains of a write cut
/// short by a crash, or damage. They are skipped 8 bytes at a time, looking
/// for the next sync word, and counted as junk. A valid record is stepped
/// over whole, so a payload that happens to contain records is never
/// mistaken for them.
pub(crate) fn scan(
    file: &File,
    len: u64,
    mut visit: impl FnMut(u64, &Checked<'_>),
) -> io::Result<Scan> {
    let mut window = Window::new(file, len);
    let mut scan = Scan::default();
    let mut offset = 0u64;
    while offset < len {
        let header = match window.bytes(offset, HEADER_LEN as u64)? {
            Some(bytes) => parse_header(bytes).ok(),
            None => None,
        };
        let mut record_len = None;
        if let Some(header) = header
            && let Some(bytes) = window.bytes(offset, header.unpadded_len())?
            && let Ok(record) = check(bytes)
        {
            visit(offset, &record);
            record_len = Some(record.header.record_len());
        }
        match record_len {
            Some(record_len) => {
                scan.records += 1;
                offset += record_len;
            }
            None => {
                let step = ALIGN.min(len - offset);
                scan.junk += step;
                offset += step;
            }
        }
    }
    Ok(scan)
}

/// Whether a valid record matching `entry` is where the entry says.
pub(crate) fn matches(window: &mut Window<'_>, entry: &Entry) -> io::Result<bool> {
    let header = entry.header();
    Ok(match window.bytes(entry.offset, header.unpadded_len())? {
        Some(bytes) => {
            matches!(check(bytes), Ok(found) if found.header == header && found.crc == entry.crc)
        }
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use braid_format::{Kind, ObjectId};

    use super::*;
    use crate::fsutil::write_all_at;
    use crate::record::encode;

    #[test]
    fn finds_records_around_junk() {
        let dir = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("s"))
            .unwrap();
        let payloads: Vec<Vec<u8>> = vec![
            b"first".to_vec(),
            b"second, a little longer and repeated ".repeat(10),
            b"third".to_vec(),
        ];
        let records: Vec<_> = payloads
            .iter()
            .map(|p| encode(Kind::Blob, ObjectId::hash(Kind::Blob, p), p).unwrap())
            .collect();

        // first record, half of a record cut short, then the others.
        let mut offset = 0u64;
        let mut expected = Vec::new();
        write_all_at(&file, &records[0].bytes, offset).unwrap();
        expected.push(offset);
        offset += records[0].bytes.len() as u64;
        let torn = &records[1].bytes[..records[1].bytes.len() / 2];
        write_all_at(&file, torn, offset).unwrap();
        offset = crate::record::align(offset + torn.len() as u64);
        for record in &records[1..] {
            write_all_at(&file, &record.bytes, offset).unwrap();
            expected.push(offset);
            offset += record.bytes.len() as u64;
        }

        let mut seen = Vec::new();
        let scan = scan(&file, offset, |at, record| {
            seen.push((at, record.header.id))
        })
        .unwrap();
        assert_eq!(scan.records, 3);
        assert_eq!(seen.iter().map(|(at, _)| *at).collect::<Vec<_>>(), expected);
        assert_eq!(
            seen.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            records.iter().map(|r| r.header.id).collect::<Vec<_>>()
        );
        assert_eq!(scan.junk, crate::record::align(torn.len() as u64));
    }

    #[test]
    fn records_inside_payloads_are_not_scanned() {
        // A blob that is itself a valid record, stored raw.
        let inner_payload = b"inner";
        let inner = encode(
            Kind::Blob,
            ObjectId::hash(Kind::Blob, inner_payload),
            inner_payload,
        )
        .unwrap();
        let mut outer_payload = vec![0u8; 4];
        outer_payload.extend_from_slice(&inner.bytes);
        let outer = encode(
            Kind::Blob,
            ObjectId::hash(Kind::Blob, &outer_payload),
            &outer_payload,
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("s"))
            .unwrap();
        write_all_at(&file, &outer.bytes, 0).unwrap();
        let mut ids = Vec::new();
        let scan = scan(&file, outer.bytes.len() as u64, |_, record| {
            ids.push(record.header.id)
        })
        .unwrap();
        assert_eq!(ids, vec![outer.header.id]);
        assert_eq!(scan.junk, 0);
    }
}
