// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The op log: refs, and the history of every operation that moved them.
//!
//! ```text
//! oplog/lock         exclusive while a transaction runs, shared while reading
//! oplog/log          operation records, append-only
//! oplog/ckpt.<seq>   every ref as of operation <seq>
//! ```
//!
//! An operation record (`uv`, `sv` and `bytes` as in section 1 of the format
//! spec, fixed-size integers little-endian):
//!
//! ```text
//! u32 magic = 0xB7A10910 | u32 body_len | body | u32 record_len | u32 crc32c
//!
//! body:     u8 version = 1 | uv seq | sv milliseconds since the Unix epoch
//!           bytes agent | bytes description | uv seq undone, 0 for none
//!           uv n | n × update
//! update:   bytes name | u8 which (bit 0: old present, bit 1: new present)
//!           [32] old, if present | [32] new, if present
//! ```
//!
//! `record_len` is `body_len + 16`, so the log can be read backwards from its
//! end; the CRC covers everything before it. Sequence numbers start at 1 and
//! have no gaps. Each record keeps the old value of every ref it changed, so
//! undoing it is a new operation that sets them back.
//!
//! A transaction holds the lock, reads the log to the end, checks every
//! update against the current value (compare and swap), appends one record
//! and flushes it. A record cut short by a crash is ignored by readers and
//! cut off by the next writer, but only when no whole record follows it:
//! damage in the middle of the log is an error, never a reason to drop data.
//!
//! A checkpoint, written through a temporary file and a rename:
//!
//! ```text
//! [8] magic "braidckp" | u32 version = 1 | u32 0 | u64 seq | u64 end of record seq in the log
//! uv n | n × (bytes name | [32] id), names strictly ascending
//! [32] BLAKE3 hash of everything before it
//! ```

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use braid_format::codec::{Reader, put_bytes, put_id, put_sv, put_uv};
use braid_format::{DecodeError, ObjectId};

use crate::error::{Error, TransactError, io_error};
use crate::fsutil::{
    publish_file, read_range, remove_stale_temps, sync_dir, sync_file, write_all_at,
};
use crate::record::array;
use crate::{Durability, Options, Store};

/// The first four bytes of every operation record.
pub(crate) const MAGIC: u32 = 0xB7A1_0910;
/// Bytes a record adds around its body.
const OVERHEAD: u64 = 16;
const BODY_VERSION: u8 = 1;
/// The largest record body accepted, and so the largest operation.
const MAX_BODY: u64 = 16 << 20;
/// The longest agent label.
const MAX_AGENT: usize = 1024;
/// The longest operation description.
const MAX_DESCRIPTION: usize = 64 << 10;
/// The longest ref name.
const MAX_REF_NAME: usize = 4096;

const CHECKPOINT_MAGIC: &[u8; 8] = b"braidckp";
const CHECKPOINT_VERSION: u32 = 1;
/// Checkpoint file names are this prefix and a sequence number.
pub(crate) const CHECKPOINT_PREFIX: &str = "ckpt.";
/// How many checkpoints are kept. A reader falls back to an older one if the
/// newest is damaged.
const CHECKPOINTS_KEPT: usize = 2;

/// Every ref and the object it points at, as of one operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refs {
    seq: u64,
    map: BTreeMap<String, ObjectId>,
}

impl Refs {
    /// The sequence number of the newest operation these refs include, or 0
    /// before the first one.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The object a ref points at.
    pub fn get(&self, name: &str) -> Option<ObjectId> {
        self.map.get(name).copied()
    }

    /// Number of refs.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether there are no refs.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every ref, by name in byte order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ObjectId)> + '_ {
        self.map.iter().map(|(name, id)| (name.as_str(), *id))
    }

    /// The refs as a map.
    pub fn as_map(&self) -> &BTreeMap<String, ObjectId> {
        &self.map
    }

    fn apply(&mut self, op: &Operation) {
        for update in &op.updates {
            match update.new {
                Some(id) => {
                    self.map.insert(update.name.clone(), id);
                }
                None => {
                    self.map.remove(&update.name);
                }
            }
        }
        self.seq = op.seq;
    }
}

/// A change to one ref, applied only if the ref still holds `old`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefUpdate {
    /// The ref's name: non-empty, at most 4,096 bytes, no NUL.
    pub name: String,
    /// The value the ref must hold, `None` for a ref that must not exist.
    pub old: Option<ObjectId>,
    /// The value to set, `None` to delete the ref.
    pub new: Option<ObjectId>,
}

impl RefUpdate {
    /// An update of `name` from `old` to `new`.
    pub fn new(name: impl Into<String>, old: Option<ObjectId>, new: Option<ObjectId>) -> Self {
        Self {
            name: name.into(),
            old,
            new,
        }
    }
}

/// One recorded operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    /// Its position in the log, from 1, without gaps.
    pub seq: u64,
    /// When it was recorded, in milliseconds since the Unix epoch, by the
    /// clock of the process that recorded it.
    pub time_ms: i64,
    /// Who recorded it: an agent or session label.
    pub agent: String,
    /// What it did, for people.
    pub description: String,
    /// The operation it reverted, if it is an undo.
    pub undoes: Option<u64>,
    /// The refs it changed, each with its old and new value.
    pub updates: Vec<RefUpdate>,
}

/// The op log half of a store: the `oplog/` directory.
#[derive(Debug)]
pub(crate) struct OpLog {
    dir: PathBuf,
    durability: Durability,
    checkpoint_interval: u64,
    /// The newest state this handle has seen, so that the next read only
    /// parses the records after it.
    cache: Mutex<Option<State>>,
}

/// The refs at the end of a valid prefix of the log.
#[derive(Clone, Debug, Default)]
struct State {
    refs: Refs,
    /// Where the record `refs.seq` ends.
    end: u64,
    /// The newest checkpoint this handle knows of.
    checkpoint: u64,
}

thread_local! {
    /// Set while a transaction's closure runs on this thread.
    static IN_TRANSACTION: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as running a transaction's closure.
struct InTransaction;

impl InTransaction {
    fn enter() -> Self {
        IN_TRANSACTION.with(|flag| flag.set(true));
        InTransaction
    }
}

impl Drop for InTransaction {
    fn drop(&mut self) {
        IN_TRANSACTION.with(|flag| flag.set(false));
    }
}

/// Fails if the current thread is running a transaction's closure.
pub(crate) fn not_in_transaction() -> Result<(), Error> {
    if IN_TRANSACTION.with(Cell::get) {
        Err(Error::Reentrant)
    } else {
        Ok(())
    }
}

impl Store {
    /// Every ref, as of the newest operation. Waits while a transaction is
    /// being recorded, so it never returns an operation before it is
    /// durable.
    pub fn refs(&self) -> Result<Refs, Error> {
        not_in_transaction()?;
        let _shared = self.oplog.lock(false)?;
        let Some(log) = self.oplog.open_log(false)? else {
            return Ok(Refs::default());
        };
        Ok(self.oplog.load(&log)?.0.refs)
    }

    /// Records one operation.
    ///
    /// Takes the op log lock, reads the refs as of the newest operation and
    /// passes them to `f`, which returns the updates to make. Each update is
    /// applied only if its ref still holds the update's `old` value; if any
    /// does not, nothing is recorded and the error is a conflict (see
    /// [`TransactError::is_conflict`]), so the caller can read again and
    /// retry. Every `new` value must be an object in the store, and is made
    /// durable before the record is appended. The record is flushed before
    /// this returns.
    ///
    /// The lock is held while `f` runs, so `f` should be quick. It may read
    /// and write objects, but must not use the op log: that fails with
    /// [`Error::Reentrant`] instead of waiting forever.
    pub fn transact<F, E>(
        &self,
        agent: &str,
        description: &str,
        f: F,
    ) -> Result<Operation, TransactError<E>>
    where
        F: FnOnce(&Refs) -> Result<Vec<RefUpdate>, E>,
    {
        not_in_transaction()?;
        check_labels(agent, description)?;
        self.transact_with(agent, |refs| {
            f(refs).map(|updates| (updates, description.to_owned()))
        })
    }

    /// Records one operation whose description depends on what `f` finds:
    /// like [`Store::transact`], except that `f` returns the description
    /// with the updates, so that the record can say what the operation did,
    /// such as how a landing went.
    pub fn transact_with<F, E>(&self, agent: &str, f: F) -> Result<Operation, TransactError<E>>
    where
        F: FnOnce(&Refs) -> Result<(Vec<RefUpdate>, String), E>,
    {
        not_in_transaction()?;
        check_labels(agent, "")?;
        // Flush this handle's objects before taking the lock, to keep the
        // critical section short.
        self.segments.sync()?;
        let lock = self.oplog.lock(true)?;
        let log = self.oplog.open_for_write()?;
        let mut state = self.oplog.load_for_write(&log)?;
        let (updates, description) = {
            let _inside = InTransaction::enter();
            f(&state.refs).map_err(TransactError::Aborted)?
        };
        check_labels(agent, &description)?;
        let op = self.commit(&log, &mut state, agent, &description, None, updates)?;
        drop(log);
        drop(lock);
        self.oplog.maybe_checkpoint(&state);
        Ok(op)
    }

    /// The newest operations, newest first, at most `limit` of them: what
    /// `braid op log` shows.
    pub fn log(&self, limit: usize) -> Result<Vec<Operation>, Error> {
        not_in_transaction()?;
        let _shared = self.oplog.lock(false)?;
        let Some(log) = self.oplog.open_log(false)? else {
            return Ok(Vec::new());
        };
        let (state, _) = self.oplog.load(&log)?;
        let mut ops = Vec::new();
        if limit > 0 {
            self.oplog.walk_back(&log, state.end, |op| {
                ops.push(op);
                if ops.len() < limit {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            })?;
        }
        Ok(ops)
    }

    /// Reverts the newest operation that is neither an undo nor undone
    /// already, as a new operation by `agent` that sets every ref it changed
    /// back to its old value. Repeated calls step further back, one
    /// operation at a time. Fails with [`Error::NothingToUndo`] when there is
    /// nothing left.
    ///
    /// Undoes always revert the newest operation still in effect, so the
    /// refs it changed still hold its new values; the updates are checked
    /// with compare and swap all the same.
    pub fn undo_last(&self, agent: &str) -> Result<Operation, Error> {
        not_in_transaction()?;
        check_labels(agent, "")?;
        let lock = self.oplog.lock(true)?;
        let log = self.oplog.open_for_write()?;
        let mut state = self.oplog.load_for_write(&log)?;
        let target = self
            .oplog
            .undo_target(&log, state.end)?
            .ok_or(Error::NothingToUndo)?;
        let updates = target
            .updates
            .iter()
            .map(|update| RefUpdate::new(update.name.clone(), update.new, update.old))
            .collect();
        let description = format!("undo operation {}", target.seq);
        let op = self.commit(
            &log,
            &mut state,
            agent,
            &description,
            Some(target.seq),
            updates,
        )?;
        drop(log);
        drop(lock);
        self.oplog.maybe_checkpoint(&state);
        Ok(op)
    }

    /// Checks updates against the current refs, makes their targets durable,
    /// and appends the operation.
    fn commit(
        &self,
        log: &File,
        state: &mut State,
        agent: &str,
        description: &str,
        undoes: Option<u64>,
        updates: Vec<RefUpdate>,
    ) -> Result<Operation, Error> {
        let mut names = HashSet::new();
        let mut targets = Vec::new();
        for update in &updates {
            check_ref_name(&update.name)?;
            if !names.insert(update.name.as_str()) {
                return Err(Error::DuplicateRef {
                    name: update.name.clone(),
                });
            }
            let found = state.refs.get(&update.name);
            if found != update.old {
                return Err(Error::Conflict {
                    name: update.name.clone(),
                    expected: update.old,
                    found,
                });
            }
            if let Some(id) = update.new {
                if !self.segments.contains(&id)? {
                    return Err(Error::MissingObject {
                        name: update.name.clone(),
                        id,
                    });
                }
                targets.push(id);
            }
        }
        // The record must never point at an object that is not durable.
        self.segments.make_durable(&targets)?;
        let op = Operation {
            seq: state.refs.seq + 1,
            time_ms: now_ms(),
            agent: agent.to_owned(),
            description: description.to_owned(),
            undoes,
            updates,
        };
        self.oplog.append(log, state, &op)?;
        Ok(op)
    }
}

impl OpLog {
    pub fn new(dir: PathBuf, options: &Options) -> Self {
        Self {
            dir,
            durability: options.durability,
            checkpoint_interval: options.checkpoint_interval,
            cache: Mutex::new(None),
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }

    /// Locks `oplog/lock`, exclusively for a transaction or shared for a
    /// read, until the returned file is dropped.
    pub fn lock(&self, exclusive: bool) -> Result<File, Error> {
        let path = self.dir.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error("open", &path))?;
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
        .map_err(io_error("lock", &path))?;
        Ok(file)
    }

    /// Opens the log for reading, or `None` if it does not exist yet.
    fn open_log(&self, write: bool) -> Result<Option<File>, Error> {
        let path = self.log_path();
        match OpenOptions::new().read(true).write(write).open(&path) {
            Ok(file) => Ok(Some(file)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(io_error("open", &path)(err)),
        }
    }

    /// Opens the log for a transaction, creating it if needed.
    fn open_for_write(&self) -> Result<File, Error> {
        if let Some(file) = self.open_log(true)? {
            return Ok(file);
        }
        let path = self.log_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error("create", &path))?;
        sync_dir(&self.dir)?;
        Ok(file)
    }

    /// The state at the end of the valid records of `log`, and the log's
    /// length. Fails if the log is damaged anywhere but at its very end.
    fn load(&self, log: &File) -> Result<(State, u64), Error> {
        let path = self.log_path();
        let len = log.metadata().map_err(io_error("read", &path))?.len();
        let mut state = self.start(log, len)?;
        self.read_forward(log, len, &mut state)?;
        if state.end < len {
            self.check_torn_tail(log, &state, len)?;
        }
        self.remember(&state);
        Ok((state, len))
    }

    /// Loads the state for a transaction, under the exclusive lock, and cuts
    /// off the end of a record that a writer which died left behind.
    fn load_for_write(&self, log: &File) -> Result<State, Error> {
        let (state, len) = self.load(log)?;
        if state.end < len {
            let path = self.log_path();
            log.set_len(state.end)
                .and_then(|()| sync_file(log, self.durability))
                .map_err(io_error("truncate", &path))?;
        }
        Ok(state)
    }

    /// Where reading starts: this handle's cached state, else the newest
    /// valid checkpoint, else the beginning.
    fn start(&self, log: &File, len: u64) -> Result<State, Error> {
        let cached = self.lock_cache().clone();
        if let Some(state) = cached
            && self.ends_at(log, len, state.end, state.refs.seq)?
        {
            return Ok(state);
        }
        for (seq, path) in self.checkpoints()?.into_iter().rev() {
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(checkpoint) = decode_checkpoint(&bytes) else {
                continue;
            };
            if checkpoint.refs.seq == seq && self.ends_at(log, len, checkpoint.end, seq)? {
                return Ok(State {
                    refs: checkpoint.refs,
                    end: checkpoint.end,
                    checkpoint: seq,
                });
            }
        }
        Ok(State::default())
    }

    /// Whether a valid record with sequence number `seq` ends at `end`.
    fn ends_at(&self, log: &File, len: u64, end: u64, seq: u64) -> Result<bool, Error> {
        if seq == 0 {
            return Ok(end == 0);
        }
        if end > len {
            return Ok(false);
        }
        let before = record_before(log, end).map_err(io_error("read", &self.log_path()))?;
        Ok(before.is_some_and(|(op, _)| op.seq == seq))
    }

    /// Applies the records after `state.end`, up to the first that is not
    /// whole.
    fn read_forward(&self, log: &File, len: u64, state: &mut State) -> Result<(), Error> {
        if state.end >= len {
            return Ok(());
        }
        let path = self.log_path();
        let bytes = read_range(log, state.end, len - state.end).map_err(io_error("read", &path))?;
        let mut at = 0usize;
        while at < bytes.len() {
            match parse_record(&bytes[at..]) {
                Ok((op, len)) => {
                    if op.seq != state.refs.seq + 1 {
                        return Err(Error::corrupt(
                            &path,
                            state.end,
                            format!("operation {} follows operation {}", op.seq, state.refs.seq),
                        ));
                    }
                    state.refs.apply(&op);
                    state.end += len;
                    at += len as usize;
                }
                Err(Bad::Torn) => break,
                Err(Bad::Damaged(detail, _)) => {
                    return Err(Error::corrupt(&path, state.end, detail));
                }
            }
        }
        Ok(())
    }

    /// Fails unless the bytes after the last valid record can only be the
    /// remains of one append cut short by a crash.
    fn check_torn_tail(&self, log: &File, state: &State, len: u64) -> Result<(), Error> {
        let path = self.log_path();
        let tail = read_range(log, state.end, len - state.end).map_err(io_error("read", &path))?;
        if next_record(&tail, 1, state.refs.seq).is_some() {
            return Err(Error::corrupt(
                &path,
                state.end,
                "a damaged operation record is followed by valid ones",
            ));
        }
        Ok(())
    }

    /// Appends an operation and flushes it.
    fn append(&self, log: &File, state: &mut State, op: &Operation) -> Result<(), Error> {
        let bytes = encode_record(op);
        if bytes.len() as u64 > MAX_BODY + OVERHEAD {
            return Err(Error::TooLong {
                what: "operation record",
                len: bytes.len(),
                max: (MAX_BODY + OVERHEAD) as usize,
            });
        }
        let path = self.log_path();
        let written =
            write_all_at(log, &bytes, state.end).and_then(|()| sync_file(log, self.durability));
        if let Err(err) = written {
            // Take the record back where possible, so that the failed
            // operation is not left half recorded for the next reader.
            let _ = log
                .set_len(state.end)
                .and_then(|()| sync_file(log, self.durability));
            return Err(io_error("append to", &path)(err));
        }
        state.refs.apply(op);
        state.end += bytes.len() as u64;
        self.remember(state);
        Ok(())
    }

    /// The newest operation that is neither an undo nor undone already.
    fn undo_target(&self, log: &File, end: u64) -> Result<Option<Operation>, Error> {
        let mut undone = HashSet::new();
        let mut target = None;
        self.walk_back(log, end, |op| {
            if let Some(seq) = op.undoes {
                undone.insert(seq);
                ControlFlow::Continue(())
            } else if undone.contains(&op.seq) {
                ControlFlow::Continue(())
            } else {
                target = Some(op);
                ControlFlow::Break(())
            }
        })?;
        Ok(target)
    }

    /// Visits the records that end at or before `end`, newest first.
    fn walk_back(
        &self,
        log: &File,
        mut end: u64,
        mut visit: impl FnMut(Operation) -> ControlFlow<()>,
    ) -> Result<(), Error> {
        let path = self.log_path();
        while end > 0 {
            let Some((op, start)) = record_before(log, end).map_err(io_error("read", &path))?
            else {
                return Err(Error::corrupt(
                    &path,
                    end,
                    "no valid operation record ends here",
                ));
            };
            if visit(op).is_break() {
                break;
            }
            end = start;
        }
        Ok(())
    }

    /// Writes a checkpoint if enough operations have passed since the newest
    /// one. Best effort: the operation is already recorded, and a missing
    /// checkpoint only makes the next cold start read more of the log.
    fn maybe_checkpoint(&self, state: &State) {
        let interval = self.checkpoint_interval;
        if interval == 0 || state.refs.seq < state.checkpoint.saturating_add(interval) {
            return;
        }
        // Another process may have written one since this handle last looked.
        let newest = self
            .checkpoints()
            .ok()
            .and_then(|all| all.last().map(|(seq, _)| *seq))
            .unwrap_or(0);
        if newest.saturating_add(interval) > state.refs.seq {
            self.note_checkpoint(newest);
            return;
        }
        remove_stale_temps(&self.dir);
        let name = format!("{CHECKPOINT_PREFIX}{}", state.refs.seq);
        if publish_file(&self.dir, &name, &encode_checkpoint(state)).is_err() {
            return;
        }
        self.note_checkpoint(state.refs.seq);
        if let Ok(all) = self.checkpoints() {
            for (_, path) in all.iter().rev().skip(CHECKPOINTS_KEPT) {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn note_checkpoint(&self, seq: u64) {
        if let Some(state) = self.lock_cache().as_mut() {
            state.checkpoint = state.checkpoint.max(seq);
        }
    }

    /// The checkpoints in the directory, oldest first.
    pub fn checkpoints(&self) -> Result<Vec<(u64, PathBuf)>, Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(io_error("list", &self.dir))? {
            let entry = entry.map_err(io_error("list", &self.dir))?;
            let name = entry.file_name();
            let Some(seq) = name
                .to_str()
                .and_then(|name| name.strip_prefix(CHECKPOINT_PREFIX))
                .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|digits| digits.parse::<u64>().ok())
            else {
                continue;
            };
            out.push((seq, entry.path()));
        }
        out.sort();
        Ok(out)
    }

    fn remember(&self, state: &State) {
        let mut cache = self.lock_cache();
        let checkpoint = cache.as_ref().map_or(0, |cached| cached.checkpoint);
        if cache
            .as_ref()
            .is_none_or(|cached| cached.refs.seq <= state.refs.seq)
        {
            let mut state = state.clone();
            state.checkpoint = state.checkpoint.max(checkpoint);
            *cache = Some(state);
        }
    }

    fn lock_cache(&self) -> MutexGuard<'_, Option<State>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

fn check_labels(agent: &str, description: &str) -> Result<(), Error> {
    if agent.len() > MAX_AGENT {
        return Err(Error::TooLong {
            what: "agent label",
            len: agent.len(),
            max: MAX_AGENT,
        });
    }
    if description.len() > MAX_DESCRIPTION {
        return Err(Error::TooLong {
            what: "operation description",
            len: description.len(),
            max: MAX_DESCRIPTION,
        });
    }
    Ok(())
}

fn check_ref_name(name: &str) -> Result<(), Error> {
    let reason = if name.is_empty() {
        "empty"
    } else if name.len() > MAX_REF_NAME {
        "longer than 4096 bytes"
    } else if name.contains('\0') {
        "contains NUL"
    } else {
        return Ok(());
    };
    Err(Error::InvalidRefName {
        name: name.to_owned(),
        reason,
    })
}

/// Why bytes are not an operation record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Bad {
    /// Not a whole record: what a crash leaves at the end of the log.
    Torn,
    /// A whole record by its checksum, of this many bytes, whose contents no
    /// writer produces.
    Damaged(&'static str, u64),
}

pub(crate) fn encode_record(op: &Operation) -> Vec<u8> {
    let mut body = vec![BODY_VERSION];
    put_uv(&mut body, op.seq);
    put_sv(&mut body, op.time_ms);
    put_bytes(&mut body, op.agent.as_bytes());
    put_bytes(&mut body, op.description.as_bytes());
    put_uv(&mut body, op.undoes.unwrap_or(0));
    put_uv(&mut body, op.updates.len() as u64);
    for update in &op.updates {
        put_bytes(&mut body, update.name.as_bytes());
        body.push(u8::from(update.old.is_some()) | (u8::from(update.new.is_some()) << 1));
        if let Some(old) = &update.old {
            put_id(&mut body, old);
        }
        if let Some(new) = &update.new {
            put_id(&mut body, new);
        }
    }
    let total = body.len() as u64 + OVERHEAD;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&(total as u32).to_le_bytes());
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Parses the record at the start of `bytes`, and returns it with its length.
pub(crate) fn parse_record(bytes: &[u8]) -> Result<(Operation, u64), Bad> {
    if bytes.len() < OVERHEAD as usize || u32::from_le_bytes(array(&bytes[0..4])) != MAGIC {
        return Err(Bad::Torn);
    }
    let body_len = u64::from(u32::from_le_bytes(array(&bytes[4..8])));
    if body_len > MAX_BODY {
        return Err(Bad::Torn);
    }
    let total = body_len + OVERHEAD;
    if (bytes.len() as u64) < total {
        return Err(Bad::Torn);
    }
    let body_end = 8 + body_len as usize;
    let stored_total = u64::from(u32::from_le_bytes(array(&bytes[body_end..body_end + 4])));
    let crc = u32::from_le_bytes(array(&bytes[body_end + 4..body_end + 8]));
    if stored_total != total || crc32c::crc32c(&bytes[..body_end + 4]) != crc {
        return Err(Bad::Torn);
    }
    let op = decode_body(&bytes[8..body_end]).map_err(|detail| Bad::Damaged(detail, total))?;
    Ok((op, total))
}

/// The first offset at or after `from` where a whole record of an operation
/// after `seq` starts.
pub(crate) fn next_record(bytes: &[u8], from: usize, seq: u64) -> Option<usize> {
    let magic = MAGIC.to_le_bytes();
    (from..bytes.len()).find(|&at| {
        bytes[at..].starts_with(&magic)
            && matches!(parse_record(&bytes[at..]), Ok((op, _)) if op.seq > seq)
    })
}

/// The record that ends at `end`, and where it starts, if a whole one does.
fn record_before(log: &File, end: u64) -> io::Result<Option<(Operation, u64)>> {
    if end < OVERHEAD {
        return Ok(None);
    }
    let trailer = read_range(log, end - 8, 8)?;
    let total = u64::from(u32::from_le_bytes(array(&trailer[0..4])));
    if !(OVERHEAD..=MAX_BODY + OVERHEAD).contains(&total) || total > end {
        return Ok(None);
    }
    let bytes = read_range(log, end - total, total)?;
    Ok(match parse_record(&bytes) {
        Ok((op, len)) if len == total => Some((op, end - total)),
        _ => None,
    })
}

fn malformed(_: DecodeError) -> &'static str {
    "malformed operation record"
}

fn decode_body(body: &[u8]) -> Result<Operation, &'static str> {
    let mut reader = Reader::new(body);
    if reader.u8().map_err(malformed)? != BODY_VERSION {
        return Err("unknown operation record version");
    }
    let seq = reader.uv().map_err(malformed)?;
    if seq == 0 {
        return Err("operation with sequence number 0");
    }
    let time_ms = reader.sv().map_err(malformed)?;
    let agent = text(reader.bytes().map_err(malformed)?)?;
    let description = text(reader.bytes().map_err(malformed)?)?;
    let undoes = match reader.uv().map_err(malformed)? {
        0 => None,
        undone if undone < seq => Some(undone),
        _ => return Err("operation undoes one that comes after it"),
    };
    // Every update takes at least a name length and the presence byte.
    let count = reader.count(2).map_err(malformed)?;
    let mut updates = Vec::with_capacity(count);
    for _ in 0..count {
        let name = text(reader.bytes().map_err(malformed)?)?;
        if name.is_empty() {
            return Err("update of a ref with an empty name");
        }
        let which = reader.u8().map_err(malformed)?;
        if which > 3 {
            return Err("unknown update flags");
        }
        let old = if which & 1 != 0 {
            Some(reader.id().map_err(malformed)?)
        } else {
            None
        };
        let new = if which & 2 != 0 {
            Some(reader.id().map_err(malformed)?)
        } else {
            None
        };
        updates.push(RefUpdate { name, old, new });
    }
    reader.finish().map_err(malformed)?;
    Ok(Operation {
        seq,
        time_ms,
        agent,
        description,
        undoes,
        updates,
    })
}

fn text(bytes: &[u8]) -> Result<String, &'static str> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "text that is not UTF-8")
}

/// A decoded checkpoint.
#[derive(Debug)]
pub(crate) struct Checkpoint {
    /// The refs, with the sequence number they are as of.
    pub refs: Refs,
    /// Where that operation's record ends in the log.
    pub end: u64,
}

fn encode_checkpoint(state: &State) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CHECKPOINT_MAGIC);
    out.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&state.refs.seq.to_le_bytes());
    out.extend_from_slice(&state.end.to_le_bytes());
    put_uv(&mut out, state.refs.map.len() as u64);
    for (name, id) in &state.refs.map {
        put_bytes(&mut out, name.as_bytes());
        put_id(&mut out, id);
    }
    let hash = blake3::hash(&out);
    out.extend_from_slice(hash.as_bytes());
    out
}

pub(crate) fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint, &'static str> {
    if bytes.len() < 32 + 1 + 32 {
        return Err("checkpoint cut short");
    }
    let (body, footer) = bytes.split_at(bytes.len() - 32);
    if blake3::hash(body).as_bytes() != footer {
        return Err("checkpoint checksum mismatch");
    }
    if &body[0..8] != CHECKPOINT_MAGIC {
        return Err("not a checkpoint");
    }
    if u32::from_le_bytes(array(&body[8..12])) != CHECKPOINT_VERSION
        || u32::from_le_bytes(array(&body[12..16])) != 0
    {
        return Err("unknown checkpoint version");
    }
    let seq = u64::from_le_bytes(array(&body[16..24]));
    let end = u64::from_le_bytes(array(&body[24..32]));
    let bad = |_: DecodeError| "malformed checkpoint";
    let mut reader = Reader::new(&body[32..]);
    let count = reader.count(1 + 32).map_err(bad)?;
    let mut map = BTreeMap::new();
    let mut last: Option<String> = None;
    for _ in 0..count {
        let name = text(reader.bytes().map_err(bad)?)?;
        let id = reader.id().map_err(bad)?;
        if name.is_empty() || last.as_ref().is_some_and(|last| *last >= name) {
            return Err("checkpoint names are not strictly ascending");
        }
        map.insert(name.clone(), id);
        last = Some(name);
    }
    reader.finish().map_err(bad)?;
    Ok(Checkpoint {
        refs: Refs { seq, map },
        end,
    })
}

#[cfg(test)]
mod tests {
    use braid_format::Kind;

    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId::hash(Kind::Blob, &[n])
    }

    fn op(seq: u64) -> Operation {
        Operation {
            seq,
            time_ms: 1_700_000_000_000 + seq as i64,
            agent: format!("agent-{seq}"),
            description: "move main".to_owned(),
            undoes: (seq > 2).then(|| seq - 2),
            updates: vec![
                RefUpdate::new("main", Some(id(1)), Some(id(2))),
                RefUpdate::new("feature/x", None, Some(id(3))),
                RefUpdate::new("old", Some(id(4)), None),
            ],
        }
    }

    #[test]
    fn records_round_trip() {
        for seq in 1..5 {
            let op = op(seq);
            let bytes = encode_record(&op);
            assert_eq!(parse_record(&bytes), Ok((op.clone(), bytes.len() as u64)));
            // Trailing bytes belong to the next record.
            let mut longer = bytes.clone();
            longer.extend_from_slice(b"next");
            assert_eq!(parse_record(&longer), Ok((op, bytes.len() as u64)));
        }
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        let bytes = encode_record(&op(3));
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x04;
            assert!(parse_record(&damaged).is_err(), "byte {i}");
        }
        for len in 0..bytes.len() {
            assert_eq!(parse_record(&bytes[..len]), Err(Bad::Torn), "cut at {len}");
        }
    }

    #[test]
    fn bad_contents_under_a_valid_checksum_are_damage() {
        let reseal = |mut body: Vec<u8>| {
            let mut out = MAGIC.to_le_bytes().to_vec();
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.append(&mut body);
            out.extend_from_slice(&((out.len() + 8) as u32).to_le_bytes());
            let crc = crc32c::crc32c(&out);
            out.extend_from_slice(&crc.to_le_bytes());
            out
        };
        let good = encode_record(&op(1));
        let body = good[8..good.len() - 8].to_vec();
        let mut version = body.clone();
        version[0] = 9;
        assert!(matches!(
            parse_record(&reseal(version)),
            Err(Bad::Damaged(..))
        ));
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(matches!(
            parse_record(&reseal(trailing)),
            Err(Bad::Damaged(..))
        ));
    }

    #[test]
    fn checkpoints_round_trip_and_catch_damage() {
        let mut refs = Refs::default();
        refs.apply(&op(1));
        let state = State {
            refs,
            end: 1234,
            checkpoint: 0,
        };
        let bytes = encode_checkpoint(&state);
        let checkpoint = decode_checkpoint(&bytes).unwrap();
        assert_eq!(checkpoint.refs, state.refs);
        assert_eq!(checkpoint.end, 1234);
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x80;
            assert!(decode_checkpoint(&damaged).is_err(), "byte {i}");
        }
    }

    #[test]
    fn ref_names_are_checked() {
        assert!(check_ref_name("main").is_ok());
        assert!(check_ref_name("").is_err());
        assert!(check_ref_name("a\0b").is_err());
        assert!(check_ref_name(&"x".repeat(MAX_REF_NAME)).is_ok());
        assert!(check_ref_name(&"x".repeat(MAX_REF_NAME + 1)).is_err());
    }
}
