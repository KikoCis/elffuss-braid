// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! File system access inside a workspace that never follows a symlink.
//!
//! Everything goes through [`cap_std`] directory handles, one name at a time:
//! a directory is opened from its parent's handle with `O_NOFOLLOW`, a file
//! is opened or created by its single name in its directory, and the only
//! path that is ever resolved as a whole is the workspace root itself. A
//! symlink in the working copy, whoever made it, is therefore a leaf: braid
//! reads its target as data, replaces or removes it, and never goes through
//! it. On Unix, a directory's listing, its own metadata and its entries'
//! metadata are read through the same handles with plain system calls: the
//! listing through a second handle opened as `.`, and an entry's metadata by
//! its single name, without following a symlink.

use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, Metadata, OpenOptions};

use crate::error::Error;
use crate::fsprobe::{SCRATCH_PREFIX, random_hex};
use crate::paths::{names, os_name, show};

/// What a file system entry is, as far as a workspace cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatKind {
    File,
    Dir,
    Symlink,
    /// A device, a socket, a pipe: never part of a working copy.
    Other,
}

/// The parts of a file's metadata that a workspace uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub kind: StatKind,
    /// The owner may execute it.
    pub executable: bool,
    pub size: u64,
    /// Modification time: seconds and nanoseconds since the Unix epoch.
    pub mtime: (i64, u32),
    /// The file's number on its device, 0 where there is none.
    pub ino: u64,
}

impl Stat {
    pub(crate) fn of(meta: &Metadata) -> Stat {
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            StatKind::Symlink
        } else if file_type.is_dir() {
            StatKind::Dir
        } else if file_type.is_file() {
            StatKind::File
        } else {
            StatKind::Other
        };
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            Stat {
                kind,
                executable: meta.mode() & 0o100 != 0,
                size: meta.size(),
                mtime: (meta.mtime(), u32::try_from(meta.mtime_nsec()).unwrap_or(0)),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok())
                .map_or((0, 0), |since| {
                    (since.as_secs() as i64, since.subsec_nanos())
                });
            Stat {
                kind,
                executable: false,
                size: meta.len(),
                mtime,
                ino: 0,
            }
        }
    }
}

/// What a directory's own metadata says of its entries: when they last
/// changed, by its modification and status change times, and which
/// directory it is. Adding, removing or renaming an entry changes both
/// times; only the first can be set back by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirStamp {
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
    pub ino: u64,
}

impl DirStamp {
    /// Whether both times are strictly older than `time`.
    pub(crate) fn older_than(&self, time: (i64, u32)) -> bool {
        self.mtime < time && self.ctime < time
    }
}

/// The stamp of an open directory, where the system gives one.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
pub(crate) fn dir_stamp(dir: &Dir) -> Option<DirStamp> {
    let stat = rustix::fs::fstat(dir).ok()?;
    Some(DirStamp {
        mtime: (
            i64::try_from(stat.st_mtime).ok()?,
            u32::try_from(stat.st_mtime_nsec).ok()?,
        ),
        ctime: (
            i64::try_from(stat.st_ctime).ok()?,
            u32::try_from(stat.st_ctime_nsec).ok()?,
        ),
        ino: u64::try_from(stat.st_ino).ok()?,
    })
}

/// The stamp of an open directory, where the system gives one: not here,
/// where there is no status change time.
#[cfg(not(unix))]
pub(crate) fn dir_stamp(_dir: &Dir) -> Option<DirStamp> {
    None
}

impl Stat {
    /// The metadata in a `stat` structure, as [`Stat::of`] reads it from
    /// [`Metadata`]. Its fields' types differ between systems.
    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    pub(crate) fn of_raw(stat: &rustix::fs::Stat) -> Stat {
        use rustix::fs::{FileType, RawMode};
        let mode = stat.st_mode as RawMode;
        let kind = match FileType::from_raw_mode(mode) {
            FileType::Symlink => StatKind::Symlink,
            FileType::Directory => StatKind::Dir,
            FileType::RegularFile => StatKind::File,
            _ => StatKind::Other,
        };
        Stat {
            kind,
            executable: mode & 0o100 != 0,
            size: u64::try_from(stat.st_size).unwrap_or(0),
            mtime: (
                i64::try_from(stat.st_mtime).unwrap_or(0),
                u32::try_from(stat.st_mtime_nsec).unwrap_or(0),
            ),
            ino: u64::try_from(stat.st_ino).unwrap_or(0),
        }
    }
}

/// What is at a path of the working copy.
#[derive(Debug)]
pub(crate) enum Found<T> {
    /// This.
    Here(T),
    /// Nothing, and every directory above it that exists is a directory.
    Missing,
    /// Nothing can be there: the prefix of this many bytes is a file, a
    /// symlink or something else that is not a directory.
    Blocked(usize),
}

/// The root directory of a workspace, open.
#[derive(Debug)]
pub(crate) struct Root {
    dir: Dir,
    path: PathBuf,
}

impl Root {
    /// Opens the workspace directory at `path`, which must exist.
    pub(crate) fn open(path: &Path) -> Result<Root, Error> {
        let dir = Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(Error::io("open", path))?;
        Ok(Root {
            dir,
            path: path.to_path_buf(),
        })
    }

    /// The workspace's root directory.
    pub(crate) fn dir(&self) -> &Dir {
        &self.dir
    }

    /// The path the root was opened at, for messages.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The directory at `path` (empty for the root), opened name by name
    /// without following symlinks.
    pub(crate) fn open_dir(&self, path: &[u8]) -> Result<Found<Dir>, Error> {
        let mut dir = self
            .dir
            .try_clone()
            .map_err(Error::io("open", &self.path))?;
        let mut end = 0;
        for name in names(path) {
            end += name.len() + usize::from(end > 0);
            let os = os_name(name).ok_or_else(|| unrepresentable(&path[..end]))?;
            match dir.open_dir_nofollow(os) {
                Ok(next) => dir = next,
                Err(err) => {
                    return match dir.symlink_metadata(os) {
                        Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                            Ok(Found::Missing)
                        }
                        Ok(meta) if !meta.is_dir() => Ok(Found::Blocked(end)),
                        _ => Err(self.error("open", &path[..end], err)),
                    };
                }
            }
        }
        Ok(Found::Here(dir))
    }

    /// The metadata of the entry at `path`, without following symlinks.
    pub(crate) fn stat(&self, path: &[u8]) -> Result<Found<Stat>, Error> {
        let (parent, name) = crate::paths::split_last(path);
        let dir = match self.open_dir(parent)? {
            Found::Here(dir) => dir,
            Found::Missing => return Ok(Found::Missing),
            Found::Blocked(at) => return Ok(Found::Blocked(at)),
        };
        let os = os_name(name).ok_or_else(|| unrepresentable(path))?;
        match dir.symlink_metadata(os) {
            Ok(meta) => Ok(Found::Here(Stat::of(&meta))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Found::Missing),
            Err(err) => Err(self.error("read", path, err)),
        }
    }

    /// The directory at `path`, creating the directories that are missing.
    /// A name on the way that is not a directory is an error, never
    /// something to go through.
    pub(crate) fn create_dirs(&self, path: &[u8]) -> Result<Dir, Error> {
        let mut dir = self
            .dir
            .try_clone()
            .map_err(Error::io("open", &self.path))?;
        let mut end = 0;
        for name in names(path) {
            end += name.len() + usize::from(end > 0);
            let os = os_name(name).ok_or_else(|| unrepresentable(&path[..end]))?;
            match dir.create_dir(os) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(self.error("create directory", &path[..end], err)),
            }
            dir = dir.open_dir_nofollow(os).map_err(|_| Error::Unsafe {
                path: path[..end].to_vec(),
                reason: "it is in the way of a directory: a file or a symlink".to_owned(),
            })?;
        }
        Ok(dir)
    }

    /// The modification time the file system gives a file written now: a
    /// one-byte rewrite of the workspace's `.braid` file, read back. This is
    /// the clock and the granularity of the workspace's own file system,
    /// which the system clock may not match.
    pub(crate) fn file_system_now(&self) -> io::Result<(i64, u32)> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        let mut file = self.dir.open_with(braid_repo::DIR, &options)?;
        let mut first = [0u8; 1];
        file.read_exact(&mut first)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&first)?;
        let meta = file.metadata()?;
        Ok(Stat::of(&meta).mtime)
    }

    pub(crate) fn error(&self, action: &'static str, path: &[u8], source: io::Error) -> Error {
        Error::Io {
            action,
            path: self.path.join(show(path).as_ref()),
            source,
        }
    }
}

fn unrepresentable(path: &[u8]) -> Error {
    Error::Unsafe {
        path: path.to_vec(),
        reason: "this platform cannot represent the name".to_owned(),
    }
}

/// The names in `dir`, less `.` and `..`, in no order, each with whether
/// the listing says it is a directory. Read through a handle of its own on
/// the same directory, opened as `.`, so that `dir` stays usable meanwhile
/// and its position in the listing is never shared.
#[cfg(unix)]
pub(crate) fn list(dir: &Dir) -> io::Result<Vec<(Vec<u8>, bool)>> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut stream = rustix::fs::Dir::new(rustix::fs::openat(dir, ".", flags, Mode::empty())?)?;
    let mut names = Vec::new();
    while let Some(entry) = stream.read() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            let is_dir = entry.file_type() == rustix::fs::FileType::Directory;
            names.push((name.to_vec(), is_dir));
        }
    }
    Ok(names)
}

/// The names in `dir`, less `.` and `..`, in no order, each with whether
/// the listing says it is a directory.
#[cfg(not(unix))]
pub(crate) fn list(dir: &Dir) -> io::Result<Vec<(Vec<u8>, bool)>> {
    let mut names = Vec::new();
    for entry in dir.entries()? {
        let entry = entry?;
        // A name that is not UTF-8 has no repository bytes here.
        if let Ok(name) = entry.file_name().into_string() {
            names.push((
                name.into_bytes(),
                entry.file_type().is_ok_and(|kind| kind.is_dir()),
            ));
        }
    }
    Ok(names)
}

/// The metadata of the entry `name` of `dir`, without following a symlink.
#[cfg(unix)]
pub(crate) fn stat_entry(dir: &Dir, name: &OsStr) -> io::Result<Stat> {
    let stat = rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(Stat::of_raw(&stat))
}

/// The metadata of the entry `name` of `dir`, without following a symlink.
#[cfg(not(unix))]
pub(crate) fn stat_entry(dir: &Dir, name: &OsStr) -> io::Result<Stat> {
    Ok(Stat::of(&dir.symlink_metadata(name)?))
}

/// Opens the directory `name` of `dir` without following a symlink: `None`
/// when nothing is there, or something that is not a directory.
pub(crate) fn open_child_dir(dir: &Dir, name: &OsStr) -> io::Result<Option<Dir>> {
    match dir.open_dir_nofollow(name) {
        Ok(child) => Ok(Some(child)),
        Err(err) => match dir.symlink_metadata(name) {
            Err(missing) if missing.kind() == io::ErrorKind::NotFound => Ok(None),
            Ok(meta) if !meta.is_dir() => Ok(None),
            _ => Err(err),
        },
    }
}

/// Opens the regular file `name` of `dir` for reading, refusing a symlink.
pub(crate) fn open_file(dir: &Dir, name: &OsStr) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    dir.open_with(name, &options)
}

/// The target of the symlink `name` of `dir`, as bytes.
pub(crate) fn read_link(dir: &Dir, name: &OsStr) -> io::Result<Vec<u8>> {
    let target = dir.read_link_contents(name)?;
    crate::paths::name_bytes(target.as_os_str())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "symlink target is not text"))
}

/// Writes `name` in `dir` as a regular file with `content`, mode 0644 or
/// 0755: into a new scratch file first, then renamed over `name`. A
/// directory in the way is removed first, and must be empty.
pub(crate) fn write_file(
    dir: &Dir,
    name: &OsStr,
    content: &[u8],
    executable: bool,
) -> io::Result<()> {
    write_file_from(dir, name, executable, |file| file.write_all(content))
}

/// Writes `name` in `dir` like [`write_file`], with the content `fill`
/// writes into the scratch file, a piece at a time: a large file never
/// needs to be in memory whole. If `fill` fails, `name` is left as it was.
pub(crate) fn write_file_from(
    dir: &Dir,
    name: &OsStr,
    executable: bool,
    fill: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let (scratch, mut file) = create_scratch(dir)?;
    let written = {
        let mut out = io::BufWriter::new(&mut file);
        fill(&mut out).and_then(|()| out.flush())
    }
    .and_then(|()| set_mode(&file, executable));
    drop(file);
    finish(dir, &scratch, name, written)
}

/// Creates `name` in `dir` as a symlink to `target`, which is stored as it
/// is and never resolved; renamed over whatever file or symlink was there.
pub(crate) fn write_symlink(dir: &Dir, name: &OsStr, target: &[u8]) -> io::Result<()> {
    if target.is_empty() || target.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a symlink target must be non-empty and hold no NUL byte",
        ));
    }
    let target = os_name(target).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the platform cannot represent the target",
        )
    })?;
    loop {
        let scratch = format!("{SCRATCH_PREFIX}tmp-{}", random_hex());
        match make_symlink(dir, target, &scratch) {
            Ok(()) => return finish(dir, &scratch, name, Ok(())),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn make_symlink(dir: &Dir, target: &OsStr, name: &str) -> io::Result<()> {
    dir.symlink_contents(target, name)
}

#[cfg(not(unix))]
fn make_symlink(_dir: &Dir, _target: &OsStr, _name: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are only written on Unix-like systems",
    ))
}

/// Removes the file or symlink `name` from `dir`; nothing there is fine.
pub(crate) fn remove_entry(dir: &Dir, name: &OsStr) -> io::Result<()> {
    match dir.remove_file_or_symlink(name) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn create_scratch(dir: &Dir) -> io::Result<(String, File)> {
    loop {
        let name = format!("{SCRATCH_PREFIX}tmp-{}", random_hex());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match dir.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Moves the scratch entry over `name` once it is written, removing an
/// empty directory in the way first; removes the scratch entry on failure.
fn finish(dir: &Dir, scratch: &str, name: &OsStr, written: io::Result<()>) -> io::Result<()> {
    let result = written.and_then(|()| {
        if let Ok(meta) = dir.symlink_metadata(name)
            && meta.is_dir()
        {
            dir.remove_dir(name)?;
        }
        dir.rename(scratch, dir, name)
    });
    if result.is_err() {
        let _ = dir.remove_file_or_symlink(scratch);
    }
    result
}

/// Sets a regular file's mode to exactly 0644, or 0755 if executable.
#[cfg(unix)]
fn set_mode(file: &File, executable: bool) -> io::Result<()> {
    use cap_std::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    file.set_permissions(cap_std::fs::Permissions::from_mode(mode))
}

/// Other systems have no executable bit.
#[cfg(not(unix))]
fn set_mode(_file: &File, _executable: bool) -> io::Result<()> {
    Ok(())
}
