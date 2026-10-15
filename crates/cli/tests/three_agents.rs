// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Three agents on one repository, end to end: agent-a and agent-b edit
//! the same line of a file, agent-c appends a line at its end, and they
//! land in the order c, a, b. c fast-forwards, a is rebased and merged
//! with c's line, b conflicts with a, brings the conflict in with `braid
//! update`, resolves it, commits and lands; then `braid git sync` pushes
//! main to a git remote. Every report is checked as an agent reads it:
//! the outcomes, the names of the conflict's sides, the paths each
//! working copy got, where each changeset landed, the op log, who made
//! what, and the file everyone ends with.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const SEED: &str = "title\nalpha\nbeta\ngamma\n";
const FINAL: &str = "title\nalpha, from a and b\nbeta\ngamma\ndelta, from c\n";

/// Git without the user's or the system's configuration.
const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Ada Lovelace"),
    ("GIT_AUTHOR_EMAIL", "ada@example.org"),
    ("GIT_COMMITTER_NAME", "Ada Lovelace"),
    ("GIT_COMMITTER_EMAIL", "ada@example.org"),
];

/// Runs braid in `dir` as `agent`.
fn braid(agent: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .env("BRAID_AGENT", agent)
        .env("BRAID_AUTHOR", format!("{agent} <{agent}@example.org>"))
        .output()
        .unwrap()
}

/// Runs braid with `--json` as `agent`, expecting exit status `code`.
fn run(agent: &str, dir: &Path, args: &[&str], code: i32) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let out = braid(agent, dir, &all);
    assert_eq!(
        out.status.code(),
        Some(code),
        "{agent}: braid {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).unwrap()
}

/// The keys of a JSON object, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn id(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

fn short(id: &str) -> &str {
    &id[..12]
}

#[test]
fn three_agents_edit_one_file_and_land_in_turn() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let repo = base.join("repo");
    run("seeder", &base, &["init", repo.to_str().unwrap()], 0);
    let seed_file = base.join("seed.txt");
    std::fs::write(&seed_file, SEED).unwrap();
    let set = format!("notes.txt=@{}", seed_file.display());
    let seeded = run(
        "seeder",
        &repo,
        &["change", "create", "--base", "none", "--set", &set]
            .into_iter()
            .chain(["-m", "Seed the notes", "--land", "main"])
            .collect::<Vec<_>>(),
        0,
    );
    let seed = id(&seeded["change"]);

    // Each agent gets a workspace, edits, and commits before anyone lands.
    let dirs: Vec<PathBuf> = ["a", "b", "c"]
        .iter()
        .map(|name| {
            let dir = base.join(name);
            let made = run(
                "seeder",
                &repo,
                &["workspace", "add", name, dir.to_str().unwrap()],
                0,
            );
            assert_eq!(made["base"], seed.as_str());
            dir
        })
        .collect();
    let (a, b, c) = (&dirs[0], &dirs[1], &dirs[2]);
    std::fs::write(a.join("notes.txt"), SEED.replace("alpha", "alpha, from a")).unwrap();
    std::fs::write(b.join("notes.txt"), SEED.replace("alpha", "alpha, from b")).unwrap();
    std::fs::write(c.join("notes.txt"), format!("{SEED}delta, from c\n")).unwrap();
    let a_change = id(&run("agent-a", a, &["commit", "-m", "a: alpha"], 0)["changeset"]);
    let b_change = id(&run("agent-b", b, &["commit", "-m", "b: alpha"], 0)["changeset"]);
    let c_change = id(&run("agent-c", c, &["commit", "-m", "c: delta"], 0)["changeset"]);

    // c lands first: a fast-forward to its own changeset.
    let landed = run("agent-c", c, &["land"], 0);
    assert_eq!(landed["outcome"], "fast-forward");
    assert_eq!(landed["head_before"], seed.as_str());
    assert_eq!(landed["head"], c_change.as_str());
    assert_eq!(
        landed["landed"],
        json!([{ "from": c_change, "to": c_change }])
    );
    assert_eq!(landed["merged_paths"], json!([]));
    assert_eq!(landed["updated_paths"], json!([]));
    assert_eq!(landed["next"], Value::Null);

    // a is rebased onto c: its line and c's merge, and a's working copy
    // gets c's line.
    let landed = run("agent-a", a, &["land"], 0);
    assert_eq!(landed["outcome"], "rebased");
    assert_eq!(landed["head_before"], c_change.as_str());
    let a_landed = id(&landed["head"]);
    assert_ne!(a_landed, a_change);
    assert_eq!(
        landed["landed"],
        json!([{ "from": a_change, "to": a_landed }])
    );
    assert_eq!(landed["merged_paths"], json!(["notes.txt"]));
    assert_eq!(landed["updated_paths"], json!(["notes.txt"]));
    assert_eq!(landed["base"], a_landed.as_str());
    assert_eq!(
        read(a, "notes.txt"),
        "title\nalpha, from a\nbeta\ngamma\ndelta, from c\n"
    );

    // b is behind by both, and conflicts with a: main does not move.
    let now = run("agent-b", b, &["status"], 0);
    assert_eq!(now["behind"], true);
    assert_eq!(now["behind_by"], 2);
    assert_eq!(now["unlanded"], json!([b_change]));
    let landed = run("agent-b", b, &["land"], 2);
    assert_eq!(landed["outcome"], "conflicts");
    assert_eq!(landed["head_before"], a_landed.as_str());
    assert_eq!(landed["head"], a_landed.as_str());
    assert_eq!(landed["operation"], Value::Null);
    assert_eq!(landed["change"], b_change.as_str());
    assert_eq!(landed["landed"], json!([{ "from": b_change, "to": null }]));
    assert_eq!(landed["updated_paths"], json!([]));
    assert_eq!(landed["next"], "braid update");
    let conflict = &landed["conflicts"][0];
    assert_eq!(
        keys(conflict),
        ["base", "branch", "hunks", "kind", "path", "workspace"]
    );
    assert_eq!(conflict["path"], "notes.txt");
    assert_eq!(conflict["kind"], "CONTENT");
    let hunk = &conflict["hunks"][0];
    assert_eq!(keys(hunk), ["base", "branch", "workspace"]);
    // Line 1 counted from 0, end excluded: the second line.
    assert_eq!(
        hunk["base"],
        json!({ "start": 1, "end": 2, "text": "alpha\n" })
    );
    assert_eq!(
        hunk["branch"],
        json!({ "start": 1, "end": 2, "text": "alpha, from a\n" })
    );
    assert_eq!(
        hunk["workspace"],
        json!({ "start": 1, "end": 2, "text": "alpha, from b\n" })
    );

    // update brings the conflict in, with c's line merged around it.
    let updated = run("agent-b", b, &["update"], 2);
    assert_eq!(updated["outcome"], "conflicts");
    assert_eq!(updated["head_before"], a_landed.as_str());
    assert_eq!(updated["head"], a_landed.as_str());
    assert_eq!(updated["base"], a_landed.as_str());
    assert_eq!(updated["landed"], json!([{ "from": b_change, "to": null }]));
    assert_eq!(updated["updated_paths"], json!(["notes.txt"]));
    assert_eq!(updated["next"], "braid commit");
    assert_eq!(
        updated["conflicts"][0]["hunks"][0]["workspace"]["text"],
        "alpha, from b\n"
    );
    let merged = read(b, "notes.txt");
    assert!(merged.contains("<<<<<<< main"), "{merged}");
    assert!(merged.ends_with("delta, from c\n"), "{merged}");
    let now = run("agent-b", b, &["status"], 0);
    assert_eq!(now["unlanded"], json!([b_change]));
    assert_eq!(now["behind"], false);
    assert_eq!(now["ready_to_commit"], false);
    assert_eq!(now["blocking"][0]["reason"], "markers");

    // b resolves, commits without -m, which keeps its message, and lands.
    std::fs::write(b.join("notes.txt"), FINAL).unwrap();
    assert_eq!(run("agent-b", b, &["status"], 0)["ready_to_commit"], true);
    let committed = run("agent-b", b, &["commit"], 0);
    assert_eq!(committed["message_replaced"], false);
    assert_eq!(committed["rebase"]["outcome"], "finished");
    let b_landed = id(&committed["changeset"]);
    let landed = run("agent-b", b, &["land"], 0);
    assert_eq!(landed["outcome"], "fast-forward");
    assert_eq!(
        landed["landed"],
        json!([{ "from": b_landed, "to": b_landed }])
    );

    // The op log says who did what, and how.
    let ops = run("seeder", &repo, &["op", "log", "-n", "4"], 0);
    let ops: Vec<(String, String)> = ops
        .as_array()
        .unwrap()
        .iter()
        .map(|op| (id(&op["agent"]), id(&op["description"])))
        .collect();
    let said = |agent: &str, text: String| (agent.to_owned(), text);
    assert_eq!(
        ops,
        [
            said(
                "agent-b",
                format!(
                    "land workspace b on main: fast-forward {} -> {} (0 paths merged)",
                    short(&b_landed),
                    short(&b_landed)
                )
            ),
            said(
                "agent-a",
                format!(
                    "land workspace a on main: rebased {} -> {} (1 path merged)",
                    short(&a_change),
                    short(&a_landed)
                )
            ),
            said(
                "agent-c",
                format!(
                    "land workspace c on main: fast-forward {} -> {} (0 paths merged)",
                    short(&c_change),
                    short(&c_change)
                )
            ),
            said(
                "seeder",
                format!(
                    "land changeset on main: fast-forward {} -> {} (0 paths merged)",
                    short(&seed),
                    short(&seed)
                )
            ),
        ]
    );

    // main's history names the agent that made each change, rebased or not.
    let log = run("seeder", &repo, &["log", "main"], 0);
    let history: Vec<(String, String, String)> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (id(&c["id"]), id(&c["agent"]), id(&c["message"])))
        .collect();
    assert_eq!(
        history,
        [
            (b_landed.clone(), "agent-b".into(), "b: alpha".into()),
            (a_landed.clone(), "agent-a".into(), "a: alpha".into()),
            (c_change.clone(), "agent-c".into(), "c: delta".into()),
            (seed.clone(), "seeder".into(), "Seed the notes".into()),
        ]
    );
    let shown = run("seeder", &repo, &["show", short(&a_landed)], 0);
    assert_eq!(shown["changes"][0]["status"], "M");
    assert_eq!(shown["author"]["name"], "agent-a");

    // Everyone ends with the same file.
    run("agent-a", a, &["update"], 0);
    run("agent-c", c, &["update"], 0);
    for dir in [a, b, c] {
        assert_eq!(read(dir, "notes.txt"), FINAL, "{}", dir.display());
    }

    // git sync pushes main to a git remote, which has the same file and
    // the agent in each commit.
    let remote = base.join("remote.git");
    git(
        &base,
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    let synced = run(
        "agent-b",
        &repo,
        &["git", "sync", remote.to_str().unwrap()],
        0,
    );
    assert_eq!(synced["outcome"], "pushed");
    assert_eq!(git(&remote, &["show", "main:notes.txt"]), FINAL);
    let commit = git(&remote, &["cat-file", "commit", "main"]);
    assert!(commit.contains("\nagent agent-b\n"), "{commit}");
    assert_eq!(run("seeder", &repo, &["fsck"], 0)["clean"], true);
}
