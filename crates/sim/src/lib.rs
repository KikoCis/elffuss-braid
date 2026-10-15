// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Deterministic simulation of many agents landing changes on one branch,
//! checked against a reference model after every step.
//!
//! # What runs
//!
//! The system under test is the engine's [`commit`] and [`land`] over an
//! object store, and a lander: a serialized section that owns the head of
//! `main`, reads it, calls [`land`], and moves the branch with a
//! compare-and-swap, as the command line will do inside
//! [`Store::transact`]. The store is either in memory with a simulated ref
//! ([`MemoryBackend`]) or the real local store in a directory
//! ([`StoreBackend`]), where the lander's section is a real transaction and
//! every agent has its own handle.
//!
//! Agents, eight by default, each loop: take a snapshot of `main`, make a
//! small random change, commit it on the snapshot, and ask the lander to land
//! it. Changes add, edit and delete files, often the same files other agents
//! edit; create and remove directories; put files where directories are and
//! the other way round; add names that differ from others only in case or
//! Unicode normalisation; and, in some runs, move a directory of about a
//! thousand files across the largest flat directory, 1,024 entries. On a
//! conflict an agent takes a new snapshot and either redoes its edits there
//! or puts its own files over the head's.
//!
//! The scheduler interleaves agents and the lander in an order the seed
//! decides, with pauses long enough for bases to go stale, and injects
//! faults: answers lost on the way back (the agent times out and sends the
//! same changeset again), requests delivered twice, agents that send a
//! changeset again after it landed, and lander crashes after the outcome is
//! computed and before the compare-and-swap.
//!
//! # The model
//!
//! A state is a map from path to file. The model predicts every landing on
//! its own: per path, the change's file where the head left the base's, the
//! head's where it already has the change's, and otherwise a line merge or a
//! conflict; then the structural rules of `docs/design.md`. Its line merge
//! is written independently and must agree with the engine's `merge3`, hunks
//! included. The engine's answer must equal the model's: the outcome, the
//! merged paths, the rebased change list, the conflicts with their entries
//! and hunks, and the tree.
//!
//! # Invariants
//!
//! Checked after every step, and once more at the end after the store is
//! opened again:
//!
//! - I1: the head only moves forward; every new head has the old head as its
//!   first parent.
//! - I2: `main`'s first-parent history, and its op log, are the sequence of
//!   successful landings.
//! - I3: no lost update: the head's tree is the model's state, file by file.
//! - I4: each submitted changeset lands at most once, by id or by
//!   `rebased-from`, whatever the retries and duplicates.
//! - I5: a reported conflict leaves the head unchanged.
//! - I6: every changeset on `main`, and every changeset an agent submits,
//!   passes `admit`.
//! - I7: two changes on unrelated paths, built on the same base, land the
//!   same in either order: replayed away from the branch in both orders.
//!
//! A failure names the seed, the check and the step, with the events about
//! the changesets involved; the same seed replays the same run.
//!
//! [`commit`]: braid_engine::commit
//! [`land`]: braid_engine::land
//! [`Store::transact`]: braid_store::Store::transact

#![forbid(unsafe_code)]

mod backend;
mod model;
mod ops;
mod rng;
mod sim;
mod trace;

use std::path::Path;

pub use backend::{Backend, LandRequest, Landed, MAIN, MemoryBackend, Move, StoreBackend};
pub use sim::{Config, Failure, Outcome, Report, run};

/// Runs one seed with objects in memory and a simulated ref.
pub fn run_memory(seed: u64, config: &Config) -> Result<Outcome, Failure> {
    run(seed, config, MemoryBackend::new())
}

/// Runs one seed on a real store, created in `dir`.
pub fn run_store(seed: u64, config: &Config, dir: &Path) -> Result<Outcome, Failure> {
    let backend = StoreBackend::create(dir, config.agents).map_err(|error| Failure {
        seed,
        backend: "store",
        step: 0,
        invariant: "engine",
        message: format!("cannot create the store: {error}"),
        trace: Vec::new(),
        full_trace: Vec::new(),
    })?;
    run(seed, config, backend)
}
