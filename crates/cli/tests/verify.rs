// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid verify` end to end, on small repositories whose test is a script
//! of their own, `check.sh`, which fails while a file named `broken` is
//! there: the green ref, bisecting to the changeset that broke the tests,
//! reverting it, and the limits on reverting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// The project's test, committed in every repository here.
const CHECK: &str = "test ! -f broken\n";

fn braid_with(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .env("BRAID_AGENT", "tester")
        .env("BRAID_AUTHOR", "Ada <ada@example.org>")
        .env("BRAID_HUB", "off")
        .envs(envs.iter().copied())
        .output()
        .unwrap()
}

/// Runs braid with `--json`, expecting exit status `code`.
fn run(dir: &Path, args: &[&str], code: i32) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = braid_with(dir, &all, &[]);
    assert_eq!(
        out.status.code(),
        Some(code),
        "braid {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn id(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

struct Repo {
    _temp: tempfile::TempDir,
    /// A directory outside the repository, for the tests' own files.
    outside: PathBuf,
    dir: PathBuf,
}

impl Repo {
    /// A repository whose `main` holds `check.sh` and a notes file.
    fn new() -> Repo {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let dir = base.join("repo");
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        run(&base, &["init", dir.to_str().unwrap()], 0);
        let repo = Repo {
            _temp: temp,
            outside,
            dir,
        };
        repo.land(&[("check.sh", CHECK), ("notes", "1\n2\n3\n")], &[], "Seed");
        repo
    }

    /// Lands a change on `main` that sets and deletes paths, and returns
    /// the changeset that holds it on `main`.
    fn land(&self, sets: &[(&str, &str)], deletes: &[&str], message: &str) -> String {
        let base = self.head().unwrap_or_else(|| "none".to_owned());
        let mut args = vec!["change", "create", "--base", &base, "-m", message];
        let mut owned = Vec::new();
        for (n, (path, text)) in sets.iter().enumerate() {
            let file = self.outside.join(format!("{message}-{n}"));
            std::fs::write(&file, text).unwrap();
            owned.push(format!("{path}=@{}", file.display()));
        }
        for set in &owned {
            args.extend(["--set", set]);
        }
        for path in deletes {
            args.extend(["--delete", path]);
        }
        args.extend(["--land", "main"]);
        let out = run(&self.dir, &args, 0);
        id(&out["land"]["head"])
    }

    fn head(&self) -> Option<String> {
        self.reference("main")
    }

    fn green(&self) -> Option<String> {
        self.reference("braid/green/main")
    }

    fn reference(&self, name: &str) -> Option<String> {
        let refs = run(&self.dir, &["refs"], 0);
        refs["refs"][name].as_str().map(str::to_owned)
    }

    /// Runs `braid verify` with `flags`, then `--` and `command`, expecting
    /// exit status `code`.
    fn verify_env(
        &self,
        flags: &[&str],
        command: &[&str],
        envs: &[(&str, &str)],
        code: i32,
    ) -> Value {
        let mut args = vec!["--json", "verify"];
        args.extend_from_slice(flags);
        args.push("--");
        args.extend_from_slice(command);
        let out = braid_with(&self.dir, &args, envs);
        assert_eq!(
            out.status.code(),
            Some(code),
            "braid {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn verify(&self, flags: &[&str], code: i32) -> Value {
        self.verify_env(flags, &["sh", "check.sh"], &[], code)
    }

    /// The test command of a repository whose failing changesets are listed
    /// in a file outside it: a changeset fails when its id is in the list.
    fn listed(&self) -> (PathBuf, Vec<String>) {
        let list = self.outside.join("failing");
        std::fs::write(&list, "").unwrap();
        let script = self.outside.join("listed.sh");
        std::fs::write(
            &script,
            "if grep -qx \"$BRAID_VERIFY_CHANGESET\" \"$1\"; then exit 1; fi\n",
        )
        .unwrap();
        let command = vec![
            "sh".to_owned(),
            script.display().to_string(),
            list.display().to_string(),
        ];
        (list, command)
    }

    fn has_record(&self) -> bool {
        // The branch's name in hex.
        self.dir.join(".braid/verify/6d61696e.json").is_file()
    }

    /// Forgets the local record of verify's reverts, as a fresh clone
    /// never had it.
    fn forget_record(&self) {
        std::fs::remove_dir_all(self.dir.join(".braid/verify")).unwrap();
    }
}

/// A repository where `main` was verified green at the seed, then had four
/// changesets landed, the second of which broke the tests. Returns the
/// seed and the four.
fn broken_after_green() -> (Repo, String, Vec<String>) {
    let r = Repo::new();
    let seed = r.head().unwrap();
    let out = r.verify(&[], 0);
    assert_eq!(out["outcome"], "pass");
    let ids = vec![
        r.land(&[("a", "a\n")], &[], "Add a"),
        r.land(&[("broken", "x\n")], &[], "Break the tests"),
        r.land(&[("b", "b\n")], &[], "Add b"),
        r.land(&[("c", "c\n")], &[], "Add c"),
    ];
    (r, seed, ids)
}

#[test]
fn a_pass_moves_the_green_ref_and_status_shows_it() {
    let r = Repo::new();
    let tip = r.head().unwrap();
    let out = r.verify(&[], 0);
    assert_eq!(out["outcome"], "pass");
    assert_eq!(out["branch"], "main");
    assert_eq!(out["tip"], tip.as_str());
    assert_eq!(out["green_before"], Value::Null);
    assert_eq!(out["green"], tip.as_str());
    assert_eq!(out["tests_run"], 1);
    assert_eq!(out["culprit"], Value::Null);
    assert_eq!(out["revert"], Value::Null);
    assert_eq!(r.green().as_deref(), Some(tip.as_str()));

    // The move is an operation of its own, by braid-verify.
    let log = run(&r.dir, &["op", "log", "-n", "1"], 0);
    assert_eq!(log[0]["agent"], "braid-verify");
    assert_eq!(log[0]["updates"][0]["name"], "braid/green/main");

    // Nothing to run while the head is green.
    let again = r.verify(&[], 0);
    assert_eq!(again["outcome"], "pass");
    assert_eq!(again["tests_run"], 0);

    run(&r.dir, &["workspace", "add", "w"], 0);
    let status = run(&r.dir, &["status", "--workspace", "w"], 0);
    assert_eq!(status["green"], tip.as_str());

    let text = braid_with(&r.dir, &["verify", "--", "sh", "check.sh"], &[]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.starts_with("main is green at"), "{text}");
}

#[test]
fn a_dry_run_moves_no_green_ref() {
    let r = Repo::new();
    let out = r.verify(&["--dry-run"], 0);
    assert_eq!(out["outcome"], "pass");
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["green"], Value::Null);
    assert!(id(&out["reason"]).contains("dry run"));
    assert_eq!(r.green(), None);
}

#[test]
fn red_on_the_head_bisects_to_the_changeset_that_broke_it() {
    let (r, seed, ids) = broken_after_green();
    let out = r.verify(&["--reruns", "0"], 4);
    assert_eq!(out["outcome"], "red");
    assert_eq!(out["tip"], ids[3].as_str());
    assert_eq!(out["culprit"], ids[1].as_str());
    assert_eq!(out["green_before"], seed.as_str());
    assert_eq!(out["green"], seed.as_str());
    assert_eq!(out["revert"], Value::Null);
    // The head, then the second and the first changesets after green.
    assert_eq!(out["tests_run"], 3);
    assert!(id(&out["reason"]).contains("--revert"));
    assert_eq!(r.green(), Some(seed));
    assert_eq!(r.head().as_deref(), Some(ids[3].as_str()));
}

#[test]
fn without_a_green_ref_only_the_head_is_tested_unless_from_says() {
    let r = Repo::new();
    let seed = r.head().unwrap();
    let culprit = r.land(&[("broken", "x\n")], &[], "Break");
    r.land(&[("a", "a\n")], &[], "Add a");
    let out = r.verify(&[], 4);
    assert_eq!(out["outcome"], "red");
    assert_eq!(out["culprit"], Value::Null);
    // The head, and the rerun.
    assert_eq!(out["tests_run"], 2);
    assert!(id(&out["reason"]).contains("--from"));

    let out = r.verify(&["--from", &seed, "--reruns", "0"], 4);
    assert_eq!(out["culprit"], culprit.as_str());
    assert_eq!(r.green(), None);
}

#[test]
fn a_revert_lands_and_the_green_ref_stays_until_the_next_pass() {
    let (r, seed, ids) = broken_after_green();
    let out = r.verify(&["--revert", "--reruns", "0"], 4);
    assert_eq!(out["outcome"], "reverted");
    assert_eq!(out["culprit"], ids[1].as_str());
    let revert = id(&out["revert"]);
    assert_eq!(out["land"]["outcome"], "rebased");
    assert_eq!(out["green"], seed.as_str());
    assert_eq!(r.green().as_deref(), Some(seed.as_str()));
    let head = r.head().unwrap();
    assert_eq!(out["land"]["head"], head.as_str());
    assert!(r.has_record());

    // The revert names its culprit, and braid-verify made it.
    let shown = run(&r.dir, &["show", "main"], 0);
    assert_eq!(shown["agent"], "braid-verify");
    assert!(
        shown["extras"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(["rebased-from", revert]))
    );
    assert!(
        shown["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == "broken" && c["op"] == "delete")
    );

    // The next pass tests the new head, which passes.
    let next = r.verify(&["--revert"], 0);
    assert_eq!(next["outcome"], "pass");
    assert_eq!(next["green"], head.as_str());
}

#[test]
fn a_dry_run_finds_the_culprit_and_reverts_nothing() {
    let (r, seed, ids) = broken_after_green();
    let out = r.verify(&["--revert", "--dry-run", "--reruns", "0"], 4);
    assert_eq!(out["outcome"], "red");
    assert_eq!(out["culprit"], ids[1].as_str());
    assert_eq!(out["revert"], Value::Null);
    assert!(id(&out["reason"]).contains("dry run"));
    assert_eq!(r.head().as_deref(), Some(ids[3].as_str()));
    assert_eq!(r.green(), Some(seed));
    assert!(!r.has_record());
}

#[test]
fn reverts_stop_at_the_hourly_cap() {
    let (r, _, ids) = broken_after_green();
    let none = r.verify(
        &["--revert", "--reruns", "0", "--max-reverts-per-hour", "0"],
        4,
    );
    assert_eq!(none["outcome"], "refused");
    assert!(id(&none["reason"]).contains("--max-reverts-per-hour"));
    assert_eq!(r.head().as_deref(), Some(ids[3].as_str()));

    let first = r.verify(
        &["--revert", "--reruns", "0", "--max-reverts-per-hour", "1"],
        4,
    );
    assert_eq!(first["outcome"], "reverted");
    // Green again, then broken again: the cap of one an hour is spent. It
    // is counted on the branch, so a clone without verify's record, as CI
    // has, counts it too.
    assert_eq!(r.verify(&[], 0)["outcome"], "pass");
    let again = r.land(&[("broken", "again\n")], &[], "Break again");
    r.forget_record();
    let second = r.verify(
        &["--revert", "--reruns", "0", "--max-reverts-per-hour", "1"],
        4,
    );
    assert_eq!(second["outcome"], "refused");
    assert!(id(&second["reason"]).contains("1 revert on main in the last hour"));
    assert_eq!(r.head(), Some(again));
    assert_eq!(second["revert"], Value::Null);
}

#[test]
fn verify_does_not_revert_its_own_revert() {
    let r = Repo::new();
    let (list, command) = r.listed();
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seed = r.head().unwrap();
    let green = r.verify_env(&[], &command, &[], 0);
    assert_eq!(green["green"], seed.as_str());

    let culprit = r.land(&[("a", "a\n")], &[], "Change a");
    std::fs::write(&list, format!("{culprit}\n")).unwrap();
    let out = r.verify_env(&["--revert", "--reruns", "0"], &command, &[], 4);
    assert_eq!(out["outcome"], "reverted");
    let revert = r.head().unwrap();

    // Now the revert fails and the change it undid passes: the culprit is
    // the revert, which verify leaves alone, by what the revert says of
    // itself, without its record.
    std::fs::write(&list, format!("{revert}\n")).unwrap();
    r.forget_record();
    let out = r.verify_env(&["--revert", "--reruns", "0"], &command, &[], 4);
    assert_eq!(out["outcome"], "refused");
    assert_eq!(out["culprit"], revert.as_str());
    assert!(id(&out["reason"]).contains("its own reverts"));
    assert_eq!(r.head(), Some(revert));
}

#[test]
fn a_failure_before_every_changeset_in_range_has_no_culprit() {
    let r = Repo::new();
    let (list, command) = r.listed();
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seed = r.head().unwrap();
    r.verify_env(&[], &command, &[], 0);
    let change = r.land(&[("a", "a\n")], &[], "Change a");
    // Something outside the repository broke: the green changeset fails too.
    std::fs::write(&list, format!("{seed}\n{change}\n")).unwrap();
    let out = r.verify_env(&["--revert", "--reruns", "0"], &command, &[], 4);
    assert_eq!(out["outcome"], "no_culprit");
    assert_eq!(out["culprit"], Value::Null);
    assert_eq!(out["tests_run"], 2);
    assert_eq!(r.head(), Some(change));
}

#[test]
fn a_revert_that_conflicts_exits_with_2_and_moves_nothing() {
    let r = Repo::new();
    r.verify(&[], 0);
    r.land(&[("notes", "1\nbad\n3\n"), ("broken", "x\n")], &[], "Break");
    let later = r.land(&[("notes", "1\nworse\n3\n")], &[], "Edit the same line");
    let out = r.verify(&["--revert", "--reruns", "0"], 2);
    assert_eq!(out["outcome"], "revert_conflicts");
    assert_eq!(out["land"]["outcome"], "conflicts");
    assert_eq!(out["land"]["conflicts"][0]["path"], "notes");
    assert_eq!(r.head(), Some(later));
}

#[test]
fn a_run_that_fails_then_passes_is_flaky() {
    let r = Repo::new();
    let marker = r.outside.join("ran-once");
    let script = format!(
        "if [ -f '{0}' ]; then exit 0; fi; touch '{0}'; exit 1",
        marker.display()
    );
    let out = r.verify_env(&[], &["sh", "-c", &script], &[], 0);
    assert_eq!(out["outcome"], "flaky");
    assert_eq!(out["tests_run"], 2);
    // A flaky pass leaves the green ref where it was, unless asked.
    assert_eq!(out["green"], Value::Null);
    assert!(id(&out["reason"]).contains("--flaky-moves-green"));
    assert_eq!(r.green(), None);
    std::fs::remove_file(&marker).unwrap();
    let out = r.verify_env(&["--flaky-moves-green"], &["sh", "-c", &script], &[], 0);
    assert_eq!(out["outcome"], "flaky");
    assert_eq!(out["green"], r.head().unwrap().as_str());
}

#[test]
fn the_tests_get_a_clean_copy_and_no_credentials() {
    let r = Repo::new();
    let tip = r.head().unwrap();
    let script = format!(
        "test -z \"$BRAID_TOKEN\" && test -z \"$BRAID_HUB_URL\" && test -z \"$BRAID_HUB\" \
         && test \"$BRAID_VERIFY_CHANGESET\" = {tip} && test \"$BRAID_VERIFY_BRANCH\" = main \
         && test -f check.sh && test -f notes && test ! -e .braid && test ! -e .git"
    );
    let envs = [
        ("BRAID_TOKEN", "brd_secret"),
        ("BRAID_HUB_URL", "https://hub.example.org"),
    ];
    let out = r.verify_env(&["--dry-run"], &["sh", "-c", &script], &envs, 0);
    assert_eq!(out["outcome"], "pass");
    // Braid's other variables reach the tests.
    let leak = "test -n \"$BRAID_AGENT\"";
    let out = r.verify_env(&["--dry-run"], &["sh", "-c", leak], &envs, 0);
    assert_eq!(out["outcome"], "pass");
    // The environment as `env` prints it, with no shell to change it: PWD
    // names the copy, and OLDPWD, which named the repository, is gone.
    let envs = [
        ("BRAID_TOKEN", "brd_secret"),
        ("BRAID_HUB_URL", "https://hub.example.org"),
        ("OLDPWD", r.dir.to_str().unwrap()),
        ("PWD", r.dir.to_str().unwrap()),
    ];
    let out = braid_with(&r.dir, &["verify", "--dry-run", "--", "env"], &envs);
    assert!(out.status.success());
    let printed = String::from_utf8_lossy(&out.stderr);
    let var = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
    };
    assert_eq!(var("BRAID_TOKEN"), None);
    assert_eq!(var("BRAID_HUB_URL"), None);
    assert_eq!(var("OLDPWD"), None);
    let pwd = var("PWD").unwrap();
    assert!(pwd.contains("braid-verify-"), "{pwd}");
    assert_eq!(var("BRAID_VERIFY_BRANCH").as_deref(), Some("main"));
    // A command that cannot run is an error, not a red branch.
    let out = braid_with(&r.dir, &["verify", "--", "no-such-command-for-braid"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn a_timeout_kills_the_command_and_what_it_started() {
    let r = Repo::new();
    let pid_file = r.outside.join("pid");
    let script = format!(
        "sleep 120 > /dev/null 2>&1 & echo $! > '{}'; wait",
        pid_file.display()
    );
    let out = r.verify_env(
        &["--timeout", "1s", "--reruns", "0"],
        &["sh", "-c", &script],
        &[],
        4,
    );
    assert_eq!(out["outcome"], "red");
    assert_eq!(out["tests_run"], 1);
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    // The background sleep went with its group: gone, or a zombie waiting
    // to be reaped.
    let mut alive = true;
    for _ in 0..100 {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&ps.stdout).trim().to_owned();
        if state.is_empty() || state.starts_with('Z') {
            alive = false;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!alive, "the background process {pid} outlived the timeout");
}

/// Whether the process `pid` is gone, or a zombie waiting to be reaped,
/// within a few seconds.
#[cfg(unix)]
fn gone(pid: &str) -> bool {
    for _ in 0..100 {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&ps.stdout).trim().to_owned();
        if state.is_empty() || state.starts_with('Z') {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The contents of `path` once a command wrote them, within a few seconds.
#[cfg(unix)]
fn written(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text.trim().to_owned();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{} was never written", path.display());
}

#[cfg(unix)]
#[test]
fn what_a_passing_command_leaves_running_is_killed() {
    let r = Repo::new();
    let pid_file = r.outside.join("pid");
    let script = format!(
        "sleep 120 > /dev/null 2>&1 & echo $! > '{}'",
        pid_file.display()
    );
    let out = r.verify_env(&["--dry-run"], &["sh", "-c", &script], &[], 0);
    assert_eq!(out["outcome"], "pass");
    let pid = written(&pid_file);
    assert!(gone(&pid), "the background process {pid} outlived its test");
}

#[cfg(unix)]
#[test]
fn stopping_braid_stops_the_tests_and_removes_their_copy() {
    let r = Repo::new();
    let shell_file = r.outside.join("shell");
    let sleep_file = r.outside.join("sleep");
    let dir_file = r.outside.join("dir");
    let script = format!(
        "pwd > '{dir}'; echo $$ > '{shell}'; sleep 120 > /dev/null 2>&1 & echo $! > '{sleep}'; wait",
        dir = dir_file.display(),
        shell = shell_file.display(),
        sleep = sleep_file.display(),
    );
    let mut braid = Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(&r.dir)
        .args(["verify", "--", "sh", "-c", &script])
        .env("BRAID_HUB", "off")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let sleep = written(&sleep_file);
    let shell = written(&shell_file);
    let dir = PathBuf::from(written(&dir_file));
    assert!(dir.is_dir());
    // Only the braid this test started.
    let term = Command::new("kill")
        .args(["-TERM", &braid.id().to_string()])
        .status()
        .unwrap();
    assert!(term.success());
    braid.wait().unwrap();
    assert!(gone(&shell), "the test's shell {shell} outlived braid");
    assert!(gone(&sleep), "the test's sleep {sleep} outlived braid");
    let mut removed = false;
    for _ in 0..100 {
        if !dir.exists() {
            removed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(removed, "{} is still there", dir.display());
}

#[test]
fn a_revert_verify_made_earlier_does_not_stall_the_bisect() {
    // Green at the seed; C broke the tests and verify reverted it; then D
    // broke them again. Bisecting from the seed meets C first: verify sees
    // its revert, tests it, and goes on after it to find D.
    let r = Repo::new();
    assert_eq!(r.verify(&[], 0)["outcome"], "pass");
    let c = r.land(&[("broken", "c\n")], &[], "C breaks the tests");
    let first = r.verify(&["--revert", "--reruns", "0"], 4);
    assert_eq!(first["outcome"], "reverted");
    assert_eq!(first["culprit"], c.as_str());
    let d = r.land(
        &[("broken", "d\n"), ("d", "d\n")],
        &[],
        "D breaks them again",
    );
    let second = r.verify(&["--revert", "--reruns", "0"], 4);
    assert_eq!(second["outcome"], "reverted", "{second}");
    assert_eq!(second["culprit"], d.as_str());
    // The head, C, C's revert, then the green one is not needed.
    assert_eq!(second["tests_run"], 3);
    assert_eq!(r.verify(&[], 0)["outcome"], "pass");
}

#[test]
fn a_changeset_that_cannot_be_written_here_is_skipped() {
    // Two names that differ only in case are one file where the file
    // system ignores case: such a changeset cannot be tested there.
    let probe = tempfile::tempdir().unwrap();
    std::fs::write(probe.path().join("Aa"), "").unwrap();
    if !probe.path().join("aA").exists() {
        return;
    }
    let r = Repo::new();
    assert_eq!(r.verify(&[], 0)["outcome"], "pass");
    r.land(&[("a", "a\n")], &[], "Add a");
    let twins = r.land(&[("Case", "1\n"), ("case", "2\n")], &[], "Add twins");
    r.land(&[], &["case"], "Remove a twin");
    let culprit = r.land(&[("broken", "x\n")], &[], "Break");
    r.land(&[("z", "z\n")], &[], "Add z");
    let out = r.verify(&["--reruns", "0"], 4);
    assert_eq!(out["outcome"], "red");
    assert_eq!(out["culprit"], culprit.as_str());
    assert_eq!(out["skipped"], serde_json::json!([twins]));
}

#[cfg(unix)]
#[test]
fn an_error_after_the_revert_landed_keeps_it_in_the_report() {
    use std::os::unix::fs::PermissionsExt;
    let (r, _, ids) = broken_after_green();
    // Verify's record cannot be written.
    let record = r.dir.join(".braid/verify");
    std::fs::create_dir(&record).unwrap();
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(record.join("probe"), "").is_ok() {
        // Modes do not hold here, as for root in a container: nothing to
        // test.
        return;
    }
    let out = r.verify(&["--revert", "--reruns", "0"], 1);
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out["outcome"], "error");
    assert_eq!(out["culprit"], ids[1].as_str());
    let revert = id(&out["revert"]);
    assert_eq!(out["land"]["outcome"], "rebased");
    let head = r.head().unwrap();
    assert_eq!(out["land"]["head"], head.as_str());
    assert_ne!(head, ids[3]);
    assert!(!revert.is_empty());
    // The branch says verify made it, so the next pass will not undo it.
    let shown = run(&r.dir, &["show", "main"], 0);
    assert_eq!(shown["agent"], "braid-verify");
}

/// Starts braid's own watchdog for `dir`, as verify does, and reads its
/// handshake.
#[cfg(unix)]
fn start_watchdog(dir: &Path) -> std::process::Child {
    use std::io::{BufRead, BufReader};
    let mut watchdog = Command::new(env!("CARGO_BIN_EXE_braid"))
        .arg("__verify-watchdog")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(watchdog.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "braid verify watchdog 2 ready\n");
    watchdog
}

#[cfg(unix)]
#[test]
fn the_watchdog_stops_the_group_and_removes_the_copy_unless_told_done() {
    use std::io::Write;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    // Directories named as verify names its copies, where the watchdog
    // looks for them.
    let base = std::env::temp_dir();
    let dir = |n: u32| {
        let dir = base.join(format!("braid-verify-test-{}-{n}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), "x").unwrap();
        dir
    };
    // A process of this test's own, in a group of its own, as a test run is.
    let sleeper = || {
        Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap()
    };

    // Braid goes away: its end of the watchdog's input closes.
    let gone = dir(1);
    let mut test = sleeper();
    let mut watchdog = start_watchdog(&gone);
    let mut input = watchdog.stdin.take().unwrap();
    writeln!(input, "group {}", test.id()).unwrap();
    drop(input);
    watchdog.wait().unwrap();
    assert_eq!(test.wait().unwrap().signal(), Some(9));
    assert!(!gone.exists());

    // The run ended and braid said so: nothing is touched.
    let kept = dir(2);
    let mut test = sleeper();
    let mut watchdog = start_watchdog(&kept);
    let mut input = watchdog.stdin.take().unwrap();
    writeln!(input, "group {}\ndone", test.id()).unwrap();
    drop(input);
    watchdog.wait().unwrap();
    assert!(test.try_wait().unwrap().is_none());
    assert!(kept.join("file").is_file());
    // This test's own process, by its id.
    test.kill().unwrap();
    test.wait().unwrap();
    std::fs::remove_dir_all(&kept).unwrap();
}

#[test]
fn acting_as_braid_verify_is_warned_about() {
    let r = Repo::new();
    let warned = |args: &[&str], envs: &[(&str, &str)]| {
        let out = braid_with(&r.dir, args, envs);
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).contains("warning: the agent braid-verify")
    };
    assert!(warned(&["--agent", "braid-verify", "refs"], &[]));
    assert!(warned(&["refs"], &[("BRAID_AGENT", "braid-verify")]));
    assert!(!warned(&["refs"], &[]));
}
