// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The landing simulation: many seeds in memory, a few on the real store.
//!
//! - `BRAID_SIM_SEEDS=<n>` runs seeds `0..n` in memory instead of the default
//!   number, for long runs; `BRAID_SIM_STORE_SEEDS=<n>` does the same on the
//!   store.
//! - `BRAID_SIM_SEED=<s>` replays one seed, in memory and on the store, and
//!   prints its whole trace (pass `--nocapture` to see it).
//! - `BRAID_SIM_AGENTS=<n>` changes the number of agents.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use braid_sim::{Config, Failure, Outcome, Report, run_memory, run_store};

/// Seeds the default run covers in memory.
const MEMORY_SEEDS: u64 = 40;

/// Seeds the default run covers on the store, where every landing syncs.
const STORE_SEEDS: u64 = 3;

fn env_number(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number, not {value:?}")),
    )
}

fn memory_config() -> Config {
    let mut config = Config::default();
    if let Some(agents) = env_number("BRAID_SIM_AGENTS") {
        config.agents = agents as usize;
    }
    config
}

/// Fewer agents and changes: on the store every landing is a synced
/// transaction.
fn store_config() -> Config {
    let mut config = memory_config();
    if env_number("BRAID_SIM_AGENTS").is_none() {
        config.agents = 4;
    }
    config.tasks = 6;
    config
}

/// Runs `run` for seeds `0..count` on every core, and returns the summed
/// report, or the failure of the smallest seed that failed.
fn run_seeds(
    backend: &'static str,
    count: u64,
    run: impl Fn(u64) -> Result<Outcome, Failure> + Sync,
) -> Report {
    let next = AtomicU64::new(0);
    let results = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(count.max(1) as usize);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= count {
                        break;
                    }
                    let result = match catch_unwind(AssertUnwindSafe(|| run(seed))) {
                        Ok(result) => result.map(|outcome| outcome.report),
                        Err(panic) => Err(panicked(backend, seed, panic)),
                    };
                    results.lock().unwrap().push((seed, result));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(seed, _)| *seed);
    let mut total = Report::default();
    let mut failures = Vec::new();
    for (_, result) in results {
        match result {
            Ok(report) => total.add(&report),
            Err(failure) => failures.push(failure),
        }
    }
    if let Some(first) = failures.first() {
        panic!(
            "{} of {count} seeds failed; the first:\n{first}",
            failures.len()
        );
    }
    total
}

fn panicked(backend: &'static str, seed: u64, panic: Box<dyn std::any::Any + Send>) -> Failure {
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_default();
    Failure {
        seed,
        backend,
        step: 0,
        invariant: "panic",
        message,
        trace: Vec::new(),
        full_trace: Vec::new(),
    }
}

fn print_run(title: &str, outcome: &Outcome) {
    eprintln!("{title}");
    for line in &outcome.trace {
        eprintln!("  {line}");
    }
    eprintln!("{}", outcome.report);
}

#[test]
fn landing_in_memory() {
    let config = memory_config();
    if let Some(seed) = env_number("BRAID_SIM_SEED") {
        match run_memory(seed, &config) {
            Ok(outcome) => print_run(&format!("seed {seed} in memory"), &outcome),
            Err(failure) => panic!(
                "{failure}\nthe whole run:\n  {}",
                failure.full_trace.join("\n  ")
            ),
        }
        return;
    }
    let seeds = env_number("BRAID_SIM_SEEDS").unwrap_or(MEMORY_SEEDS);
    let report = run_seeds("memory", seeds, |seed| run_memory(seed, &config));
    eprintln!("in memory:\n{report}");
    assert_eq!(report.seeds, seeds);
    assert!(report.landings() > 0, "{report}");
    if seeds >= MEMORY_SEEDS && env_number("BRAID_SIM_AGENTS").is_none() {
        // The default run must keep reaching what it is there for.
        let reached = [
            ("fast-forwards", report.fast_forwards),
            ("rebases", report.rebased),
            ("merged paths", report.merged_paths),
            ("answers of already applied", report.already_applied),
            ("lost answers", report.lost_acks),
            ("duplicate requests", report.duplicates),
            ("resubmissions", report.resubmits),
            ("lander crashes", report.crashes),
            ("redone changes", report.redos),
            ("pairs landed in both orders", report.commutations),
            ("crossings of the flat limit", report.wide_crossings),
        ];
        for (what, count) in reached {
            assert!(count > 0, "the run reached no {what}:\n{report}");
        }
        for kind in [
            "CONTENT",
            "MODIFY_DELETE",
            "MODE",
            "DF",
            "DIR_REMOVED",
            "CASE",
        ] {
            assert!(
                report.conflict_kinds.contains_key(kind),
                "the run reached no {kind} conflict:\n{report}"
            );
        }
    }
}

#[test]
fn landing_on_the_store() {
    let config = store_config();
    let run = |seed: u64| -> Result<Outcome, Failure> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let outcome = run_store(seed, &config, &dir.path().join("store"))?;
        // The same seed in memory makes the same decisions, so main must go
        // through the same changesets.
        let memory = run_memory(seed, &config)?;
        assert_eq!(
            outcome.heads, memory.heads,
            "seed {seed}: the store and memory runs landed different changesets"
        );
        Ok(outcome)
    };
    if let Some(seed) = env_number("BRAID_SIM_SEED") {
        match run(seed) {
            Ok(outcome) => print_run(&format!("seed {seed} on the store"), &outcome),
            Err(failure) => panic!(
                "{failure}\nthe whole run:\n  {}",
                failure.full_trace.join("\n  ")
            ),
        }
        return;
    }
    let seeds = env_number("BRAID_SIM_STORE_SEEDS").unwrap_or(STORE_SEEDS);
    let report = run_seeds("store", seeds, run);
    eprintln!("on the store:\n{report}");
    assert_eq!(report.seeds, seeds);
    assert!(report.landings() > 0);
}
