// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Blocks (section 22) and the account's settings (section 3).
//!
//! An account blocks another from every repository it owns; an admin of a
//! repository blocks an account from that one alone. A blocked account
//! still reads what is public, and cannot star, watch, comment, open
//! threads, review or fork there, nor follow whoever blocked it. The lists
//! of blocks are [`UserCards`](crate::cards::UserCards): each account with
//! `since`, when it was blocked.

use serde::{Deserialize, Serialize};

/// The most accounts one account blocks, and the most one repository
/// blocks.
pub const MAX_BLOCKS: usize = 1_000;

/// Text and data mining allowed: what a new repository starts with unless
/// its owner says otherwise.
pub const TDM_ALLOWED: &str = "allowed";

/// Text and data mining reserved by the owner.
pub const TDM_RESERVED: &str = "reserved";

/// `PATCH /v1/me`: the settings of the account to change. Those left out
/// stay as they are.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MePatch {
    /// What text and data mining the account's new repositories start
    /// with: [`TDM_ALLOWED`] or [`TDM_RESERVED`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdm_default: Option<String>,
    /// Whether the repositories the account starred show on its profile
    /// and in its list of stars to others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stars_public: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_says_only_what_it_changes() {
        let empty = MePatch::default();
        assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
        let patch: MePatch = serde_json::from_str(r#"{"stars_public":false}"#).unwrap();
        assert_eq!(
            patch,
            MePatch {
                tdm_default: None,
                stars_public: Some(false),
            }
        );
        let both = MePatch {
            tdm_default: Some(TDM_RESERVED.into()),
            stars_public: Some(true),
        };
        let text = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<MePatch>(&text).unwrap(), both);
        assert!(serde_json::from_str::<MePatch>(r#"{"stars_public":"no"}"#).is_err());
    }
}
