// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Token scopes: which repositories a token reaches, and what it may do
//! there. A scope's `repo` is `owner/name`, `owner/*` or `*`; a right is
//! `read`, `interact`, `write`, `land:<ref glob>` or `admin`.
//!
//! `write` includes `read`, and `admin` includes both. `interact` covers the
//! social writes (stars, watching, following, issues, comments, reviews and
//! forks) on what the account may read; `write` and `admin` include it, and
//! it gives no read of its own. A `land:<glob>` right lets a token land on
//! the refs its glob matches, and only together with `write`.

use std::fmt;

use crate::api::Scope;
use crate::names::{NameError, RepoPath, is_owner};

/// Why a scope is not valid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// The `repo` of a scope is none of `owner/name`, `owner/*` and `*`.
    #[error("{0:?} is not a repository pattern: expected \"owner/name\", \"owner/*\" or \"*\"")]
    Repo(String),
    /// A right that does not exist.
    #[error("{0:?} is not a right: expected read, interact, write, admin or land:<ref glob>")]
    Right(String),
    /// A scope without rights.
    #[error("the scope for {0:?} grants no right")]
    NoRights(String),
}

/// One right of a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Right {
    /// Every `GET` and `HEAD`, `lacks`, `missing` and the ref watch.
    Read,
    /// Stars, watching, following, issues, comments, reviews and forks, on
    /// what the account may read. It gives no read of its own; `write` and
    /// `admin` include it.
    Interact,
    /// Uploads, and `read`.
    Write,
    /// Settings, collaborators and deleting the repository, and `write`.
    Admin,
    /// Landing on the refs the glob matches (see
    /// [`glob_matches`](crate::names::glob_matches)), with `write`.
    Land(String),
}

impl Right {
    /// Reads `read`, `interact`, `write`, `admin` or `land:<glob>`. A glob
    /// is not empty and holds no control character.
    pub fn parse(text: &str) -> Result<Right, ScopeError> {
        match text {
            "read" => Ok(Right::Read),
            "interact" => Ok(Right::Interact),
            "write" => Ok(Right::Write),
            "admin" => Ok(Right::Admin),
            _ => match text.strip_prefix("land:") {
                Some(glob)
                    if !glob.is_empty()
                        && glob.len() <= 4096
                        && !glob.chars().any(char::is_control) =>
                {
                    Ok(Right::Land(glob.to_owned()))
                }
                _ => Err(ScopeError::Right(text.to_owned())),
            },
        }
    }
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Right::Read => f.write_str("read"),
            Right::Interact => f.write_str("interact"),
            Right::Write => f.write_str("write"),
            Right::Admin => f.write_str("admin"),
            Right::Land(glob) => write!(f, "land:{glob}"),
        }
    }
}

/// The repositories a scope reaches.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeRepo {
    /// `*`: every repository.
    All,
    /// `owner/*`: every repository of one owner, lowercase.
    Owner(String),
    /// `owner/name`: one repository.
    Repo(RepoPath),
}

impl ScopeRepo {
    /// Reads `*`, `owner/*` or `owner/name`.
    pub fn parse(text: &str) -> Result<ScopeRepo, ScopeError> {
        if text == "*" {
            return Ok(ScopeRepo::All);
        }
        let invalid = || ScopeError::Repo(text.to_owned());
        match text.split_once('/') {
            Some((owner, "*")) if is_owner(owner) => {
                Ok(ScopeRepo::Owner(owner.to_ascii_lowercase()))
            }
            Some(_) => text
                .parse::<RepoPath>()
                .map(ScopeRepo::Repo)
                .map_err(|_: NameError| invalid()),
            None => Err(invalid()),
        }
    }

    /// Whether the scope reaches `repo`. Owners and names are compared
    /// without regard to case.
    pub fn matches(&self, repo: &RepoPath) -> bool {
        match self {
            ScopeRepo::All => true,
            ScopeRepo::Owner(owner) => owner.eq_ignore_ascii_case(&repo.owner),
            ScopeRepo::Repo(path) => {
                path.owner.eq_ignore_ascii_case(&repo.owner)
                    && path.name.eq_ignore_ascii_case(&repo.name)
            }
        }
    }
}

impl fmt::Display for ScopeRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeRepo::All => f.write_str("*"),
            ScopeRepo::Owner(owner) => write!(f, "{owner}/*"),
            ScopeRepo::Repo(path) => write!(f, "{path}"),
        }
    }
}

/// A scope, read and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedScope {
    /// The repositories it reaches.
    pub repo: ScopeRepo,
    /// What it allows there, at least one right.
    pub rights: Vec<Right>,
}

impl ParsedScope {
    /// Checks a scope as it comes on the wire.
    pub fn parse(scope: &Scope) -> Result<ParsedScope, ScopeError> {
        let repo = ScopeRepo::parse(&scope.repo)?;
        if scope.rights.is_empty() {
            return Err(ScopeError::NoRights(scope.repo.clone()));
        }
        let rights = scope
            .rights
            .iter()
            .map(|right| Right::parse(right))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ParsedScope { repo, rights })
    }

    /// The scope as it goes on the wire, in canonical spelling.
    pub fn to_scope(&self) -> Scope {
        Scope {
            repo: self.repo.to_string(),
            rights: self.rights.iter().map(Right::to_string).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights() {
        for (text, right) in [
            ("read", Right::Read),
            ("interact", Right::Interact),
            ("write", Right::Write),
            ("admin", Right::Admin),
            ("land:*", Right::Land("*".into())),
            ("land:release/*", Right::Land("release/*".into())),
        ] {
            assert_eq!(Right::parse(text), Ok(right.clone()));
            assert_eq!(right.to_string(), text);
        }
        for bad in [
            "",
            "READ",
            "Interact",
            "interact:*",
            "land:",
            "land",
            "push",
            "land:a\nb",
        ] {
            assert!(Right::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn repositories() {
        let demo: RepoPath = "ada/Demo".parse().unwrap();
        let other: RepoPath = "bob/demo".parse().unwrap();
        let all = ScopeRepo::parse("*").unwrap();
        let ada = ScopeRepo::parse("Ada/*").unwrap();
        let one = ScopeRepo::parse("ada/demo").unwrap();
        assert!(all.matches(&demo) && all.matches(&other));
        assert!(ada.matches(&demo) && !ada.matches(&other));
        assert!(one.matches(&demo) && !one.matches(&other));
        assert_eq!(ada.to_string(), "ada/*");
        for bad in [
            "", "ada", "ada/", "/demo", "ada/*/x", "a--b/*", "ada/.x", "**",
        ] {
            assert!(ScopeRepo::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn scopes() {
        let scope = Scope {
            repo: "ada/*".into(),
            rights: vec!["read".into(), "land:main".into()],
        };
        let parsed = ParsedScope::parse(&scope).unwrap();
        assert_eq!(parsed.rights, [Right::Read, Right::Land("main".into())]);
        assert_eq!(parsed.to_scope(), scope);
        let empty = Scope {
            repo: "*".into(),
            rights: Vec::new(),
        };
        assert_eq!(
            ParsedScope::parse(&empty),
            Err(ScopeError::NoRights("*".into()))
        );
    }
}
