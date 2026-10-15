// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Raw access to git repositories through gitoxide.
//!
//! Objects are read and written as raw bytes: no filters, no line-ending
//! conversion, no LFS. Replacement objects (`refs/replace/`) are ignored and
//! grafts are never read, so every object is exactly what its id names. Only
//! the repository's own configuration is read; the user's, the system's and
//! the environment's are not.

use std::path::Path;

use gix::bstr::{BStr, ByteSlice};
use gix::objs::{Exists, Find, FindHeader, Write};

use crate::error::{Error, ShowBytes};
use crate::oid::{GitKind, GitOid};

/// Memory for the cache of decoded pack deltas, per open repository.
const PACK_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Whether `name` can be the name of a git branch: `refs/heads/<name>`
/// follows git's rules for ref names (no `..`, no `:`, `?`, `*`, `[`, `\`,
/// `^`, `~`, space or control character, no component that starts with `.`
/// or ends with `.lock`, and so on), and is not `refs/heads/HEAD`.
pub fn is_branch_name(name: &str) -> bool {
    let full = format!("refs/heads/{name}");
    gix::validate::reference::branch_name(full.as_bytes().as_bstr()).is_ok()
}

/// The id of the empty tree.
const EMPTY_TREE: GitOid = GitOid::from_bytes([
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88,
    0xfb, 0xee, 0x49, 0x04,
]);

/// An open git repository.
pub(crate) struct GitRepo {
    repo: gix::Repository,
}

impl GitRepo {
    /// Opens the repository at `path`, its work tree or its git directory.
    /// SHA-256 repositories are refused.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let mut repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|err| {
            Error::git(
                format!("cannot open a git repository at {}", path.display()),
                err,
            )
        })?;
        if repo.object_hash() != gix::hash::Kind::Sha1 {
            return Err(Error::Sha256);
        }
        repo.objects.ignore_replacements = true;
        repo.objects.set_pack_cache(|| {
            Box::new(gix::odb::pack::cache::lru::MemoryCappedHashmap::new(
                PACK_CACHE_BYTES,
            ))
        });
        Ok(Self { repo })
    }

    /// Creates an empty bare repository at `path` and opens it.
    pub(crate) fn init_bare(path: &Path) -> Result<Self, Error> {
        gix::init_bare(path).map_err(|err| {
            Error::git(
                format!("cannot create a bare repository at {}", path.display()),
                err,
            )
        })?;
        Self::open(path)
    }

    /// Whether the repository is a shallow clone.
    pub(crate) fn is_shallow(&self) -> Result<bool, Error> {
        self.repo
            .is_shallow()
            .map_err(|err| Error::git("cannot tell whether the repository is shallow", err))
    }

    /// Reads an object and checks that its bytes hash to its id.
    ///
    /// The empty tree reads as itself even when it is not stored: git treats
    /// it as always present, and does not store it when a commit uses it.
    pub(crate) fn read(&self, oid: &GitOid) -> Result<(GitKind, Vec<u8>), Error> {
        let mut buf = Vec::new();
        let found = self
            .repo
            .objects
            .try_find(&oid.to_gix(), &mut buf)
            .map_err(|err| Error::git(format!("cannot read git object {oid}"), err))?;
        let (kind, bytes) = match found {
            Some(data) => (GitKind::from_gix(data.kind), data.data.to_vec()),
            None if *oid == EMPTY_TREE => (GitKind::Tree, Vec::new()),
            None => return Err(Error::MissingObject(*oid)),
        };
        if GitOid::for_object(kind, &bytes)? != *oid {
            return Err(Error::CorruptObject(*oid));
        }
        Ok((kind, bytes))
    }

    /// Reads an object that must be of `expected` kind.
    pub(crate) fn read_kind(&self, oid: &GitOid, expected: GitKind) -> Result<Vec<u8>, Error> {
        let (found, bytes) = self.read(oid)?;
        if found != expected {
            return Err(Error::WrongKind {
                oid: *oid,
                expected,
                found,
            });
        }
        Ok(bytes)
    }

    /// The kind of an object, from its header alone.
    pub(crate) fn kind(&self, oid: &GitOid) -> Result<GitKind, Error> {
        let header = self
            .repo
            .objects
            .try_header(&oid.to_gix())
            .map_err(|err| Error::git(format!("cannot read git object {oid}"), err))?;
        match header {
            Some(header) => Ok(GitKind::from_gix(header.kind)),
            None if *oid == EMPTY_TREE => Ok(GitKind::Tree),
            None => Err(Error::MissingObject(*oid)),
        }
    }

    /// Whether the object is stored in the repository. Unlike git, this does
    /// not count the empty tree as always present, so that a copy gets it.
    pub(crate) fn contains(&self, oid: &GitOid) -> bool {
        self.repo.objects.exists(&oid.to_gix())
    }

    /// Writes an object and returns the id gitoxide computed for it.
    pub(crate) fn write(&self, kind: GitKind, bytes: &[u8]) -> Result<GitOid, Error> {
        let id = self
            .repo
            .objects
            .write_buf(kind.to_gix(), bytes)
            .map_err(|err| Error::git(format!("cannot write a git {kind}"), err))?;
        GitOid::from_gix(&id).ok_or(Error::Sha256)
    }

    /// The refs to import, sorted by name, each with the object it points to
    /// after following symbolic refs (an annotated tag stays a tag).
    ///
    /// With no `names`, every branch (`refs/heads/`) and tag (`refs/tags/`).
    /// Otherwise the named refs, which must exist; a short name is looked up
    /// the way git does, and reported under its full name.
    pub(crate) fn refs(&self, names: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, GitOid)>, Error> {
        let mut found = Vec::new();
        let mut add = |reference: &mut gix::Reference<'_>| -> Result<(), Error> {
            let name = reference.name().as_bstr().to_vec();
            let id = reference.follow_to_object().map_err(|err| {
                Error::git(format!("cannot resolve ref {}", ShowBytes(&name)), err)
            })?;
            found.push((name, GitOid::from_gix(&id).ok_or(Error::Sha256)?));
            Ok(())
        };
        if names.is_empty() {
            let platform = self
                .repo
                .references()
                .map_err(|err| Error::git("cannot list refs", err))?;
            let branches = platform
                .local_branches()
                .map_err(|err| Error::git("cannot list branches", err))?;
            let tags = platform
                .tags()
                .map_err(|err| Error::git("cannot list tags", err))?;
            for reference in branches.chain(tags) {
                let mut reference =
                    reference.map_err(|err| Error::git("cannot read a ref", err))?;
                add(&mut reference)?;
            }
        } else {
            for name in names {
                let mut reference = self
                    .repo
                    .try_find_reference(BStr::new(name.as_slice()))
                    .map_err(|err| Error::git(format!("cannot read ref {}", ShowBytes(name)), err))?
                    .ok_or_else(|| Error::MissingRef(name.clone()))?;
                add(&mut reference)?;
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// The object a ref points to, after following symbolic refs.
    pub(crate) fn resolve_ref(&self, name: &[u8]) -> Result<Option<GitOid>, Error> {
        let Some(mut reference) = self
            .repo
            .try_find_reference(BStr::new(name))
            .map_err(|err| Error::git(format!("cannot read ref {}", ShowBytes(name)), err))?
        else {
            return Ok(None);
        };
        let id = reference
            .follow_to_object()
            .map_err(|err| Error::git(format!("cannot resolve ref {}", ShowBytes(name)), err))?;
        Ok(Some(GitOid::from_gix(&id).ok_or(Error::Sha256)?))
    }

    /// Points each ref at its object in one transaction, whatever it pointed
    /// to before. Reflogs, where the repository keeps them, name the
    /// configured committer, or a generic one when there is none.
    pub(crate) fn set_refs(
        &mut self,
        refs: &[(Vec<u8>, GitOid)],
        message: &str,
    ) -> Result<(), Error> {
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};

        let mut edits = Vec::with_capacity(refs.len());
        for (name, oid) in refs {
            let full_name = gix::refs::FullName::try_from(name.as_bstr())
                .map_err(|err| Error::git(format!("invalid ref name {}", ShowBytes(name)), err))?;
            edits.push(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: message.into(),
                    },
                    expected: PreviousValue::Any,
                    new: gix::refs::Target::Object(oid.to_gix()),
                },
                name: full_name,
                deref: false,
            });
        }
        self.repo
            .committer_or_set_generic_fallback()
            .map_err(|err| Error::git("cannot set up a reflog identity", err))?;
        self.repo
            .edit_references(edits)
            .map_err(|err| Error::git("cannot update refs", err))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        for good in ["main", "feature/x", "v1.0", "a-b_c", "caf\u{e9}"] {
            assert!(is_branch_name(good), "{good}");
        }
        for bad in [
            "", "a b", "a:b", "a..b", "a*", "a?", "a[b", "a\\b", "a^", "a~1", "/a", "a/", "a//b",
            ".a", "a/.b", "a.lock", "a.", "a@{1}", "HEAD", "a\nb", "a\0b",
        ] {
            assert!(!is_branch_name(bad), "{bad:?}");
        }
    }
}
