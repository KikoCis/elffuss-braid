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

/// The text of `line` without its command substitutions, such as
/// `"$(git branch --show-current)"`, which leaves `""`.
fn without_substitutions(line: &str) -> String {
    let mut kept = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("$(") {
        kept.push_str(&rest[..start]);
        // From the opening parenthesis to the one that closes it.
        let mut depth = 0;
        let mut end = rest.len();
        for (offset, c) in rest[start + 1..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + 1 + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end..];
    }
    kept.push_str(rest);
    kept
}

/// The braid commands a shell line runs, without its comment and its
/// command substitutions. A line may run braid after another command,
/// `test -d .braid || braid init`, pipe it into one,
/// `braid cat-object <base>:<path> | diff`, or run it inside an `if`,
/// `if ...; then braid git import . && braid agents init; else ...; fi`.
fn braid_commands(line: &str) -> Vec<String> {
    let line = without_substitutions(line.split(" #").next().unwrap());
    line.split(['|', '&', ';'])
        .map(|command| {
            let mut command = command.trim();
            for keyword in ["if ", "then ", "else ", "! "] {
                command = command.strip_prefix(keyword).unwrap_or(command).trim();
            }
            command
        })
        .filter(|command| command.starts_with("braid "))
        .map(str::to_owned)
        .collect()
}

/// Runs `line` in a new POSIX shell in `dir`, as a harness that keeps
/// nothing between commands runs it: with this build's braid first on
/// `PATH`, no braid variables, and git reading only `config` (no system
/// configuration) and no identity from the environment.
fn shell(dir: &Path, config: &Path, line: &str) -> Output {
    let bin = Path::new(env!("CARGO_BIN_EXE_braid")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .env("PATH", path)
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // The shell sets it from the directory it starts in.
        .env_remove("PWD");
    for variable in [
        "BRAID_AUTHOR",
        "BRAID_AGENT",
        "EMAIL",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        command.env_remove(variable);
    }
    command.output().unwrap()
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

/// A git project in `dir`, on `branch`, with one commit of `notes.txt`, and
/// a git configuration of its own that gives `identity` as git's user, or
/// none.
fn git_project(dir: &Path, branch: &str, identity: Option<(&str, &str)>) -> PathBuf {
    git_project_from(dir, &format!("git init -q -b {branch}"), identity)
}

/// A git project as [`git_project`] makes it, created by `init`.
fn git_project_from(dir: &Path, init: &str, identity: Option<(&str, &str)>) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let config = dir.with_extension("gitconfig");
    let user = identity.map_or_else(String::new, |(name, email)| {
        format!("[user]\n\tname = {name}\n\temail = {email}\n")
    });
    fs::write(&config, user).unwrap();
    fs::write(dir.join("notes.txt"), "one\ntwo\n").unwrap();
    for line in [
        init,
        "git add notes.txt",
        "git -c user.name=Ada -c user.email=ada@example.org commit -q -m First",
    ] {
        shell_ok(dir, &config, line);
    }
    config
}

/// The line of the guide that says whether braid is set up in a project:
/// the guide gives it to check before anything else, and ends the setup
/// with it.
fn set_up_check(guide: &str) -> String {
    let check = block_with(guide, "braid --version")
        .into_iter()
        .find(|line| line.starts_with("braid cat-object "))
        .expect("the guide checks whether braid is set up");
    let commit = block_with(guide, "git add AGENTS.md");
    assert_eq!(commit.last(), Some(&check), "the setup ends with the check");
    check
}

/// What the check prints once braid is set up.
const SET_UP: &str = "braid is set up here\n";

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
    for path in [
        "README.md",
        "site/index.html",
        "site/scion.html",
        "site/llms.txt",
    ] {
        assert!(repository_file(path).contains(PROMPT), "{path}");
    }
    for path in ["README.md", "site/index.html", "site/scion.html"] {
        assert!(repository_file(path).contains(PROMPT_ES), "{path}");
    }
    assert!(PROMPT.contains(" https://braid.elffuss.com/agents.md "));
    assert!(repository_file("site/llms.txt").contains("(https://braid.elffuss.com/agents.md)"));
}

#[test]
fn the_site_names_the_version_of_the_code() {
    // The home page's structured data says which braid it describes.
    let version = env!("CARGO_PKG_VERSION");
    let page = repository_file("site/index.html");
    for field in ["version", "softwareVersion"] {
        let pair = format!("\"{field}\": \"{version}\"");
        assert!(page.contains(&pair), "site/index.html: {pair}");
    }
}

#[test]
fn the_sitemap_lists_every_page_of_the_site() {
    let sitemap = repository_file("site/sitemap.xml");
    let site = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site");
    for entry in fs::read_dir(&site).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if !name.ends_with(".html") {
            continue;
        }
        let url = match name.as_str() {
            "index.html" => "https://braid.elffuss.com/".to_owned(),
            other => format!("https://braid.elffuss.com/{other}"),
        };
        assert!(sitemap.contains(&format!("<loc>{url}</loc>")), "{url}");
    }
    assert!(
        repository_file("site/robots.txt")
            .contains("Sitemap: https://braid.elffuss.com/sitemap.xml")
    );
}

#[test]
fn every_text_that_names_scion_says_braid_is_independent() {
    for path in [
        "README.md",
        "site/index.html",
        "site/scion.html",
        "site/llms.txt",
    ] {
        let text = repository_file(path);
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.contains("Scion") {
            assert!(
                flat.contains("not affiliated with or endorsed by Anthropic"),
                "{path}"
            );
        }
    }
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
    let commands: Vec<String> = lines.iter().flat_map(|line| braid_commands(line)).collect();
    assert!(commands.len() >= 40, "{commands:?}");
    for (line, expected) in [
        (
            "test -d .braid || braid init     # .braid/: the store",
            vec!["braid init"],
        ),
        (
            "if grep -q x f; then braid git import . && braid agents init; else echo no >&2; false; fi",
            vec!["braid git import .", "braid agents init"],
        ),
        (
            "braid cat-object \"$(git branch --show-current):AGENTS.md\" | grep -q x",
            vec!["braid cat-object \":AGENTS.md\""],
        ),
    ] {
        assert_eq!(braid_commands(line), expected);
    }
    let groups = ["agents", "change", "git", "op", "ref", "workspace"];
    for command in &commands {
        let words: Vec<&str> = command.split_whitespace().skip(1).collect();
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
    let guide = part_one();
    let setup = block_with(&guide, "test -d .braid || braid init");
    let commit = block_with(&guide, "git add AGENTS.md");
    let check = set_up_check(&guide);
    // The import and AGENTS.md wait on the name and the email, in the line
    // that checks them, so that the setup stops there however it is run.
    let gate = setup.len() - 1;
    assert!(
        setup[gate].starts_with("if grep -q '^name = .' .braid/config"),
        "{setup:?}"
    );
    for identity in [Some(("Ada Lovelace", "ada@example.org")), None] {
        let temp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(temp.path()).unwrap().join("project");
        let config = git_project(&dir, "main", identity);
        assert!(!shell(&dir, &config, &check).status.success());
        // Each line in a shell of its own, as written: only the check fails
        // where git has no identity.
        let run = || -> Vec<Option<i32>> {
            setup
                .iter()
                .map(|line| shell(&dir, &config, line).status.code())
                .collect()
        };
        let failing = |index: usize| Some(i32::from(identity.is_none() && index == gate));
        assert_eq!(run(), (0..setup.len()).map(failing).collect::<Vec<_>>());
        let written = read(&dir, ".braid/config");
        let Some((name, email)) = identity else {
            assert_eq!(written, "name = \nemail = \n");
            // The setup says why it stopped, and has imported nothing and
            // written no AGENTS.md, whether its lines run one at a time or
            // as one script, as a harness may run a block.
            let stopped = shell(&dir, &config, &setup[gate]);
            assert_eq!(stopped.status.code(), Some(1));
            let said = String::from_utf8_lossy(&stopped.stderr);
            assert!(said.contains("ask the user who signs"), "{said}");
            let script = shell(&dir, &config, &setup.join("\n"));
            assert_eq!(script.status.code(), Some(1));
            assert_eq!(shell_ok(&dir, &config, "braid refs"), "");
            assert!(!dir.join("AGENTS.md").exists());
            // The commit block, run all the same, commits nothing: git
            // refuses to make up who signs, and the check still fails.
            fs::write(dir.join("AGENTS.md"), "# Rules\n").unwrap();
            let script = shell(&dir, &config, &commit.join("\n"));
            assert_eq!(script.status.code(), Some(1));
            assert_eq!(shell_ok(&dir, &config, "git log --format=%s"), "First\n");
            shell_ok(&dir, &config, "git reset -q && rm AGENTS.md");
            // Fixed as the guide says, the setup runs again from its first
            // line without a failure, and git's commit is signed as braid's
            // changesets are.
            for line in block_with(&guide, "git config user.name") {
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
            for line in &commit {
                shell_ok(&dir, &config, line);
            }
            assert_eq!(
                shell_ok(&dir, &config, "git log -1 --format='%an <%ae>'"),
                "Grace Hopper <grace@example.org>\n"
            );
            assert_eq!(shell_ok(&dir, &config, &check), SET_UP);
            continue;
        };
        assert_eq!(written, format!("name = {name}\nemail = {email}\n"));
        assert!(read(&dir, "AGENTS.md").contains(START));
        // Written but not committed, the section is not on braid's branch
        // yet: braid is not set up.
        assert!(!shell(&dir, &config, &check).status.success());
        let said: Vec<String> = commit
            .iter()
            .map(|line| shell_ok(&dir, &config, line))
            .collect();
        assert_eq!(said.last().map(String::as_str), Some(SET_UP));
        // Running the setup again changes nothing.
        assert!(run().iter().all(|code| *code == Some(0)));
        assert_eq!(read(&dir, ".braid/config"), written);
        // Without anyone to sign, braid is not set up either, and the setup
        // run again puts that right.
        fs::remove_file(dir.join(".braid/config")).unwrap();
        assert!(!shell(&dir, &config, &check).status.success());
        assert!(run().iter().all(|code| *code == Some(0)));
        assert_eq!(shell_ok(&dir, &config, &check), SET_UP);
    }
}

#[test]
fn a_session_names_who_signs_in_the_config_of_the_project_root() {
    let temp = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(temp.path()).unwrap().join("project");
    let config = git_project(&dir, "main", Some(("Ada Lovelace", "ada@example.org")));
    for line in block_with(&part_one(), "test -d .braid || braid init") {
        shell_ok(&dir, &config, &line);
    }
    // Nobody signs: the session's status says so first.
    fs::write(dir.join(".braid/config"), "name = \nemail = \n").unwrap();
    shell_ok(&dir, &config, "braid workspace add agent-1 --track main");
    let workspace = dir.join(".braid/work/agent-1");
    assert_eq!(status(&workspace)["blocking"][0]["reason"], "author");
    // In the workspace, `.braid` is a file: the config the section means
    // is the project root's, where the lines it gives, added after the
    // empty ones, name who signs.
    assert!(workspace.join(".braid").is_file());
    let section = section().split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        section.contains(
            "write the answer into `.braid/config` in the project root, the one file there you may edit, as `name = <name>` and `email = <email>` lines"
        ),
        "{section}"
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(dir.join(".braid/config"))
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        b"name = Grace Hopper\nemail = grace@example.org\n",
    )
    .unwrap();
    assert_eq!(status(&workspace)["blocking"], serde_json::json!([]));
}

#[test]
fn the_setup_runs_as_written_where_the_branch_is_master() {
    let temp = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let dir = parent.join("project");
    let config = git_project(&dir, "master", Some(("Ada Lovelace", "ada@example.org")));
    let guide = part_one();
    let check = set_up_check(&guide);
    // The setup, its commit, and a remote made where there is none, each
    // line as written.
    for first in [
        "test -d .braid || braid init",
        "git add AGENTS.md",
        "git init --bare -b",
    ] {
        for line in block_with(&guide, first) {
            shell_ok(&dir, &config, &line);
        }
    }
    assert_eq!(shell_ok(&dir, &config, &check), SET_UP);
    let rules = read(&dir, "AGENTS.md");
    assert!(rules.contains("--track master\n"), "{rules}");
    // The new remote's HEAD names the branch, so that a clone checks it
    // out, with the setup's commit.
    let bare = parent.join("project.git");
    assert_eq!(
        shell_ok(&bare, &config, "git symbolic-ref HEAD"),
        "refs/heads/master\n"
    );
    shell_ok(&parent, &config, "git clone -q project.git clone");
    assert_eq!(read(&parent.join("clone"), "AGENTS.md"), rules);
}

#[test]
fn the_setup_keeps_braid_out_of_git_without_an_info_directory() {
    // A template that holds only hooks, as hook managers set one up, gives
    // repositories with no `.git/info`.
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let template = root.join("template");
    fs::create_dir_all(template.join("hooks")).unwrap();
    let dir = root.join("project");
    let init = format!("git init -q -b main --template='{}'", template.display());
    let config = git_project_from(&dir, &init, Some(("Ada Lovelace", "ada@example.org")));
    assert!(!dir.join(".git/info").exists());
    for line in block_with(&part_one(), "test -d .braid || braid init") {
        shell_ok(&dir, &config, &line);
    }
    assert_eq!(read(&dir, ".git/info/exclude"), ".braid\n");
    assert_eq!(
        shell_ok(
            &dir,
            &config,
            "git status --porcelain --untracked-files=all"
        ),
        "?? AGENTS.md\n"
    );
}

/// The line the guides give to keep Python's build output out of a
/// workspace, `.braidignore` included.
const BRAIDIGNORE: &str = "printf '%s\\n' .braidignore __pycache__/ >> .braidignore";

#[test]
fn a_session_without_a_remote_makes_one_that_sync_and_clones_use() {
    let temp = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let dir = parent.join("project");
    let config = git_project(&dir, "main", Some(("Ada Lovelace", "ada@example.org")));
    for line in block_with(&part_one(), "test -d .braid || braid init") {
        shell_ok(&dir, &config, &line);
    }
    let section = section();
    let sync = block_with(&section, "braid --agent agent-1 git sync");
    let recipe = block_with(&section, "git init --bare");
    // The guide gives the same lines for a project with no remote, with the
    // branch the shared checkout is on, which the setup has be the main
    // one, where the section names its branch.
    let guide: Vec<String> = block_with(&part_one(), "git init --bare -b")
        .iter()
        .map(|line| line.replace("\"$(git branch --show-current)\"", "main"))
        .collect();
    assert_eq!(guide, recipe);
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

/// The heading of the guide that says how to keep the git remote in step,
/// and what to do when a sync fails.
const SYNC_HEADING: &str = "Keeping the git remote up to date";

#[test]
fn the_section_sends_a_failed_sync_to_the_guide() {
    assert!(part_one().contains(&format!("\n### {SYNC_HEADING}\n")));
    let section = section().split_whitespace().collect::<Vec<_>>().join(" ");
    for said in [
        "If the sync exits with 2 (`conflicts`)",
        &format!("as <https://braid.elffuss.com/agents.md> shows under \"{SYNC_HEADING}\""),
        "With `remote_moved` (exit status 1), sync again; with `rejected`, ask the user.",
        "If `git pull --ff-only` refuses,",
    ] {
        assert!(section.contains(said), "{said}: {section}");
    }
}

#[test]
fn a_sync_conflict_is_merged_in_git_as_the_guide_says() {
    let temp = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let dir = parent.join("project");
    let config = git_project(&dir, "main", Some(("Ada Lovelace", "ada@example.org")));
    let guide = part_one();
    // A remote, the setup and its commit, each line as written, and a sync
    // that takes the commit to the remote.
    for first in [
        "git init --bare -b",
        "test -d .braid || braid init",
        "git add AGENTS.md",
        "braid --agent agent-1 git sync",
    ] {
        for line in block_with(&guide, first) {
            shell_ok(&dir, &config, &line);
        }
    }
    let sync = block_with(&guide, "braid --agent agent-1 git sync")
        .into_iter()
        .find(|line| !line.contains("--dry-run") && line.starts_with("braid "))
        .unwrap();

    // A session lands an edit of a line that someone else edits and pushes.
    shell_ok(&dir, &config, "braid workspace add agent-1 --track main");
    let workspace = dir.join(".braid/work/agent-1");
    fs::write(workspace.join("notes.txt"), "one\ntwo, the agent's\n").unwrap();
    for line in [
        "braid --agent agent-1 commit -m 'The agent edits notes'",
        "braid --agent agent-1 land",
    ] {
        shell_ok(&workspace, &config, line);
    }
    shell_ok(&parent, &config, "git clone -q project.git person");
    let person = parent.join("person");
    fs::write(person.join("notes.txt"), "one\ntwo, the person's\n").unwrap();
    shell_ok(
        &person,
        &config,
        "git commit -q -am 'The person edits notes'",
    );
    shell_ok(&person, &config, "git push -q");

    // The sync stops, and says where the steps are.
    let stopped = shell(&dir, &config, &sync);
    assert_eq!(stopped.status.code(), Some(2));
    let said = String::from_utf8_lossy(&stopped.stdout);
    assert!(said.contains("CONTENT       notes.txt"), "{said}");
    assert!(
        said.contains(&format!(
            "https://braid.elffuss.com/agents.md shows how, under \"{SYNC_HEADING}\""
        )),
        "{said}"
    );

    // The guide's steps, with the conflict fixed where git stops.
    let recipe = block_with(&guide, "git init --bare .braid/export.git");
    let merge = recipe
        .iter()
        .position(|line| line.starts_with("git merge --no-edit origin/main"))
        .expect("the remote's branch is merged into braid's");
    assert_eq!(
        recipe[merge + 1],
        "# if it stopped: fix those files and git add them, then git commit --no-edit --cleanup=strip"
    );
    for (index, line) in recipe.iter().enumerate() {
        let out = shell(&dir, &config, line);
        if index == merge {
            assert_eq!(out.status.code(), Some(1), "git stops at the conflict");
            fs::write(dir.join("notes.txt"), "one\ntwo, both\n").unwrap();
            shell_ok(&dir, &config, "git add notes.txt");
            shell_ok(&dir, &config, "git commit -q --no-edit --cleanup=strip");
            continue;
        }
        assert!(
            out.status.success(),
            "{line}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The merge's message names the branch, not the remote's address, and
    // braid's branch is its first parent; braid's main and the remote's are
    // the merge.
    let merged = shell_ok(&dir, &config, "git log -1 --format=%B");
    assert_eq!(
        merged.trim_end(),
        "Merge remote-tracking branch 'origin/main'"
    );
    assert_eq!(
        shell_ok(&dir, &config, "git log -1 --format=%s HEAD^1"),
        "The agent edits notes\n"
    );
    let head = shell_ok(&dir, &config, "git rev-parse HEAD");
    assert_eq!(
        shell_ok(&parent.join("project.git"), &config, "git rev-parse main"),
        head
    );
    assert!(
        shell_ok(&dir, &config, "braid log main -n 1")
            .contains("Merge remote-tracking branch 'origin/main'")
    );
    // The session carries on from the merge, and the next sync has nothing
    // to do.
    shell_ok(&workspace, &config, "braid --agent agent-1 update");
    assert_eq!(status(&workspace)["behind_by"], 0);
    assert_eq!(read(&workspace, "notes.txt"), "one\ntwo, both\n");
    assert!(shell_ok(&dir, &config, &sync).contains("up to date"));
    assert!(!dir.join(".braid/export.git").exists());
}
