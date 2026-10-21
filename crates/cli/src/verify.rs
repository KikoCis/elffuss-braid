// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `verify`: run a project's tests on a branch's head, keep a green ref at
//! the newest changeset that passed them, and when they fail, find the
//! changeset that broke them and, if asked, revert it.
//!
//! One pass per invocation. The tests run in a fresh copy of the
//! changeset's files outside the repository, with no shell, in a process
//! group of their own that a watchdog process kills if braid goes away. The
//! green ref, `braid/green/<branch>`, is in braid's own namespace, so it is
//! never exported to git or sent to a hub: it is local to the repository.
//! It moves with a compare-and-swap, in an operation of its own.
//!
//! What verify must not do again it reads from the branch itself, so that a
//! fresh clone, as CI has, follows the same rules: a revert verify made
//! carries a `reverts` field and the agent `braid-verify`, and verify never
//! reverts such a changeset, nor lands more reverts an hour than it is
//! allowed, counting them by their own committer time. A label anyone can
//! write is safe to refuse on. `.braid/verify/` keeps a record of the
//! reverts this repository landed, as a second source for both rules.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use braid_engine::{AGENT as AGENT_FIELD, REVERTS};
use braid_format::{Changeset, ObjectId};
use braid_repo::Repo;
use braid_store::RefUpdate;
use braid_wc::RevertRequest;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::{self, link::Link};
use crate::revert::{branch_or_main, land_revert};
use crate::work::{IdentityArgs, Report, count, hex, identity, print_conflicts, short};
use crate::{Cli, open_repo, print_json, resolve_changeset};

/// The agent label of everything verify records: the green ref's moves,
/// and the reverts it makes and lands.
pub(crate) const AGENT: &str = "braid-verify";

/// What braid says when someone else acts as [`AGENT`].
pub(crate) const AGENT_WARNING: &str = "the agent braid-verify (--agent or BRAID_AGENT) is the name braid verify records its own work under: verify takes a revert made under that name for one of its own, never reverts it, and counts it against --max-reverts-per-hour";

/// The hidden subcommand that watches a test run for braid going away.
pub(crate) const WATCHDOG: &str = "__verify-watchdog";

/// Exit status of a pass that leaves the branch red: the tests fail on its
/// head, whether or not the culprit was found or reverted.
const RED: u8 = 4;

/// The exit status of a revert that conflicts, as for every conflict.
const CONFLICTS: u8 = crate::work::CONFLICTS;

/// The prefix of the scratch directories the tests run in.
const SCRATCH_PREFIX: &str = "braid-verify-";

/// How many changesets down the branch verify looks for its own reverts of
/// the last hour, at most.
const HISTORY_DEPTH: usize = 10_000;

/// The longest branch name in hex that a record file is named with; a
/// longer name is hashed instead. With `.json`, and the `.new` of the
/// scratch file the record is written through, a file name stays under 143
/// bytes, the most some encrypted file systems take.
const MAX_HEX_NAME: usize = 128;

/// What the watchdog says once it runs, before braid starts any test. The
/// number changes with the protocol, so that another braid's watchdog is
/// never taken for this one's.
const READY: &[u8] = b"braid verify watchdog 2 ready\n";

/// How long braid waits for the watchdog to say it runs.
#[cfg(unix)]
const READY_WAIT: Duration = Duration::from_secs(30);

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
in a new directory under the system's temporary directory, outside the
repository and with no .braid file and no .git, so each run starts from
scratch; the directory is removed after the run. Its output goes to
standard error. It gets BRAID_VERIFY_CHANGESET (the changeset's id),
BRAID_VERIFY_BRANCH and PWD (its directory); BRAID_TOKEN, every variable
whose name starts with BRAID_HUB, and OLDPWD are taken out.

That is all the isolation there is: the command runs as you, with your
access to every file, the network, the braid command, and any hub login
saved in braid's configuration directory. Run verify in a sandbox, a
container or as a separate user when the code it tests is not trusted.

A run fails when the command exits with a status other than 0, is killed,
or runs longer than --timeout (10m by default; s, m and h are the units).
On Unix the command runs in a process group of its own: when it exits or
times out, the group is killed, and so are processes it started that are
still in it (one that left the group, as a daemon does, is not). A watchdog
process, started before the command, kills the group and removes the
directory if braid is killed or exits while the tests run; when the
watchdog cannot start, or stops, the run fails. Elsewhere (Windows is not
verified) only the command itself is killed, and nothing watches braid. In
a container, run braid under an init such as tini (docker run --init), not
as PID 1, which gets no default signal handling and must reap the
processes others leave behind.

  - The head is the green changeset already: pass, and nothing runs.
  - The tests pass: the green ref moves to the head, in an operation of
    its own by braid-verify. A run that failed and passed when run again
    (--reruns, 1 by default) is flaky: the green ref stays, unless
    --flaky-moves-green.
  - They fail, again on every rerun: verify bisects the changesets after
    the green one along first parents, the head's changes and its
    parents', to find the first that fails. With no green ref yet it does
    not bisect unless --from names a changeset to take as passing. A
    changeset whose files cannot be written on this system is skipped, and
    listed in skipped. When the first failing changeset was reverted later
    in the range, verify tests the revert and, if it passes, goes on
    bisecting after it. Otherwise midpoints run as they are.
  - With --revert, the culprit is reverted and the revert landed, as
    `braid revert` does. Verify does not revert a merge, nor a revert it
    made (one with a reverts field and the agent braid-verify), nor more
    than --max-reverts-per-hour (3 by default), which it counts by each
    revert's own committer time among the last 10,000 changesets of the
    branch, whatever the dates of those between them, and in
    .braid/verify/. The green ref does not move: the next run tests the new
    head.

The green ref is local: a fresh clone has none, so give --from there.
--dry-run runs the tests but moves no ref, records nothing, lands nothing
and does not fetch. In a repository linked to a hub, verify fetches first
and lands a revert on the hub; --local, or BRAID_HUB=off, keeps it here.
Run one verify per branch at a time.

With --json it prints branch; tip, the head it tested; green_before and
green, the green ref before and after; outcome, one of pass, flaky, red,
reverted, revert_conflicts, refused, no_culprit or error; culprit, the
first failing changeset, or null; revert, the revert made, or null;
skipped, the changesets that could not be tested; tests_run, how many
times the command started; reason, why, in words, or null; dry_run; and
land, the landing report of the revert as `braid land --help` describes
it, or null.

Exit status: 0 for pass and flaky; 4 for red, reverted, refused and
no_culprit; 2 for revert_conflicts; 1 for an error. Once the tests have
started, or a revert was made, an error prints the report too, with what
was done until then.";

#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    /// The branch to verify (default: main, master, or the repository's
    /// only branch).
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Revert the changeset that broke the tests, and land the revert.
    #[arg(long)]
    revert: bool,
    /// Run the tests, but move no ref, record nothing, land nothing and
    /// fetch nothing.
    #[arg(long)]
    dry_run: bool,
    /// The most reverts verify lands on the branch in an hour.
    #[arg(long, value_name = "N", default_value_t = 3)]
    max_reverts_per_hour: usize,
    /// How many times to run a failing command again before it counts as
    /// failing.
    #[arg(long, value_name = "N", default_value_t = 1)]
    reruns: u32,
    /// Move the green ref when the tests passed only on a rerun.
    #[arg(long)]
    flaky_moves_green: bool,
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

/// What one pass found, filled in as it goes, so that an error keeps what
/// was done before it.
#[derive(Default)]
struct Verdict {
    outcome: &'static str,
    green: Option<ObjectId>,
    culprit: Option<ObjectId>,
    revert: Option<ObjectId>,
    skipped: Vec<ObjectId>,
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

    fn red(&mut self, outcome: &'static str, reason: String) {
        self.outcome = outcome;
        self.reason = Some(reason);
    }
}

/// The reverts verify landed on one branch from this repository, as
/// `.braid/verify/<branch in hex>.json` keeps them.
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
    /// one; null when the landing's answer was lost.
    landed: Option<String>,
    /// When it landed, in seconds since the Unix epoch.
    time: u64,
}

impl Record {
    /// The file of `branch`: its name in hex, so that no two branches share
    /// a file, even on a file system that ignores case, or for a name whose
    /// hex would be longer than [`MAX_HEX_NAME`], `b3-` and the BLAKE3 hash
    /// of the name, which no name in hex can look like.
    fn path(repo: &Repo, branch: &str) -> PathBuf {
        let hex: String = branch.bytes().map(|byte| format!("{byte:02x}")).collect();
        let name = if hex.len() <= MAX_HEX_NAME {
            hex
        } else {
            format!("b3-{}", blake3::hash(branch.as_bytes()).to_hex())
        };
        repo.root()
            .join(braid_repo::DIR)
            .join("verify")
            .join(format!("{name}.json"))
    }

    fn load(path: &Path) -> Result<Record> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is not verify's record of its reverts; delete it to start a new one (verify also finds its reverts on the branch)",
                    path.display()
                )
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Record::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Writes the record whole, through a scratch file that reaches the
    /// disk before it is renamed into place.
    fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("the record has no directory")?;
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let scratch = path.with_extension("json.new");
        let mut text = serde_json::to_vec_pretty(self)?;
        text.push(b'\n');
        File::create(&scratch)
            .and_then(|mut file| {
                file.write_all(&text)?;
                file.sync_all()
            })
            .with_context(|| format!("cannot write {}", scratch.display()))?;
        fs::rename(&scratch, path).with_context(|| format!("cannot write {}", path.display()))?;
        #[cfg(unix)]
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Whether this repository's verify made `id`: a revert, as made or
    /// landed.
    fn made(&self, id: ObjectId) -> bool {
        let hex = id.to_hex();
        self.reverts
            .iter()
            .any(|r| r.revert == hex || r.landed.as_deref() == Some(hex.as_str()))
    }

    /// The reverts landed in the hour before `now`, by the id that holds
    /// each on the branch.
    fn in_last_hour(&self, now: u64) -> HashSet<String> {
        self.reverts
            .iter()
            .filter(|r| r.time + 3600 > now)
            .map(|r| r.landed.clone().unwrap_or_else(|| r.revert.clone()))
            .collect()
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The value of an extra field of `changeset`.
fn extra<'c>(changeset: &'c Changeset, key: &[u8]) -> Option<&'c [u8]> {
    changeset
        .extras
        .iter()
        .find(|(k, _)| k.as_slice() == key)
        .map(|(_, value)| value.as_slice())
}

/// Whether `changeset` says it is a revert verify made: a `reverts` field,
/// and `braid-verify` as its agent. Anyone can write both, which is why it
/// is only ever a reason to refuse.
fn is_verify_revert(changeset: &Changeset) -> bool {
    extra(changeset, REVERTS).is_some() && extra(changeset, AGENT_FIELD) == Some(AGENT.as_bytes())
}

/// The reverts verify made among the `depth` changesets down first parents
/// from `tip`, that were committed in the hour before `now`, each by its own
/// committer time. Dates are not in order along a branch (a changeset that
/// landed by a fast-forward keeps the date it was made with, and a git
/// commit can carry any date), so an old one stops nothing: only the depth
/// bounds the walk.
fn reverts_in_last_hour(
    repo: &Repo,
    tip: ObjectId,
    now: u64,
    depth: usize,
) -> Result<HashSet<String>> {
    let hour_ago = i64::try_from(now).unwrap_or(i64::MAX).saturating_sub(3600);
    let mut found = HashSet::new();
    let mut current = Some(tip);
    for _ in 0..depth {
        let Some(id) = current else { break };
        let changeset = braid_engine::read_changeset(repo.store(), id)?;
        if changeset.committer.seconds > hour_ago && is_verify_revert(&changeset) {
            found.insert(id.to_hex());
        }
        current = changeset.parents.first().copied();
    }
    Ok(found)
}

/// How a changeset fared.
enum Check {
    /// The tests passed: `flaky` when a run failed first.
    Passed { flaky: bool },
    /// Every run failed.
    Failed,
    /// Its files cannot be written on this system, so it was not tested.
    Skipped,
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
    /// reruns after it fails.
    fn check(&mut self, changeset: ObjectId) -> Result<Check> {
        for attempt in 0..=self.reruns {
            match self.run(changeset)? {
                None => return Ok(Check::Skipped),
                Some(true) => return Ok(Check::Passed { flaky: attempt > 0 }),
                Some(false) => {}
            }
        }
        Ok(Check::Failed)
    }

    /// Runs the command once on a fresh copy of `changeset`: whether it
    /// passed, or `None` when the files cannot be written here.
    fn run(&mut self, changeset: ObjectId) -> Result<Option<bool>> {
        let dir = scratch_dir()?;
        let result = self.run_in(changeset, &dir);
        // Gone already, unless the watchdog did not start.
        let _ = fs::remove_dir_all(&dir);
        let Some(end) = result? else {
            return Ok(None);
        };
        let passed = matches!(&end, RunEnd::Exited(status) if status.success());
        let said = match &end {
            RunEnd::Exited(status) if status.success() => "passed".to_owned(),
            RunEnd::Exited(status) => format!("failed ({status})"),
            RunEnd::TimedOut => format!("timed out after {:?} and was killed", self.timeout),
        };
        eprintln!("braid verify: {} {said}", changeset.short());
        Ok(Some(passed))
    }

    /// Runs the command in `dir` under a watchdog, started first: from then
    /// on, however braid ends, the directory goes and the tests stop.
    fn run_in(&mut self, changeset: ObjectId, dir: &Path) -> Result<Option<RunEnd>> {
        let mut watchdog = Watchdog::start(dir)?;
        let end = self.watched(changeset, dir, &mut watchdog);
        // Whatever the command left goes too, while the watchdog still
        // watches: braid ending now leaves nothing behind either.
        let _ = fs::remove_dir_all(dir);
        watchdog.done();
        end
    }

    fn watched(
        &mut self,
        changeset: ObjectId,
        dir: &Path,
        watchdog: &mut Watchdog,
    ) -> Result<Option<RunEnd>> {
        match braid_wc::materialize(self.repo.store(), changeset, dir) {
            Ok(_) => {}
            Err(err @ (braid_wc::Error::Unsafe { .. } | braid_wc::Error::Collision { .. })) => {
                eprintln!(
                    "braid verify: {} skipped: its files cannot be written here: {err}",
                    changeset.short()
                );
                return Ok(None);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("cannot write changeset {} to test it", changeset.short())
                });
            }
        }
        let (program, args) = self.command.split_first().context("no command to run")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(to_stderr()?)
            .stderr(to_stderr()?)
            .env("BRAID_VERIFY_CHANGESET", changeset.to_hex())
            .env("BRAID_VERIFY_BRANCH", self.branch)
            .env("PWD", dir)
            .env_remove("OLDPWD");
        for (name, _) in std::env::vars_os() {
            if is_credential(&name) {
                command.env_remove(&name);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A group of its own, so that what it started goes with it.
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
        // A run counts once the command started, so that one cut short
        // still shows in the report; a command that cannot start does not.
        self.runs += 1;
        if let Err(err) = watchdog.watch(&child) {
            kill_group(&mut child);
            let _ = child.wait();
            return Err(err);
        }
        self.wait(&mut child, watchdog).map(Some)
    }

    /// Waits for the command, at most until the timeout, then kills its
    /// group: whatever it left running, or all of it on a timeout. Should
    /// the watchdog stop meanwhile, or the wait fail, the group is killed
    /// too, and the run fails.
    fn wait(&self, child: &mut Child, watchdog: &mut Watchdog) -> Result<RunEnd> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let gone = match exited(child) {
                Ok(gone) => gone,
                Err(err) => {
                    kill_group(child);
                    let _ = child.wait();
                    return Err(err.context("cannot wait for the tests; they were stopped"));
                }
            };
            if gone {
                // The command is not reaped yet, so its group's id cannot
                // be anyone else's.
                kill_group(child);
                return Ok(RunEnd::Exited(child.wait()?));
            }
            if watchdog.stopped() {
                kill_group(child);
                let _ = child.wait();
                bail!("verify's watchdog stopped while the tests ran, so they were stopped too");
            }
            let now = Instant::now();
            if now >= deadline {
                kill_group(child);
                let _ = child.wait();
                return Ok(RunEnd::TimedOut);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }
}

/// Whether the command exited, without reaping it where the system can.
fn exited(child: &mut Child) -> Result<bool> {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        loop {
            match waitid(WaitId::Pid(Pid::from_child(child)), options) {
                Ok(found) => return Ok(found.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(io::Error::from(err).into()),
            }
        }
    }
    #[cfg(not(unix))]
    {
        Ok(child.try_wait()?.is_some())
    }
}

/// A process that kills a test run's group, and removes its directory,
/// if braid goes away while it runs. Braid starts it before the test, and
/// starts no test until it says [`READY`]; then tells it the test's process
/// group, a `group <id>` line on its standard input, and at the end `done`.
/// Its standard input closes however braid ends: without `done` before,
/// the watchdog kills the group and removes the directory. On systems other
/// than Unix there is none.
struct Watchdog {
    child: Option<Child>,
}

impl Watchdog {
    /// Starts the watchdog of a run in `dir`, from this very braid.
    fn start(dir: &Path) -> Result<Watchdog> {
        #[cfg(unix)]
        {
            let mut command = Command::new(own_exe()?);
            command.arg(WATCHDOG).arg(dir);
            Watchdog::spawn(command, READY_WAIT)
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
            Ok(Watchdog { child: None })
        }
    }

    /// Starts `command` as the watchdog, out of braid's process group, and
    /// waits up to `wait` for it to say [`READY`].
    #[cfg(unix)]
    fn spawn(mut command: Command, wait: Duration) -> Result<Watchdog> {
        use std::io::{BufReader, Read};
        use std::os::unix::process::CommandExt;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Out of braid's group, so that a Ctrl-C meant for braid does
            // not stop it too.
            .process_group(0)
            .spawn()
            .context("cannot start verify's watchdog, so the tests did not run")?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = child.stdout.take().map(|stdout| {
            std::thread::Builder::new()
                .name("verify watchdog".to_owned())
                .spawn(move || {
                    // No more than the line it must say: a program that says
                    // more is not this braid's watchdog anyway.
                    let mut line = Vec::new();
                    let read = BufReader::new(stdout.take(READY.len() as u64))
                        .read_until(b'\n', &mut line);
                    let _ = sender.send(read.map(|_| line));
                })
        });
        let said = match reader {
            Some(Ok(_)) => receiver.recv_timeout(wait),
            Some(Err(err)) => {
                kill_group(&mut child);
                let _ = child.wait();
                return Err(err)
                    .context("cannot listen to verify's watchdog, so the tests did not run");
            }
            None => Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        let why = match said {
            Ok(Ok(line)) if line == READY => return Ok(Watchdog { child: Some(child) }),
            Ok(Ok(line)) if line.is_empty() => "it ended without a word".to_owned(),
            Ok(Ok(line)) => format!("it said {:?}", String::from_utf8_lossy(&line)),
            Ok(Err(err)) => format!("it could not be read: {err}"),
            Err(_) => format!("it said nothing for {wait:?}"),
        };
        // It runs in a group of its own: whatever it started goes too.
        kill_group(&mut child);
        let _ = child.wait();
        bail!(
            "verify's watchdog did not start ({why}), so the tests did not run; is the braid command being replaced, or another braid in its place?"
        )
    }

    /// Tells the watchdog the process group of `test`, which runs in one of
    /// its own.
    fn watch(&mut self, test: &Child) -> Result<()> {
        let Some(child) = &mut self.child else {
            return Ok(());
        };
        let stdin = child
            .stdin
            .as_mut()
            .context("verify's watchdog cannot be told what to watch")?;
        writeln!(stdin, "group {}", test.id())
            .and_then(|()| stdin.flush())
            .context("verify's watchdog went away, so the tests were stopped")
    }

    /// Whether the watchdog stopped, which it should not do before `done`.
    fn stopped(&mut self) -> bool {
        match &mut self.child {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Tells the watchdog the run is over, and waits for it.
    fn done(mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"done\n");
            }
            let _ = child.wait();
        }
    }
}

/// The braid command to start the watchdog with: on Linux this very binary,
/// through `/proc/self/exe`, even if its file was replaced or removed since
/// it started; elsewhere the file it was started from; and where neither
/// can be found, as on Linux without `/proc`, the name it was started by,
/// looked up in `PATH`. The handshake turns away any other program.
#[cfg(unix)]
fn own_exe() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let this = PathBuf::from("/proc/self/exe");
        if this.exists() {
            return Ok(this);
        }
    }
    if let Ok(path) = std::env::current_exe() {
        return Ok(path);
    }
    let name = std::env::args_os().next().unwrap_or_default();
    started_as(&name, std::env::var_os("PATH").as_deref()).context(
        "cannot find the braid command to start verify's watchdog, so the tests did not run",
    )
}

/// The file that a command started as `name` runs: `name` itself when it
/// holds a `/`, or else the first executable file of that name in the
/// directories of `path`, as a shell finds it.
#[cfg(unix)]
fn started_as(name: &std::ffi::OsStr, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    if name.is_empty() {
        return None;
    }
    if name.as_bytes().contains(&b'/') {
        return Some(PathBuf::from(name));
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The watchdog of one test run in `dir`, as [`Watchdog`] starts it. Braid
/// 0.1.1 started its watchdog with the test's process group, `given`,
/// before the directory, after the test, and said nothing but `done`: a
/// newer braid put in its place mid-run still watches that run.
pub(crate) fn watchdog(given: Option<&std::ffi::OsStr>, dir: &Path) -> Result<ExitCode> {
    {
        let mut out = io::stdout().lock();
        let _ = out.write_all(READY).and_then(|()| out.flush());
    }
    let mut group = given.and_then(|id| group_id(id.as_encoded_bytes()));
    for line in io::stdin().lock().split(b'\n') {
        let Ok(line) = line else { break };
        if line == b"done" {
            return Ok(ExitCode::SUCCESS);
        }
        if let Some(id) = line.strip_prefix(b"group ") {
            group = group_id(id);
        }
    }
    // Braid went away while the tests ran, or before they started.
    #[cfg(unix)]
    if let Some(pid) = group.and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = group;
    // Only ever one of verify's scratch directories.
    let ours = dir.parent() == Some(std::env::temp_dir().as_path())
        && dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(SCRATCH_PREFIX));
    if ours {
        for _ in 0..10 {
            if fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A process group's id, as the watchdog is given it: a number above 1.
fn group_id(text: &[u8]) -> Option<i32> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|id| id.parse::<i32>().ok())
        .filter(|id| *id > 1)
}

/// Whether an environment variable is one of the hub's credentials or
/// settings, which the tests never see: `BRAID_TOKEN` and every variable
/// whose name starts with `BRAID_HUB`.
fn is_credential(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == "BRAID_TOKEN" || name.starts_with("BRAID_HUB")
}

/// Kills the command and every process left in its group. A group already
/// gone is no error.
fn kill_group(child: &mut Child) {
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
            "{SCRATCH_PREFIX}{}-{nanos:08x}-{n}",
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

/// `a, b and c`, of short ids.
fn listed(ids: &[ObjectId]) -> String {
    let shorts: Vec<String> = ids.iter().map(ObjectId::short).collect();
    match shorts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Everything one pass needs.
struct Pass<'a> {
    args: &'a VerifyArgs,
    repo: &'a Repo,
    link: Option<Link>,
    branch: &'a str,
    tip: ObjectId,
    green_before: Option<ObjectId>,
    /// `--from`, resolved.
    from: Option<ObjectId>,
}

impl Pass<'_> {
    fn run(&self, tester: &mut Tester<'_>, verdict: &mut Verdict) -> Result<()> {
        if self.green_before == Some(self.tip) {
            verdict.outcome = "pass";
            verdict.reason = Some("the head is the green changeset already".to_owned());
            return Ok(());
        }
        let flaky = match tester.check(self.tip)? {
            Check::Passed { flaky } => flaky,
            Check::Failed => return self.red(tester, verdict),
            Check::Skipped => bail!(
                "the files of the head, {}, cannot be written on this system, so it cannot be tested here",
                self.tip.short()
            ),
        };
        verdict.outcome = if flaky { "flaky" } else { "pass" };
        let mut reasons = Vec::new();
        if flaky {
            reasons.push("a run failed, and a rerun passed".to_owned());
        }
        if flaky && !self.args.flaky_moves_green {
            reasons.push("the green ref stays; --flaky-moves-green would move it".to_owned());
        } else if self.args.dry_run {
            reasons.push(format!(
                "dry run: the green ref would move to {}",
                self.tip.short()
            ));
        } else {
            self.move_green()?;
            verdict.green = Some(self.tip);
        }
        if !reasons.is_empty() {
            verdict.reason = Some(reasons.join("; "));
        }
        Ok(())
    }

    /// The tests fail on the head: find the first changeset that fails.
    fn red(&self, tester: &mut Tester<'_>, verdict: &mut Verdict) -> Result<()> {
        verdict.outcome = "red";
        let Some(good) = self.from.or(self.green_before) else {
            verdict.reason = Some(
                "the tests fail on the head, and with no green changeset there is nothing to bisect from: --from names one"
                    .to_owned(),
            );
            return Ok(());
        };
        let Some(range) = range(self.repo, self.tip, good)? else {
            verdict.reason = Some(format!(
                "{} is not on {}'s first-parent chain: there is nothing to bisect",
                good.short(),
                self.branch
            ));
            return Ok(());
        };
        if range.is_empty() {
            verdict.red(
                "no_culprit",
                format!("the head is {}, which was to pass", good.short()),
            );
            return Ok(());
        }
        let last = range.len() as isize - 1;
        // `range[lo]` passes (or is `good`, at -1), and the head fails.
        let mut lo = -1;
        loop {
            let (passing, failing, untested) = bisect(tester, &range, lo, last, verdict)?;
            if !untested.is_empty() {
                let mut candidates: Vec<ObjectId> =
                    untested.iter().map(|i| range[*i as usize]).collect();
                candidates.push(range[failing as usize]);
                verdict.reason = Some(format!(
                    "the first changeset that fails is one of {}: the others could not be tested here",
                    listed(&candidates)
                ));
                return Ok(());
            }
            let culprit = range[failing as usize];
            if let Some(at) = reverted_at(self.repo, &range, failing)? {
                // Fixed once already: what fails now came after, hidden by
                // the culprit until its revert.
                let revert = range[at];
                let hidden = format!(
                    "{} fails, and {revert_short} reverted it later, but the tests fail at {revert_short} too: what broke them since is hidden behind {}",
                    culprit.short(),
                    culprit.short(),
                    revert_short = revert.short()
                );
                if at as isize == last {
                    verdict.red("no_culprit", hidden);
                    return Ok(());
                }
                match tester.check(revert)? {
                    Check::Passed { .. } => {
                        lo = at as isize;
                        continue;
                    }
                    Check::Failed => {
                        verdict.red("no_culprit", hidden);
                        return Ok(());
                    }
                    Check::Skipped => {
                        verdict.skipped.push(revert);
                        verdict.reason = Some(format!(
                            "{} fails, and {} reverted it later, but the revert could not be tested here",
                            culprit.short(),
                            revert.short()
                        ));
                        return Ok(());
                    }
                }
            }
            if passing < 0 {
                match tester.check(good)? {
                    Check::Passed { .. } => {}
                    Check::Failed => {
                        // The changeset taken as passing fails now too:
                        // something outside the range broke the tests.
                        verdict.red(
                            "no_culprit",
                            format!(
                                "{} fails too, though it was to pass: the failure is not in the changesets after it",
                                good.short()
                            ),
                        );
                        return Ok(());
                    }
                    // It cannot be checked; it was to pass.
                    Check::Skipped => verdict.skipped.push(good),
                }
            }
            verdict.culprit = Some(culprit);
            return self.revert(culprit, verdict);
        }
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
    fn revert(&self, culprit: ObjectId, verdict: &mut Verdict) -> Result<()> {
        let first = format!(
            "the tests fail from {}, the first changeset that fails",
            culprit.short()
        );
        if !self.args.revert {
            verdict.reason = Some(format!("{first}; --revert would revert it"));
            return Ok(());
        }
        let changeset = braid_engine::read_changeset(self.repo.store(), culprit)?;
        let path = Record::path(self.repo, self.branch);
        let mut record = Record::load(&path)?;
        let now = now_seconds();
        let mut recent = reverts_in_last_hour(self.repo, self.tip, now, HISTORY_DEPTH)?;
        recent.extend(record.in_last_hour(now));
        let refusal = if changeset.parents.len() > 1 {
            Some(format!(
                "{first}, and it is a merge: verify does not revert merges"
            ))
        } else if is_verify_revert(&changeset) || record.made(culprit) {
            Some(format!(
                "{first}, and it is a revert verify made: verify does not revert its own reverts"
            ))
        } else if recent.len() >= self.args.max_reverts_per_hour {
            Some(format!(
                "{first}, but verify landed {} on {} in the last hour, the most --max-reverts-per-hour allows",
                count(recent.len(), "revert"),
                self.branch
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            verdict.red("refused", reason);
            return Ok(());
        }
        if self.args.dry_run {
            verdict.reason = Some(format!("{first}; dry run: verify would revert it"));
            return Ok(());
        }
        let author = identity(&self.args.identity, self.repo)?;
        let change = braid_wc::create_revert(
            self.repo.store(),
            &RevertRequest {
                culprit,
                mainline: false,
                author: author.clone(),
                agent: AGENT.to_owned(),
            },
        )?;
        verdict.revert = Some(change);
        let landed = land_revert(
            self.repo,
            self.link.as_ref(),
            self.branch,
            change,
            AGENT,
            &author,
        );
        let mut report = match landed {
            Ok(report) => report,
            Err(err) => {
                // The hub may have landed it all the same: the record
                // keeps it, as the branch will once fetched.
                record.reverts.push(Reverted {
                    culprit: culprit.to_hex(),
                    revert: change.to_hex(),
                    landed: None,
                    time: now,
                });
                let _ = record.save(&path);
                return Err(err);
            }
        };
        let outcome = report.outcome.clone();
        match outcome.as_str() {
            "fast-forward" | "rebased" => {
                verdict.red(
                    "reverted",
                    format!("{first}; its revert landed on {}", self.branch),
                );
                record.reverts.push(Reverted {
                    culprit: culprit.to_hex(),
                    revert: change.to_hex(),
                    landed: report.head.map(|id| id.to_hex()),
                    time: now,
                });
                verdict.land = Some(report);
                record.save(&path)?;
                return Ok(());
            }
            "conflicts" => {
                verdict.red(
                    "revert_conflicts",
                    format!(
                        "{first}; its revert conflicts with what landed since, so {} did not move: revert it by hand in a workspace",
                        self.branch
                    ),
                );
                report.next = None;
            }
            "already-applied" => verdict.red(
                "refused",
                format!(
                    "{first}, but {} has what its revert would change already",
                    self.branch
                ),
            ),
            other => {
                verdict.land = Some(report);
                bail!("the revert's landing ended with {other}");
            }
        }
        verdict.land = Some(report);
        Ok(())
    }
}

/// Bisects `range` between `lo`, which passes (-1 for the changeset before
/// the range), and `hi`, which fails, skipping changesets that cannot be
/// tested. Returns the last passing and the first failing index found, and
/// the skipped indexes between them.
fn bisect(
    tester: &mut Tester<'_>,
    range: &[ObjectId],
    mut lo: isize,
    mut hi: isize,
    verdict: &mut Verdict,
) -> Result<(isize, isize, Vec<isize>)> {
    let mut skipped = BTreeSet::new();
    loop {
        let middle = lo + (hi - lo) / 2;
        let Some(pick) = (lo + 1..hi)
            .filter(|i| !skipped.contains(i))
            .min_by_key(|i| (i - middle).abs())
        else {
            break;
        };
        match tester.check(range[pick as usize])? {
            Check::Passed { .. } => lo = pick,
            Check::Failed => hi = pick,
            Check::Skipped => {
                skipped.insert(pick);
                verdict.skipped.push(range[pick as usize]);
            }
        }
    }
    let untested = skipped.into_iter().filter(|i| *i > lo && *i < hi).collect();
    Ok((lo, hi, untested))
}

/// Where in `range`, after `index`, a changeset reverts `range[index]`, if
/// one does.
fn reverted_at(repo: &Repo, range: &[ObjectId], index: isize) -> Result<Option<usize>> {
    let culprit = range[index as usize].to_hex().into_bytes();
    for (at, id) in range.iter().enumerate().skip(index as usize + 1) {
        let changeset = braid_engine::read_changeset(repo.store(), *id)?;
        if extra(&changeset, REVERTS) == Some(culprit.as_slice()) {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

pub(crate) fn verify(cli: &Cli, args: &VerifyArgs) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let branch = branch_or_main(&repo, args.branch.as_deref())?;
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
    let from = match &args.from {
        Some(revision) => Some(resolve_changeset(&repo, revision)?),
        None => None,
    };
    let green_before = refs.get(&green_ref(&branch));
    let pass = Pass {
        args,
        repo: &repo,
        link,
        branch: &branch,
        tip,
        green_before,
        from,
    };
    let mut tester = Tester {
        repo: &repo,
        branch: &branch,
        command: &args.command,
        timeout: args.timeout,
        reruns: args.reruns,
        runs: 0,
    };
    let mut verdict = Verdict {
        green: green_before,
        ..Verdict::default()
    };
    if let Err(err) = pass.run(&mut tester, &mut verdict) {
        // Before anything ran or was made, an error is only an error.
        if tester.runs == 0 && verdict.revert.is_none() {
            return Err(err);
        }
        eprintln!("braid: error: {err:#}");
        verdict.outcome = "error";
        verdict.reason = Some(format!("{err:#}"));
    }
    if cli.json {
        print_json(&json!({
            "branch": branch,
            "tip": tip.to_hex(),
            "green_before": hex(green_before),
            "green": hex(verdict.green),
            "outcome": verdict.outcome,
            "culprit": hex(verdict.culprit),
            "revert": hex(verdict.revert),
            "skipped": verdict.skipped.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
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
    let ran = count(runs, "run");
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
    if let Some(revert) = verdict.revert {
        println!("Revert: {}", revert.short());
    }
    if !verdict.skipped.is_empty() {
        println!("Not tested here: {}", listed(&verdict.skipped));
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
    fn record_files_are_named_in_hex_and_kept_whole() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        let path = Record::path(&repo, "feature/x");
        assert_eq!(path.file_name().unwrap(), "666561747572652f78.json");
        // Two names that differ only in case get two files.
        assert_ne!(Record::path(&repo, "Main"), Record::path(&repo, "main"));
        let mut record = Record::default();
        record.reverts.push(Reverted {
            culprit: "c".repeat(64),
            revert: "a".repeat(64),
            landed: Some("b".repeat(64)),
            time: 10_000,
        });
        let landed: ObjectId = "b".repeat(64).parse().unwrap();
        assert!(record.made(landed));
        assert_eq!(record.in_last_hour(10_000 + 3599).len(), 1);
        assert_eq!(record.in_last_hour(10_000 + 3600).len(), 0);
        record.save(&path).unwrap();
        assert_eq!(Record::load(&path).unwrap().reverts.len(), 1);
        assert!(!path.with_extension("json.new").exists());

        // A damaged record says how to recover.
        fs::write(&path, "{").unwrap();
        let err = format!("{:#}", Record::load(&path).unwrap_err());
        assert!(err.contains("delete it"), "{err}");
    }

    #[test]
    fn long_branch_names_are_hashed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        let name = |branch: &str| {
            Record::path(&repo, branch)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let longest = "b".repeat(MAX_HEX_NAME / 2);
        assert_eq!(
            name(&longest),
            format!("{}.json", "62".repeat(longest.len()))
        );
        let longer = format!("{longest}x");
        let hashed = name(&longer);
        assert_eq!(
            hashed,
            format!("b3-{}.json", blake3::hash(longer.as_bytes()).to_hex())
        );
        assert_ne!(name(&format!("{longest}y")), hashed);
        // The longest file names, with the scratch file's `.new`, stay under
        // 143 bytes, whatever the length of the branch's name.
        for branch in [longest, longer, "z".repeat(4096)] {
            let scratch = format!("{}.new", name(&branch));
            assert!(scratch.len() < 143, "{scratch}");
        }
    }

    #[test]
    fn the_hourly_cap_counts_by_each_reverts_own_time() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        let now: i64 = 1_800_000_000;
        let signature = |seconds| braid_format::Signature {
            name: b"Ada".to_vec(),
            email: b"ada@example.org".to_vec(),
            seconds,
            offset_minutes: 0,
        };
        let revert_by = |agent: &str| {
            vec![
                (AGENT_FIELD.to_vec(), agent.as_bytes().to_vec()),
                (REVERTS.to_vec(), "c".repeat(64).into_bytes()),
            ]
        };
        let commit = |parent: Option<ObjectId>, ago: i64, extras| {
            braid_engine::commit(
                repo.store(),
                parent.into_iter().collect(),
                Vec::new(),
                signature(now - ago),
                signature(now - ago),
                b"change\n".to_vec(),
                extras,
            )
            .unwrap()
        };
        // Down from the tip: a revert by someone else, one of verify's, a
        // changeset that landed by a fast-forward two days after it was
        // made, another of verify's below it, and one older than an hour.
        let base = commit(None, 3 * 86_400, Vec::new());
        let too_old = commit(Some(base), 2 * 3600, revert_by(AGENT));
        let below = commit(Some(too_old), 30 * 60, revert_by(AGENT));
        let made_earlier = commit(Some(below), 2 * 86_400, Vec::new());
        let recent = commit(Some(made_earlier), 10 * 60, revert_by(AGENT));
        let others = commit(Some(recent), 5 * 60, revert_by("ada"));
        let tip = commit(Some(others), 60, Vec::new());
        let found = reverts_in_last_hour(&repo, tip, now as u64, HISTORY_DEPTH).unwrap();
        let expected: HashSet<String> = [below, recent].iter().map(ObjectId::to_hex).collect();
        assert_eq!(found, expected);

        // Only the depth bounds the walk: four changesets down from the tip
        // stop above `below`, five reach it.
        let found = reverts_in_last_hour(&repo, tip, now as u64, 4).unwrap();
        assert_eq!(found, HashSet::from([recent.to_hex()]));
        let found = reverts_in_last_hour(&repo, tip, now as u64, 5).unwrap();
        assert_eq!(found, expected);
    }

    #[cfg(unix)]
    #[test]
    fn without_its_own_file_braid_is_found_as_a_shell_finds_it() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let program = bin.join("braid-like");
        fs::write(&program, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let plain = bin.join("not-a-program");
        fs::write(&plain, "").unwrap();
        let path = std::env::join_paths([temp.path().join("missing"), bin.clone()]).unwrap();
        let found = |name: &str| started_as(name.as_ref(), Some(path.as_os_str()));
        assert_eq!(found("braid-like"), Some(program.clone()));
        assert_eq!(found("not-a-program"), None);
        assert_eq!(found("absent"), None);
        assert_eq!(found(""), None);
        // A name with a slash is a path already.
        assert_eq!(
            found("./target/braid"),
            Some(PathBuf::from("./target/braid"))
        );
        assert_eq!(started_as("braid-like".as_ref(), None), None);
    }

    /// A stand-in for the watchdog: `script`, run by `sh`.
    #[cfg(unix)]
    fn fake_watchdog(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(unix)]
    #[test]
    fn no_test_runs_unless_the_watchdog_says_it_runs() {
        // Long enough for any start: these end on their own first.
        let ample = Duration::from_secs(30);
        // An older braid, which has no such command, or another program.
        let err = Watchdog::spawn(fake_watchdog("echo 'no such command' >&2; exit 2"), ample)
            .err()
            .unwrap();
        let err = format!("{err:#}");
        assert!(
            err.contains("did not start") && err.contains("without a word"),
            "{err}"
        );
        // One that answers with another protocol.
        let older = fake_watchdog("echo 'braid verify watchdog 1 ready'; exec cat");
        let err = format!("{:#}", Watchdog::spawn(older, ample).err().unwrap());
        assert!(err.contains("it said"), "{err}");
        // One that says far more than the line, all on one: it is not read
        // past the line's length.
        let chatty = fake_watchdog("yes braid | tr -d '\\n'");
        let err = format!("{:#}", Watchdog::spawn(chatty, ample).err().unwrap());
        assert!(err.contains("it said \"braidbraid"), "{err}");
        // One that never answers: it echoes its input, and gets none. Here
        // the wait runs out, whatever it is.
        let silent = fake_watchdog("exec cat");
        let wait = Duration::from_secs(1);
        let err = format!("{:#}", Watchdog::spawn(silent, wait).err().unwrap());
        assert!(err.contains("said nothing"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_watchdog_is_told_the_group_and_when_the_run_is_over() {
        let temp = tempfile::tempdir().unwrap();
        let heard = temp.path().join("heard");
        let script = format!(
            "echo '{}'; cat > '{}'",
            String::from_utf8_lossy(READY).trim_end(),
            heard.display()
        );
        let mut watchdog =
            Watchdog::spawn(fake_watchdog(&script), Duration::from_secs(10)).unwrap();
        let mut test = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        watchdog.watch(&test).unwrap();
        assert!(!watchdog.stopped());
        watchdog.done();
        test.wait().unwrap();
        assert_eq!(
            fs::read_to_string(&heard).unwrap(),
            format!("group {}\ndone\n", test.id())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_run_stops_when_its_watchdog_does() {
        use std::os::unix::process::CommandExt;
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        let tester = Tester {
            repo: &repo,
            branch: "main",
            command: &[],
            timeout: Duration::from_secs(60),
            reruns: 0,
            runs: 0,
        };
        // It answers, hears the group, and stops.
        let mut watchdog = Watchdog::spawn(
            fake_watchdog(&format!(
                "echo '{}'; read line; exit 0",
                String::from_utf8_lossy(READY).trim_end()
            )),
            Duration::from_secs(10),
        )
        .unwrap();
        let mut test = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        watchdog.watch(&test).unwrap();
        let err = match tester.wait(&mut test, &mut watchdog) {
            Ok(_) => panic!("the run went on without its watchdog"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("watchdog stopped"), "{err}");
        // The test went with it, and was reaped.
        assert!(test.try_wait().unwrap().is_some());
        watchdog.done();
    }

    #[test]
    fn ids_are_listed_in_words() {
        let ids: Vec<ObjectId> = ["a", "b", "c"]
            .iter()
            .map(|c| c.repeat(64).parse().unwrap())
            .collect();
        assert_eq!(listed(&ids[..1]), "a".repeat(12));
        assert_eq!(
            listed(&ids),
            format!(
                "{}, {} and {}",
                "a".repeat(12),
                "b".repeat(12),
                "c".repeat(12)
            )
        );
    }
}
