// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Verification (section 25): keeping a repository's default branch green.
//! The bodies of the routes under `/v1/r/{owner}/{name}/verify`, the closed
//! sets of words they use, and the names of the hub's own refs that say
//! where the branch stands.
//!
//! A reporter, such as a CI job, tells the hub how a suite of tests went on
//! a changeset of the default branch ([`VerifyReport`]). The hub keeps
//! `braid/green/<branch>` at the newest changeset with a pass it accepts
//! and, while the branch is red, `braid/red/<branch>` at the changeset it
//! blames, or at the first fail it cannot explain. It names a culprit only
//! when a single changeset separates a pass from a fail. The hub runs no
//! code of a repository: every verdict comes from outside it.
//!
//! Every field of these bodies has a default, and none refuses a field it
//! does not know, so that a client reads what a newer hub answers. The
//! words of a closed set are strings: a client keeps one it does not know
//! as it came, and takes a state it does not know for one that is not
//! green. Ids are 64 lowercase hex digits; times are RFC 3339, in UTC;
//! logins are lowercase.

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::hex;

/// What `GET /v1` lists in `features` when the hub verifies branches: the
/// routes of section 25 answer, and landings say how the branch stands.
pub const FEATURE: &str = "verify";

/// The prefix of the green refs, `braid/green/<branch>`.
pub const GREEN_PREFIX: &str = "braid/green/";

/// The prefix of the red refs, `braid/red/<branch>`.
pub const RED_PREFIX: &str = "braid/red/";

/// The hub's green ref of `branch`: the newest changeset of the branch with
/// a pass the hub accepts.
pub fn green_ref(branch: &str) -> String {
    format!("{GREEN_PREFIX}{branch}")
}

/// The hub's red ref of `branch`: while the branch is red, the changeset
/// the hub blames, or the first fail it cannot explain.
pub fn red_ref(branch: &str) -> String {
    format!("{RED_PREFIX}{branch}")
}

/// Mode: the hub verifies nothing, and keeps neither ref.
pub const MODE_OFF: &str = "off";

/// Mode: the hub takes the verdicts of its reporters, and keeps the refs.
pub const MODE_REPORT: &str = "report";

/// The modes of a policy.
pub const MODES: [&str; 2] = [MODE_OFF, MODE_REPORT];

/// Source: the verdicts reporters post.
pub const SOURCE_REPORT: &str = "report";

/// The sources a policy's `green_from` may name.
pub const SOURCES: [&str; 1] = [SOURCE_REPORT];

/// Outcome: the suite passed.
pub const PASS: &str = "pass";

/// Outcome: the suite failed.
pub const FAIL: &str = "fail";

/// Outcome: the suite failed, then passed when run again.
pub const FLAKY: &str = "flaky";

/// The outcomes a report gives.
pub const OUTCOMES: [&str; 3] = [PASS, FAIL, FLAKY];

/// State: the policy's mode is `off`.
pub const STATE_OFF: &str = "off";

/// State: nothing is known yet, such as before the first verdict.
pub const STATE_UNKNOWN: &str = "unknown";

/// State: the head of the branch is green.
pub const STATE_GREEN: &str = "green";

/// State: no fail is left unexplained, and the changesets above green wait
/// for a verdict.
pub const STATE_PENDING: &str = "pending";

/// State: a fail is left unexplained; `braid/red/<branch>` names the
/// culprit, or the first such fail.
pub const STATE_RED: &str = "red";

/// State: a revert of the culprit is landing.
pub const STATE_REVERTING: &str = "reverting";

/// State: a revert of the culprit landed, and waits for a verdict.
pub const STATE_REVERTED: &str = "reverted";

/// State: red, and the hub cannot go further by itself; `code` says why.
pub const STATE_STUCK: &str = "stuck";

/// State: verification waits on something outside the branch.
pub const STATE_PAUSED: &str = "paused";

/// The states of a branch, as `VerifyStatus.state` gives them.
pub const STATES: [&str; 9] = [
    STATE_OFF,
    STATE_UNKNOWN,
    STATE_GREEN,
    STATE_PENDING,
    STATE_RED,
    STATE_REVERTING,
    STATE_REVERTED,
    STATE_STUCK,
    STATE_PAUSED,
];

/// Code: the branch is red, and has no green changeset to start from.
pub const CODE_NO_GREEN: &str = "no_green";

/// Code: more than one changeset separates the last pass from the first
/// fail, so none is named the culprit.
pub const CODE_CULPRIT_UNKNOWN: &str = "culprit_unknown";

/// Code: a revert fails itself, so it hides what it reverted.
pub const CODE_HIDDEN: &str = "hidden";

/// Code: more than 10,000 changesets are above green, too many to look at.
pub const CODE_RANGE_TOO_LONG: &str = "range_too_long";

/// Code: green was no longer on the branch's first-parent chain, as after
/// the branch was made again, so the hub started over.
pub const CODE_GREEN_OFF_CHAIN: &str = "green_off_chain";

/// Code: the newest verdict is flaky.
pub const CODE_FLAKY: &str = "flaky";

/// The codes that say why a branch is in its state.
pub const CODES: [&str; 6] = [
    CODE_NO_GREEN,
    CODE_CULPRIT_UNKNOWN,
    CODE_HIDDEN,
    CODE_RANGE_TOO_LONG,
    CODE_GREEN_OFF_CHAIN,
    CODE_FLAKY,
];

/// A culprit named because one changeset separates a pass from a fail.
pub const DECIDED_SINGLE: &str = "single";

/// A culprit named by bisecting.
pub const DECIDED_BISECT: &str = "bisect";

/// How a culprit was named.
pub const DECIDED_BY: [&str; 2] = [DECIDED_SINGLE, DECIDED_BISECT];

/// The window starts at green.
pub const LOWER_GREEN: &str = "green";

/// The window starts at a changeset that passed.
pub const LOWER_PASS: &str = "pass";

/// The window starts at a revert.
pub const LOWER_REVERT: &str = "revert";

/// The window has no lower bound.
pub const LOWER_NONE: &str = "none";

/// What the lower bound of a window is.
pub const LOWER_KINDS: [&str; 4] = [LOWER_GREEN, LOWER_PASS, LOWER_REVERT, LOWER_NONE];

/// Effect: the report turned the branch red.
pub const EFFECT_RED_SET: &str = "red_set";

/// Effect: the report moved green forward.
pub const EFFECT_GREEN_MOVED: &str = "green_moved";

/// Effect: the report ended the branch's red.
pub const EFFECT_RED_CLEARED: &str = "red_cleared";

/// Effect: the report was recorded, and moved no ref.
pub const EFFECT_RECORDED: &str = "recorded";

/// Effect: the hub had this verdict from this reporter already.
pub const EFFECT_DUPLICATE: &str = "duplicate";

/// Effect: the changeset is at or below green: nothing was recorded.
pub const EFFECT_STALE: &str = "stale";

/// What a report did.
pub const EFFECTS: [&str; 6] = [
    EFFECT_RED_SET,
    EFFECT_GREEN_MOVED,
    EFFECT_RED_CLEARED,
    EFFECT_RECORDED,
    EFFECT_DUPLICATE,
    EFFECT_STALE,
];

/// Event: an admin changed the policy.
pub const EVENT_POLICY: &str = "policy";

/// Event: green moved.
pub const EVENT_GREEN: &str = "green";

/// Event: the branch turned red.
pub const EVENT_RED: &str = "red";

/// Event: the hub named a culprit.
pub const EVENT_CULPRIT: &str = "culprit";

/// Event: the branch is no longer red.
pub const EVENT_FIXED: &str = "fixed";

/// Event: the branch is stuck.
pub const EVENT_STUCK: &str = "stuck";

/// Event: green or the fails were reset, by an admin or by the hub.
pub const EVENT_RESET: &str = "reset";

/// Event: a verdict was flaky.
pub const EVENT_FLAKY: &str = "flaky";

/// The events of the log.
pub const EVENTS: [&str; 8] = [
    EVENT_POLICY,
    EVENT_GREEN,
    EVENT_RED,
    EVENT_CULPRIT,
    EVENT_FIXED,
    EVENT_STUCK,
    EVENT_RESET,
    EVENT_FLAKY,
];

/// Notice: the default branch turned red.
pub const NOTICE_RED: &str = "verify_red";

/// Notice: the default branch is no longer red.
pub const NOTICE_FIXED: &str = "verify_fixed";

/// Notice: the default branch is stuck.
pub const NOTICE_STUCK: &str = "verify_stuck";

/// The codes of the notices of verification in the inbox (section 21),
/// whose reason is `system`.
pub const NOTICES: [&str; 3] = [NOTICE_RED, NOTICE_FIXED, NOTICE_STUCK];

/// The longest suite label.
pub const MAX_SUITE: usize = 32;

/// The most reporters a policy names.
pub const MAX_REPORTERS: usize = 10;

/// The largest body of `PUT …/verify/policy`, in bytes.
pub const MAX_POLICY_BODY: u64 = 16 * 1024;

/// The largest body of `POST …/verify/report`, in bytes.
pub const MAX_REPORT_BODY: u64 = 4 * 1024;

/// Whether `text` is a suite label: 1 to 32 of `a-z 0-9 . _ -`, starting
/// with a letter or a digit.
pub fn is_suite(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    bytes.len() <= MAX_SUITE
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
}

/// Whether `text` is a git commit as the protocol writes one: 40 lowercase
/// hex digits.
pub fn is_git_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `GET` and `PUT …/verify/policy`: how a repository's default branch is
/// verified. `PUT` replaces it whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyPolicy {
    /// [`MODE_OFF`] or [`MODE_REPORT`].
    pub mode: String,
    /// The sources whose passes move green: [`SOURCE_REPORT`].
    pub green_from: Vec<String>,
    /// Whether a flaky changeset moves green too.
    pub flaky_moves_green: bool,
    /// Who reports, and on which suite; needed with [`MODE_REPORT`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ReportPolicy>,
}

impl Default for VerifyPolicy {
    /// The policy of a repository whose verification was never set: off.
    fn default() -> Self {
        VerifyPolicy {
            mode: MODE_OFF.to_owned(),
            green_from: Vec::new(),
            flaky_moves_green: false,
            report: None,
        }
    }
}

/// The reporters of a policy, and the suite they report on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportPolicy {
    /// The suite's label (see [`is_suite`]).
    pub suite: String,
    /// The logins whose reports count, 1 to [`MAX_REPORTERS`], lowercase.
    pub reporters: Vec<String>,
}

/// `GET …/verify`: how the default branch stands. Also in the answers of
/// `POST …/verify/report` and `POST …/verify/reset`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyStatus {
    /// The policy's mode.
    pub mode: String,
    /// The branch verified, the default branch; `None` with mode `off`.
    pub branch: Option<String>,
    /// One of [`STATES`].
    pub state: String,
    /// Why, one of [`CODES`], when there is something to say.
    pub code: Option<String>,
    /// The head of the branch the hub last looked at.
    #[serde(with = "hex::opt_id")]
    pub head: Option<ObjectId>,
    /// `braid/green/<branch>`.
    #[serde(with = "hex::opt_id")]
    pub green: Option<ObjectId>,
    /// `braid/red/<branch>`.
    #[serde(with = "hex::opt_id")]
    pub red: Option<ObjectId>,
    /// The changeset the hub blames, when it names one.
    #[serde(with = "hex::opt_id")]
    pub culprit: Option<ObjectId>,
    /// How the culprit was named: one of [`DECIDED_BY`].
    pub decided_by: Option<String>,
    /// Since when the branch is red.
    pub since: Option<String>,
    /// Where the culprit is looked for, while the branch is red.
    pub window: Option<VerifyWindow>,
    /// Verdicts the hub waits for; empty in this version of the protocol.
    pub pending: Vec<PendingVerdict>,
    /// Kept as the hub gave it: `None` in this version of the protocol.
    pub round: Option<serde_json::Value>,
    /// Kept as the hub gave it: `None` in this version of the protocol.
    pub revert: Option<serde_json::Value>,
    /// Kept as the hub gave it: `None` in this version of the protocol.
    pub hold: Option<serde_json::Value>,
    /// What only the repository's admins see.
    pub admin: Option<AdminView>,
}

impl Default for VerifyStatus {
    /// The status of a repository whose verification is off.
    fn default() -> Self {
        VerifyStatus {
            mode: MODE_OFF.to_owned(),
            branch: None,
            state: STATE_OFF.to_owned(),
            code: None,
            head: None,
            green: None,
            red: None,
            culprit: None,
            decided_by: None,
            since: None,
            window: None,
            pending: Vec::new(),
            round: None,
            revert: None,
            hold: None,
            admin: None,
        }
    }
}

/// The changesets a culprit is looked for among: above `lower`, up to and
/// including `upper`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyWindow {
    /// The last changeset known to be good, or `None`.
    #[serde(with = "hex::opt_id")]
    pub lower: Option<ObjectId>,
    /// What it is: one of [`LOWER_KINDS`].
    pub lower_kind: String,
    /// The first fail the hub cannot explain.
    #[serde(with = "hex::opt_id")]
    pub upper: Option<ObjectId>,
    /// How many changesets the window holds.
    pub candidates: u64,
}

impl Default for VerifyWindow {
    fn default() -> Self {
        VerifyWindow {
            lower: None,
            lower_kind: LOWER_NONE.to_owned(),
            upper: None,
            candidates: 0,
        }
    }
}

/// A verdict the hub waits for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingVerdict {
    /// The changeset.
    #[serde(with = "hex::opt_id")]
    pub changeset: Option<ObjectId>,
    /// Where the verdict will come from.
    pub source: String,
    /// Since when the hub waits.
    pub since: String,
}

/// What [`VerifyStatus`] shows the repository's admins only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminView {
    /// The account that landed the culprit, when the hub knows it.
    pub lander: Option<String>,
}

/// `POST …/verify/report`: how a suite went on a changeset of the default
/// branch, named by its git commit or by its id, exactly one of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyReport {
    /// The git commit the changeset exports to, 40 lowercase hex digits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// The changeset.
    #[serde(with = "hex::opt_id", skip_serializing_if = "Option::is_none")]
    pub changeset: Option<ObjectId>,
    /// One of [`OUTCOMES`].
    pub outcome: String,
    /// The suite, as the policy names it.
    pub suite: String,
}

/// The answer of `POST …/verify/report`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyReportAnswer {
    /// What the report did: one of [`EFFECTS`].
    pub effect: String,
    /// How the branch stands now.
    pub status: VerifyStatus,
}

/// One event of `GET …/verify/log`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyEvent {
    /// Its id, which grows with each event.
    pub id: u64,
    /// When.
    pub at: String,
    /// What happened: one of [`EVENTS`].
    pub code: String,
    /// The changeset it is about, if any.
    #[serde(with = "hex::opt_id")]
    pub changeset: Option<ObjectId>,
    /// Another id, or a code, that goes with it.
    pub other: Option<String>,
    /// The login that did it; `None` for the hub, and for callers who are
    /// not the repository's admins.
    pub actor: Option<String>,
}

/// `GET …/verify/log`: a page of events, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyEvents {
    /// The events.
    pub events: Vec<VerifyEvent>,
    /// The `after` of the next page, or `None` on the last one.
    pub next: Option<u64>,
}

/// `POST …/verify/reset`: move green, or dismiss the fails there are now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyReset {
    /// Where green goes, on the head's first-parent chain, backward too:
    /// the verdicts given before the reset on the changesets above it are
    /// forgotten, so that only new ones move green or bring red back.
    /// `None` dismisses the fails there are now instead.
    #[serde(with = "hex::opt_id")]
    pub green: Option<ObjectId>,
    /// What the caller saw: when the refs moved since, the hub answers 409
    /// `moved` and does nothing.
    pub expect: Option<ResetExpect>,
}

/// The refs a reset expects.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResetExpect {
    /// `braid/green/<branch>`, or `None` when there is none.
    #[serde(with = "hex::opt_id")]
    pub green: Option<ObjectId>,
    /// `braid/red/<branch>`, or `None` when there is none.
    #[serde(with = "hex::opt_id")]
    pub red: Option<ObjectId>,
}

/// `verify` in the answer of a landing on the default branch of a
/// repository whose verification is on: how the branch stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LandVerify {
    /// One of [`STATES`].
    pub state: String,
    /// `braid/green/<branch>`.
    #[serde(with = "hex::opt_id")]
    pub green: Option<ObjectId>,
    /// `braid/red/<branch>`.
    #[serde(with = "hex::opt_id")]
    pub red: Option<ObjectId>,
    /// The changeset the hub blames, when it names one.
    #[serde(with = "hex::opt_id")]
    pub culprit: Option<ObjectId>,
    /// Reverts the hub landed that the landing was rebased over; empty in
    /// this version of the protocol.
    pub reverted: Vec<RevertedPair>,
}

/// A changeset, and the revert of it the hub landed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RevertedPair {
    /// The changeset reverted.
    #[serde(with = "hex::opt_id")]
    pub culprit: Option<ObjectId>,
    /// The revert.
    #[serde(with = "hex::opt_id")]
    pub revert: Option<ObjectId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    fn id(text: &[u8]) -> ObjectId {
        ObjectId::hash(Kind::Changeset, text)
    }

    /// Written and read again, `value` is itself.
    fn round_trips<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let text = serde_json::to_string(value).unwrap();
        assert_eq!(&serde_json::from_str::<T>(&text).unwrap(), value, "{text}");
    }

    #[test]
    fn refs_and_closed_sets() {
        assert_eq!(green_ref("main"), "braid/green/main");
        assert_eq!(red_ref("release/1"), "braid/red/release/1");
        assert!(green_ref("x").starts_with(GREEN_PREFIX));
        assert!(red_ref("x").starts_with(RED_PREFIX));
        // Each set holds each word once, and the words are what the wire
        // says.
        for set in [
            &MODES[..],
            &SOURCES,
            &OUTCOMES,
            &STATES,
            &CODES,
            &DECIDED_BY,
            &LOWER_KINDS,
            &EFFECTS,
            &EVENTS,
            &NOTICES,
        ] {
            let mut words = set.to_vec();
            words.sort_unstable();
            words.dedup();
            assert_eq!(words.len(), set.len(), "{set:?}");
            for word in set {
                assert!(
                    word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "{word}"
                );
            }
        }
        assert!(NOTICES.iter().all(|code| code.starts_with("verify_")));
        assert_eq!(FEATURE, "verify");
    }

    #[test]
    fn suites_and_commits() {
        for good in ["ci", "0", "unit-tests", "a.b_c-d", &"a".repeat(32)] {
            assert!(is_suite(good), "{good:?}");
        }
        for bad in [
            "",
            "CI",
            "-ci",
            ".ci",
            "_ci",
            "c i",
            "ci/x",
            "é",
            &"a".repeat(33),
        ] {
            assert!(!is_suite(bad), "{bad:?}");
        }
        assert!(is_git_commit(&"0a".repeat(20)));
        for bad in [
            "",
            &"0A".repeat(20),
            &"0".repeat(39),
            &"0".repeat(64),
            &"g".repeat(40),
        ] {
            assert!(!is_git_commit(bad), "{bad:?}");
        }
    }

    #[test]
    fn every_body_round_trips() {
        let (a, b, c) = (id(b"a"), id(b"b"), id(b"c"));
        round_trips(&VerifyPolicy {
            mode: MODE_REPORT.into(),
            green_from: vec![SOURCE_REPORT.into()],
            flaky_moves_green: true,
            report: Some(ReportPolicy {
                suite: "ci".into(),
                reporters: vec!["ci-bot".into()],
            }),
        });
        round_trips(&VerifyPolicy::default());
        let status = VerifyStatus {
            mode: MODE_REPORT.into(),
            branch: Some("main".into()),
            state: STATE_RED.into(),
            code: Some(CODE_CULPRIT_UNKNOWN.into()),
            head: Some(c),
            green: Some(a),
            red: Some(c),
            culprit: None,
            decided_by: None,
            since: Some("2026-10-22T12:03:00Z".into()),
            window: Some(VerifyWindow {
                lower: Some(a),
                lower_kind: LOWER_GREEN.into(),
                upper: Some(c),
                candidates: 2,
            }),
            pending: vec![PendingVerdict {
                changeset: Some(b),
                source: SOURCE_REPORT.into(),
                since: "2026-10-22T12:00:00Z".into(),
            }],
            round: Some(json!({"n": 4})),
            revert: None,
            hold: None,
            admin: Some(AdminView {
                lander: Some("ada".into()),
            }),
        };
        round_trips(&status);
        round_trips(&VerifyStatus::default());
        round_trips(&VerifyReport {
            git_commit: Some("0a".repeat(20)),
            changeset: None,
            outcome: FAIL.into(),
            suite: "ci".into(),
        });
        round_trips(&VerifyReport {
            git_commit: None,
            changeset: Some(b),
            outcome: PASS.into(),
            suite: "ci".into(),
        });
        round_trips(&VerifyReportAnswer {
            effect: EFFECT_RED_SET.into(),
            status: status.clone(),
        });
        round_trips(&VerifyEvents {
            events: vec![VerifyEvent {
                id: 17,
                at: "2026-10-22T12:03:00Z".into(),
                code: EVENT_RED.into(),
                changeset: Some(c),
                other: Some(CODE_CULPRIT_UNKNOWN.into()),
                actor: None,
            }],
            next: Some(17),
        });
        round_trips(&VerifyReset {
            green: Some(a),
            expect: Some(ResetExpect {
                green: Some(b),
                red: None,
            }),
        });
        round_trips(&VerifyReset::default());
        round_trips(&LandVerify {
            state: STATE_RED.into(),
            green: Some(a),
            red: Some(c),
            culprit: Some(c),
            reverted: vec![RevertedPair {
                culprit: Some(b),
                revert: Some(c),
            }],
        });
    }

    #[test]
    fn on_the_wire() {
        let a = id(b"a");
        // A report names exactly what it was given.
        let report = VerifyReport {
            git_commit: Some("0a".repeat(20)),
            changeset: None,
            outcome: PASS.into(),
            suite: "ci".into(),
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"git_commit": "0a".repeat(20), "outcome": "pass", "suite": "ci"})
        );
        // A policy never set says so, and leaves out the block it lacks.
        assert_eq!(
            serde_json::to_value(VerifyPolicy::default()).unwrap(),
            json!({"mode": "off", "green_from": [], "flaky_moves_green": false})
        );
        // Ids the hub has none of are null, and so are the parts of the
        // status this version of the protocol leaves empty.
        let status = serde_json::to_value(VerifyStatus::default()).unwrap();
        for key in ["branch", "head", "green", "red", "culprit", "round", "hold"] {
            assert_eq!(status[key], Value::Null, "{key}");
        }
        assert_eq!(status["state"], "off");
        assert_eq!(status["pending"], json!([]));
        // A reset that expects no green says so with null.
        let reset = VerifyReset {
            green: None,
            expect: Some(ResetExpect {
                green: None,
                red: Some(a),
            }),
        };
        assert_eq!(
            serde_json::to_value(&reset).unwrap(),
            json!({"green": null, "expect": {"green": null, "red": a.to_hex()}})
        );
        // A bad id is refused.
        assert!(serde_json::from_str::<LandVerify>(r#"{"state":"red","red":"xyz"}"#).is_err());
    }

    #[test]
    fn what_a_hub_leaves_out_or_adds_still_reads() {
        // An answer of a newer hub, with fields this crate does not know.
        let status: VerifyStatus = serde_json::from_str(
            r#"{"mode":"auto","state":"held","hold":{"until":"x"},"later":[1,2],"window":{"upper":null,"later":true}}"#,
        )
        .unwrap();
        assert_eq!(
            (status.mode.as_str(), status.state.as_str()),
            ("auto", "held")
        );
        assert_eq!(status.hold, Some(json!({"until": "x"})));
        assert_eq!(status.window.unwrap().lower_kind, LOWER_NONE);
        // A body with nothing in it takes every default.
        assert_eq!(
            serde_json::from_str::<VerifyStatus>("{}").unwrap(),
            VerifyStatus::default()
        );
        assert_eq!(
            serde_json::from_str::<VerifyPolicy>("{}").unwrap(),
            VerifyPolicy::default()
        );
        let answer: VerifyReportAnswer = serde_json::from_str(r#"{"effect":"stale"}"#).unwrap();
        assert_eq!(answer.status.state, STATE_OFF);
        let land: LandVerify = serde_json::from_str(r#"{"state":"green"}"#).unwrap();
        assert!(land.reverted.is_empty() && land.culprit.is_none());
        let events: VerifyEvents = serde_json::from_str(r#"{"events":[{"id":3}]}"#).unwrap();
        assert_eq!((events.events[0].id, events.next), (3, None));
    }
}
