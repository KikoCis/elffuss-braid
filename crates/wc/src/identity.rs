// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Who makes a change: from `--author`, then `BRAID_AUTHOR`, then the
//! `name` and `email` of the repository's `.braid/config` (see
//! [`braid_repo::Config`]).
//!
//! Signatures record the current time with the offset of the local time
//! zone, or of UTC when it cannot be determined (see
//! [`braid_repo::local_now`]).

use braid_format::Signature;
use braid_repo::Repo;

use crate::error::Error;

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

    /// The identity that the repository's `.braid/config` sets, if it sets
    /// a name. It must set an email address too.
    pub fn from_config(repo: &Repo) -> Result<Option<Identity>, Error> {
        let config = repo.config()?;
        let Some(name) = config.name else {
            return Ok(None);
        };
        let invalid = |reason| Error::InvalidAuthor {
            text: format!("name = {name} in .braid/config"),
            reason,
        };
        let email = config
            .email
            .ok_or_else(|| invalid("the config sets a name but no email"))?;
        Identity::new(&name, &email).map(Some).map_err(invalid)
    }

    /// The identity given (by `--author` or `BRAID_AUTHOR`), or else the
    /// one in the repository's config.
    pub fn resolve(given: Option<&str>, repo: &Repo) -> Result<Identity, Error> {
        match given {
            Some(text) => Identity::parse(text),
            None => Identity::from_config(repo)?.ok_or(Error::NoAuthor),
        }
    }

    /// A signature by this identity, now, in local time.
    pub fn now(&self) -> Signature {
        braid_repo::signature_now(self.name.as_bytes(), self.email.as_bytes())
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
        assert_eq!(signature.email, b"ada@example.org");
        assert!(signature.seconds > 1_700_000_000);
        assert!(signature.offset_minutes.abs() < 24 * 60);
    }

    #[test]
    fn config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        assert_eq!(Identity::from_config(&repo).unwrap(), None);
        assert!(matches!(
            Identity::resolve(None, &repo),
            Err(Error::NoAuthor)
        ));
        let config = dir
            .path()
            .join(braid_repo::DIR)
            .join(braid_repo::CONFIG_FILE);
        std::fs::write(&config, "# who commits\nname = Grace Hopper\n").unwrap();
        assert!(matches!(
            Identity::resolve(None, &repo),
            Err(Error::InvalidAuthor { .. })
        ));
        std::fs::write(
            &config,
            "# who commits\nname = Grace Hopper\nemail = grace@example.org\n",
        )
        .unwrap();
        let grace = Identity::resolve(None, &repo).unwrap();
        assert_eq!(grace.name, "Grace Hopper");
        assert_eq!(grace.email, "grace@example.org");
        let given = Identity::resolve(Some("Ada <ada@example.org>"), &repo).unwrap();
        assert_eq!(given.name, "Ada");
    }
}
