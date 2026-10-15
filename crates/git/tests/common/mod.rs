// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Git repositories for the tests, built with the git command line.
//!
//! Every repository is bare and built with plumbing only, so a test never
//! depends on how a file system treats names, modes or symlinks. Commands run
//! without the user's or the system's configuration, with fixed identities
//! and a clock that ticks once per commit or tag, so every id is reproducible.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// What a path holds in a tree.
#[derive(Clone, Debug)]
pub enum Item {
    /// A regular file.
    File(Vec<u8>),
    /// An executable file.
    Exec(Vec<u8>),
    /// A symlink to this target.
    Link(Vec<u8>),
    /// A submodule at this commit.
    Submodule(String),
}

/// A path and what it holds.
pub type Entry = (Vec<u8>, Item);

/// A path, what it holds, and the id of its blob once stored.
type Stored = (Vec<u8>, Item, Option<String>);

pub fn file(path: impl AsRef<[u8]>, content: impl AsRef<[u8]>) -> Entry {
    (
        path.as_ref().to_vec(),
        Item::File(content.as_ref().to_vec()),
    )
}

pub fn exec(path: impl AsRef<[u8]>, content: impl AsRef<[u8]>) -> Entry {
    (
        path.as_ref().to_vec(),
        Item::Exec(content.as_ref().to_vec()),
    )
}

pub fn link(path: impl AsRef<[u8]>, target: impl AsRef<[u8]>) -> Entry {
    (path.as_ref().to_vec(), Item::Link(target.as_ref().to_vec()))
}

pub fn submodule(path: impl AsRef<[u8]>, commit: &str) -> Entry {
    (path.as_ref().to_vec(), Item::Submodule(commit.to_string()))
}

/// Raw tree bytes from `(mode, name, id)` entries, written as given: any
/// mode, any order, duplicates allowed.
pub fn raw_tree(entries: &[(&str, &[u8], &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (mode, name, id) in entries {
        out.extend_from_slice(mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(name);
        out.push(0);
        out.extend_from_slice(&hex(id));
    }
    out
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// The first commit time; each commit or tag adds a minute.
const EPOCH: i64 = 1_700_000_000;

/// Variables that would point git somewhere else, or change what it reads.
const LEAKY_VARIABLES: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
];

/// A bare repository in a temporary directory.
pub struct Repo {
    dir: TempDir,
    path: PathBuf,
    clock: i64,
}

impl Repo {
    /// An empty SHA-1 repository whose default branch is `main`.
    pub fn new() -> Self {
        Self::init(&[])
    }

    /// An empty repository created with extra `git init` arguments.
    pub fn init(args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.git");
        let mut init = git_command(dir.path(), EPOCH);
        init.args(["init", "--quiet", "--bare"])
            .args(args)
            .arg(&path);
        run(init, None);
        Self {
            dir,
            path,
            clock: EPOCH,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A scratch directory next to the repository.
    pub fn scratch(&self) -> PathBuf {
        let path = self.dir.path().join("scratch");
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A git command that runs in the repository.
    pub fn command(&self) -> Command {
        git_command(&self.path, self.clock)
    }

    /// Runs git and returns its output, trimmed.
    pub fn git(&self, args: &[&str]) -> String {
        let mut command = self.command();
        command.args(args);
        trimmed(run(command, None))
    }

    /// Runs git with `input` on its standard input.
    pub fn git_input(&self, args: &[&str], input: &[u8]) -> String {
        let mut command = self.command();
        command.args(args);
        trimmed(run(command, Some(input)))
    }

    /// Runs git and returns its raw output.
    pub fn git_bytes(&self, args: &[&str]) -> Vec<u8> {
        let mut command = self.command();
        command.args(args);
        run(command, None)
    }

    pub fn blob(&self, content: &[u8]) -> String {
        self.git_input(&["hash-object", "-w", "--stdin"], content)
    }

    /// Writes many blobs with one git process, in order.
    pub fn blobs(&self, contents: &[&[u8]]) -> Vec<String> {
        if contents.is_empty() {
            return Vec::new();
        }
        let scratch = self.scratch();
        let mut paths = String::new();
        for (index, content) in contents.iter().enumerate() {
            let path = scratch.join(format!("blob-{index}"));
            std::fs::write(&path, content).unwrap();
            paths.push_str(path.to_str().unwrap());
            paths.push('\n');
        }
        let out = self.git_input(&["hash-object", "-w", "--stdin-paths"], paths.as_bytes());
        let ids: Vec<String> = out.lines().map(str::to_string).collect();
        assert_eq!(ids.len(), contents.len());
        ids
    }

    /// Stores any bytes as an object of `kind`, without git's checks.
    pub fn literally(&self, kind: &str, bytes: &[u8]) -> String {
        self.git_input(
            &["hash-object", "-t", kind, "-w", "--literally", "--stdin"],
            bytes,
        )
    }

    /// A tree from `(mode, type, id, name)` entries, written by `git mktree`.
    pub fn mktree(&self, entries: &[(&str, &str, &str, &[u8])]) -> String {
        let mut input = Vec::new();
        for (mode, kind, id, name) in entries {
            write!(input, "{mode} {kind} {id}\t").unwrap();
            input.extend_from_slice(name);
            input.push(0);
        }
        self.git_input(&["mktree", "-z", "--missing"], &input)
    }

    /// The tree holding exactly `entries`, with directories made from the
    /// paths.
    pub fn tree(&self, entries: &[Entry]) -> String {
        let contents: Vec<&[u8]> = entries
            .iter()
            .filter_map(|(_, item)| match item {
                Item::File(content) | Item::Exec(content) | Item::Link(content) => {
                    Some(content.as_slice())
                }
                Item::Submodule(_) => None,
            })
            .collect();
        let mut ids = self.blobs(&contents).into_iter();
        let with_ids: Vec<Stored> = entries
            .iter()
            .map(|(path, item)| {
                let id = match item {
                    Item::Submodule(_) => None,
                    _ => ids.next(),
                };
                (path.clone(), item.clone(), id)
            })
            .collect();
        self.tree_level(&with_ids)
    }

    fn tree_level(&self, entries: &[Stored]) -> String {
        let mut dirs: BTreeMap<Vec<u8>, Vec<Stored>> = BTreeMap::new();
        let mut input = Vec::new();
        for (path, item, id) in entries {
            if let Some(slash) = path.iter().position(|byte| *byte == b'/') {
                dirs.entry(path[..slash].to_vec()).or_default().push((
                    path[slash + 1..].to_vec(),
                    item.clone(),
                    id.clone(),
                ));
                continue;
            }
            let (mode, kind, id) = match item {
                Item::File(_) => ("100644", "blob", id.clone().unwrap()),
                Item::Exec(_) => ("100755", "blob", id.clone().unwrap()),
                Item::Link(_) => ("120000", "blob", id.clone().unwrap()),
                Item::Submodule(commit) => ("160000", "commit", commit.clone()),
            };
            write!(input, "{mode} {kind} {id}\t").unwrap();
            input.extend_from_slice(path);
            input.push(0);
        }
        for (name, children) in &dirs {
            let id = self.tree_level(children);
            write!(input, "040000 tree {id}\t").unwrap();
            input.extend_from_slice(name);
            input.push(0);
        }
        self.git_input(&["mktree", "-z", "--missing"], &input)
    }

    /// The tree `git write-tree` writes from a fresh index filled with
    /// `git update-index --add --cacheinfo <mode>,<id>,<path>` for each entry.
    pub fn index_tree(&self, cacheinfo: &[String]) -> String {
        let index = self.scratch().join("index");
        let _ = std::fs::remove_file(&index);
        for info in cacheinfo {
            let mut command = self.command();
            command.env("GIT_INDEX_FILE", &index);
            command.args(["update-index", "--add", "--cacheinfo", info]);
            run(command, None);
        }
        let mut command = self.command();
        command.env("GIT_INDEX_FILE", &index).arg("write-tree");
        trimmed(run(command, None))
    }

    fn tick(&mut self) {
        self.clock += 60;
    }

    /// A commit of `tree` with these parents and message.
    pub fn commit(&mut self, tree: &str, parents: &[&str], message: &str) -> String {
        self.tick();
        let mut args = vec!["commit-tree", tree];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        args.extend(["-m", message]);
        self.git(&args)
    }

    /// The commit a branch points to, if the branch exists.
    pub fn head(&self, branch: &str) -> Option<String> {
        let mut command = self.command();
        command.args(["rev-parse", "--verify", "--quiet"]);
        command.arg(format!("refs/heads/{branch}"));
        let output = command.output().unwrap();
        output.status.success().then(|| trimmed(output.stdout))
    }

    /// Commits exactly `entries` on `branch`, after its current head.
    pub fn snapshot(&mut self, branch: &str, entries: &[Entry], message: &str) -> String {
        self.merge(branch, &[], entries, message)
    }

    /// Commits `entries` on `branch` with the heads of `others` as further
    /// parents.
    pub fn merge(
        &mut self,
        branch: &str,
        others: &[&str],
        entries: &[Entry],
        message: &str,
    ) -> String {
        let tree = self.tree(entries);
        let mut parents: Vec<String> = self.head(branch).into_iter().collect();
        for other in others {
            parents.push(self.head(other).unwrap());
        }
        let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
        let commit = self.commit(&tree, &parents, message);
        self.set_ref(&format!("refs/heads/{branch}"), &commit);
        commit
    }

    pub fn set_ref(&self, name: &str, id: &str) {
        self.git(&["update-ref", name, id]);
    }

    /// An annotated tag named `name` on `target`; returns the tag object.
    pub fn annotated_tag(&mut self, name: &str, target: &str, message: &str) -> String {
        self.tick();
        self.git(&["tag", "--annotate", "-m", message, name, target]);
        self.git(&["rev-parse", &format!("refs/tags/{name}")])
    }

    /// Commits reachable from branches and tags, as git counts them without
    /// replacement objects or grafts.
    pub fn count_commits(&self) -> usize {
        self.git(&[
            "--no-replace-objects",
            "rev-list",
            "--count",
            "--branches",
            "--tags",
        ])
        .parse()
        .unwrap()
    }
}

/// A git command with a fixed environment, run in `dir`.
fn git_command(dir: &Path, clock: i64) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    for variable in LEAKY_VARIABLES {
        command.env_remove(variable);
    }
    let date = format!("{clock} +0100");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_GRAFT_FILE", dir.join("no-grafts"))
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.org")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", "Charles Babbage")
        .env("GIT_COMMITTER_EMAIL", "charles@example.org")
        .env("GIT_COMMITTER_DATE", &date)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "gc.auto=0",
            "-c",
            "core.autocrlf=false",
            "-c",
            "advice.nestedTag=false",
        ]);
    command
}

fn run(mut command: Command, input: Option<&[u8]>) -> Vec<u8> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("git must be installed to run these tests");
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn trimmed(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap().trim().to_string()
}
