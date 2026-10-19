// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub api METHOD PATH`: any route of the hub's API, with the token
//! braid holds for the hub, and its answer printed as it came: what an
//! agent uses for a route no command covers.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Args;

use super::creds::{self, HubEnv};
use super::remote::HubError;
use super::social_common::{REFUSED, UNREACHABLE, exit_for};
use crate::Cli;

/// The long help of `hub api`.
pub(crate) const API_HELP: &str =
    "Send one request to the hub's API and print its answer's body as it came,
with the token braid holds for the hub (BRAID_TOKEN, else the one `braid
login` saved). PATH starts with /v1, such as /v1/me or
/v1/r/OWNER/NAME/refs; docs/hub.md lists the routes. --data or --data-file
(- for standard input) give a JSON body.

The exit status is 0 when the hub did it; 1 when it refused, with its error
printed on standard output; 3 when it could not be reached or failed.";

/// The methods the API takes.
const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// The largest body sent: the protocol's largest JSON body.
const MAX_DATA: u64 = braid_hubproto::limits::MAX_JSON as u64;

/// `hub api`.
#[derive(Debug, Args)]
pub(crate) struct ApiArgs {
    /// The method: GET, HEAD, POST, PUT, PATCH or DELETE.
    #[arg(value_name = "METHOD")]
    pub(crate) method: String,
    /// The route, from /v1 on, such as /v1/me.
    #[arg(value_name = "PATH")]
    pub(crate) path: String,
    /// The request's JSON body.
    #[arg(long, value_name = "JSON", conflicts_with = "data_file")]
    pub(crate) data: Option<String>,
    /// A file holding the request's JSON body; - reads standard input.
    #[arg(long, value_name = "FILE")]
    pub(crate) data_file: Option<PathBuf>,
    /// The hub (default: BRAID_HUB_URL, else the hub signed in to last,
    /// else the public hub).
    #[arg(long, value_name = "URL")]
    pub(crate) hub: Option<String>,
}

/// Checks a route given on the command line: a path of the API, never an
/// address of its own, so that the token goes to the hub and nowhere else.
fn check_path(path: &str) -> Result<()> {
    let route = path.split(['?', '#']).next().unwrap_or("");
    let fine = (route == "/v1" || route.starts_with("/v1/"))
        && !path.contains("://")
        && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && !route.split('/').any(|part| part == ".." || part == ".");
    if !fine {
        bail!(
            "{path:?} is not a route of the API: give a path that starts with /v1, such as /v1/me"
        );
    }
    Ok(())
}

/// The body `args` give, read whole.
fn data(args: &ApiArgs) -> Result<Option<Vec<u8>>> {
    if let Some(text) = &args.data {
        return Ok(Some(text.clone().into_bytes()));
    }
    let Some(path) = &args.data_file else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    if path.as_os_str() == "-" {
        std::io::stdin()
            .lock()
            .take(MAX_DATA + 1)
            .read_to_end(&mut bytes)
            .context("cannot read standard input")?;
    } else {
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_DATA + 1).read_to_end(&mut bytes))
            .with_context(|| format!("cannot read {}", path.display()))?;
    }
    if bytes.len() as u64 > MAX_DATA {
        bail!("the body is larger than {MAX_DATA} bytes, the most the hub takes");
    }
    Ok(Some(bytes))
}

/// Writes an answer's body to standard output, as it came.
fn emit(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

pub(crate) fn api(cli: &Cli, args: &ApiArgs) -> Result<ExitCode> {
    let _ = cli;
    let method = args.method.to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        bail!(
            "{:?} is not a method the API takes: {}",
            args.method,
            METHODS.join(", ")
        );
    }
    check_path(&args.path)?;
    let body = data(args)?;
    let env = HubEnv::from_process();
    let creds = env.credentials()?;
    let url = creds::hub_url(&env, &creds, args.hub.as_deref())?;
    let (hub, _) = super::client(&env, &url)?;
    let headers: &[(&str, &str)] = if body.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    match hub.call(&method, &args.path, headers, body.as_deref()) {
        Ok(answer) => {
            emit(&answer)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(HubError::Api { status, body }) => {
            emit(serde_json::to_string(&body)?.as_bytes())?;
            Ok(ExitCode::from(if status < 500 {
                REFUSED
            } else {
                UNREACHABLE
            }))
        }
        Err(err) => {
            eprintln!("braid: error: {err}");
            Ok(ExitCode::from(exit_for(&err)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_of_the_api_only() {
        for fine in [
            "/v1",
            "/v1/me",
            "/v1/r/ada/demo/refs",
            "/v1/explore?q=braid",
        ] {
            assert!(check_path(fine).is_ok(), "{fine}");
        }
        for bad in [
            "v1/me",
            "/v2/me",
            "/v1x",
            "https://elsewhere.example/v1/me",
            "/v1/../etc",
            "/v1/me?next=https://elsewhere.example",
            "/v1/a b",
            "/v1/a\nb",
            "//elsewhere.example/v1",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
