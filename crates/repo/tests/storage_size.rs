// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! How much room a store takes for an imported history.
//!
//! Each test imports a whole history twice: into a store that gets no
//! hints, so that every object is stored whole, and into one that gets the
//! importer's hints, so that blobs and trees can be deltas against the same
//! path in the first parent. One test makes a small history on the spot;
//! the other clones a real public repository with the git command line,
//! packs it with `git gc --aggressive`, and prints the size of each store
//! as a ratio to git's packed objects. Both check that the deltas help and
//! that both stores are sound.
//!
//! The second one needs the network, so it does not run by default:
//!
//! ```text
//! cargo test --release -p elffuss-braid-repo --test storage_size -- --ignored --nocapture
//! ```
//!
//! `BRAID_SIZE_REPO` names another repository to measure: a URL, or a path
//! to a local git repository.

use std::fs;
use std::path::Path;
use std::process::Command;

use braid_format::{Kind, ObjectId, Objects};
use braid_git::{GitMap, ImportOptions, import};
use braid_store::{Options, Store};

/// A small public repository of ordinary source code with a long history.
const DEFAULT_REPO: &str = "https://github.com/rust-lang/log.git";

/// The store without hints: [`Objects::put_similar`] falls back to
/// [`Objects::put`], so nothing becomes a delta.
struct NoHints<'a>(&'a Store);

impl Objects for NoHints<'_> {
    type Error = braid_store::Error;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        self.0.get(id)
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        self.0.put(kind, payload)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        self.0.contains(id)
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        self.0.stat(id)
    }
}

/// Bytes of every file below `dir`.
fn size_of(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            total += size_of(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

fn git(args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Runs git in `dir` with a fixed identity and date, ignoring the user's
/// configuration.
fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.org")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.org")
        .env("GIT_AUTHOR_DATE", "1700000000 +0100")
        .env("GIT_COMMITTER_DATE", "1700000000 +0100")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Source-like text: lines that compress only a little on their own.
fn text(seed: u64, lines: usize) -> String {
    let mut state = seed | 1;
    let mut out = String::new();
    for i in 0..lines {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!("pub const C{i}: u64 = 0x{state:016x};\n"));
    }
    out
}

/// A history made on the spot: a file edited a little in every commit, in a
/// directory with enough files to be split into several tree nodes.
#[test]
fn an_imported_history_keeps_versions_as_deltas() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(work.join("src/many")).unwrap();
    git_in(&work, &["init", "-q"]);
    for i in 0..1100 {
        fs::write(work.join(format!("src/many/f{i:04}.txt")), format!("{i}\n")).unwrap();
    }
    let mut lib = text(1, 2000);
    for n in 0..12 {
        let at = (n * 4099) % (lib.len() - 20);
        lib.replace_range(at..at + 10, &format!("/*{n:06}*/"));
        fs::write(work.join("src/lib.rs"), &lib).unwrap();
        fs::write(
            work.join(format!("src/many/f{:04}.txt", n * 97)),
            format!("v{n}\n"),
        )
        .unwrap();
        git_in(&work, &["add", "-A"]);
        git_in(&work, &["commit", "-q", "-m", &format!("change {n}")]);
    }

    let whole = import_into(&work, &tmp.path().join("whole"), false);
    let deltas = import_into(&work, &tmp.path().join("deltas"), true);
    let whole_report = whole.fsck().unwrap();
    let report = deltas.fsck().unwrap();
    assert!(whole_report.is_clean(), "{:#?}", whole_report.problems);
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(whole_report.delta_records, 0);
    // Eleven later versions of the file, and of most tree nodes on the path
    // to the files that changed: a node may stay whole when a delta would
    // not take a quarter of it or less.
    assert!(report.delta_records >= 11 * 2, "{report:?}");
    assert_eq!(report.objects, whole_report.objects);
    let stats = deltas.stats().unwrap();
    let delta = stats
        .codecs
        .iter()
        .find(|c| c.codec == braid_store::Codec::Delta)
        .unwrap();
    assert!(delta.stored_bytes * 50 < delta.raw_bytes, "{delta:?}");
    assert!(size_of(deltas.root()) < size_of(whole.root()));
}

/// Importing again stores the git map, which grew by a few pairings, as a
/// delta against the map it grew from rather than whole again.
#[test]
fn a_git_map_is_stored_against_the_one_it_grew_from() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(work.join("files")).unwrap();
    git_in(&work, &["init", "-q"]);
    for i in 0..300 {
        fs::write(work.join(format!("files/f{i:03}.txt")), format!("{i}\n")).unwrap();
    }
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "-q", "-m", "first"]);
    let repo = braid_repo::Repo::init(&tmp.path().join("braid")).unwrap();
    repo.git_import(&work, "tester").unwrap();

    fs::write(work.join("files/f000.txt"), "changed\n").unwrap();
    git_in(&work, &["commit", "-q", "-a", "-m", "second"]);
    repo.git_import(&work, "tester").unwrap();

    let stats = repo.store().stats().unwrap();
    let maps = stats
        .kinds
        .iter()
        .find(|k| k.kind == Kind::Derived)
        .unwrap();
    assert_eq!(maps.objects, 2);
    // Two maps of almost the same size: the second one takes little room.
    assert!(maps.stored_bytes * 10 < maps.raw_bytes * 6, "{maps:?}");
    assert!(repo.store().fsck().unwrap().is_clean());
}

/// Imports every branch and tag of the git repository `git_dir` into a new
/// store at `dir`, with or without hints, and returns the store.
fn import_into(git_dir: &Path, dir: &Path, hints: bool) -> Store {
    let store = Store::init(dir, Options::default()).unwrap();
    let mut map = GitMap::new();
    let options = ImportOptions::default();
    let report = if hints {
        import(git_dir, &store, &mut map, &options)
    } else {
        import(git_dir, &NoHints(&store), &mut map, &options)
    }
    .unwrap_or_else(|err| panic!("import: {err}"));
    assert!(report.commits > 0, "{report:?}");
    // The map is part of what an import keeps, as `braid git import` does.
    store.put(Kind::Derived, &map.to_bytes()).unwrap();
    store.sync().unwrap();
    store
}

#[test]
#[ignore = "clones a public repository; run with --ignored"]
fn size_of_a_real_history() {
    let source = std::env::var("BRAID_SIZE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_owned());
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("repo.git");
    let bare_text = bare.to_str().unwrap();
    git(&[
        "clone",
        "--quiet",
        "--bare",
        "--no-local",
        &source,
        bare_text,
    ]);
    git(&[
        "-C",
        bare_text,
        "gc",
        "--quiet",
        "--aggressive",
        "--prune=now",
    ]);
    let git_size = size_of(&bare.join("objects"));

    let whole = import_into(&bare, &tmp.path().join("whole"), false);
    let deltas = import_into(&bare, &tmp.path().join("deltas"), true);
    let whole_size = size_of(whole.root());
    let deltas_size = size_of(deltas.root());

    let ratio = |a: u64, b: u64| a as f64 / b as f64;
    let stats = deltas.stats().unwrap();
    let report = deltas.fsck().unwrap();
    eprintln!(
        "braid / git gc --aggressive, without deltas: {:.2}",
        ratio(whole_size, git_size)
    );
    eprintln!(
        "braid / git gc --aggressive, with deltas:    {:.2}",
        ratio(deltas_size, git_size)
    );
    eprintln!(
        "with deltas / without:                       {:.2}",
        ratio(deltas_size, whole_size)
    );
    eprintln!(
        "objects stored as deltas: {} of {}",
        report.delta_records, report.records
    );
    for codec in &stats.codecs {
        if codec.objects > 0 {
            eprintln!(
                "  {}: {:.3} of the objects, {:.3} of the stored bytes",
                codec.codec.name(),
                ratio(codec.objects, stats.objects),
                ratio(codec.stored_bytes, stats.stored_bytes)
            );
        }
    }
    for kind in &stats.kinds {
        if kind.objects > 0 {
            eprintln!(
                "  {}: {:.3} of the objects, {:.3} of the stored bytes",
                kind.kind.name(),
                ratio(kind.objects, stats.objects),
                ratio(kind.stored_bytes, stats.stored_bytes)
            );
        }
    }

    assert!(report.is_clean(), "{:#?}", report.problems);
    assert!(report.delta_records > 0, "{report:?}");
    assert!(whole.fsck().unwrap().is_clean());
    assert!(deltas_size < whole_size);
    // Both stores hold the same objects.
    assert_eq!(whole.stats().unwrap().objects, stats.objects);
}
