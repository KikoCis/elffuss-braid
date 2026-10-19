// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Moving objects between a repository and a hub: which objects the hub
//! lacks, how they are cut into `POST objects` bodies and streamed from the
//! store, and how an object stream from the hub goes into the store.

use std::collections::HashSet;
use std::io::{self, Read};

use anyhow::{Context, Result, bail};
use braid_engine::{Reach, Reached, reachable, read_changeset};
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::stream::{self, MAGIC};
use braid_store::Store;

use super::remote::{HubError, Remote};

/// Bytes of a record before its payload: the kind, the id and the length.
const RECORD_HEADER: u64 = 1 + ObjectId::LEN as u64 + 8;

/// Bytes of a stream around its records: the magic and the terminator.
const FRAME: u64 = MAGIC.len() as u64 + 1;

/// The largest object taken from the hub. Objects reach a hub through
/// bodies of at most 256 MiB, unless it imported them from git.
const MAX_OBJECT: u64 = 1 << 30;

/// The objects `reach` reaches that the hub lacks, in the walk's order: a
/// changeset before the objects it names. `lacks` is asked about `batch`
/// ids at a time.
pub(crate) fn lacking<O: Objects>(
    objects: &O,
    reach: &Reach<'_>,
    batch: usize,
    mut lacks: impl FnMut(&[ObjectId]) -> Result<Vec<ObjectId>, HubError>,
) -> Result<Vec<Reached>> {
    let candidates = reachable(objects, reach)?;
    let mut lacked = HashSet::new();
    for chunk in candidates.chunks(batch.max(1)) {
        let ids: Vec<ObjectId> = chunk.iter().map(|reached| reached.id).collect();
        lacked.extend(lacks(&ids)?);
    }
    Ok(candidates
        .into_iter()
        .filter(|reached| lacked.contains(&reached.id))
        .collect())
}

/// Bytes of the stream that holds `objects`.
pub(crate) fn stream_len(objects: &[Reached]) -> u64 {
    FRAME
        + objects
            .iter()
            .map(|object| RECORD_HEADER + object.size)
            .sum::<u64>()
}

/// Cuts `objects` into runs whose streams take at most `max_body` bytes
/// each. An object too large for any body is an error.
pub(crate) fn batches(objects: &[Reached], max_body: u64) -> Result<Vec<&[Reached]>> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut size = FRAME;
    for (index, object) in objects.iter().enumerate() {
        let record = RECORD_HEADER + object.size;
        if FRAME + record > max_body {
            bail!(
                "{} {} is {} bytes: more than the hub takes in one request ({max_body} bytes)",
                object.kind,
                object.id,
                object.size
            );
        }
        if size + record > max_body {
            runs.push(&objects[start..index]);
            start = index;
            size = FRAME;
        }
        size += record;
    }
    if start < objects.len() {
        runs.push(&objects[start..]);
    }
    Ok(runs)
}

/// An object stream read from the store as it is sent, one object at a
/// time: the body of a `POST objects`.
pub(crate) struct StreamBody<'a, O> {
    objects: &'a O,
    list: &'a [Reached],
    next: usize,
    started: bool,
    finished: bool,
    buffer: Vec<u8>,
    at: usize,
}

impl<'a, O: Objects> StreamBody<'a, O> {
    /// The stream of `list`, which is [`stream_len`] bytes long.
    pub(crate) fn new(objects: &'a O, list: &'a [Reached]) -> StreamBody<'a, O> {
        StreamBody {
            objects,
            list,
            next: 0,
            started: false,
            finished: false,
            buffer: Vec::new(),
            at: 0,
        }
    }

    /// Puts the next piece in the buffer: the magic, a record, or the
    /// terminator. `false` once the stream is over.
    fn refill(&mut self) -> io::Result<bool> {
        self.buffer.clear();
        self.at = 0;
        if !self.started {
            self.started = true;
            self.buffer.extend_from_slice(MAGIC);
        } else if let Some(object) = self.list.get(self.next) {
            self.next += 1;
            let found = self.objects.get(&object.id).map_err(io::Error::other)?;
            let Some((kind, payload)) = found else {
                return Err(io::Error::other(format!(
                    "{} {} is not in the store",
                    object.kind, object.id
                )));
            };
            // The length was announced: the payload must have it.
            if kind != object.kind || payload.len() as u64 != object.size {
                return Err(io::Error::other(format!(
                    "{} {} is not what the walk found",
                    object.kind, object.id
                )));
            }
            self.buffer.push(kind.code());
            self.buffer.extend_from_slice(object.id.as_bytes());
            self.buffer
                .extend_from_slice(&(payload.len() as u64).to_le_bytes());
            self.buffer.extend_from_slice(&payload);
        } else if !self.finished {
            self.finished = true;
            self.buffer.push(0);
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

impl<O: Objects> Read for StreamBody<'_, O> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at == self.buffer.len() {
            if !self.refill()? {
                return Ok(0);
            }
        }
        let n = out.len().min(self.buffer.len() - self.at);
        out[..n].copy_from_slice(&self.buffer[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// What a series of uploads sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sent {
    /// Objects sent.
    pub(crate) objects: u64,
    /// Bytes of their payloads.
    pub(crate) bytes: u64,
}

/// Sends `objects` to the hub in bodies of at most `max_body` bytes.
pub(crate) fn send<O: Objects>(
    objects: &O,
    remote: &impl Remote,
    list: &[Reached],
    max_body: u64,
) -> Result<Sent> {
    let mut sent = Sent::default();
    for run in batches(list, max_body)? {
        let mut body = StreamBody::new(objects, run);
        let len = stream_len(run);
        match remote.send_objects(&mut body, len) {
            Ok(_) => {}
            // Bodies keep to what the hub says it takes: a refusal comes
            // from a proxy in front of it, or a hub that did not say.
            Err(err @ HubError::Api { status: 413, .. }) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot upload objects to the hub: a body of {len} bytes was refused; set BRAID_HUB_MAX_BODY to a smaller size in bytes"
                )));
            }
            // A proxy that refuses a body as soon as it reads its length
            // answers 413 and closes before braid has sent it, which braid
            // only sees as the connection breaking off.
            Err(err @ HubError::Transport { .. }) if broke_off(&err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot upload objects to the hub: the connection broke off while a body of {len} bytes went up, as it does when the hub or a proxy in front of it refuses a body as too large before reading it; set BRAID_HUB_MAX_BODY to a smaller size in bytes, such as 1048576, and run the command again"
                )));
            }
            Err(err) => return Err(err).context("cannot upload objects to the hub"),
        }
        sent.objects += run.len() as u64;
        sent.bytes += run.iter().map(|object| object.size).sum::<u64>();
    }
    Ok(sent)
}

/// Whether a transfer failed because the other side closed the connection
/// while braid was still writing.
fn broke_off(err: &HubError) -> bool {
    match err {
        HubError::Transport { detail, .. } => {
            let detail = detail.to_ascii_lowercase();
            detail.contains("broken pipe") || detail.contains("connection reset")
        }
        _ => false,
    }
}

/// Sends what the hub lacks of what `reach` reaches.
pub(crate) fn upload(store: &Store, remote: &impl Remote, reach: &Reach<'_>) -> Result<Sent> {
    let list = lacking(store, reach, remote.max_ids(), |ids| remote.lacks(ids))?;
    if list.is_empty() {
        return Ok(Sent::default());
    }
    send(store, remote, &list, remote.max_body())
}

/// Sends the objects `ids`, which the hub said it needs: each one, and for
/// a changeset, what it adds to its first parent too, since a hub that
/// lacks a changeset cannot tell what else it lacks of it.
pub(crate) fn upload_missing(
    store: &Store,
    remote: &impl Remote,
    ids: &[ObjectId],
) -> Result<Sent> {
    let mut list = Vec::with_capacity(ids.len());
    let mut listed = HashSet::new();
    for id in ids {
        let Some((kind, size)) = store.stat(id)? else {
            bail!("the hub needs object {id}, which this repository does not hold");
        };
        if kind != Kind::Changeset {
            if listed.insert(*id) {
                list.push(Reached {
                    id: *id,
                    kind,
                    size,
                });
            }
            continue;
        }
        let parent: Vec<ObjectId> = read_changeset(store, *id)?
            .parents
            .first()
            .copied()
            .into_iter()
            .collect();
        let added = reachable(
            store,
            &Reach {
                want: std::slice::from_ref(id),
                have: &parent,
                history: Some(0),
                paths: None,
                max_objects: None,
            },
        )?;
        list.extend(added.into_iter().filter(|object| listed.insert(object.id)));
    }
    send(store, remote, &list, remote.max_body())
}

/// What an object stream from the hub brought.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Received {
    /// Objects stored.
    pub(crate) objects: u64,
    /// The changesets among them.
    pub(crate) changesets: Vec<ObjectId>,
    /// The tree nodes among them.
    pub(crate) trees: Vec<ObjectId>,
}

/// Reads an object stream into the store, every record checked against its
/// id, and makes what it stored durable. A changeset's root tree and
/// parents must be stored by the end: they come in the same stream, or the
/// store held them already.
pub(crate) fn store_stream(store: &Store, input: impl Read) -> Result<Received> {
    let mut reader =
        stream::Reader::new(input, MAX_OBJECT).context("the hub's object stream is not valid")?;
    let mut received = Received::default();
    while let Some(record) = reader
        .next_record()
        .context("the hub's object stream is not valid")?
    {
        let id = store.put(record.kind, &record.payload)?;
        if id != record.id {
            bail!("object {} was stored as {id}", record.id);
        }
        match record.kind {
            Kind::Changeset => received.changesets.push(id),
            Kind::Tree => received.trees.push(id),
            _ => {}
        }
        received.objects += 1;
    }
    store.sync()?;
    for id in &received.changesets {
        let changeset = read_changeset(store, *id)?;
        for needed in changeset.parents.iter().chain([&changeset.root]) {
            if *needed != braid_engine::EMPTY_TREE && !store.contains(needed)? {
                bail!("the hub sent changeset {id} without {needed}, which it needs");
            }
        }
    }
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::remote::LACKS_BATCH;
    use crate::hub::testing::{Fixture, ada_change};
    use braid_engine::Reach;

    #[test]
    fn the_hub_gets_only_what_it_lacks() {
        let f = Fixture::new();
        let one = ada_change(&f.repo, None, &[("a.txt", "1\n"), ("b.txt", "b\n")]);
        let two = ada_change(&f.repo, Some(one), &[("a.txt", "2\n")]);
        let three = ada_change(&f.repo, Some(two), &[("c.txt", "c\n")]);
        let store = f.repo.store();
        let all = reachable(
            store,
            &Reach {
                want: &[three],
                ..Reach::default()
            },
        )
        .unwrap();
        // A hub that holds everything `one` reaches.
        let held: HashSet<ObjectId> = reachable(
            store,
            &Reach {
                want: &[one],
                ..Reach::default()
            },
        )
        .unwrap()
        .iter()
        .map(|reached| reached.id)
        .collect();
        let mut asked = Vec::new();
        let list = lacking(
            store,
            &Reach {
                want: &[three],
                ..Reach::default()
            },
            2,
            |ids| {
                assert!(ids.len() <= 2);
                asked.extend_from_slice(ids);
                Ok(ids
                    .iter()
                    .filter(|id| !held.contains(id))
                    .copied()
                    .collect())
            },
        )
        .unwrap();
        // Every candidate was asked about once, in batches of two.
        assert_eq!(asked.len(), all.len());
        let ids: HashSet<ObjectId> = list.iter().map(|reached| reached.id).collect();
        assert!(ids.contains(&two) && ids.contains(&three));
        assert!(!ids.contains(&one));
        assert!(ids.iter().all(|id| !held.contains(id)));
        assert_eq!(list.len(), all.len() - held.len());
        // The walk's order is kept: the newest changeset first.
        assert_eq!(list[0].id, three);
        // Errors from the hub stop the walk.
        let failed = lacking(
            store,
            &Reach {
                want: &[three],
                ..Reach::default()
            },
            LACKS_BATCH,
            |_| Err(HubError::api(500, "internal", "down")),
        );
        assert!(failed.is_err());
    }

    fn reached(size: u64) -> Reached {
        Reached {
            id: ObjectId::hash(Kind::Blob, &size.to_le_bytes()),
            kind: Kind::Blob,
            size,
        }
    }

    #[test]
    fn bodies_stay_under_the_limit() {
        let objects: Vec<Reached> = [10, 20, 30, 5, 60].into_iter().map(reached).collect();
        let limit = FRAME + 2 * RECORD_HEADER + 35;
        let runs = batches(&objects, limit).unwrap();
        let sizes: Vec<Vec<u64>> = runs
            .iter()
            .map(|run| run.iter().map(|o| o.size).collect())
            .collect();
        assert_eq!(sizes, [vec![10, 20], vec![30, 5], vec![60]]);
        for run in &runs {
            assert!(stream_len(run) <= limit, "{run:?}");
        }
        assert!(batches(&[], limit).unwrap().is_empty());
        let err = batches(&[reached(1000)], limit).unwrap_err().to_string();
        assert!(err.contains("more than the hub takes"), "{err}");
    }

    #[test]
    fn streams_go_out_and_come_back() {
        let from = Fixture::new();
        let one = ada_change(&from.repo, None, &[("a.txt", "1\n"), ("dir/b.txt", "b\n")]);
        let two = ada_change(&from.repo, Some(one), &[("a.txt", "2\n")]);
        let list = reachable(
            from.repo.store(),
            &Reach {
                want: &[two],
                ..Reach::default()
            },
        )
        .unwrap();
        let mut bytes = Vec::new();
        StreamBody::new(from.repo.store(), &list)
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes.len() as u64, stream_len(&list));
        // The protocol's own reader takes it, record by record.
        let records: Vec<stream::Record> = stream::Reader::new(&bytes[..], 1 << 20)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), list.len());
        assert!(records.iter().zip(&list).all(|(r, l)| r.id == l.id));

        let to = Fixture::new();
        let received = store_stream(to.repo.store(), &bytes[..]).unwrap();
        assert_eq!(received.objects, list.len() as u64);
        assert_eq!(received.changesets.len(), 2);
        for object in &list {
            assert!(to.repo.store().contains(&object.id).unwrap());
        }
        let read = read_changeset(to.repo.store(), two).unwrap();
        braid_engine::read_tree(to.repo.store(), read.root).unwrap();

        // A stream cut short, or with a changed byte, is refused.
        let fresh = Fixture::new();
        assert!(store_stream(fresh.repo.store(), &bytes[..bytes.len() - 1]).is_err());
        let mut flipped = bytes.clone();
        let at = flipped.len() - 3;
        flipped[at] ^= 1;
        assert!(store_stream(fresh.repo.store(), &flipped[..]).is_err());
        // A changeset without its parent is refused.
        let only_two = reachable(
            from.repo.store(),
            &Reach {
                want: &[two],
                history: Some(0),
                ..Reach::default()
            },
        )
        .unwrap();
        assert!(only_two.iter().all(|object| object.id != one));
        let mut partial = Vec::new();
        StreamBody::new(from.repo.store(), &only_two)
            .read_to_end(&mut partial)
            .unwrap();
        let err = store_stream(Fixture::new().repo.store(), &partial[..])
            .unwrap_err()
            .to_string();
        assert!(err.contains("without"), "{err}");
    }

    #[test]
    fn a_body_whose_object_vanished_fails() {
        let f = Fixture::new();
        let ghost = reached(5);
        let mut body = StreamBody::new(f.repo.store(), std::slice::from_ref(&ghost));
        let mut out = Vec::new();
        assert!(body.read_to_end(&mut out).is_err());
    }
}
