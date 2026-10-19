// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The short forms of repositories and accounts that lists hold: explore
//! and search, topics, profiles, stars, watching, followers and forks. A
//! list holds at most one page, and `next`, when there is more, is the
//! opaque cursor that asks for the page after it (`?after=`).

use serde::{Deserialize, Serialize};

/// A repository, named by its owner and its name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// The owner's login, lowercase.
    pub owner: String,
    /// The name.
    pub name: String,
}

/// A repository in a list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCard {
    /// The owner's login, lowercase.
    pub owner: String,
    /// The name.
    pub name: String,
    /// Whether only those given access can read it.
    pub private: bool,
    /// A description.
    #[serde(default)]
    pub description: String,
    /// Its topics.
    #[serde(default)]
    pub topics: Vec<String>,
    /// The accounts that starred it.
    #[serde(default)]
    pub stars: u64,
    /// Its forks the caller may see.
    #[serde(default)]
    pub forks: u64,
    /// When it was made.
    pub created: String,
    /// When a landing last moved one of its refs, if one ever did.
    #[serde(default)]
    pub pushed: Option<String>,
    /// The branch the web shows.
    pub default_branch: String,
    /// The repository it was forked from, when the caller may read that
    /// one.
    #[serde(default)]
    pub fork_of: Option<RepoRef>,
    /// The public GitHub repository it mirrors to, if any.
    #[serde(default)]
    pub mirror_url: Option<String>,
    /// Text and data mining of its content: `allowed` or `reserved`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tdm: String,
    /// In a list of stars: when the account starred it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred_at: Option<String>,
    /// In a list of watched repositories: `all`, `participating` or
    /// `ignore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// An account in a list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCard {
    /// Its login, lowercase.
    pub login: String,
    /// Its display name, if any.
    #[serde(default)]
    pub name: Option<String>,
    /// Since when it is in the list: when it starred, followed or was
    /// followed.
    pub since: String,
}

/// A page of repositories.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCards {
    /// The repositories.
    pub repos: Vec<RepoCard>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// A page of accounts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCards {
    /// The accounts.
    pub users: Vec<UserCard>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_on_the_wire() {
        let card = RepoCard {
            owner: "ada".into(),
            name: "demo".into(),
            created: "2026-01-01T00:00:00Z".into(),
            default_branch: "main".into(),
            tdm: "allowed".into(),
            fork_of: Some(RepoRef {
                owner: "bob".into(),
                name: "demo".into(),
            }),
            ..RepoCard::default()
        };
        let text = serde_json::to_string(&card).unwrap();
        assert!(
            !text.contains("starred_at") && !text.contains("level"),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<RepoCard>(&text).unwrap(), card);
        // A short card, from a hub that says less, still reads.
        let short: RepoCard = serde_json::from_str(
            r#"{"owner":"ada","name":"x","private":false,"created":"t","default_branch":"main"}"#,
        )
        .unwrap();
        assert_eq!((short.stars, short.tdm.as_str()), (0, ""));
        let page: UserCards =
            serde_json::from_str(r#"{"users":[{"login":"eve","since":"t"}]}"#).unwrap();
        assert_eq!((page.users[0].name.as_deref(), page.next), (None, None));
    }
}
