// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of `braid agents init`: the braid section of a project's
//! AGENTS.md, written once and rewritten in place, CLAUDE.md, dry runs, JSON
//! and the branch the section names.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const START: &str = "<!-- braid:start -->";
const END: &str = "<!-- braid:end -->";

/// Runs braid in `dir`.
fn braid(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
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

#[test]
fn a_fresh_directory_gets_an_agents_md_and_no_claude_md() {
    let (_temp, dir) = temp();
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert!(names(&report, "unchanged").is_empty());
    assert_eq!(report["branch"], "main");
    assert_eq!(report["dry_run"], false);
    assert!(report["repository"].is_null());
    assert_eq!(report["dir"], dir.to_str().unwrap());

    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(&format!("{START}\n")), "{text}");
    assert!(text.ends_with(&format!("{END}\n")), "{text}");
    assert!(text.contains("braid workspace add agent-1 .braid/work/agent-1 --track main\n"));
    assert!(text.contains("https://braid.elffuss.com/agents.md"));
    assert!(!dir.join("CLAUDE.md").exists());

    // In text, it says so, and that braid itself is not set up yet.
    fs::remove_file(dir.join("AGENTS.md")).unwrap();
    let said = ok(&dir, &["agents", "init"]);
    assert!(said.contains("Created AGENTS.md"), "{said}");
    assert!(said.contains("run `braid init`"), "{said}");
}

#[test]
fn only_the_section_changes_and_a_second_run_changes_nothing() {
    let (_temp, dir) = temp();
    let above = "# Our project\n\nTabs, not spaces.\n\n";
    let older = format!("{START}\n## Braid\n\nAn older section.\n{END}\n");
    let below = "\n## Releases\n\nTagged by hand.\n";
    fs::write(dir.join("AGENTS.md"), format!("{above}{older}{below}")).unwrap();

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    let text = read(&dir.join("AGENTS.md"));
    assert!(text.starts_with(above), "{text}");
    assert!(text.ends_with(below), "{text}");
    let section = section_of(&text);
    assert_eq!(text, format!("{above}{section}{below}"));
    assert!(!section.contains("An older section"));
    assert!(section.contains("--track main"));

    // Idempotent: the same bytes, reported unchanged.
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert_eq!(read(&dir.join("AGENTS.md")), text);
    let said = ok(&dir, &["agents", "init"]);
    assert!(said.contains("has this braid section already"), "{said}");
}

#[test]
fn a_file_without_markers_gets_the_section_at_its_end_once() {
    let (_temp, dir) = temp();
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
    let (_temp, dir) = temp();
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
            let out = braid(&dir, args);
            assert_eq!(out.status.code(), Some(1), "{text:?} {args:?}");
            assert!(out.stdout.is_empty());
            let err = String::from_utf8(out.stderr).unwrap();
            assert!(err.contains("unbalanced braid markers"), "{err}");
            assert!(err.contains(lines), "{text:?}: {err}");
            assert!(err.contains("nothing was written"), "{err}");
            assert_eq!(read(&dir.join("AGENTS.md")), text);
            assert_eq!(read(&dir.join("CLAUDE.md")), claude);
        }
    }
}

#[test]
fn claude_md_gets_the_include_line_and_only_that() {
    let (_temp, dir) = temp();
    let claude = "# Notes for Claude\n\nBe brief.";
    fs::write(dir.join("CLAUDE.md"), claude).unwrap();

    // --no-claude leaves it alone, and does not list it.
    let report = init_json(&dir, &["--no-claude"]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert!(names(&report, "updated").is_empty());
    assert!(names(&report, "unchanged").is_empty());
    assert_eq!(read(&dir.join("CLAUDE.md")), claude);

    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "updated"), ["CLAUDE.md"]);
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    assert_eq!(
        read(&dir.join("CLAUDE.md")),
        format!("{claude}\n@AGENTS.md\n")
    );

    // Once it includes AGENTS.md, it is left as it is.
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
}

#[cfg(unix)]
#[test]
fn links_stay_links() {
    let (_temp, dir) = temp();
    // CLAUDE.md is AGENTS.md under another name: it holds the section, and
    // never includes itself.
    std::os::unix::fs::symlink("AGENTS.md", dir.join("CLAUDE.md")).unwrap();
    let report = init_json(&dir, &[]);
    assert_eq!(names(&report, "created"), ["AGENTS.md"]);
    assert_eq!(names(&report, "unchanged"), ["CLAUDE.md"]);
    assert!(!read(&dir.join("AGENTS.md")).contains("@AGENTS.md"));
    assert!(
        fs::symlink_metadata(dir.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // An AGENTS.md that links to a file elsewhere: that file is the one
    // written, and the link stays.
    let (_other, elsewhere) = temp();
    let shared = elsewhere.join("rules.md");
    fs::write(&shared, "# Shared rules\n").unwrap();
    let (_project, project) = temp();
    std::os::unix::fs::symlink(&shared, project.join("AGENTS.md")).unwrap();
    let report = init_json(&project, &[]);
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    let link = fs::symlink_metadata(project.join("AGENTS.md")).unwrap();
    assert!(link.file_type().is_symlink());
    assert!(read(&shared).starts_with("# Shared rules\n\n<!-- braid:start -->\n"));
    // Nothing is left behind next to it.
    let left: Vec<_> = fs::read_dir(&elsewhere).unwrap().collect();
    assert_eq!(left.len(), 1);
}

#[test]
fn a_dry_run_says_what_would_change_and_writes_nothing() {
    let (_temp, dir) = temp();
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
    // The section it would write, whole.
    assert!(said.ends_with(&format!("{END}\n")), "{said}");
    assert!(said.contains(START));
    assert!(!dir.join("AGENTS.md").exists());
    // Nothing to do: nothing to show either.
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

    // A repository whose only branch has another name.
    let (_other, trunk) = temp();
    repository_with_branch(&trunk, "trunk");
    let report = init_json(&trunk, &[]);
    assert_eq!(report["branch"], "trunk");
    let text = read(&trunk.join("AGENTS.md"));
    assert!(text.contains("--track trunk\n"), "{text}");
    assert!(text.contains("--branch trunk`"), "{text}");
    // Running it again keeps that branch, even once main exists too.
    ok(&trunk, &["ref", "set", "main", "trunk"]);
    let report = init_json(&trunk, &[]);
    assert_eq!(report["branch"], "trunk");
    assert_eq!(names(&report, "unchanged"), ["AGENTS.md"]);
    // --branch says otherwise.
    let report = init_json(&trunk, &["--branch", "main"]);
    assert_eq!(report["branch"], "main");
    assert_eq!(names(&report, "updated"), ["AGENTS.md"]);
    assert!(read(&trunk.join("AGENTS.md")).contains("--track main\n"));

    // Two branches, neither main nor master: braid will not guess.
    let (_third, two) = temp();
    repository_with_branch(&two, "trunk");
    ok(&two, &["ref", "set", "develop", "trunk"]);
    let out = braid(&two, &["agents", "init"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("develop, trunk; pass --branch"), "{err}");
    assert!(!two.join("AGENTS.md").exists());
    for bad in ["two words", "tags/v1", "braid/git-map"] {
        let out = braid(&two, &["agents", "init", "--branch", bad]);
        assert_eq!(out.status.code(), Some(1), "{bad}");
    }
}

#[test]
fn it_writes_at_the_root_of_the_repository_or_of_the_workspace() {
    let (_temp, dir) = temp();
    repository_with_branch(&dir, "main");
    let deep = dir.join("src/deep");
    fs::create_dir_all(&deep).unwrap();
    let report = init_json(&deep, &[]);
    assert_eq!(report["dir"], dir.to_str().unwrap());
    assert!(dir.join("AGENTS.md").is_file());
    assert!(!deep.join("AGENTS.md").exists());

    // A workspace, under .braid/work as the section says, on another
    // branch: the section goes at its root and names its branch.
    ok(
        &dir,
        &[
            "workspace",
            "add",
            "agent-1",
            ".braid/work/agent-1",
            "--track",
            "release",
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
    // It is the workspace's change, to commit and land like any other.
    let status: Value = serde_json::from_str(&ok(&workspace, &["status", "--json"])).unwrap();
    assert_eq!(status["added"][0], "AGENTS.md");

    // --dir names the directory whatever the current one is.
    let (_other, elsewhere) = temp();
    let report = init_json(&elsewhere, &["--dir", dir.join("src").to_str().unwrap()]);
    assert_eq!(report["dir"], dir.join("src").to_str().unwrap());
    assert_eq!(report["branch"], "main");
    assert!(dir.join("src/AGENTS.md").is_file());
    let out = braid(&elsewhere, &["agents", "init", "--dir", "no-such-dir"]);
    assert_eq!(out.status.code(), Some(1));
}
