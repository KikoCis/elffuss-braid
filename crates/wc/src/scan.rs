// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reading a working copy: every file that counts, and its blob id.
//!
//! The walk goes one directory at a time from the workspace root, never
//! through a symlink, skips `.braid`, `.git` and the names that alias them,
//! braid's own scratch files, ignored paths that are not tracked, and
//! untracked directories that are another workspace or repository. Files
//! the stat cache cannot vouch for are hashed afterwards, in parallel, one
//! directory per task.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use braid_format::{ChangeOp, IdHasher, Kind, ObjectId};
use cap_std::fs::Dir;
use rayon::prelude::*;

use crate::cache::{self, Cache};
use crate::error::Error;
use crate::fsprobe::SCRATCH_PREFIX;
use crate::ignores::{self, Chain};
use crate::paths::{join, name_bytes, os_name};
use crate::safe::{Found, Root, Stat, StatKind, open_file, read_link};
use crate::tree::Flat;

/// What a scan found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Every file that counts, by path: regular files, executables and
    /// symlinks with their blobs, and the gitlinks of the base whose
    /// directory is there.
    pub files: BTreeMap<Vec<u8>, ChangeOp>,
    /// The stat cache for the next scan.
    pub cache: Cache,
}

/// A file the cache could not vouch for.
struct Miss {
    name: Vec<u8>,
    path: Vec<u8>,
    stat: Stat,
}

/// Scans the working copy at `root`, whose tracked files are those of
/// `base`, using `old` to avoid hashing files that did not change.
pub(crate) fn scan(root: &Root, base: &Flat, old: &Cache) -> Result<Scan, Error> {
    let time = root.file_system_now().unwrap_or_else(|_| fallback_now());
    let mut found = Scan {
        files: BTreeMap::new(),
        cache: Cache {
            time,
            entries: HashMap::new(),
        },
    };
    let mut misses: BTreeMap<Vec<u8>, Vec<Miss>> = BTreeMap::new();

    // Directories to read: path, rules in force above them, and whether
    // they are ignored (so only tracked files count inside).
    let mut pending: Vec<(Vec<u8>, Chain, bool)> = vec![(Vec::new(), None, false)];
    while let Some((path, above, ignored)) = pending.pop() {
        let dir = match root.open_dir(&path)? {
            Found::Here(dir) => dir,
            // Replaced while the walk ran: whatever is there now is not a
            // directory to read.
            Found::Missing | Found::Blocked(_) => continue,
        };
        let rules = if ignored {
            above
        } else {
            ignores::enter(&dir, &path, &above)
        };
        for os in sorted_names(root, &dir, &path)? {
            let Some(name) = name_bytes(&os) else {
                continue;
            };
            if skipped(name) {
                continue;
            }
            let child = join(&path, name);
            let stat = match dir.symlink_metadata(&os) {
                Ok(meta) => Stat::of(&meta),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(root.error("read", &child, err)),
            };
            let tracked = base.files.get(&child);
            match stat.kind {
                StatKind::Dir => {
                    if let Some(gitlink @ ChangeOp::Gitlink { .. }) = tracked {
                        // A submodule's directory: its content is not ours.
                        found.files.insert(child, gitlink.clone());
                        continue;
                    }
                    let tracked_dir = base.dirs.contains(&child);
                    let child_ignored = ignored || ignores::is_ignored(&rules, &child, true);
                    if child_ignored && !tracked_dir {
                        continue;
                    }
                    if !tracked_dir && holds_marker(&dir, &os) {
                        continue;
                    }
                    pending.push((child, rules.clone(), child_ignored));
                }
                StatKind::File | StatKind::Symlink => {
                    let child_ignored = ignored || ignores::is_ignored(&rules, &child, false);
                    if child_ignored && tracked.is_none() {
                        continue;
                    }
                    // Symlinks are cheap to read again, and their size in
                    // the metadata is not reliable everywhere.
                    let cached = match stat.kind {
                        StatKind::File => old.lookup(&child, &stat),
                        _ => None,
                    };
                    match cached {
                        Some(blob) => {
                            found
                                .files
                                .insert(child.clone(), op_for(&stat, blob, stat.size));
                            if let Some(entry) = cache::Entry::new(&stat, blob) {
                                found.cache.entries.insert(child, entry);
                            }
                        }
                        None => misses.entry(path.clone()).or_default().push(Miss {
                            name: name.to_vec(),
                            path: child,
                            stat,
                        }),
                    }
                }
                StatKind::Other => {}
            }
        }
    }

    let hashed: Vec<Vec<Hashed>> = misses
        .into_par_iter()
        .map(|(dir_path, files)| hash_directory(root, &dir_path, files))
        .collect::<Result<_, Error>>()?;
    for hashed in hashed.into_iter().flatten() {
        if let Some(entry) = hashed
            .stable
            .and_then(|stat| cache::Entry::new(&stat, hashed.blob))
        {
            found.cache.entries.insert(hashed.path.clone(), entry);
        }
        found.files.insert(hashed.path, hashed.op);
    }
    Ok(found)
}

/// Names the walk never looks at: `.braid` and `.git` and every name that
/// some file system would read as one of them, and braid's scratch files.
fn skipped(name: &[u8]) -> bool {
    braid_format::name::is_reserved(name) || name.starts_with(SCRATCH_PREFIX.as_bytes())
}

/// Whether the directory `name` of `dir` holds a `.braid` or `.git` entry:
/// another workspace or repository, which an untracked directory is not part
/// of this one.
fn holds_marker(dir: &Dir, name: &OsStr) -> bool {
    [braid_repo::DIR, ".git"]
        .iter()
        .any(|marker| dir.symlink_metadata(Path::new(name).join(marker)).is_ok())
}

/// The names in `dir`, sorted.
fn sorted_names(root: &Root, dir: &Dir, path: &[u8]) -> Result<Vec<std::ffi::OsString>, Error> {
    let mut names = Vec::new();
    let entries = dir.entries().map_err(|err| root.error("read", path, err))?;
    for entry in entries {
        let entry = entry.map_err(|err| root.error("read", path, err))?;
        names.push(entry.file_name());
    }
    names.sort();
    Ok(names)
}

/// The change operation that sets a path to this file.
fn op_for(stat: &Stat, blob: ObjectId, size: u64) -> ChangeOp {
    match stat.kind {
        StatKind::Symlink => ChangeOp::Symlink { blob, size },
        _ if stat.executable => ChangeOp::Executable { blob, size },
        _ => ChangeOp::File { blob, size },
    }
}

/// One file hashed.
struct Hashed {
    path: Vec<u8>,
    op: ChangeOp,
    blob: ObjectId,
    /// Its metadata, if it did not change while it was read.
    stable: Option<Stat>,
}

/// Hashes the files of one directory. Files that vanished since the walk
/// are left out.
fn hash_directory(root: &Root, dir_path: &[u8], files: Vec<Miss>) -> Result<Vec<Hashed>, Error> {
    let dir = match root.open_dir(dir_path)? {
        Found::Here(dir) => dir,
        Found::Missing | Found::Blocked(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(files.len());
    for miss in files {
        let Some(os) = os_name(&miss.name) else {
            continue;
        };
        let read = match miss.stat.kind {
            StatKind::Symlink => hash_symlink(&dir, os),
            _ => match hash_file(&dir, os) {
                // Replaced by a symlink since the walk.
                Err(_) if dir.symlink_metadata(os).is_ok_and(|meta| meta.is_symlink()) => {
                    hash_symlink(&dir, os)
                }
                other => other,
            },
        };
        match read {
            Ok((op, blob, stable)) => out.push(Hashed {
                path: miss.path,
                op,
                blob,
                stable,
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(root.error("read", &miss.path, err)),
        }
    }
    Ok(out)
}

type Read3 = (ChangeOp, ObjectId, Option<Stat>);

fn hash_file(dir: &Dir, name: &OsStr) -> io::Result<Read3> {
    let mut file = open_file(dir, name)?;
    let before = Stat::of(&file.metadata()?);
    if before.kind != StatKind::File {
        return Err(io::Error::other("not a regular file"));
    }
    let mut hasher = IdHasher::new(Kind::Blob);
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let after = Stat::of(&file.metadata()?);
    let blob = hasher.finalize();
    let stable = (before == after && size == after.size).then_some(after);
    Ok((op_for(&before, blob, size), blob, stable))
}

fn hash_symlink(dir: &Dir, name: &OsStr) -> io::Result<Read3> {
    let target = read_link(dir, name)?;
    let blob = ObjectId::hash(Kind::Blob, &target);
    Ok((
        ChangeOp::Symlink {
            blob,
            size: target.len() as u64,
        },
        blob,
        None,
    ))
}

/// The system clock less two seconds, for when the file system's own clock
/// cannot be read: older than any modification a coarse file system clock
/// could still date to the same instant.
fn fallback_now() -> (i64, u32) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(Duration::from_secs(2));
    (since.as_secs() as i64, since.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration;

    fn blob(content: &str) -> ChangeOp {
        ChangeOp::File {
            blob: ObjectId::hash(Kind::Blob, content.as_bytes()),
            size: content.len() as u64,
        }
    }

    /// A directory with a `.braid` file, as every workspace has.
    fn workspace() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(braid_repo::DIR), b"braid-workspace 1\n").unwrap();
        let root = Root::open(dir.path()).unwrap();
        (dir, root)
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn unchanged_files_are_not_read_again() {
        let (dir, root) = workspace();
        let path = dir.path().join("a.txt");
        fs::write(&path, "one").unwrap();
        let past = SystemTime::now() - Duration::from_secs(30);
        set_mtime(&path, past);
        let base = Flat::default();

        let first = scan(&root, &base, &Cache::default()).unwrap();
        assert_eq!(first.files[&b"a.txt".to_vec()], blob("one"));
        assert!(first.cache.entries.contains_key(&b"a.txt".to_vec()));

        // Rewrite the content in place behind the cache's back: same inode,
        // same size, same modification time. The cache vouches for it, so
        // the file is not read, and the old blob is what the scan reports.
        let mut file = fs::File::options().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"two").unwrap();
        drop(file);
        set_mtime(&path, past);
        let second = scan(&root, &base, &first.cache).unwrap();
        assert_eq!(second.files[&b"a.txt".to_vec()], blob("one"));

        // A file whose time is not strictly older than the cache's scan is
        // read again, whatever the rest of its metadata says.
        let (seconds, nanos) = second.cache.time;
        let same_instant = UNIX_EPOCH + Duration::new(seconds as u64, nanos);
        set_mtime(&path, same_instant);
        let mut racy = second.cache.clone();
        racy.entries.get_mut(&b"a.txt".to_vec()).unwrap().mtime = (seconds, nanos);
        let third = scan(&root, &base, &racy).unwrap();
        assert_eq!(third.files[&b"a.txt".to_vec()], blob("two"));
    }

    #[test]
    fn what_the_walk_skips() {
        let (dir, root) = workspace();
        let at = |p: &str| dir.path().join(p);
        fs::write(at(".gitignore"), "*.log\nbuild/\n").unwrap();
        fs::write(at("app.log"), "ignored").unwrap();
        fs::write(at("kept.log"), "tracked, so not ignored").unwrap();
        fs::create_dir_all(at("build/sub")).unwrap();
        fs::write(at("build/out.o"), "ignored").unwrap();
        fs::write(at("build/sub/tracked.txt"), "tracked").unwrap();
        fs::create_dir_all(at(".git")).unwrap();
        fs::write(at(".git/config"), "skipped").unwrap();
        fs::write(at(".braid-tmp-0123"), "scratch").unwrap();
        fs::create_dir_all(at("nested")).unwrap();
        fs::write(at("nested/.braid"), "another workspace").unwrap();
        fs::write(at("nested/file"), "not ours").unwrap();
        fs::write(at("src.rs"), "code").unwrap();

        let mut base = Flat::default();
        base.files.insert(b"kept.log".to_vec(), blob("old"));
        base.files
            .insert(b"build/sub/tracked.txt".to_vec(), blob("tracked"));
        base.dirs.insert(b"build".to_vec());
        base.dirs.insert(b"build/sub".to_vec());

        let found = scan(&root, &base, &Cache::default()).unwrap();
        let paths: Vec<String> = found
            .files
            .keys()
            .map(|p| String::from_utf8(p.clone()).unwrap())
            .collect();
        assert_eq!(
            paths,
            [".gitignore", "build/sub/tracked.txt", "kept.log", "src.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_read_not_followed() {
        let (dir, root) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let found = scan(&root, &Flat::default(), &Cache::default()).unwrap();
        let target = outside.path().as_os_str().as_encoded_bytes().to_vec();
        assert_eq!(found.files.len(), 1);
        assert_eq!(
            found.files[&b"link".to_vec()],
            ChangeOp::Symlink {
                blob: ObjectId::hash(Kind::Blob, &target),
                size: target.len() as u64,
            }
        );
    }
}
