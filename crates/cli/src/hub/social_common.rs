// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What the commands of stars, issues, forks, requests and the rest share:
//! the repository a command names, printing JSON or text, the body of a
//! text, idempotency keys, the agent's label, and the exit statuses.
//!
//! Exit statuses: 0 when done, 1 when the hub refused, 2 on conflicts, 3
//! when the hub could not be reached or failed, so that signing in again
//! would not help.

#![allow(dead_code)]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use braid_hubproto::RepoPath;
use clap::Args;
use serde_json::Value;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::link::Link;
use super::remote::HubError;
use crate::{Cli, open_repo, print_json};

/// Done.
pub(crate) const DONE: u8 = 0;

/// The hub refused: the request is not allowed, or not valid.
pub(crate) const REFUSED: u8 = 1;

/// Conflicts, printed as `braid land --json` prints them.
pub(crate) const CONFLICTS: u8 = 2;

/// The hub could not be reached, or failed.
pub(crate) const UNREACHABLE: u8 = 3;

/// The largest body of an issue, a comment or a review, in bytes.
pub(crate) const MAX_TEXT: usize = 64 * 1024;

/// `-R/--hub-repo`, for the commands that act on a repository.
#[derive(Clone, Debug, Default, Args)]
pub(crate) struct HubRepoArg {
    /// The repository on the hub, OWNER/NAME (default: the one this
    /// repository is linked to).
    #[arg(short = 'R', long = "hub-repo", value_name = "OWNER/NAME")]
    pub(crate) hub_repo: Option<String>,
}

/// The hub and the repository a command acts on: `given` (OWNER/NAME) on
/// the hub signed in to, else the repository this one is linked to, on its
/// hub.
pub(crate) fn hub_repo(cli: &Cli, given: Option<&str>) -> Result<(HttpHub, RepoPath)> {
    if let Some(text) = given {
        let path: RepoPath = text
            .parse()
            .with_context(|| format!("{text:?} is not OWNER/NAME"))?;
        let env = HubEnv::from_process();
        let creds = env.credentials()?;
        let url = creds::hub_url(&env, &creds, None)?;
        let (hub, _) = super::client(&env, &url)?;
        return Ok((hub, path));
    }
    let repo = open_repo(cli).context(
        "no repository named: give -R OWNER/NAME, or run this in a repository linked to a hub",
    )?;
    let link =
        Link::read(&repo)?.context("this repository is not linked to a hub: give -R OWNER/NAME")?;
    Ok((super::connect(&link)?, link.repo))
}

/// Prints `value` with `--json`, and `text` otherwise.
pub(crate) fn print(cli: &Cli, value: &Value, text: impl FnOnce() -> String) -> Result<()> {
    if cli.json {
        print_json(value)
    } else {
        println!("{}", text());
        Ok(())
    }
}

/// The body of a text: `--body`, or `--body-file` (`-` reads standard
/// input), at most [`MAX_TEXT`] bytes of UTF-8; `None` when neither is
/// given.
pub(crate) fn body_text(body: Option<&str>, file: Option<&Path>) -> Result<Option<String>> {
    let text = match (body, file) {
        (Some(_), Some(_)) => bail!("give --body or --body-file, not both"),
        (None, None) => return Ok(None),
        (Some(text), None) => text.to_owned(),
        (None, Some(path)) => {
            let mut bytes = Vec::new();
            let limit = MAX_TEXT as u64 + 1;
            if path == Path::new("-") {
                std::io::stdin()
                    .lock()
                    .take(limit)
                    .read_to_end(&mut bytes)
                    .context("cannot read standard input")?;
            } else {
                std::fs::File::open(path)
                    .and_then(|file| file.take(limit).read_to_end(&mut bytes))
                    .with_context(|| format!("cannot read {}", path.display()))?;
            }
            String::from_utf8(bytes).context("the body is not UTF-8")?
        }
    };
    if text.len() > MAX_TEXT {
        bail!("the body is longer than {MAX_TEXT} bytes, the most the hub takes");
    }
    Ok(Some(text))
}

/// A new `Idempotency-Key`: 32 hex digits that differ between calls and
/// processes, so that a command sent again after a lost answer is not done
/// twice.
pub(crate) fn idempotency_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let half = || {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

/// The agent label a command sends: the global `--agent` (or
/// `BRAID_AGENT`), unless it is the default, `cli`.
pub(crate) fn agent(cli: &Cli) -> Option<String> {
    (cli.agent != "cli").then(|| cli.agent.clone())
}

/// The exit status of a request that failed with `err`: [`UNREACHABLE`]
/// when the hub could not be reached or failed, [`REFUSED`] otherwise.
pub(crate) fn exit_for(err: &HubError) -> u8 {
    match err {
        HubError::Api { status, .. } if *status < 500 => REFUSED,
        HubError::Refused { .. } => REFUSED,
        _ => UNREACHABLE,
    }
}

/// What a command that is not in this version of braid answers.
pub(crate) fn not_yet(command: &str) -> Result<ExitCode> {
    bail!("braid hub {command} is not available in this version of braid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_bodies_and_statuses() {
        let key = idempotency_key();
        assert_eq!(key.len(), 32);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(idempotency_key(), key);

        assert_eq!(body_text(None, None).unwrap(), None);
        assert_eq!(body_text(Some("hi"), None).unwrap().as_deref(), Some("hi"));
        assert!(body_text(Some("a"), Some(Path::new("b"))).is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("body.md");
        std::fs::write(&file, "from a file\n").unwrap();
        assert_eq!(
            body_text(None, Some(&file)).unwrap().as_deref(),
            Some("from a file\n")
        );
        std::fs::write(&file, vec![b'a'; MAX_TEXT + 1]).unwrap();
        assert!(body_text(None, Some(&file)).is_err());
        assert!(body_text(Some(&"a".repeat(MAX_TEXT + 1)), None).is_err());

        let refused = HubError::api(403, "forbidden", "no");
        assert_eq!(exit_for(&refused), REFUSED);
        let failed = HubError::api(500, "internal", "failed");
        assert_eq!(exit_for(&failed), UNREACHABLE);
        let away = HubError::Transport {
            hub: "https://hub.example".into(),
            detail: "refused".into(),
        };
        assert_eq!(exit_for(&away), UNREACHABLE);
    }
}
