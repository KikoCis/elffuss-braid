// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Owners, repository names, and the globs of `land:<glob>` rights.

use std::fmt;
use std::str::FromStr;

/// Why a name is not valid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Not a GitHub login.
    #[error(
        "{0:?} is not a GitHub login: 1 to 39 letters, digits or single hyphens, not at either end"
    )]
    Owner(String),
    /// Not a repository name.
    #[error(
        "{0:?} is not a repository name: 1 to 100 of letters, digits, '.', '_' and '-', not starting with '.' or ending with \".git\""
    )]
    Name(String),
    /// Not `owner/name`.
    #[error("{0:?} is not a repository: expected \"owner/name\"")]
    Path(String),
}

/// Whether `owner` is a GitHub login: 1 to 39 ASCII letters, digits and
/// hyphens, with no hyphen at either end and no two in a row.
pub fn is_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && !owner.contains("--")
}

/// Whether `name` is a repository name: 1 to 100 of ASCII letters, digits,
/// `.`, `_` and `-`, not starting with `.` and not ending with `.git`.
pub fn is_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !name.starts_with('.')
        && !name.to_ascii_lowercase().ends_with(".git")
}

/// A repository on a hub: `owner/name`, the owner lowercase.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath {
    /// The owner's GitHub login, lowercase.
    pub owner: String,
    /// The repository's name, as given.
    pub name: String,
}

impl RepoPath {
    /// Checks both parts; the owner is lowercased.
    pub fn new(owner: &str, name: &str) -> Result<RepoPath, NameError> {
        if !is_owner(owner) {
            return Err(NameError::Owner(owner.to_owned()));
        }
        if !is_repo_name(name) {
            return Err(NameError::Name(name.to_owned()));
        }
        Ok(RepoPath {
            owner: owner.to_ascii_lowercase(),
            name: name.to_owned(),
        })
    }

    /// The path of the repository's routes: `/v1/r/<owner>/<name>`.
    pub fn api_path(&self) -> String {
        format!("/v1/r/{}/{}", self.owner, self.name)
    }
}

impl FromStr for RepoPath {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (owner, name) = text
            .split_once('/')
            .ok_or_else(|| NameError::Path(text.to_owned()))?;
        RepoPath::new(owner, name)
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Whether a ref name is one of braid's own, which clients never write:
/// under `braid/` or `remote/`.
pub fn is_internal_ref(name: &str) -> bool {
    name.starts_with("braid/") || name.starts_with("remote/")
}

/// Whether the glob of a `land:<glob>` right matches the ref `name`. `*`
/// alone matches every ref; otherwise `*` matches any run of characters
/// other than `/`, and everything else matches itself.
///
/// Since `*` never matches a `/`, the glob's parts between slashes match the
/// name's one for one, and each part is matched going back no further than
/// its last `*`: time bounded by the product of the lengths, never
/// exponential, whatever globs and names a client sends.
pub fn glob_matches(glob: &str, name: &str) -> bool {
    if glob == "*" {
        return true;
    }
    let mut globs = glob.split('/');
    let mut names = name.split('/');
    loop {
        match (globs.next(), names.next()) {
            (None, None) => return true,
            (Some(glob), Some(name)) if part_matches(glob.as_bytes(), name.as_bytes()) => {}
            _ => return false,
        }
    }
}

/// Whether `glob`, in which `*` matches any run of bytes, matches all of
/// `name`.
fn part_matches(glob: &[u8], name: &[u8]) -> bool {
    let (mut g, mut n) = (0, 0);
    // Just after the last `*` seen, and where its match ends so far.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if glob.get(g) == Some(&b'*') {
            g += 1;
            star = Some((g, n));
        } else if glob.get(g) == Some(&name[n]) {
            g += 1;
            n += 1;
        } else if let Some((after, end)) = star {
            g = after;
            n = end + 1;
            star = Some((after, n));
        } else {
            return false;
        }
    }
    glob[g..].iter().all(|byte| *byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners() {
        for good in ["kikocis", "Kiko-Cis", "a", "a1-b2"] {
            assert!(is_owner(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a--b", "a_b", "a.b", &"a".repeat(40)] {
            assert!(!is_owner(bad), "{bad}");
        }
    }

    #[test]
    fn repo_names() {
        for good in ["braid", "elffuss-braid", "a.b_c-d", "x"] {
            assert!(is_repo_name(good), "{good}");
        }
        for bad in [
            "",
            ".hidden",
            "a/b",
            "a b",
            "repo.git",
            "repo.GIT",
            &"a".repeat(101),
        ] {
            assert!(!is_repo_name(bad), "{bad}");
        }
    }

    #[test]
    fn repo_paths() {
        let path: RepoPath = "KikoCis/elffuss-braid".parse().unwrap();
        assert_eq!(path.owner, "kikocis");
        assert_eq!(path.to_string(), "kikocis/elffuss-braid");
        assert_eq!(path.api_path(), "/v1/r/kikocis/elffuss-braid");
        assert!("nobody".parse::<RepoPath>().is_err());
        assert!("a/b/c".parse::<RepoPath>().is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*", "main"));
        assert!(glob_matches("*", "release/1.0"));
        assert!(glob_matches("main", "main"));
        assert!(!glob_matches("main", "mainline"));
        assert!(glob_matches("release/*", "release/1.0"));
        assert!(!glob_matches("release/*", "release/1/2"));
        assert!(glob_matches("agents/*/work", "agents/a1/work"));
        assert!(!glob_matches("agents/*", "main"));
        assert!(glob_matches("release/*", "release/"));
        assert!(glob_matches("*-x*", "a-xb") && glob_matches("a*b*c", "abc"));
        assert!(glob_matches("a*b*c", "a-b-b-c") && !glob_matches("a*b*c", "a-b-b-"));
        assert!(glob_matches("**", "x") && !glob_matches("**", "x/y"));
        // Globs that backtrack forever, matched at once.
        let name = "a".repeat(4096);
        let glob = format!("{}b", "*a".repeat(50));
        assert!(!glob_matches(&glob, &name));
        assert!(glob_matches(&glob, &format!("{name}b")));
    }
}
