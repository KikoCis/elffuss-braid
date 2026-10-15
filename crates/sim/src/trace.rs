// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What happened during a run, one event per line: printed in full when a
//! seed is replayed, and cut down to the changesets involved when a check
//! fails.

use std::collections::BTreeSet;

use braid_format::ObjectId;

/// One event.
#[derive(Clone, Debug)]
pub struct Event {
    /// The scheduler step it happened in.
    pub step: u64,
    /// The simulated time.
    pub time: u64,
    /// Who acted: an agent, the lander, a check.
    pub actor: String,
    /// What happened.
    pub text: String,
    /// The changesets it is about.
    pub ids: Vec<ObjectId>,
}

impl Event {
    /// The event as one line.
    pub fn line(&self) -> String {
        format!(
            "step {:>5}  t{:<6} {:<9} {}",
            self.step, self.time, self.actor, self.text
        )
    }
}

/// Every event of a run, in order.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    events: Vec<Event>,
}

impl Trace {
    /// Records an event.
    pub fn push(&mut self, step: u64, time: u64, actor: &str, text: String, ids: &[ObjectId]) {
        self.events.push(Event {
            step,
            time,
            actor: actor.to_owned(),
            text,
            ids: ids.to_vec(),
        });
    }

    /// Every event as a line.
    pub fn lines(&self) -> Vec<String> {
        self.events.iter().map(Event::line).collect()
    }

    /// The events about any of `ids`, the first event, and the last `tail`
    /// events, as lines.
    pub fn about(&self, ids: &BTreeSet<ObjectId>, tail: usize) -> Vec<String> {
        let from = self.events.len().saturating_sub(tail);
        self.events
            .iter()
            .enumerate()
            .filter(|(at, event)| {
                *at == 0 || *at >= from || event.ids.iter().any(|id| ids.contains(id))
            })
            .map(|(_, event)| event.line())
            .collect()
    }
}

/// The first hex digits of an id, enough to tell the changesets of a run
/// apart.
pub fn short(id: ObjectId) -> String {
    id.to_hex()[..10].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    #[test]
    fn about_keeps_the_relevant_events() {
        let a = ObjectId::hash(Kind::Changeset, b"a");
        let b = ObjectId::hash(Kind::Changeset, b"b");
        let mut trace = Trace::default();
        trace.push(0, 0, "setup", "start".to_owned(), &[]);
        trace.push(1, 1, "agent-1", "about a".to_owned(), &[a]);
        trace.push(2, 2, "agent-2", "about b".to_owned(), &[b]);
        trace.push(3, 3, "lander", "about nothing".to_owned(), &[]);
        trace.push(4, 4, "lander", "last".to_owned(), &[]);
        let lines = trace.about(&BTreeSet::from([a]), 1);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("start"));
        assert!(lines[1].ends_with("about a"));
        assert!(lines[2].ends_with("last"));
        assert_eq!(trace.lines().len(), 5);
        assert_eq!(short(a).len(), 10);
    }
}
