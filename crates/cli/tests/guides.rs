// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The guides for coding agents: part 1 of AGENTS.md, which the site also
//! serves as agents.md, and the section `braid agents init` writes. They say
//! the same things in several places, they install the braid they describe,
//! and every braid command they give must exist, with every flag it is
//! given.

use std::path::Path;
use std::process::Command;

const START: &str = "<!-- braid:start -->";
const END: &str = "<!-- braid:end -->";

/// The prompt that sends an agent to the guide. It asks for the page whole,
/// since some harnesses fetch a summary of a page rather than its text.
const PROMPT: &str = "Install braid in this project and set it up for agents, following https://braid.elffuss.com/agents.md (read it whole, with curl -fsSL)";

/// The same prompt, as the Spanish page and the README's Spanish part give
/// it.
const PROMPT_ES: &str = "Instala braid en este proyecto y configúralo para agentes siguiendo https://braid.elffuss.com/agents.md (léelo entero, con curl -fsSL)";

fn repository_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|err| panic!("{path}: {err}"))
}

/// Runs braid, expecting success, and returns what it printed.
fn braid(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "braid {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The text of a Markdown file from `from` to `until` or the end, with every
/// heading one level higher when `promote` is set.
fn part(text: &str, from: &str, until: Option<&str>, promote: bool) -> String {
    let start = text
        .find(from)
        .unwrap_or_else(|| panic!("no {from:?} in the file"));
    let rest = &text[start..];
    let end = until
        .and_then(|until| rest.find(until))
        .unwrap_or(rest.len());
    rest[..end]
        .lines()
        .map(|line| match line.strip_prefix('#') {
            Some(heading) if promote && heading.starts_with('#') => heading.to_owned(),
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Part 1 of AGENTS.md.
fn part_one() -> String {
    part(
        &repository_file("AGENTS.md"),
        "## 1.",
        Some("\n## 2."),
        false,
    )
}

/// The section `braid agents init` writes, as its dry run prints it.
fn section() -> String {
    let temp = tempfile::tempdir().unwrap();
    let said = braid(
        temp.path(),
        &["agents", "init", "--dry-run", "--branch", "main"],
    );
    let start = said.find(START).expect("the dry run prints the section");
    said[start..].to_owned()
}

/// The lines of the ```bash blocks of a Markdown text, without their
/// indentation.
fn bash_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            inside = !inside && line == "```bash";
            continue;
        }
        if inside {
            lines.push(line.to_owned());
        }
    }
    lines
}

#[test]
fn the_guide_for_agents_is_the_same_in_agents_md_and_on_the_site() {
    let agents = repository_file("AGENTS.md");
    let site = repository_file("site/agents.md");
    let first = "### Install";
    let ours = part(&agents, first, Some("\n## 2."), true);
    let theirs = part(&site, &first[1..], None, false);
    assert_eq!(ours.trim_end(), theirs.trim_end());
    for text in [&agents, &site] {
        assert!(
            !text
                .lines()
                .any(|line| line.trim() == START || line.trim() == END)
        );
    }
    assert_eq!(repository_file("CLAUDE.md"), "@AGENTS.md\n");
}

#[test]
fn the_prompt_for_agents_is_the_same_everywhere() {
    for path in ["README.md", "site/index.html", "site/llms.txt"] {
        assert!(repository_file(path).contains(PROMPT), "{path}");
    }
    for path in ["README.md", "site/index.html"] {
        assert!(repository_file(path).contains(PROMPT_ES), "{path}");
    }
    assert!(PROMPT.contains(" https://braid.elffuss.com/agents.md "));
    assert!(repository_file("site/llms.txt").contains("(https://braid.elffuss.com/agents.md)"));
}

#[test]
fn the_guide_installs_the_braid_it_describes() {
    // The guide is served apart from the code: it installs a tag, so that
    // the braid an agent builds is the one the guide was written for, and
    // the version check names that version.
    let version = env!("CARGO_PKG_VERSION");
    let guide = part_one();
    let install = format!(
        "cargo install --locked --git https://github.com/KikoCis/elffuss-braid --tag v{version} elffuss-braid-cli\n"
    );
    assert!(guide.contains(&install), "{install}");
    assert!(guide.contains(&format!("\"braid {version}\", or newer")));
    let printed = braid(Path::new("."), &["--version"]);
    assert_eq!(printed, format!("braid {version}\n"));
    // The guide's version has `agents init`: an older one is what the check
    // sends back to the install.
    braid(Path::new("."), &["agents", "init", "--help"]);
}

#[test]
fn every_braid_command_in_the_guides_exists_with_its_flags() {
    let mut lines: Vec<String> = bash_lines(&part_one());
    lines.extend(bash_lines(&section()));
    // A line may run braid after another command: `test -d .braid || braid
    // init`.
    let commands: Vec<String> = lines
        .iter()
        .flat_map(|line| line.split("||").flat_map(|part| part.split("&&")))
        .map(str::trim)
        .filter(|command| command.starts_with("braid "))
        .map(str::to_owned)
        .collect();
    assert!(commands.len() >= 15, "{commands:?}");
    let groups = ["agents", "change", "git", "op", "ref", "workspace"];
    for command in &commands {
        let words: Vec<&str> = command
            .split(" #")
            .next()
            .unwrap()
            .split_whitespace()
            .skip(1)
            .collect();
        let mut rest = words.as_slice();
        while let [flag, _, tail @ ..] = rest {
            if matches!(*flag, "--agent" | "--workspace" | "--repo") {
                rest = tail;
            } else {
                break;
            }
        }
        if rest[0] == "--version" {
            let version = braid(Path::new("."), &["--version"]);
            assert!(version.starts_with("braid "), "{version}");
            continue;
        }
        let mut path = vec![rest[0]];
        if groups.contains(&rest[0]) {
            path.push(rest[1]);
        }
        let mut help_args = path.clone();
        help_args.push("--help");
        let help = braid(Path::new("."), &help_args);
        for word in &words {
            let flag = word.split('=').next().unwrap();
            if flag.starts_with("--") {
                assert!(
                    help.contains(flag),
                    "{command}: no {flag} in braid {path:?}"
                );
            } else if flag.len() == 2 && flag.starts_with('-') {
                assert!(
                    help.contains(&format!("{flag},")),
                    "{command}: no {flag} in braid {path:?}"
                );
            }
        }
    }
}
