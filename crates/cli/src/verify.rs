// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `verify`: run a project's tests on a branch's head, keep a green ref at
//! the newest changeset that passed them, and when they fail, find the
//! changeset that broke them and, if asked, revert it.
//!
//! One pass per invocation. The tests run in a fresh copy of the
//! changeset's files outside the repository, with no shell, and with the
//! hub's credentials taken out of their environment. The green ref,
//! `braid/green/<branch>`, is in braid's own namespace, so it is never
//! exported to git or sent to a hub; it moves with a compare-and-swap, in
//! an operation of its own. The reverts verify landed are recorded in
//! `.braid/verify/<branch>.json`, which is what it trusts to tell its own
//! reverts, never the `reverts` field a changeset carries.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use braid_format::ObjectId;
use braid_repo::Repo;
use braid_store::RefUpdate;
use braid_wc::RevertRequest;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::{self, link::Link};
use crate::revert::{branch_or_main, land_revert};
use crate::work::{IdentityArgs, Report, hex, identity, print_conflicts, short};
use crate::{Cli, open_repo, print_json, resolve_changeset};

/// The agent label of everything verify records: the green ref's moves,
/// and the reverts it makes and lands.
pub(crate) const AGENT: &str = "braid-verify";

/// Exit status of a pass that leaves the branch red: the tests fail on its
/// head, whether or not the culprit was found or reverted.
const RED: u8 = 4;

/// The exit status of a revert that conflicts, as for every conflict.
const CONFLICTS: u8 = crate::work::CONFLICTS;

/// The green ref of `branch`: the newest changeset of it that passed.
pub(crate) fn green_ref(branch: &str) -> String {
    format!("braid/green/{branch}")
}

/// The long help of `verify`.
pub(crate) const VERIFY_HELP: &str =
    "Run the project's tests on the head of a branch, once, and keep the
branch green: braid/green/<branch> points at the newest changeset of the
branch that passed. When the tests fail, find the changeset that broke
them, and with --revert, revert it.

The command comes after `--` and runs as given, with no shell: write
`-- sh -c '...'` for one. It runs in a fresh copy of the changeset's files,
in a new directory under the system's temporary directory: outside the
repository, with no .braid file and no .git, so each run starts from
scratch and nothing it writes stays. Its output goes to standard error. It
gets BRAID_VERIFY_CHANGESET (the changeset's id) and BRAID_VERIFY_BRANCH,
and neither BRAID_TOKEN nor any BRAID_HUB variable. A run fails when the
command exits with a status other than 0, is killed, or runs longer than
--timeout (10m by default; s, m and h are the units), which kills it and
every process it started.

  - The head is the green changeset already: pass, and nothing runs.
  - The tests pass: the green ref moves to the head, in an operation of
    its own by braid-verify. A run that failed and passed when run again
    (--reruns, 1 by default) is flaky, and moves it too.
  - They fail, again on every rerun: verify bisects the changesets after
    the green one along first parents, the head's changes and its
    parents', to find the first that fails. With no green ref yet it does
    not bisect unless --from names a changeset to take as passing.
    Midpoints run as they are: an earlier revert in the range is not
    undone first.
  - With --revert, the culprit is reverted and the revert landed, as
    `braid revert` does. Verify does not revert a merge, nor a revert it
    made itself (it keeps those in .braid/verify/<branch>.json), nor more
    than --max-reverts-per-hour (3 by default). The green ref does not move:
    the next run tests the new head.

--dry-run runs the tests but moves no ref, records nothing and lands
nothing. In a repository linked to a hub, verify fetches first and lands a
revert on the hub; --local, or BRAID_HUB=off, keeps it here. Run one verify
per branch at a time.

With --json it prints branch; tip, the head it tested; green_before and
green, the green ref before and after; outcome, one of pass, flaky, red,
reverted, revert_conflicts, refused, no_culprit or error; culprit, the
first failing changeset, or null; revert, the revert made, or null;
tests_run, how many times the command ran; reason, why, in words, or null;
dry_run; and land, the landing report of the revert as `braid land --help`
describes it, or null.

Exit status: 0 for pass and flaky; 4 for red, reverted, refused and
no_culprit; 2 for revert_conflicts; 1 for an error, with the report
printed when the tests had started.";

#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    /// The branch to verify (default: main, master, or the repository's
    /// only branch).
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Revert the changeset that broke the tests, and land the revert.
    #[arg(long)]
    revert: bool,
    /// Run the tests, but move no ref, record nothing and land nothing.
    #[arg(long)]
    dry_run: bool,
    /// The most reverts verify lands on the branch in an hour.
    #[arg(long, value_name = "N", default_value_t = 3)]
    max_reverts_per_hour: usize,
    /// How many times to run a failing command again before it counts as
    /// failing.
    #[arg(long, value_name = "N", default_value_t = 1)]
    reruns: u32,
    /// How long one run may take: a number with s, m or h.
    #[arg(long, value_name = "DURATION", default_value = "10m", value_parser = parse_duration)]
    timeout: Duration,
    /// A changeset of the branch to take as passing, to bisect from instead
    /// of the green one.
    #[arg(long, value_name = "REV")]
    from: Option<String>,
    #[command(flatten)]
    identity: IdentityArgs,
    /// Stay in this repository even if it is linked to a hub.
    #[arg(long)]
    local: bool,
    /// The test command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

/// A duration: a whole number followed by `s`, `m` or `h`, or a number of
/// seconds; never zero.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("{text:?} is not a duration such as 30s, 10m or 1h")),
    };
    match digits.parse::<u64>() {
        Ok(n) if n > 0 => n
            .checked_mul(scale)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("{text:?} is too long")),
        _ => Err(format!("{text:?} is not a duration such as 30s, 10m or 1h")),
    }
}

/// What one pass found.
#[derive(Default)]
struct Verdict {
    outcome: &'static str,
    green: Option<ObjectId>,
    culprit: Option<ObjectId>,
    revert: Option<ObjectId>,
    reason: Option<String>,
    land: Option<Report>,
}

impl Verdict {
    fn exit_code(&self) -> ExitCode {
        match self.outcome {
            "pass" | "flaky" => ExitCode::SUCCESS,
            "revert_conflicts" => ExitCode::from(CONFLICTS),
            "error" => ExitCode::FAILURE,
            _ => ExitCode::from(RED),
        }
    }
}

/// The reverts verify landed on one branch, as `.braid/verify/<branch>.json`
/// keeps them.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    reverts: Vec<Reverted>,
}

/// One revert verify landed.
#[derive(Debug, Serialize, Deserialize)]
struct Reverted {
    /// The changeset it reverted, in hex.
    culprit: String,
    /// The revert as verify made it, in hex.
    revert: String,
    /// The changeset that holds it on the branch: itself, or the rebased
    /// one.
    landed: Option<String>,
    /// When it landed, in seconds since the Unix epoch.
    time: u64,
}

impl Record {
    /// The file of `branch`. A branch name may hold `/` and other bytes a
    /// file name should not, so all but letters, digits, `.`, `_` and `-`
    /// are written as `%XX`.
    fn path(repo: &Repo, branch: &str) -> PathBuf {
        let mut name = String::new();
        for byte in branch.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                name.push(byte as char);
            } else {
                name.push_str(&format!("%{byte:02X}"));
            }
        }
        repo.root()
            .join(braid_repo::DIR)
            .join("verify")
            .join(format!("{name}.json"))
    }

    fn load(path: &Path) -> Result<Record> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not verify's record", path.display())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Record::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Writes the record whole, through a scratch file and a rename.
    fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("the record has no directory")?;
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let scratch = path.with_extension("json.new");
        let mut text = serde_json::to_vec_pretty(self)?;
        text.push(b'\n');
        fs::write(&scratch, text).with_context(|| format!("cannot write {}", scratch.display()))?;
        fs::rename(&scratch, path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    /// Whether verify made `id`: a revert of its own, as made or landed.
    fn made(&self, id: ObjectId) -> bool {
        let hex = id.to_hex();
        self.reverts
            .iter()
            .any(|r| r.revert == hex || r.landed.as_deref() == Some(hex.as_str()))
    }

    /// How many reverts landed in the hour before `now`.
    fn in_last_hour(&self, now: u64) -> usize {
        self.reverts.iter().filter(|r| r.time + 3600 > now).count()
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Runs the test command on changesets, counting the runs.
struct Tester<'a> {
    repo: &'a Repo,
    branch: &'a str,
    command: &'a [OsString],
    timeout: Duration,
    reruns: u32,
    runs: usize,
}

/// How one run ended.
enum RunEnd {
    Exited(ExitStatus),
    TimedOut,
}

impl Tester<'_> {
    /// Whether the tests pass on `changeset`: the first run, or one of the
    /// reruns after it fails. Returns whether they passed, and whether a
    /// run failed before one passed.
    fn passes(&mut self, changeset: ObjectId) -> Result<(bool, bool)> {
        for attempt in 0..=self.reruns {
            if self.run(changeset)? {
                return Ok((true, attempt > 0));
            }
        }
        Ok((false, false))
    }

    /// Runs the command once on a fresh copy of `changeset`.
    fn run(&mut self, changeset: ObjectId) -> Result<bool> {
        let dir = scratch_dir()?;
        let result = self.run_in(changeset, &dir);
        // Whatever the command left goes too.
        let _ = fs::remove_dir_all(&dir);
        // Only a command that ran counts.
        let end = result?;
        self.runs += 1;
        let passed = matches!(&end, RunEnd::Exited(status) if status.success());
        let said = match &end {
            RunEnd::Exited(status) if status.success() => "passed".to_owned(),
            RunEnd::Exited(status) => format!("failed ({status})"),
            RunEnd::TimedOut => format!("timed out after {:?} and was killed", self.timeout),
        };
        eprintln!("braid verify: {} {said}", changeset.short());
        Ok(passed)
    }

    fn run_in(&self, changeset: ObjectId, dir: &Path) -> Result<RunEnd> {
        braid_wc::materialize(self.repo.store(), changeset, dir)
            .with_context(|| format!("cannot write changeset {} to test it", changeset.short()))?;
        let (program, args) = self.command.split_first().context("no command to run")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(to_stderr()?)
            .stderr(to_stderr()?)
            .env("BRAID_VERIFY_CHANGESET", changeset.to_hex())
            .env("BRAID_VERIFY_BRANCH", self.branch);
        for (name, _) in std::env::vars_os() {
            if is_credential(&name) {
                command.env_remove(&name);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A group of its own, so that a timeout kills everything it
            // started.
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(RunEnd::Exited(status));
            }
            let now = Instant::now();
            if now >= deadline {
                kill(&mut child);
                let _ = child.wait();
                return Ok(RunEnd::TimedOut);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }
}

/// Whether an environment variable is one of the hub's credentials or
/// settings, which the tests never see: `BRAID_TOKEN` and every
/// `BRAID_HUB…`.
fn is_credential(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == "BRAID_TOKEN" || name.starts_with("BRAID_HUB")
}

/// Kills the command and every process in its group.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = rustix::process::Pid::from_child(child);
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}

/// Standard error, for the command's output, so that standard output keeps
/// only the report.
fn to_stderr() -> Result<Stdio> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(Stdio::from(io::stderr().as_handle().try_clone_to_owned()?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(Stdio::inherit())
    }
}

/// A new, empty directory under the system's temporary directory, readable
/// only by this user where the system has modes.
fn scratch_dir() -> Result<PathBuf> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let base = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "braid-verify-{}-{nanos:08x}-{n}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && n < 1000 => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("cannot create {}", dir.display()));
            }
        }
    }
}

/// The changesets after `good` up to `tip`, along first parents, oldest
/// first, or `None` when `good` is not on the chain.
fn range(repo: &Repo, tip: ObjectId, good: ObjectId) -> Result<Option<Vec<ObjectId>>> {
    let mut chain = Vec::new();
    let mut current = tip;
    while current != good {
        chain.push(current);
        match braid_engine::read_changeset(repo.store(), current)?
            .parents
            .first()
        {
            Some(parent) => current = *parent,
            None => return Ok(None),
        }
    }
    chain.reverse();
    Ok(Some(chain))
}

/// Everything one pass needs.
struct Pass<'a> {
    args: &'a VerifyArgs,
    repo: &'a Repo,
    link: Option<Link>,
    branch: &'a str,
    tip: ObjectId,
    green_before: Option<ObjectId>,
}

impl Pass<'_> {
    fn run(&self, tester: &mut Tester<'_>) -> Result<Verdict> {
        let mut verdict = Verdict {
            green: self.green_before,
            ..Verdict::default()
        };
        if self.green_before == Some(self.tip) {
            verdict.outcome = "pass";
            verdict.reason = Some("the head is the green changeset already".to_owned());
            return Ok(verdict);
        }
        let (passed, flaky) = tester.passes(self.tip)?;
        if passed {
            verdict.outcome = if flaky { "flaky" } else { "pass" };
            if flaky {
                verdict.reason = Some("a run failed, and a rerun passed".to_owned());
            }
            if self.args.dry_run {
                let moved = format!("dry run: the green ref would move to {}", self.tip.short());
                verdict.reason = Some(match verdict.reason.take() {
                    Some(reason) => format!("{reason}; {moved}"),
                    None => moved,
                });
            } else {
                self.move_green()?;
                verdict.green = Some(self.tip);
            }
            return Ok(verdict);
        }

        verdict.outcome = "red";
        let good = match &self.args.from {
            Some(revision) => Some(resolve_changeset(self.repo, revision)?),
            None => self.green_before,
        };
        let Some(good) = good else {
            verdict.reason = Some(
                "the tests fail on the head, and with no green changeset there is nothing to bisect from: --from names one"
                    .to_owned(),
            );
            return Ok(verdict);
        };
        let Some(range) = range(self.repo, self.tip, good)? else {
            verdict.reason = Some(format!(
                "{} is not on {}'s first-parent chain: there is nothing to bisect",
                good.short(),
                self.branch
            ));
            return Ok(verdict);
        };
        if range.is_empty() {
            verdict.outcome = "no_culprit";
            verdict.reason = Some(format!("the head is {}, which was to pass", good.short()));
            return Ok(verdict);
        }
        // `range[lo]` passes (or is `good`, at -1), `range[hi]` fails.
        let (mut lo, mut hi) = (-1_isize, range.len() as isize - 1);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if tester.passes(range[mid as usize])?.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let culprit = range[hi as usize];
        if lo < 0 && !tester.passes(good)?.0 {
            // The changeset taken as passing fails now too: something
            // outside the range broke the tests.
            verdict.outcome = "no_culprit";
            verdict.reason = Some(format!(
                "{} fails too, though it was to pass: the failure is not in the changesets after it",
                good.short()
            ));
            return Ok(verdict);
        }
        verdict.culprit = Some(culprit);
        self.revert(culprit, verdict)
    }

    /// Moves the green ref from where it was to the tip, unless someone
    /// moved it meanwhile.
    fn move_green(&self) -> Result<()> {
        let name = green_ref(self.branch);
        let description = format!("verify: {name} at {}", self.tip.short());
        self.repo
            .store()
            .transact(AGENT, &description, |refs| {
                let current = refs.get(&name);
                if current != self.green_before {
                    return Err(format!(
                        "{name} moved while verify ran, to {}: is another verify running?",
                        short(current)
                    ));
                }
                Ok(vec![RefUpdate::new(
                    name.clone(),
                    self.green_before,
                    Some(self.tip),
                )])
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Decides whether to revert `culprit`, and does.
    fn revert(&self, culprit: ObjectId, mut verdict: Verdict) -> Result<Verdict> {
        let first = format!(
            "the tests fail from {}, the first changeset that fails",
            culprit.short()
        );
        if !self.args.revert {
            verdict.reason = Some(format!("{first}; --revert would revert it"));
            return Ok(verdict);
        }
        let parents = braid_engine::read_changeset(self.repo.store(), culprit)?
            .parents
            .len();
        let path = Record::path(self.repo, self.branch);
        let mut record = Record::load(&path)?;
        let now = now_seconds();
        let refusal = if parents > 1 {
            Some(format!(
                "{first}, and it is a merge: verify does not revert merges"
            ))
        } else if record.made(culprit) {
            Some(format!(
                "{first}, and it is a revert verify made: verify does not revert its own reverts"
            ))
        } else if record.in_last_hour(now) >= self.args.max_reverts_per_hour {
            Some(format!(
                "{first}, but verify landed {} on {} in the last hour, the most --max-reverts-per-hour allows",
                crate::work::count(record.in_last_hour(now), "revert"),
                self.branch
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            verdict.outcome = "refused";
            verdict.reason = Some(reason);
            return Ok(verdict);
        }
        if self.args.dry_run {
            verdict.reason = Some(format!("{first}; dry run: verify would revert it"));
            return Ok(verdict);
        }
        let author = identity(&self.args.identity, self.repo)?;
        let change = braid_wc::create_revert(
            self.repo.store(),
            &RevertRequest {
                culprit,
                allow_merge: false,
                author: author.clone(),
                agent: AGENT.to_owned(),
            },
        )?;
        verdict.revert = Some(change);
        let mut report = land_revert(
            self.repo,
            self.link.as_ref(),
            self.branch,
            change,
            AGENT,
            &author,
        )?;
        match report.outcome.as_str() {
            "fast-forward" | "rebased" => {
                record.reverts.push(Reverted {
                    culprit: culprit.to_hex(),
                    revert: change.to_hex(),
                    landed: report.head.map(|id| id.to_hex()),
                    time: now,
                });
                record.save(&path)?;
                verdict.outcome = "reverted";
                verdict.reason = Some(format!("{first}; its revert landed on {}", self.branch));
            }
            "conflicts" => {
                verdict.outcome = "revert_conflicts";
                verdict.reason = Some(format!(
                    "{first}; its revert conflicts with what landed since, so {} did not move: revert it by hand in a workspace",
                    self.branch
                ));
                report.next = None;
            }
            "already-applied" => {
                verdict.outcome = "refused";
                verdict.reason = Some(format!(
                    "{first}, but {} has what its revert would change already",
                    self.branch
                ));
            }
            other => bail!("the revert's landing ended with {other}"),
        }
        verdict.land = Some(report);
        Ok(verdict)
    }
}

pub(crate) fn verify(cli: &Cli, args: &VerifyArgs) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let branch = branch_or_main(&repo, args.branch.as_deref())?;
    if !braid_repo::is_branch(&branch) {
        bail!("{branch:?} is in one of braid's own namespaces, not a branch");
    }
    let link = hub::linked(&repo, args.local)?;
    if let Some(link) = &link
        && !args.dry_run
    {
        let hub = hub::connect(link)?;
        let remote = hub.repo(link.repo.clone());
        hub::fetch::fetch(&repo, &remote, link, AGENT, None).context(
            "cannot fetch from the hub before verifying (--local verifies the local branch as it is)",
        )?;
    }
    let refs = repo.store().refs()?;
    let Some(tip) = refs.get(&branch) else {
        bail!("there is no branch {branch:?} to verify");
    };
    let green_before = refs.get(&green_ref(&branch));
    let pass = Pass {
        args,
        repo: &repo,
        link,
        branch: &branch,
        tip,
        green_before,
    };
    let mut tester = Tester {
        repo: &repo,
        branch: &branch,
        command: &args.command,
        timeout: args.timeout,
        reruns: args.reruns,
        runs: 0,
    };
    let verdict = match pass.run(&mut tester) {
        Ok(verdict) => verdict,
        // Before anything ran, an error is only an error.
        Err(err) if tester.runs == 0 => return Err(err),
        Err(err) => {
            eprintln!("braid: error: {err:#}");
            Verdict {
                outcome: "error",
                green: green_before,
                reason: Some(format!("{err:#}")),
                ..Verdict::default()
            }
        }
    };
    if cli.json {
        print_json(&json!({
            "branch": branch,
            "tip": tip.to_hex(),
            "green_before": hex(green_before),
            "green": hex(verdict.green),
            "outcome": verdict.outcome,
            "culprit": hex(verdict.culprit),
            "revert": hex(verdict.revert),
            "tests_run": tester.runs,
            "reason": verdict.reason,
            "dry_run": args.dry_run,
            "land": verdict.land.as_ref().map_or(Value::Null, Report::json),
        }))?;
    } else {
        print_text(&branch, tip, &verdict, tester.runs);
    }
    Ok(verdict.exit_code())
}

fn print_text(branch: &str, tip: ObjectId, verdict: &Verdict, runs: usize) {
    let ran = crate::work::count(runs, "run");
    match verdict.outcome {
        "pass" | "flaky" => println!(
            "{branch} is green at {} ({}, {ran})",
            tip.short(),
            verdict.outcome
        ),
        outcome => println!("{branch} at {}: {outcome} ({ran})", tip.short()),
    }
    if let Some(reason) = &verdict.reason {
        println!("{reason}");
    }
    if let Some(report) = &verdict.land
        && report.conflicted()
    {
        print_conflicts(&report.conflicts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("45"), Ok(Duration::from_secs(45)));
        for bad in ["", "0s", "m", "10x", "1.5m", "-1s", "10 m"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn credentials_are_recognised() {
        for name in [
            "BRAID_TOKEN",
            "BRAID_HUB",
            "BRAID_HUB_URL",
            "BRAID_HUB_MAX_BODY",
        ] {
            assert!(is_credential(name.as_ref()), "{name}");
        }
        for name in ["BRAID_AGENT", "BRAID_AUTHOR", "PATH", "BRAID_TOKENS"] {
            assert!(!is_credential(name.as_ref()), "{name}");
        }
    }

    #[test]
    fn record_paths_escape_branch_names() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        let path = Record::path(&repo, "feature/x y");
        assert_eq!(path.file_name().unwrap(), "feature%2Fx%20y.json");
        assert_eq!(
            Record::path(&repo, "main").file_name().unwrap(),
            "main.json"
        );
        let mut record = Record::default();
        record.reverts.push(Reverted {
            culprit: "c".repeat(64),
            revert: "a".repeat(64),
            landed: Some("b".repeat(64)),
            time: 10_000,
        });
        let landed: ObjectId = "b".repeat(64).parse().unwrap();
        assert!(record.made(landed));
        assert_eq!(record.in_last_hour(10_000 + 3599), 1);
        assert_eq!(record.in_last_hour(10_000 + 3600), 0);
        record.save(&path).unwrap();
        assert_eq!(Record::load(&path).unwrap().reverts.len(), 1);
    }
}
