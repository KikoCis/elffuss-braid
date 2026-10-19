// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Discovery: explore, search, topics and profiles (section 16).
//!
//! Explore and search list public repositories that landed something, of
//! owners established on the hub. A query is at most [`MAX_QUERY`]
//! characters, in at most [`MAX_TERMS`] terms, and each term matches the
//! start of a word of a repository's owner, name, description or topics:
//! [`terms`] says which queries a hub takes, so that a client can check
//! one before it sends it. Lists come a page at a time, at most
//! [`MAX_PAGES`] pages of one query.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The most characters of a query.
pub const MAX_QUERY: usize = 100;

/// The most terms of a query.
pub const MAX_TERMS: usize = 10;

/// The most pages of one query of explore, search or topics: past them,
/// a page answers no `next`.
pub const MAX_PAGES: usize = 50;

/// The order of best matches first: the default with a query.
pub const MATCH: &str = "match";

/// The order of the latest landing first: the default without a query.
pub const RECENT: &str = "recent";

/// The order of the most stars first.
pub const STARS: &str = "stars";

/// The order of the newest repositories first.
pub const NEW: &str = "new";

/// The orders of explore and search.
pub const SORTS: [&str; 4] = [MATCH, RECENT, STARS, NEW];

/// Every repository of an account the caller may read: the default `type`.
pub const ALL: &str = "all";

/// Its public repositories only.
pub const PUBLIC: &str = "public";

/// Its private repositories only, of those the caller may read.
pub const PRIVATE: &str = "private";

/// Its repositories that are not forks.
pub const SOURCES: &str = "sources";

/// Its forks.
pub const FORKS: &str = "forks";

/// The kinds of an account's repositories a list holds (`type`).
pub const REPO_TYPES: [&str; 5] = [ALL, PUBLIC, PRIVATE, SOURCES, FORKS];

/// An account's repositories by their latest landing, newest first, those
/// that never landed last: the default `sort`.
pub const PUSHED: &str = "pushed";

/// By name.
pub const NAME: &str = "name";

/// By when they were made, newest first.
pub const CREATED: &str = "created";

/// The orders of an account's repositories (`sort`).
pub const REPO_SORTS: [&str; 3] = [PUSHED, NAME, CREATED];

/// Why a query is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// It holds more than [`MAX_QUERY`] characters.
    TooLong,
    /// It holds more than [`MAX_TERMS`] terms.
    TooManyTerms,
    /// It holds a character no term takes.
    Character(char),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::TooLong => write!(f, "a query holds at most {MAX_QUERY} characters"),
            QueryError::TooManyTerms => write!(f, "a query holds at most {MAX_TERMS} terms"),
            QueryError::Character(c) => write!(
                f,
                "a query holds terms of letters, digits, '.', '_' and '-', between spaces or '/', not {c:?}"
            ),
        }
    }
}

impl std::error::Error for QueryError {}

/// Whether a term may hold `c`: a letter, a digit, `.`, `_` or `-`.
pub fn is_term_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The terms of the query `query`: its words between spaces and `/`, so
/// that `owner/name` finds a repository by both. A word of `.`, `_` and
/// `-` alone matches nothing, and is left out; a query left with no term
/// lists what a query-less one lists.
pub fn terms(query: &str) -> Result<Vec<String>, QueryError> {
    if query.chars().count() > MAX_QUERY {
        return Err(QueryError::TooLong);
    }
    let mut terms = Vec::new();
    for word in query.split(|c: char| c.is_whitespace() || c == '/') {
        if let Some(bad) = word.chars().find(|c| !is_term_char(*c)) {
            return Err(QueryError::Character(bad));
        }
        if word.chars().any(char::is_alphanumeric) {
            terms.push(word.to_owned());
        }
    }
    if terms.len() > MAX_TERMS {
        return Err(QueryError::TooManyTerms);
    }
    Ok(terms)
}

/// A topic, and how many listed repositories have it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicCount {
    /// The topic: lowercase letters, digits and `-`.
    pub name: String,
    /// How many public repositories of established owners have it, forks
    /// left out.
    pub repos: u64,
}

/// `GET /v1/topics`: topics, the most used first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topics {
    /// The topics.
    pub topics: Vec<TopicCount>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `GET /v1/users/{login}`: an account, as the caller may see it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Its login, lowercase.
    pub login: String,
    /// Its display name, if any.
    #[serde(default)]
    pub name: Option<String>,
    /// When the account was made.
    pub created: String,
    /// The accounts that follow it.
    #[serde(default)]
    pub followers: u64,
    /// The accounts it follows.
    #[serde(default)]
    pub following: u64,
    /// Its repositories the caller may read.
    #[serde(default)]
    pub repos: u64,
    /// Its stars of repositories the caller may read, or `None` when it
    /// keeps its stars from the caller.
    #[serde(default)]
    pub stars: Option<u64>,
    /// What the caller itself does with the account, for a signed-in
    /// caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<ProfileViewer>,
}

/// What the caller does with an account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileViewer {
    /// Whether it follows it.
    pub follows: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_split_into_terms() {
        assert_eq!(terms("").unwrap(), Vec::<String>::new());
        assert_eq!(terms("  rust   cli ").unwrap(), ["rust", "cli"]);
        assert_eq!(terms("ada/demo").unwrap(), ["ada", "demo"]);
        assert_eq!(
            terms("my-project v1.2 a_b").unwrap(),
            ["my-project", "v1.2", "a_b"]
        );
        assert_eq!(terms("café\tNEAR OR").unwrap(), ["café", "NEAR", "OR"]);
        // Punctuation alone is no term.
        assert_eq!(terms("- . _ x").unwrap(), ["x"]);
        assert_eq!(terms("---").unwrap(), Vec::<String>::new());
        for (bad, c) in [
            ("\"rust\"", '"'),
            ("rust*", '*'),
            ("(a)", '('),
            ("a:b", ':'),
            ("a^b", '^'),
            ("a\0b", '\0'),
            ("%_", '%'),
        ] {
            assert_eq!(terms(bad), Err(QueryError::Character(c)), "{bad:?}");
        }
        let long = "a".repeat(MAX_QUERY + 1);
        assert_eq!(terms(&long), Err(QueryError::TooLong));
        // Characters, not bytes.
        assert!(terms(&"é".repeat(MAX_QUERY)).is_ok());
        let many = ["a"; MAX_TERMS + 1].join(" ");
        assert_eq!(terms(&many), Err(QueryError::TooManyTerms));
        assert!(terms(&["a"; MAX_TERMS].join(" ")).is_ok());
        assert!(
            QueryError::Character('\u{7}')
                .to_string()
                .contains("\\u{7}")
        );
    }

    #[test]
    fn discovery_on_the_wire() {
        let hidden = Profile {
            login: "ada".into(),
            created: "2026-01-01T00:00:00Z".into(),
            repos: 2,
            ..Profile::default()
        };
        let text = serde_json::to_string(&hidden).unwrap();
        // Stars kept private are null, not left out; a viewer is.
        assert!(text.contains(r#""stars":null"#), "{text}");
        assert!(!text.contains("viewer"), "{text}");
        assert_eq!(serde_json::from_str::<Profile>(&text).unwrap(), hidden);
        let seen = Profile {
            stars: Some(3),
            viewer: Some(ProfileViewer { follows: true }),
            ..hidden
        };
        let text = serde_json::to_string(&seen).unwrap();
        assert!(text.contains(r#""viewer":{"follows":true}"#), "{text}");
        let topics: Topics =
            serde_json::from_str(r#"{"topics":[{"name":"rust","repos":4}]}"#).unwrap();
        assert_eq!(topics.topics[0].repos, 4);
        assert_eq!(topics.next, None);
        for sort in SORTS {
            assert!(!sort.is_empty());
        }
        assert_eq!(REPO_TYPES.len(), 5);
        assert_eq!(REPO_SORTS[0], PUSHED);
    }
}
