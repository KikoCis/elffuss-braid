// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Many processes sharing one store, and processes killed at random moments.
//!
//! The tests run copies of this test binary that execute only [`child`],
//! configured through environment variables. A child writes batches of
//! objects, syncs, and acknowledges each object in a file of its own. Then it
//! increments a shared counter ref with compare and swap, retrying when
//! another process got there first, and acknowledges that too. The parent
//! checks the store against the acknowledgements: fsck is clean, every
//! acknowledged object is readable, the log has no gaps, every operation
//! moved the counter up by exactly one, and every acknowledged increment is
//! in the log.
//!
//! `BRAID_STRESS=1` runs longer: more processes, more work, more rounds of
//! killing. `BRAID_STRESS_SEED` fixes the seed of the crash test.

use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use braid_store::{FsckReport, Kind, ObjectId, Objects, Options, RefUpdate, Store};

/// Set in the environment of a child process.
const CHILD: &str = "BRAID_STORE_TEST_CHILD";

/// The heavy tests share the disk, so they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn stress() -> bool {
    env::var_os("BRAID_STRESS").is_some_and(|value| value != "0")
}

/// Small segments and frequent checkpoints, so that sealing and
/// checkpointing happen many times while processes race.
fn options() -> Options {
    Options {
        segment_size: 16 << 10,
        checkpoint_interval: 5,
        ..Options::default()
    }
}

/// What one child process does.
struct Job {
    store: PathBuf,
    acks: PathBuf,
    child: u64,
    round: u64,
    iterations: u64,
    batch: u64,
}

const OBJECTS_PER_BATCH: u64 = 6;

/// The entry point of child processes. In a normal test run it does nothing.
#[test]
fn child() {
    if env::var_os(CHILD).is_none() {
        return;
    }
    let var = |name: &str| env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let number = |name: &str| var(name).parse::<u64>().expect("a number");
    run(&Job {
        store: var("BRAID_TEST_STORE").into(),
        acks: var("BRAID_TEST_ACKS").into(),
        child: number("BRAID_TEST_CHILD"),
        round: number("BRAID_TEST_ROUND"),
        iterations: number("BRAID_TEST_ITERATIONS"),
        batch: number("BRAID_TEST_BATCH"),
    });
}

fn run(job: &Job) {
    let store = Store::open(&job.store, options()).expect("open the store");
    let mut acks = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&job.acks)
        .expect("open the acknowledgements");
    let agent = format!("child-{}", job.child);
    let branch = format!("child/{}", job.child);
    for i in 0..job.iterations {
        let mut written = Vec::new();
        for j in 0..job.batch {
            let (kind, payload) = object(job.round, job.child, i, j);
            written.push((j, store.put(kind, &payload).expect("put")));
        }
        store.sync().expect("sync");
        // Acknowledged only once synced. One write per batch: a kill can
        // only cut the last line short, and partial lines are ignored.
        let mut lines = String::new();
        for (j, id) in &written {
            lines.push_str(&format!(
                "object {} {} {i} {j} {id}\n",
                job.round, job.child
            ));
        }
        acks.write_all(lines.as_bytes()).expect("acknowledge");
        let (seq, value) = increment(&store, &agent, &branch, written[0].1);
        acks.write_all(format!("increment {seq} {value}\n").as_bytes())
            .expect("acknowledge");
    }
}

/// Adds one to the shared counter with compare and swap, retrying while
/// other processes get there first, and points `branch` at `tip` in the same
/// operation. Returns the operation's sequence number and the new value.
fn increment(store: &Store, agent: &str, branch: &str, tip: ObjectId) -> (u64, u64) {
    loop {
        let refs = store.refs().expect("read the refs");
        let current = refs.get("counter");
        let value = current.map_or(0, |id| counter_value(store, &id)) + 1;
        let next = store
            .put(Kind::Blob, value.to_string().as_bytes())
            .expect("put");
        store.sync().expect("sync");
        let updates = vec![
            RefUpdate::new("counter", current, Some(next)),
            RefUpdate::new(branch, refs.get(branch), Some(tip)),
        ];
        match store.transact(agent, "increment the counter", |_| {
            Ok::<_, Infallible>(updates)
        }) {
            Ok(op) => return (op.seq, value),
            Err(err) if err.is_conflict() => continue,
            Err(err) => panic!("transaction failed: {err}"),
        }
    }
}

fn counter_value(store: &Store, id: &ObjectId) -> u64 {
    let (kind, payload) = store
        .get(id)
        .expect("read the counter")
        .expect("the counter's object is in the store");
    assert_eq!(kind, Kind::Blob);
    String::from_utf8(payload)
        .expect("text")
        .parse()
        .expect("a number")
}

/// The object a child writes as number `j` of batch `i`. The first of every
/// batch is the same in every child of a round, so several processes store
/// the same object at the same moment.
fn object(round: u64, child: u64, i: u64, j: u64) -> (Kind, Vec<u8>) {
    if j == 0 {
        let text = format!("shared by every child of round {round}, batch {i}");
        return (Kind::Blob, text.into_bytes());
    }
    let mut rng = Rng::new(round, child, i, j);
    let kind = Kind::ALL[(rng.next() % Kind::ALL.len() as u64) as usize];
    let payload = match j % 3 {
        // Small enough to be stored raw.
        1 => (0..rng.next() % 64).map(|_| rng.next() as u8).collect(),
        // Compressible.
        2 => format!("round {round} child {child} batch {i} object {j}\n")
            .repeat(1 + (rng.next() % 300) as usize)
            .into_bytes(),
        // Noise, up to a third of a segment.
        _ => (0..100 + rng.next() % 20_000)
            .map(|_| rng.next() as u8)
            .collect(),
    };
    (kind, payload)
}

/// SplitMix64.
struct Rng(u64);

impl Rng {
    /// A generator for one object. Each part goes through the mixer, so
    /// different inputs give unrelated streams.
    fn new(round: u64, child: u64, i: u64, j: u64) -> Self {
        let mut rng = Rng(round);
        for part in [child, i, j] {
            rng.0 = rng.next() ^ part;
        }
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn spawn(job: &Job, stderr: &Path) -> Child {
    let exe = env::current_exe().expect("the test binary");
    Command::new(exe)
        .args(["--exact", "child", "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .env("BRAID_TEST_STORE", &job.store)
        .env("BRAID_TEST_ACKS", &job.acks)
        .env("BRAID_TEST_CHILD", job.child.to_string())
        .env("BRAID_TEST_ROUND", job.round.to_string())
        .env("BRAID_TEST_ITERATIONS", job.iterations.to_string())
        .env("BRAID_TEST_BATCH", job.batch.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(stderr).expect("create a log for the child"))
        .spawn()
        .expect("start a child")
}

/// What the children acknowledged.
#[derive(Debug, Default)]
struct Acks {
    /// Round, child, batch, number in the batch, and id.
    objects: Vec<(u64, u64, u64, u64, ObjectId)>,
    /// Sequence number and counter value.
    increments: Vec<(u64, u64)>,
}

fn read_acks(dir: &Path) -> Acks {
    let mut acks = Acks::default();
    for entry in fs::read_dir(dir).expect("list acknowledgements") {
        let text = fs::read_to_string(entry.expect("an entry").path()).expect("read");
        // A kill can cut the last line short: it was never acknowledged.
        for line in text.split_inclusive('\n').filter(|l| l.ends_with('\n')) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let number = |i: usize| fields[i].parse::<u64>().expect("a number");
            match fields[0] {
                "object" => acks.objects.push((
                    number(1),
                    number(2),
                    number(3),
                    number(4),
                    fields[5].parse().expect("an id"),
                )),
                "increment" => acks.increments.push((number(1), number(2))),
                other => panic!("unexpected acknowledgement {other:?}"),
            }
        }
    }
    acks
}

/// Checks the store against the acknowledgements, and returns the number
/// of operations in the log with the fsck report.
fn verify(store: &Store, acks: &Acks, context: &str) -> (u64, FsckReport) {
    let report = store.fsck().expect("run fsck");
    assert!(
        report.is_clean(),
        "{context}: fsck found problems: {:#?}",
        report.problems
    );

    for (round, child, i, j, id) in &acks.objects {
        let (kind, payload) = object(*round, *child, *i, *j);
        assert_eq!(*id, ObjectId::hash(kind, &payload), "{context}");
        let found = store.get(id).expect("read an acknowledged object");
        assert!(
            found == Some((kind, payload)),
            "{context}: object {id}, acknowledged by child {child} in round {round}, is not readable"
        );
    }

    // No gaps: newest first, from the newest down to 1.
    let log = store.log(usize::MAX).expect("read the log");
    let operations = log.len() as u64;
    for (k, op) in log.iter().enumerate() {
        assert_eq!(op.seq, operations - k as u64, "{context}: gap in the log");
    }
    // Every operation moved the counter up by exactly one.
    let mut values = HashMap::new();
    let mut previous = None;
    for (expected, op) in (1..).zip(log.iter().rev()) {
        let update = op
            .updates
            .iter()
            .find(|update| update.name == "counter")
            .unwrap_or_else(|| panic!("{context}: operation {} skips the counter", op.seq));
        assert_eq!(update.old, previous, "{context}: operation {}", op.seq);
        let id = update.new.expect("the counter is never deleted");
        assert_eq!(
            counter_value(store, &id),
            expected,
            "{context}: operation {}",
            op.seq
        );
        values.insert(op.seq, expected);
        previous = Some(id);
    }
    assert_eq!(
        store.refs().expect("refs").get("counter"),
        previous,
        "{context}"
    );
    for (seq, value) in &acks.increments {
        assert_eq!(
            values.get(seq),
            Some(value),
            "{context}: acknowledged increment {seq} is not in the log"
        );
    }
    (operations, report)
}

fn child_failure(child: u64, log: &Path) -> String {
    let output = fs::read_to_string(log).unwrap_or_default();
    format!("child {child} failed:\n{output}")
}

#[test]
fn many_processes_share_one_store() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let (children, iterations) = if stress() { (48, 40) } else { (32, 3) };
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let acks_dir = dir.path().join("acks");
    let logs_dir = dir.path().join("logs");
    fs::create_dir(&acks_dir).unwrap();
    fs::create_dir(&logs_dir).unwrap();
    Store::init(&store_dir, options()).unwrap();

    let running: Vec<(u64, Child)> = (0..children)
        .map(|child| {
            let job = Job {
                store: store_dir.clone(),
                acks: acks_dir.join(format!("{child}")),
                child,
                round: 0,
                iterations,
                batch: OBJECTS_PER_BATCH,
            };
            (child, spawn(&job, &logs_dir.join(format!("{child}"))))
        })
        .collect();
    for (child, mut process) in running {
        let status = process.wait().unwrap();
        assert!(
            status.success(),
            "{}",
            child_failure(child, &logs_dir.join(format!("{child}")))
        );
    }

    let store = Store::open(&store_dir, options()).unwrap();
    let acks = read_acks(&acks_dir);
    assert_eq!(
        acks.objects.len() as u64,
        children * iterations * OBJECTS_PER_BATCH
    );
    assert_eq!(acks.increments.len() as u64, children * iterations);
    let (operations, report) = verify(&store, &acks, "after every process finished");
    assert_eq!(operations, children * iterations);
    assert!(report.sealed_segments > 0, "{report:?}");
    assert!(report.checkpoints > 0, "{report:?}");
}

#[test]
fn processes_killed_at_random_moments() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let (children, rounds, window_ms) = if stress() {
        (32, 30, 800)
    } else {
        (32, 3, 400)
    };
    let seed = env::var("BRAID_STRESS_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos() as u64)
        });
    let mut rng = Rng::new(seed, 0, 0, 0);
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let acks_dir = dir.path().join("acks");
    let logs_dir = dir.path().join("logs");
    fs::create_dir(&acks_dir).unwrap();
    fs::create_dir(&logs_dir).unwrap();
    Store::init(&store_dir, options()).unwrap();

    let mut acknowledged = 0;
    for round in 1..=rounds {
        let started = Instant::now();
        let mut running: Vec<(u64, Child, Duration)> = (0..children)
            .map(|child| {
                let job = Job {
                    store: store_dir.clone(),
                    acks: acks_dir.join(format!("{round}-{child}")),
                    child,
                    round,
                    // More than any child gets through before it is killed.
                    iterations: u64::MAX,
                    batch: OBJECTS_PER_BATCH,
                };
                let process = spawn(&job, &logs_dir.join(format!("{round}-{child}")));
                let kill_at = Duration::from_millis(rng.next() % window_ms);
                (child, process, kill_at)
            })
            .collect();
        running.sort_by_key(|(_, _, kill_at)| *kill_at);
        for (_, process, kill_at) in &mut running {
            if let Some(wait) = kill_at.checked_sub(started.elapsed()) {
                thread::sleep(wait);
            }
            // SIGKILL: no destructors, no flushing, locks released by the kernel.
            let _ = process.kill();
        }
        for (child, mut process, _) in running {
            let status = process.wait().unwrap();
            // Killed is expected. Exiting by itself means it failed.
            assert!(
                status.code().is_none(),
                "seed {seed}, round {round}: {}",
                child_failure(child, &logs_dir.join(format!("{round}-{child}")))
            );
        }

        let context = format!("seed {seed}, round {round}");
        let store = Store::open(&store_dir, options()).unwrap();
        let acks = read_acks(&acks_dir);
        let (before, report) = verify(&store, &acks, &context);
        acknowledged = acks.increments.len();
        // Shown with --nocapture: what the kills left behind.
        eprintln!(
            "{context}: {before} operations, {} objects, {} segments ({} sealed), \
             leftovers: {} torn segment bytes, {} torn journal entries, \
             {} unindexed records, {} torn log bytes, {} temporary files",
            report.objects,
            report.segments,
            report.sealed_segments,
            report.torn_segment_bytes,
            report.torn_journal_entries,
            report.unindexed_records,
            report.torn_log_bytes,
            report.temporary_files,
        );

        // Writing after the crash: segments of the dead are taken over, and
        // a torn end of the log is cut off.
        let tip = store
            .put(
                Kind::Blob,
                format!("written by the parent after round {round}").as_bytes(),
            )
            .unwrap();
        let (seq, _) = increment(&store, "parent", "parent", tip);
        assert_eq!(seq, before + 1, "{context}");
        verify(&store, &acks, &format!("{context}, after the parent wrote"));
    }
    // The children got work done before they were killed.
    assert!(acknowledged > 0, "seed {seed}: no child got anything done");
}
