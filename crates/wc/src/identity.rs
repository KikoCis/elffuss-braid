// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Who makes a change: from `--author`, `BRAID_AUTHOR`, or the repository's
//! `.braid/config`.
//!
//! The config is a small text file of `key = value` lines; `#` starts a
//! comment. Two keys are read:
//!
//! ```text
//! name = Ada Lovelace
//! email = ada@example.org
//! ```
//!
//! Signatures record the current time with an offset of zero: braid does
//! not work out the local time zone yet, so times read as UTC.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use braid_format::Signature;

use crate::error::Error;

/// The repository's settings file, inside `.braid`.
pub const CONFIG_FILE: &str = "config";

/// A name and an e-mail address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The name.
    pub name: String,
    /// The e-mail address.
    pub email: String,
}

impl Identity {
    /// Reads `Name <email>`.
    pub fn parse(text: &str) -> Result<Identity, Error> {
        let invalid = |reason| Error::InvalidAuthor {
            text: text.to_owned(),
            reason,
        };
        let (name, rest) = text
            .split_once('<')
            .ok_or_else(|| invalid("expected \"Name <email>\""))?;
        let (email, after) = rest
            .split_once('>')
            .ok_or_else(|| invalid("expected \"Name <email>\""))?;
        if !after.trim().is_empty() {
            return Err(invalid("nothing may follow the e-mail address"));
        }
        Identity::new(name.trim(), email.trim()).map_err(invalid)
    }

    /// An identity from its parts, which must not hold `<`, `>`, a newline
    /// or NUL, so that it can be written as a git signature too.
    pub fn new(name: &str, email: &str) -> Result<Identity, &'static str> {
        let bad = |text: &str| text.contains(['<', '>', '\n', '\r', '\0']);
        if name.is_empty() {
            return Err("the name is empty");
        }
        if bad(name) || bad(email) {
            return Err("a signature cannot hold '<', '>', a newline or NUL");
        }
        Ok(Identity {
            name: name.to_owned(),
            email: email.to_owned(),
        })
    }

    /// The identity in the config of the repository whose root is
    /// `repo_root`, if it names one.
    pub fn from_config(repo_root: &Path) -> Result<Option<Identity>, Error> {
        let path = repo_root.join(braid_repo::DIR).join(CONFIG_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io("read", &path)(err)),
        };
        let mut name = None;
        let mut email = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "name" => name = Some(value.trim().to_owned()),
                "email" => email = Some(value.trim().to_owned()),
                _ => {}
            }
        }
        match (name, email) {
            (Some(name), Some(email)) => {
                Identity::new(&name, &email)
                    .map(Some)
                    .map_err(|reason| Error::InvalidAuthor {
                        text: format!("{name} <{email}>"),
                        reason,
                    })
            }
            _ => Ok(None),
        }
    }

    /// The identity given (by `--author` or `BRAID_AUTHOR`), or else the
    /// one in the repository's config.
    pub fn resolve(given: Option<&str>, repo_root: &Path) -> Result<Identity, Error> {
        match given {
            Some(text) => Identity::parse(text),
            None => Identity::from_config(repo_root)?.ok_or(Error::NoAuthor),
        }
    }

    /// A signature by this identity, now, with an offset of zero.
    pub fn now(&self) -> Signature {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        Signature {
            name: self.name.as_bytes().to_vec(),
            email: self.email.as_bytes().to_vec(),
            seconds,
            offset_minutes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let ada = Identity::parse("Ada Lovelace <ada@example.org>").unwrap();
        assert_eq!(ada.name, "Ada Lovelace");
        assert_eq!(ada.email, "ada@example.org");
        for bad in [
            "Ada",
            "<ada@example.org>",
            "Ada <a>b>",
            "Ada <ada> extra",
            "A\nB <x>",
        ] {
            assert!(Identity::parse(bad).is_err(), "{bad:?}");
        }
        let signature = ada.now();
        assert_eq!(signature.offset_minutes, 0);
        assert!(signature.seconds > 1_700_000_000);
    }

    #[test]
    fn config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Identity::from_config(dir.path()).unwrap(), None);
        assert!(matches!(
            Identity::resolve(None, dir.path()),
            Err(Error::NoAuthor)
        ));
        fs::create_dir(dir.path().join(braid_repo::DIR)).unwrap();
        fs::write(
            dir.path().join(braid_repo::DIR).join(CONFIG_FILE),
            "# who commits\nname = Grace Hopper\nemail = grace@example.org # work\n",
        )
        .unwrap();
        let grace = Identity::resolve(None, dir.path()).unwrap();
        assert_eq!(grace.name, "Grace Hopper");
        assert_eq!(grace.email, "grace@example.org");
        let given = Identity::resolve(Some("Ada <ada@example.org>"), dir.path()).unwrap();
        assert_eq!(given.name, "Ada");
    }
}
