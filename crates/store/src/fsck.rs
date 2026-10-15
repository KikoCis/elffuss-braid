// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Checking a whole store.
//!
//! fsck reads everything from disk, independently of what the handle has
//! cached. Leftovers of crashes are expected and only counted: the end of a
//! record cut short in a segment or in the op log, journal entries that fail
//! their CRC, records no entry points at, temporary files. Everything that
//! could make the store return wrong data or lose data is a [`Problem`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::Path;

use braid_format::ObjectId;

use crate::error::Error;
use crate::fsutil::count_temps;
use crate::journal::{self, Entry};
use crate::oplog::{Bad, decode_checkpoint, next_record, not_in_transaction, parse_record};
use crate::record::{self, Header};
use crate::segments::{Files, list, segment_path};
use crate::{Store, check_format, index, scan};

/// What fsck found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    /// Segment files.
    pub segments: usize,
    /// Segments that are sealed.
    pub sealed_segments: usize,
    /// Records whose checksum is valid.
    pub records: u64,
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
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Format { detail } => write!(f, "format file: {detail}"),
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
    content: Result<(), &'static str>,
}

impl Store {
    /// Checks the whole store: every segment record (checksum, and that its
    /// content hashes to its id), every journal and index entry (it leads to
    /// a whole record of the same object), every op log record (checksum,
    /// sequence, and that each update's old value matches), every checkpoint,
    /// and that every ref points at an object in the store.
    ///
    /// It can run while other processes use the store. Errors are only
    /// returned when fsck itself cannot run; damage goes in the report. Like
    /// the other op log reads, it cannot run inside a transaction's closure.
    pub fn fsck(&self) -> Result<FsckReport, Error> {
        not_in_transaction()?;
        let mut report = FsckReport::default();
        self.check_format(&mut report);
        // The op log first: every object a record points at was durable
        // before the record, so the segments listed afterwards hold it.
        let refs = self.check_oplog(&mut report)?;
        let objects = self.check_segments(&mut report);
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

    fn check_format(&self, report: &mut FsckReport) {
        if let Err(err) = check_format(&self.root) {
            report.problems.push(Problem::Format {
                detail: err.to_string(),
            });
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

    /// Checks every segment. Returns the objects that an entry leads to.
    fn check_segments(&self, report: &mut FsckReport) -> HashSet<ObjectId> {
        let dir = self.root.join("seg");
        let mut objects = HashSet::new();
        let listing = match list(&dir) {
            Ok(listing) => listing,
            Err(err) => {
                report.problems.push(Problem::Unreadable {
                    path: "seg".to_owned(),
                    detail: err.to_string(),
                });
                return objects;
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
            check_segment(&dir, &name, files, report, &mut objects);
        }
        objects
    }
}

fn check_segment(
    dir: &Path,
    name: &str,
    files: Files,
    report: &mut FsckReport,
    objects: &mut HashSet<ObjectId>,
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
    let scanned = scan::scan(&file, len, |offset, header, crc, stored| {
        let content = record::decode(header, stored).map(|_| ());
        records.insert(
            offset,
            Found {
                header: *header,
                crc,
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
    for (offset, found) in &records {
        if let Err(detail) = found.content {
            report.problems.push(Problem::BadRecord {
                segment: name.to_owned(),
                offset: *offset,
                detail: format!("object {}: {detail}", found.header.id),
            });
        }
    }

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
                } else if found.content.is_ok() {
                    objects.insert(entry.id);
                }
            }
        }
    }
    report.unindexed_records += (records.len() - referenced.len()) as u64;
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
        match File::open(&path).and_then(|file| journal::read(&file, 0)) {
            Ok(read) => {
                report.torn_journal_entries += read.invalid;
                return Some(Entries {
                    entries: read.entries,
                    rebuild: false,
                    sealed_len: None,
                });
            }
            // Sealed since the listing: read the index instead.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                report
                    .problems
                    .push(unreadable(&format!("seg/{name}.jrn"), &err));
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
                Err(detail) => {
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
