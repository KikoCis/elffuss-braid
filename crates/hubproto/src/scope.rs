// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Token scopes: which repositories a token reaches, and what it may do
//! there. A scope's `repo` is `owner/name`, `owner/*` or `*`; a right is
//! `read`, `interact`, `write`, `land:<ref glob>`, `verify:<ref glob>` or
//! `admin`.
//!
//! `write` includes `read`, and `admin` includes both. `interact` covers the
//! social writes (stars, watching, following, issues, comments, reviews and
//! forks) on what the account may read; `write` and `admin` include it, and
//! it gives no read of its own. A `land:<glob>` right lets a token land on
//! the refs its glob matches, and only together with `write`.
//!
//! A `verify:<glob>` right lets a token post verdicts on the branches its
//! glob matches (section 25), and nothing else: it counts only where the
//! token's account may write, and only on a token that reads the repository
//! but holds no `write`, `admin`, `land:` or `interact` there, so that the
//! token of a CI job can say how its tests went and do nothing more.
//!
//! [`Right`] may gain rights: a client keeps a scope it cannot read out of
//! what it decides, as [`ParsedScope::parse`] refuses it whole.

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
    #[error(
        "{0:?} is not a right: expected read, interact, write, admin, land:<ref glob> or verify:<ref glob>"
    )]
    Right(String),
    /// A scope without rights.
    #[error("the scope for {0:?} grants no right")]
    NoRights(String),
}

/// One right of a scope. More may come: a `match` on it ends with `_`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// Posting verdicts on the branches the glob matches (section 25),
    /// where the account may write, on a token that holds nothing more
    /// there than `read`.
    Verify(String),
}

/// The glob of a `land:` or `verify:` right: not empty, at most 4,096
/// bytes, and without control characters.
fn glob(text: &str) -> Option<String> {
    (!text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

impl Right {
    /// Reads `read`, `interact`, `write`, `admin`, `land:<glob>` or
    /// `verify:<glob>`. A glob is not empty and holds no control character.
    pub fn parse(text: &str) -> Result<Right, ScopeError> {
        let refused = || ScopeError::Right(text.to_owned());
        match text {
            "read" => Ok(Right::Read),
            "interact" => Ok(Right::Interact),
            "write" => Ok(Right::Write),
            "admin" => Ok(Right::Admin),
            _ => {
                if let Some(rest) = text.strip_prefix("land:") {
                    glob(rest).map(Right::Land).ok_or_else(refused)
                } else if let Some(rest) = text.strip_prefix("verify:") {
                    glob(rest).map(Right::Verify).ok_or_else(refused)
                } else {
                    Err(refused())
                }
            }
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
            Right::Verify(glob) => write!(f, "verify:{glob}"),
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
            ("verify:main", Right::Verify("main".into())),
            ("verify:*", Right::Verify("*".into())),
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
            "verify:",
            "verify",
            "VERIFY:main",
            "Verify:main",
            "verify:a\nb",
        ] {
            assert_eq!(
                Right::parse(bad),
                Err(ScopeError::Right(bad.to_owned())),
                "{bad:?}"
            );
        }
        let long = format!("verify:{}", "a".repeat(4096));
        assert!(Right::parse(&long).is_ok());
        assert!(Right::parse(&format!("{long}a")).is_err());
        // The error names every right there is.
        let said = ScopeError::Right("push".into()).to_string();
        assert!(
            said.contains("land:<ref glob>") && said.contains("verify:<ref glob>"),
            "{said}"
        );
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
        // The scope of a CI job's token: it reads, and posts verdicts.
        let ci = Scope {
            repo: "ada/demo".into(),
            rights: vec!["read".into(), "verify:main".into()],
        };
        let parsed = ParsedScope::parse(&ci).unwrap();
        assert_eq!(parsed.rights, [Right::Read, Right::Verify("main".into())]);
        assert_eq!(parsed.to_scope(), ci);
        // A right this crate cannot read refuses its scope whole.
        let later = Scope {
            repo: "ada/demo".into(),
            rights: vec!["read".into(), "someday:main".into()],
        };
        assert_eq!(
            ParsedScope::parse(&later),
            Err(ScopeError::Right("someday:main".into()))
        );
    }
}
