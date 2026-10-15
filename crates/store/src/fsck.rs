// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Checking a whole store.
//!
//! fsck reads everything from disk, independently of what the handle has
//! cached. Leftovers of crashes are expected and only counted: the end of a
//! record cut short in a segment or in the op log, journal entries that fail
//! their CRC, records no entry points at, temporary files. Everything that
//! could make the store return wrong data or lose data is a [`Problem`].
//!
//! Records stored whole are decoded as each segment is read. Deltas and
//! chunked blobs need other objects, so they are checked once every segment
//! has been read: a delta's base must be an object stored whole, a chunked
//! blob's chunks must be blobs of the sizes its list gives, and what they
//! decode to must hash to the record's id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use braid_format::{IdHasher, Kind, ObjectId};

use crate::chunks;
use crate::delta;
use crate::error::Error;
use crate::fsutil::{count_temps, read_range};
use crate::index::IndexError;
use crate::journal::{self, Entry};
use crate::oplog::{Bad, decode_checkpoint, next_record, not_in_transaction, parse_record};
use crate::record::{self, CODEC_CHUNKED, CODEC_DELTA, Header};
use crate::segments::{BaseCache, Files, list, segment_path};
use crate::{Store, format_file, index, scan};

/// What fsck found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    /// Segment files.
    pub segments: usize,
    /// Segments that are sealed.
    pub sealed_segments: usize,
    /// Records whose checksum is valid.
    pub records: u64,
    /// Of those, records holding a delta against a base object.
    pub delta_records: u64,
    /// Of those, records holding the chunk list of a blob stored in chunks.
    pub chunked_records: u64,
    /// Distinct objects that an index or a journal leads to.
    pub objects: u64,
    /// Valid records that no index or journal entry points at: appends whose
    /// journal entry a crash prevented, or copies left out of an index.
    pub unindexed_records: u64,
    /// Bytes of segments that are not part of any valid record: appends cut
    /// short by a crash. They are skipped.
    pub torn_segment_bytes: u64,
    /// Journal entries that fail their checksum. They are skipped.
    pub torn_journal_entries: u64,
    /// Operations in the op log.
    pub operations: u64,
    /// Bytes after the last operation record: an append cut short by a crash.
    /// Readers skip them, and the next transaction cuts them off.
    pub torn_log_bytes: u64,
    /// Checkpoints checked.
    pub checkpoints: usize,
    /// Refs as of the newest operation.
    pub refs: usize,
    /// Temporary files: being written now, or left by writers that died.
    pub temporary_files: usize,
    /// Everything that is wrong. Empty for a healthy store.
    pub problems: Vec<Problem>,
}

impl FsckReport {
    /// Whether no problem was found. Leftovers of crashes do not count.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Something wrong in a store.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Problem {
    /// The `format` file is missing, unreadable, or names another format.
    Format {
        /// What is wrong with it.
        detail: String,
    },
    /// A file or directory that could not be read.
    Unreadable {
        /// Its path within the store.
        path: String,
        /// The error.
        detail: String,
    },
    /// A record with a valid checksum whose content cannot be decoded or does
    /// not hash to its id.
    BadRecord {
        /// The segment's name.
        segment: String,
        /// Where the record starts.
        offset: u64,
        /// What failed.
        detail: String,
    },
    /// An index or journal entry that does not lead to a whole record of its
    /// object.
    BadEntry {
        /// The segment's name.
        segment: String,
        /// The object the entry names.
        id: ObjectId,
        /// Where the entry says the record is.
        offset: u64,
        /// What failed.
        detail: String,
    },
    /// An index that fails its checksum or its structure checks. Lookups work
    /// around it by scanning the segment.
    BadIndex {
        /// The segment's name.
        segment: String,
        /// What failed.
        detail: String,
    },
    /// An op log record that is damaged with valid records after it, out of
    /// sequence, or inconsistent with the refs before it.
    BadOperation {
        /// Where the record starts in the log.
        offset: u64,
        /// What failed.
        detail: String,
    },
    /// A checkpoint that cannot be read or disagrees with the log.
    BadCheckpoint {
        /// The checkpoint's file name.
        file: String,
        /// What failed.
        detail: String,
    },
    /// A ref whose target is not in the store.
    MissingObject {
        /// The ref.
        name: String,
        /// The object it points at.
        id: ObjectId,
    },
    /// A journal or an index that a newer version of braid wrote, in a store
    /// whose `format` file does not say so. Its segment is not checked.
    Newer {
        /// Its path within the store.
        path: String,
        /// What this version does not know.
        detail: String,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Format { detail } => write!(f, "format file: {detail}"),
            Problem::Newer { path, detail } => {
                write!(f, "{path}: written by a newer version of braid: {detail}")
            }
            Problem::Unreadable { path, detail } => write!(f, "{path}: cannot read: {detail}"),
            Problem::BadRecord {
                segment,
                offset,
                detail,
            } => write!(f, "segment {segment}, record at byte {offset}: {detail}"),
            Problem::BadEntry {
                segment,
                id,
                offset,
                detail,
            } => write!(
                f,
                "segment {segment}, entry for {id} at byte {offset}: {detail}"
            ),
            Problem::BadIndex { segment, detail } => {
                write!(f, "segment {segment}, index: {detail}")
            }
            Problem::BadOperation { offset, detail } => {
                write!(f, "op log, record at byte {offset}: {detail}")
            }
            Problem::BadCheckpoint { file, detail } => write!(f, "op log, {file}: {detail}"),
            Problem::MissingObject { name, id } => {
                write!(f, "ref {name:?} points at {id}, which is not in the store")
            }
        }
    }
}

/// A record found by scanning, and whether its content checks out.
struct Found {
    header: Header,
    crc: u32,
    content: Content,
}

/// Whether a record's content checks out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    /// It decodes and hashes to its id.
    Good,
    /// It does not.
    Bad,
    /// A delta or a chunk list: checked once every segment is read.
    Later,
}

/// A record that an entry leads to, by segment number and offset.
#[derive(Clone, Copy, Debug)]
struct Located {
    segment: usize,
    offset: u64,
    header: Header,
}

/// What the first pass over the segments collects for the second.
#[derive(Default)]
struct Collected {
    /// Segment paths, by number.
    segments: Vec<PathBuf>,
    /// Objects an entry leads to, whose content checks out.
    objects: HashSet<ObjectId>,
    /// Of those, a copy stored whole of each object.
    whole: HashMap<ObjectId, Located>,
    /// Deltas and chunk lists that an entry leads to, to check.
    later: Vec<Located>,
    /// Of those, the deltas that checked out, for chunks stored as deltas.
    deltas: HashMap<ObjectId, Located>,
}

impl Store {
    /// Checks the whole store: every segment record (checksum, and that its
    /// content hashes to its id), every journal and index entry (it leads to
    /// a whole record of the same object), every delta (its base is an
    /// object stored whole, and applying it gives its id), every chunked
    /// blob (its chunks are there with the sizes it lists, and together hash
    /// to its id), every op log record (checksum, sequence, and that each
    /// update's old value matches), every checkpoint, and that every ref
    /// points at an object in the store.
    ///
    /// It can run while other processes use the store. Errors are only
    /// returned when fsck itself cannot run; damage goes in the report. Like
    /// the other op log reads, it cannot run inside a transaction's closure.
    pub fn fsck(&self) -> Result<FsckReport, Error> {
        not_in_transaction()?;
        let mut report = FsckReport::default();
        let storage = self.check_format(&mut report);
        // The op log first: every object a record points at was durable
        // before the record, so the segments listed afterwards hold it.
        let refs = self.check_oplog(&mut report)?;
        let objects = self.check_segments(&mut report);
        // Records that need a storage version the store does not declare
        // could be read by a version of braid that does not know them.
        if let Some(storage) = storage
            && storage < 2
            && report.delta_records + report.chunked_records > 0
        {
            report.problems.push(Problem::Format {
                detail: format!(
                    "it says storage version {storage}, but the store holds deltas or chunked \
                     blobs, which need version 2"
                ),
            });
        }
        report.objects = objects.len() as u64;
        report.refs = refs.len();
        for (name, id) in refs {
            if !objects.contains(&id) {
                report.problems.push(Problem::MissingObject { name, id });
            }
        }
        report.temporary_files = [
            self.root.clone(),
            self.root.join("seg"),
            self.root.join("oplog"),
        ]
        .iter()
        .map(|dir| count_temps(dir))
        .sum();
        Ok(report)
    }

    /// Checks the `format` file, and returns the storage version it says.
    fn check_format(&self, report: &mut FsckReport) -> Option<u32> {
        match format_file::read(&self.root) {
            Ok(storage) => Some(storage),
            Err(err) => {
                report.problems.push(Problem::Format {
                    detail: err.to_string(),
                });
                None
            }
        }
    }

    /// Replays the whole log and checks the checkpoints against it. Returns
    /// the refs as of the newest operation.
    fn check_oplog(&self, report: &mut FsckReport) -> Result<BTreeMap<String, ObjectId>, Error> {
        // Shared: no transaction runs while the log and checkpoints are read.
        let _shared = self.oplog.lock(false)?;
        let mut refs = BTreeMap::new();
        let bytes = match fs::read(self.oplog.log_path()) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                report.problems.push(unreadable("oplog/log", &err));
                return Ok(refs);
            }
        };
        let checkpoints = match self.oplog.checkpoints() {
            Ok(checkpoints) => checkpoints,
            Err(err) => {
                report.problems.push(Problem::Unreadable {
                    path: "oplog".to_owned(),
                    detail: err.to_string(),
                });
                Vec::new()
            }
        };
        let wanted: HashSet<u64> = checkpoints.iter().map(|(seq, _)| *seq).collect();
        let mut snapshots = HashMap::new();

        let mut seq = 0u64;
        let mut at = 0usize;
        while at < bytes.len() {
            let offset = at as u64;
            match parse_record(&bytes[at..]) {
                Ok((op, len)) => {
                    if op.seq != seq + 1 {
                        report.problems.push(Problem::BadOperation {
                            offset,
                            detail: format!("operation {} follows operation {seq}", op.seq),
                        });
                    }
                    for update in &op.updates {
                        let current = refs.get(&update.name).copied();
                        if current != update.old {
                            report.problems.push(Problem::BadOperation {
                                offset,
                                detail: format!(
                                    "operation {} expects ref {:?} at {}, but it was at {}",
                                    op.seq,
                                    update.name,
                                    show(update.old),
                                    show(current)
                                ),
                            });
                        }
                        match update.new {
                            Some(id) => refs.insert(update.name.clone(), id),
                            None => refs.remove(&update.name),
                        };
                    }
                    seq = op.seq;
                    at += len as usize;
                    report.operations += 1;
                    if wanted.contains(&seq) {
                        snapshots.insert(seq, (refs.clone(), at as u64));
                    }
                }
                Err(Bad::Damaged(detail, len)) => {
                    report.problems.push(Problem::BadOperation {
                        offset,
                        detail: detail.to_owned(),
                    });
                    at += len as usize;
                }
                Err(Bad::Torn) => match next_record(&bytes, at + 1, seq) {
                    Some(next) => {
                        report.problems.push(Problem::BadOperation {
                            offset,
                            detail: "damaged record with valid records after it".to_owned(),
                        });
                        at = next;
                    }
                    None => {
                        report.torn_log_bytes = (bytes.len() - at) as u64;
                        break;
                    }
                },
            }
        }

        for (checkpoint_seq, path) in checkpoints {
            let file = format!("ckpt.{checkpoint_seq}");
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                // Removed since the listing: a newer one replaced it.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    report
                        .problems
                        .push(unreadable(&format!("oplog/{file}"), &err));
                    continue;
                }
            };
            report.checkpoints += 1;
            let detail = match decode_checkpoint(&bytes) {
                Err(detail) => detail.to_owned(),
                Ok(checkpoint) if checkpoint.refs.seq() != checkpoint_seq => {
                    format!("holds the refs of operation {}", checkpoint.refs.seq())
                }
                Ok(checkpoint) => match snapshots.get(&checkpoint_seq) {
                    None => "names an operation that is not in the log".to_owned(),
                    Some((expected, end))
                        if *expected == *checkpoint.refs.as_map() && *end == checkpoint.end =>
                    {
                        continue;
                    }
                    Some(_) => "disagrees with the log".to_owned(),
                },
            };
            report
                .problems
                .push(Problem::BadCheckpoint { file, detail });
        }
        Ok(refs)
    }

    /// Checks every segment. Returns the objects that an entry leads to and
    /// whose content checks out.
    fn check_segments(&self, report: &mut FsckReport) -> HashSet<ObjectId> {
        let dir = self.root.join("seg");
        let mut collected = Collected::default();
        let listing = match list(&dir) {
            Ok(listing) => listing,
            Err(err) => {
                report.problems.push(Problem::Unreadable {
                    path: "seg".to_owned(),
                    detail: err.to_string(),
                });
                return collected.objects;
            }
        };
        for (name, files) in listing {
            if !files.seg {
                if files.idx {
                    report.problems.push(Problem::BadIndex {
                        segment: name,
                        detail: "the segment file is missing".to_owned(),
                    });
                }
                continue;
            }
            report.segments += 1;
            check_segment(&dir, &name, files, report, &mut collected);
        }
        collected.check_later(report);
        collected.objects
    }
}

fn check_segment(
    dir: &Path,
    name: &str,
    files: Files,
    report: &mut FsckReport,
    collected: &mut Collected,
) {
    // Entries before records: an entry is written after its record, so every
    // record an entry read now points at is already in the segment.
    let Some(Entries {
        entries,
        rebuild,
        sealed_len,
    }) = segment_entries(dir, name, files, report)
    else {
        return;
    };

    let seg_path = segment_path(dir, name, "seg");
    let seg_name = format!("seg/{name}.seg");
    let file = match File::open(&seg_path) {
        Ok(file) => file,
        Err(err) => {
            report.problems.push(unreadable(&seg_name, &err));
            return;
        }
    };
    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) => {
            report.problems.push(unreadable(&seg_name, &err));
            return;
        }
    };
    if let Some(sealed_len) = sealed_len
        && sealed_len != len
    {
        report.problems.push(Problem::BadIndex {
            segment: name.to_owned(),
            detail: format!("the segment was {sealed_len} bytes when sealed and is {len} now"),
        });
    }
    let mut records = BTreeMap::new();
    let mut problems = Vec::new();
    let scanned = scan::scan(&file, len, |offset, found| {
        let codec = found.header.codec;
        let content = if codec == CODEC_DELTA || codec == CODEC_CHUNKED {
            Content::Later
        } else {
            match record::decode(&found.header, found.stored) {
                Ok(_) => Content::Good,
                Err(detail) => {
                    problems.push(Problem::BadRecord {
                        segment: name.to_owned(),
                        offset,
                        detail: format!("object {}: {detail}", found.header.id),
                    });
                    Content::Bad
                }
            }
        };
        records.insert(
            offset,
            Found {
                header: found.header,
                crc: found.crc,
                content,
            },
        );
    });
    match scanned {
        Ok(scanned) => {
            report.records += scanned.records;
            report.torn_segment_bytes += scanned.junk;
        }
        Err(err) => {
            report.problems.push(unreadable(&seg_name, &err));
            return;
        }
    }
    report.problems.append(&mut problems);
    for found in records.values() {
        match found.header.codec {
            CODEC_DELTA => report.delta_records += 1,
            CODEC_CHUNKED => report.chunked_records += 1,
            _ => {}
        }
    }
    let segment = collected.segments.len();
    collected.segments.push(seg_path);

    // A damaged index is worked around by scanning, as lookups do.
    let entries = if rebuild {
        records
            .iter()
            .map(|(offset, found)| Entry::new(*offset, &found.header, found.crc))
            .collect()
    } else {
        entries
    };
    let mut referenced = HashSet::new();
    for entry in &entries {
        let bad_entry = |detail: &str| Problem::BadEntry {
            segment: name.to_owned(),
            id: entry.id,
            offset: entry.offset,
            detail: detail.to_owned(),
        };
        match records.get(&entry.offset) {
            None => report
                .problems
                .push(bad_entry("no whole record starts there")),
            Some(found) => {
                referenced.insert(entry.offset);
                if found.header != entry.header() || found.crc != entry.crc {
                    report
                        .problems
                        .push(bad_entry("the record there is another one"));
                    continue;
                }
                let copy = Located {
                    segment,
                    offset: entry.offset,
                    header: found.header,
                };
                match found.content {
                    Content::Good => {
                        collected.objects.insert(entry.id);
                        collected.whole.entry(entry.id).or_insert(copy);
                    }
                    Content::Later => collected.later.push(copy),
                    Content::Bad => {}
                }
            }
        }
    }
    report.unindexed_records += (records.len() - referenced.len()) as u64;
}

impl Collected {
    /// The second pass: deltas, then chunked blobs, whose chunks may be
    /// deltas. A record that checks out adds its object; one that does not
    /// is a bad record.
    fn check_later(&mut self, report: &mut FsckReport) {
        let named = self.named();
        let mut reader = Reader {
            segments: &self.segments,
            files: HashMap::new(),
            bases: BaseCache::default(),
        };
        let (deltas, lists): (Vec<Located>, Vec<Located>) = std::mem::take(&mut self.later)
            .into_iter()
            .partition(|copy| copy.header.codec == CODEC_DELTA);
        let mut problems = Vec::new();
        for copy in deltas {
            match reader.delta(&copy, &self.whole, &named) {
                Ok(_) => {
                    self.objects.insert(copy.header.id);
                    self.deltas.entry(copy.header.id).or_insert(copy);
                }
                Err(detail) => problems.push(reader.bad_record(&copy, &detail)),
            }
        }
        for copy in lists {
            match reader.chunked(&copy, &self.whole, &self.deltas, &named) {
                Ok(()) => {
                    self.objects.insert(copy.header.id);
                }
                Err(detail) => problems.push(reader.bad_record(&copy, &detail)),
            }
        }
        report.problems.append(&mut problems);
    }

    /// Every object an entry leads to, whatever its content.
    fn named(&self) -> HashSet<ObjectId> {
        self.whole
            .keys()
            .chain(self.later.iter().map(|copy| &copy.header.id))
            .copied()
            .collect()
    }
}

/// Reads records for the second pass.
struct Reader<'a> {
    segments: &'a [PathBuf],
    files: HashMap<usize, File>,
    bases: BaseCache,
}

impl Reader<'_> {
    /// The bytes of a record, checked again.
    fn record(&mut self, copy: &Located) -> Result<Vec<u8>, String> {
        let path = &self.segments[copy.segment];
        let file = match self.files.entry(copy.segment) {
            std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(File::open(path).map_err(|err| err.to_string())?)
            }
        };
        let bytes = read_range(file, copy.offset, copy.header.unpadded_len())
            .map_err(|err| format!("cannot read it again: {err}"))?;
        match record::check(&bytes) {
            Ok(found) if found.header == copy.header => Ok(bytes),
            _ => Err("its record changed while fsck ran".to_owned()),
        }
    }

    /// The payload of an object stored whole.
    fn whole(&mut self, copy: &Located) -> Result<Arc<Vec<u8>>, String> {
        if let Some(payload) = self.bases.get(&copy.header.id) {
            return Ok(payload);
        }
        let bytes = self.record(copy)?;
        let found = record::check(&bytes).map_err(str::to_owned)?;
        let payload = Arc::new(record::decode(&found.header, found.stored).map_err(str::to_owned)?);
        self.bases.insert(copy.header.id, Arc::clone(&payload));
        Ok(payload)
    }

    /// The payload of a delta, checked against its base and its id.
    fn delta(
        &mut self,
        copy: &Located,
        whole: &HashMap<ObjectId, Located>,
        named: &HashSet<ObjectId>,
    ) -> Result<Vec<u8>, String> {
        let bytes = self.record(copy)?;
        let found = record::check(&bytes).map_err(str::to_owned)?;
        let base = found.reference.ok_or("a delta without a base")?;
        let base_copy = match whole.get(&base) {
            Some(base_copy) => base_copy,
            None if named.contains(&base) => {
                return Err(format!("its base {base} has no good copy stored whole"));
            }
            None => return Err(format!("its base {base} is missing")),
        };
        let base_payload = self
            .whole(base_copy)
            .map_err(|detail| format!("its base {base}: {detail}"))?;
        delta::decode(&found.header, found.stored, &base_payload).map_err(str::to_owned)
    }

    /// Checks a chunked blob: every chunk is a blob of the size the list
    /// gives, stored whole or as a delta, and together they hash to the
    /// blob's id.
    fn chunked(
        &mut self,
        copy: &Located,
        whole: &HashMap<ObjectId, Located>,
        deltas: &HashMap<ObjectId, Located>,
        named: &HashSet<ObjectId>,
    ) -> Result<(), String> {
        let bytes = self.record(copy)?;
        let found = record::check(&bytes).map_err(str::to_owned)?;
        let chunks =
            chunks::decode_list(found.stored, found.header.raw_len).map_err(str::to_owned)?;
        let mut hasher = IdHasher::new(Kind::Blob);
        for chunk in &chunks {
            let fits = |candidate: &Located| {
                candidate.header.kind == Kind::Blob && candidate.header.raw_len == chunk.size
            };
            let payload = match (whole.get(&chunk.id), deltas.get(&chunk.id)) {
                (Some(chunk_copy), _) if fits(chunk_copy) => self.whole(chunk_copy)?.to_vec(),
                (_, Some(chunk_copy)) if fits(chunk_copy) => {
                    self.delta(chunk_copy, whole, named)?
                }
                _ => {
                    return Err(format!(
                        "its chunk {} has no good copy as a blob of {} bytes",
                        chunk.id, chunk.size
                    ));
                }
            };
            hasher.update(&payload);
        }
        if hasher.finalize() != found.header.id {
            return Err("its chunks do not hash to its id".to_owned());
        }
        Ok(())
    }

    fn bad_record(&self, copy: &Located, detail: &str) -> Problem {
        let segment = self.segments[copy.segment]
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        Problem::BadRecord {
            segment,
            offset: copy.offset,
            detail: format!("object {}: {detail}", copy.header.id),
        }
    }
}

/// What a segment's index or journal says.
struct Entries {
    entries: Vec<Entry>,
    /// The index is damaged: rebuild the entries from the records, as lookups do.
    rebuild: bool,
    /// The segment's length when it was sealed.
    sealed_len: Option<u64>,
}

/// The entries of a segment: from its index if it is sealed, else from its
/// journal. `None` if they cannot be read.
fn segment_entries(
    dir: &Path,
    name: &str,
    files: Files,
    report: &mut FsckReport,
) -> Option<Entries> {
    if !files.idx && files.jrn {
        let path = segment_path(dir, name, "jrn");
        let shown = format!("seg/{name}.jrn");
        match File::open(&path) {
            Ok(file) => {
                return match journal::read(&file, 0, &path) {
                    Ok(read) => {
                        report.torn_journal_entries += read.invalid;
                        Some(Entries {
                            entries: read.entries,
                            rebuild: false,
                            sealed_len: None,
                        })
                    }
                    Err(Error::NewerStore { detail, .. }) => {
                        report.problems.push(Problem::Newer {
                            path: shown,
                            detail,
                        });
                        None
                    }
                    Err(err) => {
                        report.problems.push(Problem::Unreadable {
                            path: shown,
                            detail: err.to_string(),
                        });
                        None
                    }
                };
            }
            // Sealed since the listing: read the index instead.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                report.problems.push(unreadable(&shown, &err));
                return None;
            }
        }
    }
    let path = segment_path(dir, name, "idx");
    match fs::read(&path) {
        Ok(bytes) => {
            report.sealed_segments += 1;
            match index::decode(&bytes) {
                Ok(index) => Some(Entries {
                    entries: index.entries,
                    rebuild: false,
                    sealed_len: Some(index.segment_len),
                }),
                Err(IndexError::Newer(detail)) => {
                    report.problems.push(Problem::Newer {
                        path: format!("seg/{name}.idx"),
                        detail: format!("it holds {detail}"),
                    });
                    None
                }
                Err(IndexError::Damaged(detail)) => {
                    report.problems.push(Problem::BadIndex {
                        segment: name.to_owned(),
                        detail: detail.to_owned(),
                    });
                    Some(Entries {
                        entries: Vec::new(),
                        rebuild: true,
                        sealed_len: None,
                    })
                }
            }
        }
        // Neither an index nor a journal: nothing in it is visible yet.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Some(Entries {
            entries: Vec::new(),
            rebuild: false,
            sealed_len: None,
        }),
        Err(err) => {
            report
                .problems
                .push(unreadable(&format!("seg/{name}.idx"), &err));
            None
        }
    }
}

fn unreadable(path: &str, err: &io::Error) -> Problem {
    Problem::Unreadable {
        path: path.to_owned(),
        detail: err.to_string(),
    }
}

fn show(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "nothing".to_owned(), |id| id.to_hex())
}
