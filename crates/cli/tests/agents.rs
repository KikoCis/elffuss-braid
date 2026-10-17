// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of `braid agents init`: the braid section of a project's
//! AGENTS.md, written once and rewritten in place, CLAUDE.md and GEMINI.md,
//! dry runs, JSON, the branch the section names, where it writes, and the
//! files it refuses to write.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const START: &str = "<!-- braid:start -->";
const END: &str = "<!-- braid:end -->";

/// Git without the user's or the system's configuration.
const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Ada Lovelace"),
    ("GIT_AUTHOR_EMAIL", "ada@example.org"),
    ("GIT_COMMITTER_NAME", "Ada Lovelace"),
    ("GIT_COMMITTER_EMAIL", "ada@example.org"),
];

/// Runs braid in `dir`.
fn braid(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .env("BRAID_AUTHOR", "Ada <ada@example.org>")
        .env("BRAID_AGENT", "tester")
        .output()
        .unwrap()
}

/// Runs braid in `dir`, expecting success, and returns what it printed.
fn ok(dir: &Path, args: &[&str]) -> String {
    let out = braid(dir, args);
    assert!(
        out.status.success(),
        "braid {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Runs braid in `dir`, expecting it to fail with exit status 1 and to
/// print nothing on standard output, and returns its error.
fn fails(dir: &Path, args: &[&str]) -> String {
    let out = braid(dir, args);
    assert_eq!(
        out.status.code(),
        Some(1),
        "braid {args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.stdout.is_empty(), "braid {args:?}");
    String::from_utf8(out.stderr).unwrap()
}

/// Runs `braid agents init` with `--json` and these arguments in `dir`.
fn init_json(dir: &Path, args: &[&str]) -> Value {
    let mut all = vec!["agents", "init", "--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(dir, &all)).unwrap()
}

fn names(report: &Value, list: &str) -> Vec<String> {
    report[list]
        .as_array()
        .unwrap_or_else(|| panic!("no {list} in {report}"))
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect()
}

/// A temporary directory, canonical so that paths compare as braid prints
/// them.
fn temp() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    (temp, path)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// The braid section of `text`, markers included.
fn section_of(text: &str) -> &str {
    let start = text.find(START).expect("a start marker");
    let end = text.find(END).expect("an end marker") + END.len() + 1;
    &text[start..end]
}

/// A braid repository in `dir` whose only branch is `branch`.
fn repository_with_branch(dir: &Path, branch: &str) {
    ok(dir, &["init"]);
    fs::write(dir.join("seed.txt"), "seed\n").unwrap();
    ok(
        dir,
        &[
            "change",
            "create",
            "--base",
            "none",
            "--set",
            "seed.txt=@seed.txt",
            "-m",
            "Seed",
            "--land",
            branch,
        ],
    );
}

/// A temporary braid repository whose only branch is `main`.
fn project() -> (tempfile::TempDir, PathBuf) {
    let (temp, dir) = temp();
    repository_with_branch(&dir, "main");
    (temp, dir)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .output()
        .expect("git must be installed to run this test");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn without_a_repository_the_branch_must_be_given() {
    let (_temp, dir) = temp();
    let err = fails(&dir, &["agents", "init"]);
    assert!(err.contains("no braid repository here"), "{err}");
    assert!(
        err.contains("`braid init`, then `braid git import .`"),
        "{err}"
    );
    assert!(err.contains("--branch"), "{err}");
    assert!(!dir.join("AGENTS.md").exists());

    let report = init_json(&dir, &["--branch", "main"]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert!(names(&report, "unchanged").is_empty());
    assert_eq!(report["branch"], "main");
    assert_eq!(report["previous_branch"], Value::Null);
    assert_eq!(report["dry_run"], false);
    assert!(report["repository"].is_null());
    assert_eq!(report["dir"], dir.to_str().unwrap());
    assert_eq!(report["paths"]["AGENTS.md"], "AGENTS.md");

    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(&format!("{START}\n")), "{text}");
    assert!(text.ends_with(&format!("{END}\n")), "{text}");
    assert!(text.contains("braid workspace add agent-1 --track main\n"));
    assert!(text.contains("https://braid.elffuss.com/agents.md"));
    assert!(!dir.join("CLAUDE.md").exists());
    assert!(!dir.join("GEMINI.md").exists());

    // The section names the branch now: a second run keeps it.
    let said = ok(&dir, &["agents", "init"]);
    assert!(said.contains("has this braid section already"), "{said}");
    assert!(
        said.contains("`braid init`, then `braid git import .`"),
        "{said}"
    );
}

#[test]
fn only_the_section_changes_and_a_second_run_changes_nothing() {
    let (_temp, dir) = project();
    let above = "# Our project\n\nTabs, not spaces.\n\n";
    let older = format!("{START}\n## Braid\n\nAn older section.\n{END}\n");
    let below = "\n## Releases\n\nTagged by hand.\n";
    fs::write(dir.join("AGENTS.md"), format!("{above}{older}{below}")).unwrap();

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert_eq!(report["repository"], dir.to_str().unwrap());
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(above), "{text}");
    assert!(text.ends_with(below), "{text}");
    let section = section_of(&text);
    assert_eq!(text, format!("{above}{section}{below}"));
    assert!(!section.contains("An older section"));
    assert!(section.contains("--track main"));

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert_eq!(read(&dir.join("AGENTS.md")), text);
    let said = ok(&dir, &["agents", "init"]);
    assert!(said.contains("has this braid section already"), "{said}");
    assert!(!said.contains("braid init"), "{said}");
}

#[test]
fn a_file_without_markers_gets_the_section_at_its_end_once() {
    let (_temp, dir) = project();
    let ours = "# Rules\n\nRun the tests before you push.";
    fs::write(dir.join("AGENTS.md"), ours).unwrap();
    let said = ok(&dir, &["agents", "init"]);
    assert!(
        said.contains("Added the braid section to AGENTS.md"),
        "{said}"
    );
    let text = read(&dir.join("AGENTS.md"));
    assert_eq!(text, format!("{ours}\n\n{}", section_of(&text)));
    assert_eq!(
        names(&init_json(&dir, &[]), "unchanged"),
        ["AGENTS.md"],
        "a second run changes nothing"
    );
    assert_eq!(read(&dir.join("AGENTS.md")), text);
}

#[test]
fn unbalanced_markers_are_refused_and_nothing_is_written() {
    let (_temp, dir) = project();
    let claude = "# Claude\n";
    fs::write(dir.join("CLAUDE.md"), claude).unwrap();
    for (text, lines) in [
        (format!("# Rules\n{START}\nhalf a section\n"), "on line 2"),
        (format!("# Rules\n{END}\n"), "on no line"),
        (format!("{END}\n{START}\n"), "on line 2"),
        (format!("{START}\n{START}\n{END}\n"), "on lines 1 and 2"),
        (
            format!("{START}\n{END}\n{START}\n{END}\n"),
            "on lines 1 and 3",
        ),
    ] {
        fs::write(dir.join("AGENTS.md"), &text).unwrap();
        for args in [
            &["agents", "init"][..],
            &["agents", "init", "--json"],
            &["agents", "init", "--dry-run"],
        ] {
            let err = fails(&dir, args);
            assert!(err.contains("unbalanced braid markers"), "{err}");
            assert!(err.contains(lines), "{text:?}: {err}");
            assert!(err.contains("nothing was written"), "{err}");
            assert_eq!(read(&dir.join("AGENTS.md")), text);
            assert_eq!(read(&dir.join("CLAUDE.md")), claude);
        }
    }
}

#[test]
fn markers_in_a_code_block_are_an_example_and_stay_one() {
    let (_temp, dir) = project();
    let ours = format!(
        "# Rules\n\nWhat braid writes looks like this:\n\n```markdown\n{START}\n(the rules braid writes)\n{END}\n```\n\nMore rules.\n"
    );
    fs::write(dir.join("AGENTS.md"), &ours).unwrap();
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(&ours), "the example stays: {text}");
    assert_eq!(text, format!("{ours}\n{}", section_of(&text[ours.len()..])));
    // The example next to the real section is not a second pair.
    assert_eq!(names(&init_json(&dir, &[]), "unchanged"), ["AGENTS.md"]);
    assert_eq!(read(&dir.join("AGENTS.md")), text);

    // A file that ends inside a code block would have the section read as
    // code: refused.
    let open = "# Rules\n\n```bash\nmake test\n";
    fs::write(dir.join("AGENTS.md"), open).unwrap();
    let err = fails(&dir, &["agents", "init", "--dry-run"]);
    assert!(err.contains("opens on line 3 and is never closed"), "{err}");
    assert!(err.contains("nothing was written"), "{err}");
    fails(&dir, &["agents", "init"]);
    assert_eq!(read(&dir.join("AGENTS.md")), open);
}

#[test]
fn a_byte_order_mark_before_the_section_stays_and_hides_nothing() {
    let (_temp, dir) = project();
    ok(&dir, &["agents", "init"]);
    let written = read(&dir.join("AGENTS.md"));
    let marked = format!("\u{feff}{written}");
    fs::write(dir.join("AGENTS.md"), &marked).unwrap();
    assert_eq!(names(&init_json(&dir, &[]), "unchanged"), ["AGENTS.md"]);
    assert_eq!(read(&dir.join("AGENTS.md")), marked);
    // A section of another branch is rewritten after the mark.
    ok(&dir, &["ref", "set", "trunk", "main"]);
    assert_eq!(
        names(&init_json(&dir, &["--branch", "trunk"]), "updated"),
        ["AGENTS.md"]
    );
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(&format!("\u{feff}{START}\n")), "{text:?}");
    assert!(text.contains("--track trunk\n"));
}

#[test]
fn claude_md_gets_the_include_line_and_only_that() {
    let (_temp, dir) = project();
    let claude = "# Notes for Claude\n\nBe brief.";
    fs::write(dir.join("CLAUDE.md"), claude).unwrap();

    let report = init_json(&dir, &["--no-claude"]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert!(names(&report, "unchanged").is_empty());
    assert_eq!(read(&dir.join("CLAUDE.md")), claude);

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["CLAUDE.md"]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    assert_eq!(report["paths"]["CLAUDE.md"], "CLAUDE.md");
    assert_eq!(
        read(&dir.join("CLAUDE.md")),
        format!("{claude}\n@AGENTS.md\n")
    );

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md", "CLAUDE.md"]);
    for including in ["See @AGENTS.md for the rules.\n", "@./AGENTS.md\n"] {
        fs::write(dir.join("CLAUDE.md"), including).unwrap();
        let said = ok(&dir, &["agents", "init"]);
        assert!(
            said.contains("CLAUDE.md includes AGENTS.md already"),
            "{said}"
        );
        assert_eq!(read(&dir.join("CLAUDE.md")), including);
    }

    // An import shown in a code block imports nothing.
    let shown = "# Claude notes\n\nThe import looks like this:\n\n```text\n@AGENTS.md\n```\n";
    fs::write(dir.join("CLAUDE.md"), shown).unwrap();
    assert_eq!(names(&init_json(&dir, &[]), "updated"), ["CLAUDE.md"]);
    assert_eq!(read(&dir.join("CLAUDE.md")), format!("{shown}@AGENTS.md\n"));

    // A CLAUDE.md that ends inside a code block would hide the line.
    let open = "# Claude\n\n~~~\nan example\n";
    fs::write(dir.join("CLAUDE.md"), open).unwrap();
    let err = fails(&dir, &["agents", "init"]);
    assert!(
        err.contains("CLAUDE.md ends inside a fenced code block"),
        "{err}"
    );
    assert_eq!(read(&dir.join("CLAUDE.md")), open);
}

#[test]
fn gemini_md_gets_the_include_line_too() {
    let (_temp, dir) = project();
    fs::write(dir.join("CLAUDE.md"), "# Claude\n").unwrap();
    fs::write(dir.join("GEMINI.md"), "# Gemini\n").unwrap();
    let report = init_json(&dir, &["--no-gemini"]);
    assert_eq!(names(&report, "updated"), ["CLAUDE.md"]);
    assert_eq!(read(&dir.join("GEMINI.md")), "# Gemini\n");
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["GEMINI.md"]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md", "CLAUDE.md"]);
    assert_eq!(read(&dir.join("GEMINI.md")), "# Gemini\n@AGENTS.md\n");
    let said = ok(&dir, &["agents", "init"]);
    assert!(
        said.contains("GEMINI.md includes AGENTS.md already"),
        "{said}"
    );
}

#[cfg(unix)]
#[test]
fn links_inside_the_project_are_followed() {
    let (_temp, dir) = project();
    std::os::unix::fs::symlink("AGENTS.md", dir.join("CLAUDE.md")).unwrap();
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert_eq!(names(&report, "unchanged"), ["CLAUDE.md"]);
    assert_eq!(report["paths"]["CLAUDE.md"], "AGENTS.md");
    assert!(!read(&dir.join("AGENTS.md")).contains("@AGENTS.md"));
    assert!(
        fs::symlink_metadata(dir.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md", "CLAUDE.md"]);

    // AGENTS.md may be the link, to a file elsewhere in the project: that
    // file gets the section, and the JSON names it.
    let (_other, linked) = project();
    fs::create_dir(linked.join("docs")).unwrap();
    fs::write(linked.join("docs/rules.md"), "# Shared rules\n").unwrap();
    std::os::unix::fs::symlink("docs/rules.md", linked.join("AGENTS.md")).unwrap();
    let report = init_json(&linked, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert_eq!(report["paths"]["AGENTS.md"], "docs/rules.md");
    let link = fs::symlink_metadata(linked.join("AGENTS.md")).unwrap();
    assert!(link.file_type().is_symlink());
    assert!(
        read(&linked.join("docs/rules.md")).starts_with("# Shared rules\n\n<!-- braid:start -->\n")
    );
    let said = ok(&linked, &["agents", "init"]);
    assert!(
        said.contains(
            "AGENTS.md (docs/rules.md, which it links to) has this braid section already"
        ),
        "{said}"
    );
    let left: Vec<_> = fs::read_dir(linked.join("docs")).unwrap().collect();
    assert_eq!(left.len(), 1, "no temporary file is left");

    // The layout of projects that started with Claude Code: AGENTS.md a
    // link to CLAUDE.md, which holds the section itself.
    let (_third, claude) = project();
    fs::write(claude.join("CLAUDE.md"), "# Claude rules\n").unwrap();
    std::os::unix::fs::symlink("CLAUDE.md", claude.join("AGENTS.md")).unwrap();
    let report = init_json(&claude, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert_eq!(names(&report, "unchanged"), ["CLAUDE.md"]);
    let text = read(&claude.join("CLAUDE.md"));
    assert!(
        text.starts_with("# Claude rules\n\n<!-- braid:start -->\n"),
        "{text}"
    );
    assert!(!text.contains("@AGENTS.md"), "{text}");
    assert_eq!(
        names(&init_json(&claude, &[]), "unchanged"),
        ["AGENTS.md", "CLAUDE.md"]
    );
}

#[cfg(unix)]
#[test]
fn files_braid_must_not_write_are_refused_in_a_dry_run_too() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let both = |dir: &Path, said: &str| {
        for args in [&["agents", "init", "--dry-run"][..], &["agents", "init"]] {
            let err = fails(dir, args);
            assert!(err.contains(said), "{args:?}: {err}");
            assert!(err.contains("nothing was written"), "{args:?}: {err}");
        }
    };

    // A link outside the project: other projects may read that file.
    let (_shared, shared) = temp();
    fs::write(shared.join("CLAUDE.md"), "# Shared notes\n").unwrap();
    let (_project, dir) = project();
    symlink(shared.join("CLAUDE.md"), dir.join("CLAUDE.md")).unwrap();
    both(&dir, "CLAUDE.md is a link to");
    assert_eq!(read(&shared.join("CLAUDE.md")), "# Shared notes\n");
    assert!(!dir.join("AGENTS.md").exists());
    ok(&dir, &["agents", "init", "--no-claude"]);
    fs::remove_file(dir.join("AGENTS.md")).unwrap();

    // A link to a file that does not exist fails the dry run as it fails
    // the real run.
    symlink("docs/rules.md", dir.join("AGENTS.md")).unwrap();
    both(&dir, "which does not exist");
    fs::remove_file(dir.join("AGENTS.md")).unwrap();

    // A FIFO is never opened: the dry run does not wait on it.
    let fifo = dir.join("AGENTS.md");
    let made = Command::new("mkfifo").arg(&fifo).status();
    if made.is_ok_and(|status| status.success()) {
        both(&dir, "is not a regular file");
        fs::remove_file(&fifo).unwrap();
    }

    // A read-only file is left as it is.
    fs::write(dir.join("AGENTS.md"), "# Rules\n").unwrap();
    fs::set_permissions(dir.join("AGENTS.md"), fs::Permissions::from_mode(0o444)).unwrap();
    both(&dir, "AGENTS.md is read-only");
    assert_eq!(read(&dir.join("AGENTS.md")), "# Rules\n");
    fs::set_permissions(dir.join("AGENTS.md"), fs::Permissions::from_mode(0o644)).unwrap();

    // A file with a hard link elsewhere would change under that name too.
    fs::hard_link(dir.join("AGENTS.md"), shared.join("AGENTS.md")).unwrap();
    both(&dir, "has hard links");
    assert_eq!(read(&shared.join("AGENTS.md")), "# Rules\n");
}

#[cfg(unix)]
#[test]
fn hard_linked_guides_stay_one_file() {
    use std::os::unix::fs::MetadataExt;

    let (_temp, dir) = project();
    fs::write(
        dir.join("AGENTS.md"),
        "# Shared rules for every agent\n\nRun the tests.\n",
    )
    .unwrap();
    fs::hard_link(dir.join("AGENTS.md"), dir.join("CLAUDE.md")).unwrap();
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert_eq!(names(&report, "unchanged"), ["CLAUDE.md"]);
    let (agents, claude) = (
        fs::metadata(dir.join("AGENTS.md")).unwrap(),
        fs::metadata(dir.join("CLAUDE.md")).unwrap(),
    );
    assert_eq!(agents.ino(), claude.ino());
    assert_eq!(agents.nlink(), 2);
    let text = read(&dir.join("CLAUDE.md"));
    assert!(text.contains(START), "{text}");
    assert!(!text.contains("@AGENTS.md"), "{text}");
    assert_eq!(
        names(&init_json(&dir, &[]), "unchanged"),
        ["AGENTS.md", "CLAUDE.md"]
    );
}

#[cfg(unix)]
#[test]
fn a_failed_write_leaves_every_file_as_it_was() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (_temp, dir) = project();
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("CLAUDE.md"), "# Claude\n").unwrap();
    symlink("sub/CLAUDE.md", dir.join("CLAUDE.md")).unwrap();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
    // Where permissions do not bind, as for root, there is nothing to test.
    let binding = fs::write(sub.join("probe"), "").is_err();
    if binding {
        let err = fails(&dir, &["agents", "init", "--json"]);
        assert!(err.contains("cannot write sub/CLAUDE.md"), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
        assert!(!dir.join("AGENTS.md").exists());
        assert_eq!(read(&sub.join("CLAUDE.md")), "# Claude\n");
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".braid-"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn files_that_are_not_utf8_are_refused_and_left_as_they_are() {
    let (_temp, dir) = project();
    let latin1 = b"# R\xe8gles\n".to_vec();
    fs::write(dir.join("AGENTS.md"), &latin1).unwrap();
    fs::write(dir.join("CLAUDE.md"), "# Claude\n").unwrap();
    let err = fails(&dir, &["agents", "init"]);
    assert!(
        err.contains("AGENTS.md is not UTF-8 text: nothing was written"),
        "{err}"
    );
    assert_eq!(fs::read(dir.join("AGENTS.md")).unwrap(), latin1);
    assert_eq!(read(&dir.join("CLAUDE.md")), "# Claude\n");

    fs::remove_file(dir.join("AGENTS.md")).unwrap();
    fs::write(dir.join("CLAUDE.md"), &latin1).unwrap();
    let err = fails(&dir, &["agents", "init"]);
    assert!(
        err.contains("CLAUDE.md is not UTF-8 text: nothing was written"),
        "{err}"
    );
    assert!(!dir.join("AGENTS.md").exists());
    assert_eq!(fs::read(dir.join("CLAUDE.md")).unwrap(), latin1);
}

#[test]
fn a_dry_run_says_what_would_change_and_writes_nothing() {
    let (_temp, dir) = project();
    fs::write(dir.join("CLAUDE.md"), "# Claude\n").unwrap();
    let report = init_json(&dir, &["--dry-run"]);
    assert_eq!(report["dry_run"], true);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert_eq!(names(&report, "updated"), ["CLAUDE.md"]);
    assert!(!dir.join("AGENTS.md").exists());
    assert_eq!(read(&dir.join("CLAUDE.md")), "# Claude\n");

    let said = ok(&dir, &["agents", "init", "--dry-run"]);
    assert!(said.contains("Would create AGENTS.md"), "{said}");
    assert!(said.contains("Would add @AGENTS.md to CLAUDE.md"), "{said}");
    assert!(said.contains("Nothing was written."), "{said}");
    assert!(said.ends_with(&format!("{END}\n")), "{said}");
    assert!(said.contains(START));
    assert!(!dir.join("AGENTS.md").exists());
    ok(&dir, &["agents", "init"]);
    let said = ok(&dir, &["agents", "init", "--dry-run"]);
    assert!(!said.contains(START), "{said}");
}

#[test]
fn the_section_names_the_branch_agents_land_on() {
    let (_temp, dir) = temp();
    repository_with_branch(&dir, "master");
    let report = init_json(&dir, &["--dry-run"]);
    assert_eq!(report["branch"], "master");
    assert_eq!(report["repository"], dir.to_str().unwrap());

    let (_other, trunk) = temp();
    repository_with_branch(&trunk, "trunk");
    let report = init_json(&trunk, &[]);
    assert_eq!(report["branch"], "trunk");
    let text = read(&trunk.join("AGENTS.md"));
    assert!(text.contains("--track trunk\n"), "{text}");
    assert!(text.contains("--branch trunk\n"), "{text}");
    ok(&trunk, &["ref", "set", "main", "trunk"]);
    let report = init_json(&trunk, &[]);
    assert_eq!(report["branch"], "trunk");
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    let report = init_json(&trunk, &["--branch", "main"]);
    assert_eq!(report["branch"], "main");
    assert_eq!(report["previous_branch"], "trunk");
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert!(read(&trunk.join("AGENTS.md")).contains("--track main\n"));

    let (_third, two) = temp();
    repository_with_branch(&two, "trunk");
    ok(&two, &["ref", "set", "develop", "trunk"]);
    let err = fails(&two, &["agents", "init"]);
    assert!(err.contains("develop, trunk; pass --branch"), "{err}");
    assert!(!two.join("AGENTS.md").exists());
    for bad in ["two words", "tags/v1", "braid/git-map"] {
        fails(&two, &["agents", "init", "--branch", bad]);
    }
}

#[test]
fn a_branch_the_repository_does_not_have_is_refused() {
    let (_temp, dir) = project();
    let err = fails(&dir, &["agents", "init", "--branch", "mian", "--dry-run"]);
    assert!(err.contains("has no branch \"mian\"; it has main"), "{err}");
    fails(&dir, &["agents", "init", "--branch", "mian"]);
    assert!(!dir.join("AGENTS.md").exists());
}

#[test]
fn branch_names_that_shell_commands_would_misread_are_refused() {
    let (_temp, dir) = temp();
    repository_with_branch(&dir, "feature/x.y_z-1");
    let report = init_json(&dir, &[]);
    assert_eq!(report["branch"], "feature/x.y_z-1");
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.contains("--track feature/x.y_z-1\n"), "{text}");
    // It reads back exactly: a second run changes nothing.
    assert_eq!(names(&init_json(&dir, &[]), "unchanged"), ["AGENTS.md"]);

    for bad in [
        "feat(x)",
        "a;b",
        "a;touch${IFS}pwned",
        "q$(id)",
        "x`id`",
        "it's",
        "a|b",
        "a>b",
        "-x",
        "--no-claude",
    ] {
        let flag = format!("--branch={bad}");
        let err = fails(&dir, &["agents", "init", "--dry-run", &flag]);
        assert!(
            err.contains("cannot name the branch agents land on"),
            "{bad}: {err}"
        );
    }
    assert_eq!(read(&dir.join("AGENTS.md")), text);
}

#[test]
fn a_branch_named_before_the_project_was_imported_is_chosen_again() {
    // A project whose only branch is master, set up out of order.
    let (_temp, dir) = temp();
    git(&dir, &["init", "--quiet", "-b", "master"]);
    fs::write(dir.join("README.md"), "# Project\n").unwrap();
    git(&dir, &["add", "README.md"]);
    git(&dir, &["commit", "--quiet", "-m", "First"]);

    // Before braid is set up, there is no branch to name.
    let err = fails(&dir, &["agents", "init"]);
    assert!(err.contains("--branch"), "{err}");
    assert!(!dir.join("AGENTS.md").exists());
    ok(&dir, &["init"]);
    let err = fails(&dir, &["agents", "init"]);
    assert!(err.contains("no branch yet"), "{err}");
    assert!(err.contains("braid git import ."), "{err}");
    // A guess written anyway, before the import...
    let report = init_json(&dir, &["--branch", "main"]);
    assert_eq!(report["branch"], "main");

    // ...gives way, once the history is in, to the branch it has.
    ok(&dir, &["git", "import", "."]);
    let said = ok(&dir, &["agents", "init", "--dry-run"]);
    assert!(
        said.contains(
            "The section named main, which the repository does not have: it would name master now."
        ),
        "{said}"
    );
    let report = init_json(&dir, &[]);
    assert_eq!(report["branch"], "master");
    assert_eq!(report["previous_branch"], "main");
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.contains("--track master\n"), "{text}");
    assert!(!text.contains("--track main"), "{text}");
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    assert_eq!(report["previous_branch"], Value::Null);
}

#[test]
fn it_writes_at_the_root_of_the_repository_or_of_the_workspace() {
    let (_temp, dir) = project();
    let deep = dir.join("src/deep");
    fs::create_dir_all(&deep).unwrap();
    let report = init_json(&deep, &[]);
    assert_eq!(report["dir"], dir.to_str().unwrap());
    assert!(dir.join("AGENTS.md").is_file());
    assert!(!deep.join("AGENTS.md").exists());

    ok(
        &dir,
        &[
            "workspace",
            "add",
            "agent-1",
            "--track",
            "release",
            "--new-branch",
        ],
    );
    let workspace = dir.join(".braid/work/agent-1");
    let inside = workspace.join("docs");
    fs::create_dir_all(&inside).unwrap();
    let report = init_json(&inside, &[]);
    assert_eq!(report["dir"], workspace.to_str().unwrap());
    assert_eq!(report["branch"], "release");
    assert_eq!(report["repository"], dir.to_str().unwrap());
    assert!(read(&workspace.join("AGENTS.md")).contains("--track release\n"));
    let status: Value = serde_json::from_str(&ok(&workspace, &["status", "--json"])).unwrap();
    assert_eq!(status["added"], serde_json::json!(["AGENTS.md"]));

    let (_other, elsewhere) = temp();
    let report = init_json(&elsewhere, &["--dir", dir.join("src").to_str().unwrap()]);
    assert_eq!(report["dir"], dir.join("src").to_str().unwrap());
    assert_eq!(report["branch"], "main");
    assert!(dir.join("src/AGENTS.md").is_file());
    fails(&elsewhere, &["agents", "init", "--dir", "no-such-dir"]);
}

#[test]
fn workspace_and_repo_say_where_to_write() {
    let (_temp, dir) = project();
    ok(&dir, &["workspace", "add", "agent-1"]);
    let workspace = dir.join(".braid/work/agent-1");

    // From the project root, --workspace writes in that workspace, as the
    // section's commands run from the root do.
    let report = init_json(&dir, &["--workspace", "agent-1"]);
    assert_eq!(report["dir"], workspace.to_str().unwrap());
    assert_eq!(report["branch"], "main");
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert!(workspace.join("AGENTS.md").is_file());
    assert!(!dir.join("AGENTS.md").exists());
    let err = fails(
        &dir,
        &["--workspace", "nope", "agents", "init", "--dry-run"],
    );
    assert!(err.contains("no workspace named \"nope\""), "{err}");
    let err = fails(
        &dir,
        &["--workspace", "agent-1", "agents", "init", "--dir", "."],
    );
    assert!(err.contains("give one of them"), "{err}");

    // From outside the repository, --repo writes at its root.
    let (_other, elsewhere) = temp();
    let report = init_json(&elsewhere, &["--repo", dir.to_str().unwrap(), "--dry-run"]);
    assert_eq!(report["dir"], dir.to_str().unwrap());
    assert_eq!(report["repository"], dir.to_str().unwrap());
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    ok(
        &elsewhere,
        &["--repo", dir.to_str().unwrap(), "agents", "init"],
    );
    assert!(dir.join("AGENTS.md").is_file());
    assert!(!elsewhere.join("AGENTS.md").exists());
}
