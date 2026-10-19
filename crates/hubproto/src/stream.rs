// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The object stream, `application/x-braid-objects`: the 10 bytes
//! [`MAGIC`], then records, then a terminator.
//!
//! A record is 1 byte of kind code ([`Kind::code`]), the 32-byte id, the
//! payload's length as a little-endian u64, and the payload. The terminator
//! is one byte 0: a stream without it is truncated. [`Reader`] checks every
//! record's id against its kind and payload.

use std::io::{self, Read, Write};

use braid_format::{Kind, ObjectId};

/// The bytes an object stream starts with.
pub const MAGIC: &[u8; 10] = b"BRAIDOBJ1\n";

/// Why a stream could not be read.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// Reading failed.
    #[error("reading the object stream failed: {0}")]
    Io(#[from] io::Error),
    /// The stream does not start with [`MAGIC`].
    #[error("not an object stream")]
    Magic,
    /// The stream ended before its terminator.
    #[error("the object stream is truncated")]
    Truncated,
    /// A record names a kind that does not exist.
    #[error("unknown object kind code {0}")]
    Kind(u8),
    /// A record's payload is larger than the reader allows.
    #[error("object {id} is {len} bytes, more than the {limit} allowed")]
    TooLarge {
        /// The record's id.
        id: ObjectId,
        /// Its length.
        len: u64,
        /// The reader's limit.
        limit: u64,
    },
    /// A record's id is not the hash of its kind and payload.
    #[error("object {claimed} does not match its content, which hashes to {actual}")]
    Mismatch {
        /// The id the record claims.
        claimed: ObjectId,
        /// The id its kind and payload hash to.
        actual: ObjectId,
    },
}

/// Writes an object stream to `out`.
#[derive(Debug)]
pub struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    /// Starts a stream: writes [`MAGIC`].
    pub fn new(mut out: W) -> io::Result<Writer<W>> {
        out.write_all(MAGIC)?;
        Ok(Writer { out })
    }

    /// Writes one record.
    pub fn object(&mut self, kind: Kind, id: &ObjectId, payload: &[u8]) -> io::Result<()> {
        self.header(kind, id, payload.len() as u64)?;
        self.out.write_all(payload)
    }

    /// Writes the start of a record whose payload of `len` bytes the caller
    /// then writes through [`payload`](Self::payload), so that a large blob
    /// never has to be in memory whole. Anything but exactly `len` bytes
    /// breaks the stream.
    pub fn header(&mut self, kind: Kind, id: &ObjectId, len: u64) -> io::Result<()> {
        self.out.write_all(&[kind.code()])?;
        self.out.write_all(id.as_bytes())?;
        self.out.write_all(&len.to_le_bytes())
    }

    /// Where the payload announced by [`header`](Self::header) goes.
    pub fn payload(&mut self) -> &mut W {
        &mut self.out
    }

    /// Writes the terminator and returns the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(&[0])?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// One record of a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The object's kind.
    pub kind: Kind,
    /// Its id, checked against `kind` and `payload`.
    pub id: ObjectId,
    /// Its payload.
    pub payload: Vec<u8>,
}

/// Reads an object stream, checking every record.
#[derive(Debug)]
pub struct Reader<R: Read> {
    input: R,
    max_object: u64,
    done: bool,
}

impl<R: Read> Reader<R> {
    /// Starts reading: checks [`MAGIC`]. A record larger than `max_object`
    /// bytes is an error.
    pub fn new(mut input: R, max_object: u64) -> Result<Reader<R>, StreamError> {
        let mut magic = [0u8; MAGIC.len()];
        read_exact(&mut input, &mut magic)?;
        if &magic != MAGIC {
            return Err(StreamError::Magic);
        }
        Ok(Reader {
            input,
            max_object,
            done: false,
        })
    }

    /// The input, positioned after what was read: after the terminator, once
    /// [`next_record`](Self::next_record) has returned `None`, so a reader
    /// that refuses data after the terminator can look for it.
    pub fn into_inner(self) -> R {
        self.input
    }

    /// The next record, or `None` after the terminator.
    pub fn next_record(&mut self) -> Result<Option<Record>, StreamError> {
        if self.done {
            return Ok(None);
        }
        let mut code = [0u8; 1];
        read_exact(&mut self.input, &mut code)?;
        if code[0] == 0 {
            self.done = true;
            return Ok(None);
        }
        let kind = Kind::from_code(code[0]).ok_or(StreamError::Kind(code[0]))?;
        let mut id = [0u8; ObjectId::LEN];
        read_exact(&mut self.input, &mut id)?;
        let id = ObjectId::from_bytes(id);
        let mut len = [0u8; 8];
        read_exact(&mut self.input, &mut len)?;
        let len = u64::from_le_bytes(len);
        if len > self.max_object {
            return Err(StreamError::TooLarge {
                id,
                len,
                limit: self.max_object,
            });
        }
        let mut payload = Vec::new();
        (&mut self.input).take(len).read_to_end(&mut payload)?;
        if payload.len() as u64 != len {
            return Err(StreamError::Truncated);
        }
        let actual = ObjectId::hash(kind, &payload);
        if actual != id {
            return Err(StreamError::Mismatch {
                claimed: id,
                actual,
            });
        }
        Ok(Some(Record { kind, id, payload }))
    }
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<Record, StreamError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

/// `read_exact`, with an early end reported as [`StreamError::Truncated`].
fn read_exact<R: Read>(input: &mut R, buf: &mut [u8]) -> Result<(), StreamError> {
    input.read_exact(buf).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            StreamError::Truncated
        } else {
            StreamError::Io(error)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(objects: &[(Kind, &[u8])]) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new()).unwrap();
        for (kind, payload) in objects {
            writer
                .object(*kind, &ObjectId::hash(*kind, payload), payload)
                .unwrap();
        }
        writer.finish().unwrap()
    }

    #[test]
    fn round_trip() {
        let objects: Vec<(Kind, &[u8])> = vec![
            (Kind::Blob, b"hello\n"),
            (Kind::Blob, b""),
            (Kind::Tree, b"not checked here"),
        ];
        let bytes = stream(&objects);
        let records: Vec<Record> = Reader::new(&bytes[..], 1 << 20)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 3);
        for (record, (kind, payload)) in records.iter().zip(&objects) {
            assert_eq!(record.kind, *kind);
            assert_eq!(record.payload, *payload);
            assert_eq!(record.id, ObjectId::hash(*kind, payload));
        }
    }

    #[test]
    fn a_payload_written_after_its_header_reads_back() {
        let payload = b"streamed in pieces";
        let id = ObjectId::hash(Kind::Blob, payload);
        let mut writer = Writer::new(Vec::new()).unwrap();
        writer
            .header(Kind::Blob, &id, payload.len() as u64)
            .unwrap();
        writer.payload().write_all(&payload[..8]).unwrap();
        writer.payload().write_all(&payload[8..]).unwrap();
        let bytes = writer.finish().unwrap();
        assert_eq!(bytes, stream(&[(Kind::Blob, payload)]));
    }

    #[test]
    fn refuses_bad_streams() {
        let good = stream(&[(Kind::Blob, b"data")]);
        // No terminator.
        let cut = &good[..good.len() - 1];
        let result: Result<Vec<_>, _> = Reader::new(cut, 1 << 20).unwrap().collect();
        assert!(matches!(result, Err(StreamError::Truncated)));
        // Cut inside the payload.
        let cut = &good[..good.len() - 3];
        let result: Result<Vec<_>, _> = Reader::new(cut, 1 << 20).unwrap().collect();
        assert!(matches!(result, Err(StreamError::Truncated)));
        // A flipped payload byte.
        let mut flipped = good.clone();
        let at = flipped.len() - 2;
        flipped[at] ^= 1;
        let result: Result<Vec<_>, _> = Reader::new(&flipped[..], 1 << 20).unwrap().collect();
        assert!(matches!(result, Err(StreamError::Mismatch { .. })));
        // Too large.
        let result: Result<Vec<_>, _> = Reader::new(&good[..], 2).unwrap().collect();
        assert!(matches!(result, Err(StreamError::TooLarge { .. })));
        // Wrong magic, unknown kind.
        assert!(matches!(
            Reader::new(&b"BRAIDOBJ2\n\0"[..], 10),
            Err(StreamError::Magic)
        ));
        let mut unknown = MAGIC.to_vec();
        unknown.push(9);
        let result: Result<Vec<_>, _> = Reader::new(&unknown[..], 10).unwrap().collect();
        assert!(matches!(result, Err(StreamError::Kind(9))));
    }
}
