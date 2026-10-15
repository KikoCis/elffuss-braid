// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Git remotes for `git sync`: their addresses, the labels braid knows them
//! by, and the git commands that fetch from them and push to them.
//!
//! Fetching and pushing run the `git` binary as a subprocess, since gitoxide
//! does not push yet. Each command runs in braid's mirror of the remote, with
//! the caller's environment (so credentials, SSH settings and proxies work as
//! for any git command) except for the variables that would point git at
//! another repository, and with these settings on top of the user's: no
//! hooks, no automatic garbage collection or maintenance in the mirror, and
//! no `ext::` transport.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use braid_git::GitOid;

use crate::Error;

/// The longest name a label keeps from its remote, before the hash.
const MAX_LABEL_NAME: usize = 40;

/// Hex digits of the hash at the end of a label.
const LABEL_HASH_DIGITS: usize = 12;

/// A git remote: where it is, and the label braid knows it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    address: String,
    label: String,
}

impl Remote {
    /// Reads a remote as given to `git sync`: a URL (`https://`, `ssh://`,
    /// `file://` and so on), an scp-like address such as `host:path`, or a
    /// path.
    ///
    /// A path is made absolute, against the current directory, and resolved
    /// when it exists, since the git commands run in the mirror. An address
    /// that is empty, starts with `-`, or holds a control character is
    /// refused.
    ///
    /// The label is the last name of the address's path, without `.git` and
    /// with every byte other than ASCII letters, digits, `.`, `_` and `-`
    /// turned into `-` (at most 40 of them, `remote` if none is left), then a
    /// `-` and the first 12 hex digits of the BLAKE3 hash of the address
    /// without any user name or password and without a trailing `/` or
    /// `.git`. So `https://example.org/team/project.git` is `project-` and 12
    /// hex digits, with or without `.git`, a trailing `/` or credentials, and
    /// credentials never reach a label.
    pub fn new(remote: &str) -> Result<Remote, Error> {
        let invalid = |reason| Error::InvalidRemote {
            remote: remote.to_owned(),
            reason,
        };
        if remote.is_empty() {
            return Err(invalid("it is empty"));
        }
        if remote.starts_with('-') {
            return Err(invalid("it starts with '-'"));
        }
        if remote.chars().any(char::is_control) {
            return Err(invalid("it holds a control character"));
        }
        let address = if remote.contains("://") || is_scp_like(remote) {
            remote.to_owned()
        } else {
            local_path(remote)?
        };
        let label = label_of(&address);
        Ok(Remote { address, label })
    }

    /// The address git is given: the remote as written, or for a path, the
    /// absolute path.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The label: a short name for the remote, made only of ASCII letters,
    /// digits, `.`, `_` and `-`. See [`Remote::new`].
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// Whether git reads an address as `[user@]host:path`, which goes over SSH:
/// a colon with no slash before it.
fn is_scp_like(address: &str) -> bool {
    match address.find(':') {
        Some(colon) => colon > 0 && !address[..colon].contains('/'),
        None => false,
    }
}

/// A local path, absolute, and resolved when it exists.
fn local_path(path: &str) -> Result<String, Error> {
    let absolute = std::path::absolute(path)?;
    let resolved = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());
    Ok(match resolved.to_str() {
        Some(text) => text.to_owned(),
        None => absolute.to_string_lossy().into_owned(),
    })
}

/// The address without user name, password, trailing `/` or `.git`.
fn normalized(address: &str) -> String {
    let mut text = if let Some(scheme) = address.find("://") {
        let start = scheme + 3;
        let end = address[start..]
            .find('/')
            .map_or(address.len(), |slash| start + slash);
        match address[start..end].rfind('@') {
            Some(at) => format!("{}{}", &address[..start], &address[start + at + 1..]),
            None => address.to_owned(),
        }
    } else if is_scp_like(address) {
        let colon = address.find(':').unwrap_or(0);
        match address[..colon].rfind('@') {
            Some(at) => address[at + 1..].to_owned(),
            None => address.to_owned(),
        }
    } else {
        address.to_owned()
    };
    while text.ends_with('/') {
        text.pop();
    }
    if let Some(stripped) = text.strip_suffix(".git") {
        text.truncate(stripped.len());
    }
    while text.ends_with('/') {
        text.pop();
    }
    text
}

fn label_of(address: &str) -> String {
    let normalized = normalized(address);
    let last = normalized.rsplit(['/', ':']).next().unwrap_or("");
    let mut name = String::new();
    for c in last.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && name.ends_with('-')) {
            name.push(c);
        }
    }
    let mut name = name.trim_matches(['-', '.']).to_owned();
    if name.len() > MAX_LABEL_NAME {
        name.truncate(MAX_LABEL_NAME);
        name = name.trim_end_matches(['-', '.']).to_owned();
    }
    if name.is_empty() {
        name.push_str("remote");
    }
    let hash = blake3::hash(normalized.as_bytes()).to_hex();
    format!("{name}-{}", &hash[..LABEL_HASH_DIGITS])
}

/// Environment variables that would point git at another repository, or
/// change which objects it sees. The commands sync runs never inherit them.
const REPOSITORY_VARIABLES: [&str; 13] = [
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
    "GIT_QUARANTINE_PATH",
];

/// Settings every command sync runs gets on top of the user's.
const SETTINGS: [&str; 4] = [
    "core.hooksPath=/dev/null",
    "gc.auto=0",
    "maintenance.auto=false",
    "protocol.ext.allow=never",
];

/// Whether a fetch found the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// The branch was fetched into the ref asked for.
    Branch,
    /// The remote has no such branch.
    Absent,
}

/// What a push did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The remote branch points at the commit now.
    Done,
    /// The remote branch did not move; this is git's reason.
    Refused(String),
}

/// The `git` binary, run with extra environment variables.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Git<'a> {
    env: &'a [(OsString, OsString)],
}

impl<'a> Git<'a> {
    pub(crate) fn new(env: &'a [(OsString, OsString)]) -> Self {
        Git { env }
    }

    /// A git command that runs in the repository `git_dir`, if given.
    fn command(&self, git_dir: Option<&Path>) -> Command {
        let mut command = Command::new("git");
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        for variable in REPOSITORY_VARIABLES {
            command.env_remove(variable);
        }
        for setting in SETTINGS {
            command.arg("-c").arg(setting);
        }
        if let Some(dir) = git_dir {
            command.arg("--git-dir").arg(dir).current_dir(dir);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs a command and returns its output, whatever its exit status.
    fn output(&self, what: &'static str, mut command: Command) -> Result<Output, Error> {
        command.output().map_err(|err| Error::GitCommand {
            command: what,
            detail: format!("cannot run git: {err}"),
        })
    }

    /// Runs a command that must succeed.
    fn run(&self, what: &'static str, command: Command) -> Result<Output, Error> {
        let output = self.output(what, command)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(failure(what, &output))
        }
    }

    /// Creates an empty bare repository, without hooks or other templates.
    pub(crate) fn init_bare(&self, path: &Path) -> Result<(), Error> {
        let mut command = self.command(None);
        if let Some(parent) = path.parent() {
            command.current_dir(parent);
        }
        command
            .args(["init", "--quiet", "--bare", "--template="])
            .arg(path);
        self.run("init", command).map(drop)
    }

    /// Fetches the branch `branch` (a full ref name) of `remote` into the ref
    /// `into` of the repository `git_dir`, which is deleted first so that
    /// the fetch never needs to force it. No tags, no submodules.
    pub(crate) fn fetch(
        &self,
        git_dir: &Path,
        remote: &str,
        branch: &str,
        into: &str,
    ) -> Result<Fetched, Error> {
        let mut delete = self.command(Some(git_dir));
        delete.args(["update-ref", "-d", into]);
        self.run("update-ref", delete)?;

        let mut fetch = self.command(Some(git_dir));
        fetch
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                remote,
            ])
            .arg(format!("{branch}:{into}"));
        let output = self.output("fetch", fetch)?;
        if output.status.success() {
            return Ok(Fetched::Branch);
        }
        // A missing branch is not an error; anything else is.
        match self.ls_remote(git_dir, remote, branch) {
            Ok(None) => Ok(Fetched::Absent),
            _ => Err(failure("fetch", &output)),
        }
    }

    /// The commit the ref `name` (a full ref name) points to on `remote`.
    pub(crate) fn ls_remote(
        &self,
        git_dir: &Path,
        remote: &str,
        name: &str,
    ) -> Result<Option<GitOid>, Error> {
        let mut command = self.command(Some(git_dir));
        command.args(["ls-remote", "--", remote, name]);
        let output = self.run("ls-remote", command)?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some((oid, found)) = line.split_once('\t')
                && found == name
            {
                return oid.parse().map(Some).map_err(|_| Error::GitCommand {
                    command: "ls-remote",
                    detail: format!("unexpected output {line:?}"),
                });
            }
        }
        Ok(None)
    }

    /// Pushes `commit` from the repository `git_dir` to the branch `branch`
    /// (a full ref name) of `remote`, only if that branch still points to
    /// `expected` (`None`: only if it does not exist): an atomic push with a
    /// lease, never a forced one.
    pub(crate) fn push(
        &self,
        git_dir: &Path,
        remote: &str,
        branch: &str,
        commit: GitOid,
        expected: Option<GitOid>,
    ) -> Result<Pushed, Error> {
        let lease = match expected {
            Some(oid) => format!("--force-with-lease={branch}:{oid}"),
            None => format!("--force-with-lease={branch}:"),
        };
        let refspec = format!("{commit}:{branch}");
        let mut command = self.command(Some(git_dir));
        command
            .args(["push", "--atomic", "--porcelain"])
            .arg(lease)
            .args(["--", remote])
            .arg(&refspec);
        let output = self.output("push", command)?;
        // Porcelain lines are `<flag> TAB <from>:<to> TAB <summary>`.
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(flag), Some(spec)) = (fields.next(), fields.next()) else {
                continue;
            };
            if spec != refspec {
                continue;
            }
            let summary = fields.next().unwrap_or("").trim();
            return Ok(match flag {
                // A fast-forward, a new branch, or already there.
                " " | "*" | "=" => Pushed::Done,
                _ => Pushed::Refused(summary.to_owned()),
            });
        }
        Err(failure("push", &output))
    }
}

/// The error for a git command that failed: its exit status and what it
/// said.
fn failure(what: &'static str, output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let status = match output.status.code() {
        Some(code) => format!("exit status {code}"),
        None => "killed".to_owned(),
    };
    Error::GitCommand {
        command: what,
        detail: if said.is_empty() {
            status
        } else {
            format!("{status}: {}", said.join("; "))
        },
    }
}

/// A temporary bare repository, removed when dropped: where a dry run
/// fetches, borrowing the objects of the mirror when there is one.
#[derive(Debug)]
pub(crate) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(crate) fn create(git: &Git<'_>, borrow: Option<&Path>) -> Result<Scratch, Error> {
        let base = std::path::absolute(std::env::temp_dir())?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let mut attempt = 0u32;
        let scratch = loop {
            let path = base.join(format!(
                "braid-sync-{}-{nanos}-{attempt}.git",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => break Scratch { path },
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 64 => {
                    attempt += 1;
                }
                Err(err) => return Err(err.into()),
            }
        };
        git.init_bare(&scratch.path)?;
        if let Some(mirror) = borrow {
            let info = scratch.path.join("objects").join("info");
            std::fs::create_dir_all(&info)?;
            let objects = std::path::absolute(mirror.join("objects"))?;
            let mut line = objects.into_os_string();
            line.push("\n");
            std::fs::write(info.join("alternates"), line.as_encoded_bytes())?;
        }
        Ok(scratch)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(remote: &str) -> String {
        Remote::new(remote).unwrap().label().to_owned()
    }

    #[test]
    fn labels() {
        let https = label("https://example.org/team/project.git");
        assert!(https.starts_with("project-"), "{https}");
        assert_eq!(https.len(), "project-".len() + LABEL_HASH_DIGITS);
        assert!(
            https[8..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // The same remote written differently, or with credentials.
        for same in [
            "https://example.org/team/project",
            "https://example.org/team/project/",
            "https://user:secret@example.org/team/project.git",
            "https://token@example.org/team/project.git/",
        ] {
            assert_eq!(label(same), https, "{same}");
        }
        assert_ne!(label("https://example.org/other/project.git"), https);
        // scp-like addresses, with or without a user.
        let scp = label("git@example.org:team/project.git");
        assert_eq!(label("example.org:team/project"), scp);
        assert!(scp.starts_with("project-"));
        assert_ne!(scp, https);
        // Odd names are cleaned, and nothing of the user part is kept.
        assert!(label("https://example.org/a/My Repo!!.git").starts_with("My-Repo-"));
        assert!(label("https://example.org/").starts_with("example.org-"));
        assert!(label("file:///").starts_with("remote-"));
        let long = format!("https://example.org/{}", "x".repeat(100));
        assert_eq!(label(&long).len(), MAX_LABEL_NAME + 1 + LABEL_HASH_DIGITS);
        assert!(!label("https://secret@example.org/r.git").contains("secret"));
    }

    #[test]
    fn local_paths_become_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.git");
        std::fs::create_dir(&path).unwrap();
        let remote = Remote::new(path.to_str().unwrap()).unwrap();
        assert!(Path::new(remote.address()).is_absolute());
        assert!(remote.label().starts_with("remote-"));
        let relative = Remote::new("some/relative/dir.git").unwrap();
        assert!(Path::new(relative.address()).is_absolute());
        assert!(relative.address().ends_with("some/relative/dir.git"));
        assert!(relative.label().starts_with("dir-"));
        // An scp-like address and a path with a colon after a slash.
        assert_eq!(Remote::new("host:dir").unwrap().address(), "host:dir");
        assert!(Path::new(Remote::new("./a:b").unwrap().address()).is_absolute());
    }

    #[test]
    fn refused_remotes() {
        for bad in ["", "-upload-pack=touch", "a\nb", "a\0b", "a\tb"] {
            assert!(
                matches!(Remote::new(bad), Err(Error::InvalidRemote { .. })),
                "{bad:?}"
            );
        }
    }
}
