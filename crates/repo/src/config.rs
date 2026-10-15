// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The repository's own settings: the optional file `.braid/config`.
//!
//! The file holds `key = value` lines (`key: value` works too). Blank lines,
//! lines that start with `#` or `;`, and section headers in square brackets
//! are skipped, and so are keys braid does not know. A value may be quoted
//! with `"`. The keys are:
//!
//! - `name` (or `user.name`): the name braid gives the changesets it makes;
//! - `email` (or `user.email`): their email address.
//!
//! Nothing requires the file. Without an email, the changesets braid makes
//! itself, such as the merges of `git sync`, get an `example.org` address.

use std::fs;
use std::io;

use crate::{DIR, Error, Repo};

/// Name of the configuration file inside [`DIR`].
pub const CONFIG_FILE: &str = "config";

/// What `.braid/config` sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The name for changesets braid makes.
    pub name: Option<String>,
    /// The email address for changesets braid makes.
    pub email: Option<String>,
}

impl Config {
    /// Reads the text of a configuration file. Lines it does not understand
    /// are skipped, and an empty value counts as no value.
    pub fn parse(text: &str) -> Config {
        let mut config = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';', '[']) {
                continue;
            }
            let Some((key, value)) = line.split_once(['=', ':']) else {
                continue;
            };
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(value);
            let value = (!value.is_empty()).then(|| value.to_owned());
            match key.trim() {
                "name" | "user.name" => config.name = value,
                "email" | "user.email" => config.email = value,
                _ => {}
            }
        }
        config
    }
}

impl Repo {
    /// The settings in `.braid/config`; all unset when there is no such file.
    pub fn config(&self) -> Result<Config, Error> {
        match fs::read_to_string(self.root.join(DIR).join(CONFIG_FILE)) {
            Ok(text) => Ok(Config::parse(&text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let config = Config::parse(
            "# who braid is\n[user]\nname = Ada Lovelace\n  email: \"ada@example.org\"\n\
             colour = blue\nnot a setting\n",
        );
        assert_eq!(config.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(config.email.as_deref(), Some("ada@example.org"));
        let config = Config::parse("user.name=Grace\nuser.email =\n; done\n");
        assert_eq!(config.name.as_deref(), Some("Grace"));
        assert_eq!(config.email, None);
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn the_file_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        assert_eq!(repo.config().unwrap(), Config::default());
        fs::write(
            dir.path().join(DIR).join(CONFIG_FILE),
            "email = bot@example.org\n",
        )
        .unwrap();
        assert_eq!(
            repo.config().unwrap().email.as_deref(),
            Some("bot@example.org")
        );
    }
}
