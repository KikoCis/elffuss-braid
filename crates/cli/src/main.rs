// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid`, the command-line tool.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use braid_format::{IdHasher, Kind, ObjectId};
use clap::{Parser, Subcommand};

/// Version control for many agents landing changes at once.
#[derive(Debug, Parser)]
#[command(name = "braid", version, about, long_about = None)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the braid blob id of each file ("-" reads standard input).
    HashObject {
        /// Files to hash.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("braid: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::HashObject { paths } => hash_object(&paths, cli.json),
    }
}

struct Hashed<'a> {
    path: &'a Path,
    id: ObjectId,
    size: u64,
}

fn hash_object(paths: &[PathBuf], json: bool) -> Result<()> {
    let mut hashed = Vec::with_capacity(paths.len());
    for path in paths {
        let (id, size) = if path.as_os_str() == "-" {
            hash_blob(io::stdin().lock()).context("cannot read standard input")?
        } else {
            let file =
                File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
            hash_blob(file).with_context(|| format!("cannot read {}", path.display()))?
        };
        hashed.push(Hashed { path, id, size });
    }

    let mut out = io::stdout().lock();
    if json {
        let items: Vec<serde_json::Value> = hashed
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path.to_string_lossy(),
                    "kind": Kind::Blob.name(),
                    "id": h.id.to_hex(),
                    "size": h.size,
                })
            })
            .collect();
        serde_json::to_writer(&mut out, &items)?;
        writeln!(out)?;
    } else {
        for h in &hashed {
            writeln!(out, "{}  {}", h.id, h.path.display())?;
        }
    }
    Ok(())
}

/// Hashes a stream as a blob without holding it in memory.
fn hash_blob(mut reader: impl Read) -> io::Result<(ObjectId, u64)> {
    let mut hasher = IdHasher::new(Kind::Blob);
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finalize(), size))
}
