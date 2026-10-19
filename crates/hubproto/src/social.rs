// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Stars, watching and following (section 17): how an account watches a
//! repository. The lists of this section, of stars, stargazers, watched
//! repositories and follows, are [`RepoCards`](crate::cards::RepoCards)
//! and [`UserCards`](crate::cards::UserCards).

use serde::{Deserialize, Serialize};

/// Watching everything that happens in a repository: what its owner
/// watches unless it chooses otherwise.
pub const ALL: &str = "all";

/// Watching only the threads one takes part in or is mentioned in: what
/// everyone but the owner watches unless they choose otherwise.
pub const PARTICIPATING: &str = "participating";

/// Watching nothing of a repository, not even the threads one takes part
/// in.
pub const IGNORE: &str = "ignore";

/// The levels an account may watch a repository at.
pub const LEVELS: [&str; 3] = [ALL, PARTICIPATING, IGNORE];

/// Whether `text` is one of the [`LEVELS`].
pub fn is_level(text: &str) -> bool {
    LEVELS.contains(&text)
}

/// `GET …/subscription`, the answer of `PUT …/subscription`, and its body
/// (where `since` is left out): how the caller watches a repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// `all`, `participating` or `ignore`.
    pub level: String,
    /// When the caller chose this level; absent while it watches as an
    /// account does by default (see [`ALL`] and [`PARTICIPATING`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_subscriptions_on_the_wire() {
        for level in LEVELS {
            assert!(is_level(level));
        }
        for other in ["", "All", "none", "watching", "all "] {
            assert!(!is_level(other), "{other:?}");
        }
        let chosen = Subscription {
            level: IGNORE.into(),
            since: Some("2026-10-18T12:00:00Z".into()),
        };
        let text = serde_json::to_string(&chosen).unwrap();
        assert_eq!(serde_json::from_str::<Subscription>(&text).unwrap(), chosen);
        // A body that names the level alone, as a client sends it.
        let body: Subscription = serde_json::from_str(r#"{"level":"all"}"#).unwrap();
        assert_eq!((body.level.as_str(), body.since), (ALL, None));
        let default = Subscription {
            level: PARTICIPATING.into(),
            since: None,
        };
        assert_eq!(
            serde_json::to_string(&default).unwrap(),
            r#"{"level":"participating"}"#
        );
    }
}
