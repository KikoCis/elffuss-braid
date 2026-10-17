// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The guides for coding agents: part 1 of AGENTS.md, which the site also
//! serves as agents.md, and the section `braid agents init` writes. They say
//! the same things in several places, they install the braid they describe,
//! and every braid command they give must exist, with every flag it is
//! given.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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
    bash_blocks(text).concat()
}

/// The ```bash blocks of a Markdown text, each as its lines without their
/// indentation.
fn bash_blocks(text: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            inside = !inside && line == "```bash";
            if inside {
                blocks.push(Vec::new());
            }
            continue;
        }
        if inside && let Some(block) = blocks.last_mut() {
            block.push(line.to_owned());
        }
    }
    blocks
}

/// The ```bash block of `text` with a line that starts with `first`.
fn block_with(text: &str, first: &str) -> Vec<String> {
    bash_blocks(text)
        .into_iter()
        .find(|block| block.iter().any(|line| line.starts_with(first)))
        .unwrap_or_else(|| panic!("no block with {first:?}"))
}

/// Runs `line` in a new POSIX shell in `dir`, as a harness that keeps
/// nothing between commands runs it: with this build's braid first on
/// `PATH`, no braid variables, and git reading only `config` (no system
/// configuration).
fn shell(dir: &Path, config: &Path, line: &str) -> Output {
    let bin = Path::new(env!("CARGO_BIN_EXE_braid")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .env("PATH", path)
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("BRAID_AUTHOR")
        .env_remove("BRAID_AGENT")
        // The shell sets it from the directory it starts in.
        .env_remove("PWD")
        .output()
        .unwrap()
}

/// Runs `line` as [`shell`] does, expecting success.
fn shell_ok(dir: &Path, config: &Path, line: &str) -> String {
    let out = shell(dir, config, line);
    assert!(
        out.status.success(),
        "{line}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A git project in `dir`, on `main`, with one commit of `notes.txt`, and
/// a git configuration of its own that gives `identity` as git's user, or
/// none.
fn git_project(dir: &Path, identity: Option<(&str, &str)>) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let config = dir.with_extension("gitconfig");
    let user = identity.map_or_else(String::new, |(name, email)| {
        format!("[user]\n\tname = {name}\n\temail = {email}\n")
    });
    fs::write(&config, user).unwrap();
    fs::write(dir.join("notes.txt"), "one\ntwo\n").unwrap();
    for line in [
        "git init -q -b main",
        "git add notes.txt",
        "git -c user.name=Ada -c user.email=ada@example.org commit -q -m First",
    ] {
        shell_ok(dir, &config, line);
    }
    config
}

/// The text of the file at `path` in `dir`.
fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap_or_else(|err| panic!("{path}: {err}"))
}

/// What `braid status --json` says in the workspace at `dir`, with no
/// author from the environment.
fn status(dir: &Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(["status", "--json"])
        .env_remove("BRAID_AUTHOR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
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

/// The code spans of a Markdown text outside its fenced blocks, such as
/// `braid status`, wherever their lines break.
fn code_spans(text: &str) -> Vec<String> {
    let mut prose = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim().starts_with("```") {
            fenced = !fenced;
        } else if !fenced {
            prose.push(line.trim());
        }
    }
    let prose = prose.join(" ");
    assert!(
        !prose.contains("``"),
        "a code span of more than one backtick"
    );
    prose
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_braid_command_in_the_guides_exists_with_its_flags() {
    let mut lines: Vec<String> = Vec::new();
    for text in [part_one(), section()] {
        lines.extend(bash_lines(&text));
        lines.extend(code_spans(&text));
    }
    // A line may run braid after another command, `test -d .braid || braid
    // init`, or pipe it into one, `braid cat-object <base>:<path> | diff`.
    let commands: Vec<String> = lines
        .iter()
        .flat_map(|line| line.split('|').flat_map(|part| part.split("&&")))
        .map(str::trim)
        .filter(|command| command.starts_with("braid "))
        .map(str::to_owned)
        .collect();
    assert!(commands.len() >= 40, "{commands:?}");
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

#[test]
fn the_setup_stops_at_its_check_until_braid_has_a_name_and_an_email() {
    let setup = block_with(&part_one(), "test -d .braid || braid init");
    let check = setup
        .iter()
        .position(|line| line.starts_with("grep -q '^name = .' .braid/config"))
        .expect("the setup checks the name and the email it wrote");
    for identity in [Some(("Ada Lovelace", "ada@example.org")), None] {
        let temp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(temp.path()).unwrap().join("project");
        let config = git_project(&dir, identity);
        // Each line in a shell of its own, as written: only the check fails
        // where git has no identity, and nothing says why but the guide.
        let run = || -> Vec<Option<i32>> {
            setup
                .iter()
                .map(|line| shell(&dir, &config, line).status.code())
                .collect()
        };
        let failing = |index: usize| Some(i32::from(identity.is_none() && index == check));
        assert_eq!(run(), (0..setup.len()).map(failing).collect::<Vec<_>>());
        assert!(read(&dir, "AGENTS.md").contains(START));
        let commit = block_with(&part_one(), "git add AGENTS.md");
        let written = read(&dir, ".braid/config");
        let Some((name, email)) = identity else {
            assert_eq!(written, "name = \nemail = \n");
            // Every commit and landing would fail: a session's status says
            // so first.
            shell_ok(&dir, &config, "braid workspace add agent-1 --track main");
            let workspace = dir.join(".braid/work/agent-1");
            assert_eq!(status(&workspace)["blocking"][0]["reason"], "author");
            // Fixed as the guide says, the setup runs again from the top
            // without a failure, and git's commit is signed as braid's
            // changesets are, not as git would guess.
            for line in block_with(&part_one(), "git config user.name") {
                let line = line
                    .replace("<name>", "Grace Hopper")
                    .replace("<email>", "grace@example.org");
                shell_ok(&dir, &config, &line);
            }
            assert!(run().iter().all(|code| *code == Some(0)));
            assert_eq!(
                read(&dir, ".braid/config"),
                "name = Grace Hopper\nemail = grace@example.org\n"
            );
            assert_eq!(status(&workspace)["blocking"], serde_json::json!([]));
            for line in &commit {
                shell_ok(&dir, &config, line);
            }
            assert_eq!(
                shell_ok(&dir, &config, "git log -1 --format='%an <%ae>'"),
                "Grace Hopper <grace@example.org>\n"
            );
            continue;
        };
        assert_eq!(written, format!("name = {name}\nemail = {email}\n"));
        for line in &commit {
            shell_ok(&dir, &config, line);
        }
        // Running it again changes nothing.
        assert!(run().iter().all(|code| *code == Some(0)));
        assert_eq!(read(&dir, ".braid/config"), written);
    }
}

/// The line the guides give to keep Python's build output out of a
/// workspace, `.braidignore` included.
const BRAIDIGNORE: &str = "printf '%s\\n' .braidignore __pycache__/ >> .braidignore";

#[test]
fn a_session_without_a_remote_makes_one_that_sync_and_clones_use() {
    let temp = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let dir = parent.join("project");
    let config = git_project(&dir, Some(("Ada Lovelace", "ada@example.org")));
    for line in block_with(&part_one(), "test -d .braid || braid init") {
        shell_ok(&dir, &config, &line);
    }
    let section = section();
    let sync = block_with(&section, "braid --agent agent-1 git sync");
    let recipe = block_with(&section, "git init --bare");
    // The guide gives the same lines for a project with no remote.
    let guide = bash_lines(&part_one());
    for line in &recipe {
        assert!(guide.contains(line), "the guide has no {line:?}");
    }
    for text in [part_one(), section.clone()] {
        assert!(code_spans(&text).iter().any(|span| span == BRAIDIGNORE));
    }

    // A session lands a change, with build output kept out of it, and
    // what is left uncommitted holding the landing back.
    shell_ok(&dir, &config, "braid workspace add agent-1 --track main");
    let workspace = dir.join(".braid/work/agent-1");
    fs::write(workspace.join("app.py"), "print('hi')\n").unwrap();
    fs::create_dir(workspace.join("__pycache__")).unwrap();
    fs::write(workspace.join("__pycache__/app.cpython.pyc"), [0u8, 1]).unwrap();
    let before = status(&workspace)["added"].clone();
    assert_eq!(
        before,
        serde_json::json!(["__pycache__/app.cpython.pyc", "app.py"])
    );
    shell_ok(&workspace, &config, BRAIDIGNORE);
    assert_eq!(status(&workspace)["added"], serde_json::json!(["app.py"]));
    fs::write(workspace.join("notes.txt"), "one\ntwo, the agent's\n").unwrap();
    let session = "braid --agent agent-1";
    shell_ok(
        &workspace,
        &config,
        &format!("{session} commit -m 'Add app.py' app.py"),
    );
    let held = shell(&workspace, &config, &format!("{session} land"));
    assert_eq!(held.status.code(), Some(1));
    let said = String::from_utf8_lossy(&held.stderr);
    assert!(
        said.contains("uncommitted changes (1 path, \"notes.txt\")"),
        "{said}"
    );
    shell_ok(
        &workspace,
        &config,
        &format!("{session} commit -m 'Edit notes'"),
    );
    shell_ok(&workspace, &config, &format!("{session} land"));

    // The sync fails without a remote, as the section says; the recipe makes
    // one next to the project, and the sync then reaches it and the shared
    // checkout.
    assert!(!shell(&dir, &config, &sync[0]).status.success());
    for line in &recipe {
        let out = shell(&dir, &config, line);
        assert!(out.status.success(), "{line}: {:?}", out);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!said.contains("hint:"), "{line}: {said}");
    }
    let bare = parent.join("project.git");
    assert_eq!(
        shell_ok(&dir, &config, "git remote get-url origin"),
        format!("{}\n", bare.display())
    );
    assert_eq!(
        shell_ok(&bare, &config, "git symbolic-ref HEAD"),
        "refs/heads/main\n"
    );
    for line in &sync {
        shell_ok(&dir, &config, line);
    }
    assert_eq!(read(&dir, "app.py"), "print('hi')\n");
    assert_eq!(read(&dir, "notes.txt"), "one\ntwo, the agent's\n");
    // A clone of the new remote checks out its main.
    shell_ok(&parent, &config, "git clone -q project.git clone");
    assert_eq!(read(&parent.join("clone"), "app.py"), "print('hi')\n");
    for kept_out in ["__pycache__", ".braidignore", ".braid"] {
        assert!(!parent.join("clone").join(kept_out).exists(), "{kept_out}");
    }
}
