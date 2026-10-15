// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reading and writing braid objects, with kinds checked and errors converted.

use braid_format::{Changeset, Entry, Kind, ObjectId, Objects, read_directory};

use crate::error::Error;

/// Fetches an object that must be of `kind`.
pub(crate) fn get<O: Objects>(objects: &O, id: &ObjectId, kind: Kind) -> Result<Vec<u8>, Error> {
    match objects.get(id).map_err(Error::objects)? {
        Some((found, payload)) if found == kind => Ok(payload),
        Some((found, _)) => Err(Error::BadBraidObject {
            id: *id,
            detail: format!("is a {found}, expected a {kind}"),
        }),
        None => Err(Error::MissingBraidObject {
            id: *id,
            expected: kind,
        }),
    }
}

/// Stores an object and checks the id the store gives back.
pub(crate) fn put<O: Objects>(objects: &O, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
    put_hinted(objects, kind, payload, None)
}

/// Stores an object, with an object it is probably close to as a hint for
/// the store, and checks the id the store gives back.
pub(crate) fn put_hinted<O: Objects>(
    objects: &O,
    kind: Kind,
    payload: &[u8],
    similar: Option<&ObjectId>,
) -> Result<ObjectId, Error> {
    let id = match similar {
        Some(similar) => objects.put_similar(kind, payload, similar),
        None => objects.put(kind, payload),
    }
    .map_err(Error::objects)?;
    let expected = ObjectId::hash(kind, payload);
    if id != expected {
        return Err(Error::BadBraidObject {
            id,
            detail: format!("the store gave this id to the {kind} {expected}"),
        });
    }
    Ok(id)
}

/// Every entry of a directory, whether it is one flat node or split.
pub(crate) fn tree_entries<O: Objects>(objects: &O, root: ObjectId) -> Result<Vec<Entry>, Error> {
    let mut failure = None;
    let entries = read_directory(root, |id| match get(objects, id, Kind::Tree) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            failure.get_or_insert(err);
            None
        }
    });
    if let Some(err) = failure {
        return Err(err);
    }
    entries.map_err(|err| Error::BadBraidObject {
        id: root,
        detail: err.to_string(),
    })
}

/// A decoded changeset.
pub(crate) fn changeset<O: Objects>(objects: &O, id: &ObjectId) -> Result<Changeset, Error> {
    let bytes = get(objects, id, Kind::Changeset)?;
    Changeset::decode(&bytes).map_err(|err| Error::BadBraidObject {
        id: *id,
        detail: err.to_string(),
    })
}
