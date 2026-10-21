// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Writing a changeset's files into a plain directory: no workspace, no
//! state, no `.braid` file and no `.git` guard.

use std::fs;
use std::io;
use std::path::Path;

use braid_engine::{EMPTY_TREE, read_changeset};
use braid_format::ObjectId;
use braid_store::Store;

use crate::checkout::{self, Mode};
use crate::error::Error;
use crate::fsprobe::FileSystem;
use crate::safe::Root;

/// Writes the files of `changeset` into `dir`, which must be empty or not
/// exist yet (its parent must), and returns how many it wrote.
///
/// The checkout is a workspace's, with the same safety: every write goes
/// through directory handles that never follow a symlink, regular files get
/// mode 0644 or 0755 only, gitlinks are empty directories, and names this
/// platform cannot hold, or that would collide on its file system, are
/// refused before anything is written. Nothing else goes into `dir`: it is
/// not a workspace, braid keeps no state for it, and git run inside it finds
/// whatever repository is around it, as anywhere else.
pub fn materialize(store: &Store, changeset: ObjectId, dir: &Path) -> Result<usize, Error> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::DirectoryNotEmpty(dir.to_path_buf()));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(dir).map_err(Error::io("create", dir))?;
        }
        Err(err) => return Err(Error::io("read", dir)(err)),
    }
    let to = read_changeset(store, changeset)?.root;
    let root = Root::open(dir)?;
    let fs = FileSystem::probe(root.dir());
    let plan = checkout::plan(store, &root, fs, EMPTY_TREE, to, Mode::Checked)?;
    checkout::apply(store, &root, &plan)
}
