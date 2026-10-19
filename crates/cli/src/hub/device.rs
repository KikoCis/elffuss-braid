// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The waiting half of the device flow (`docs/hub.md`, section 2): polling
//! the hub every `interval` seconds until the person approves, refuses, or
//! lets the code expire.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use braid_hubproto::Token;
use braid_hubproto::api::{DevicePoll, DeviceStart};

use super::remote::HubError;

/// What polling needs from the hub.
pub(crate) trait DeviceFlow {
    /// `POST /v1/login/device/poll {"poll"}`.
    fn poll(&self, poll: &str) -> Result<DevicePoll, HubError>;
}

/// A sign-in the person approved. Its debug form shows the token's id
/// only.
#[derive(Debug)]
pub(crate) struct SignedIn {
    /// The new token.
    pub(crate) token: Token,
    /// The account's GitHub login.
    pub(crate) login: String,
    /// When the token expires.
    pub(crate) expires: String,
}

/// Polls `flow` for the flow `start` began, sleeping with `sleep` between
/// polls: `interval` seconds, more after `slow_down` or a rate limit, until
/// `expires_in` seconds have been waited.
pub(crate) fn wait_for_approval(
    flow: &impl DeviceFlow,
    start: &DeviceStart,
    mut sleep: impl FnMut(Duration),
) -> Result<SignedIn> {
    let expired =
        || anyhow!("the code expired before the sign-in was approved: run `braid login` again");
    let mut interval = start.interval.max(1);
    let mut waited = 0u64;
    loop {
        if waited >= start.expires_in {
            return Err(expired());
        }
        sleep(Duration::from_secs(interval));
        waited = waited.saturating_add(interval);
        match flow.poll(&start.poll) {
            Ok(DevicePoll::Pending) => {}
            Ok(DevicePoll::SlowDown { interval: asked }) => interval = asked.max(interval),
            Ok(DevicePoll::Denied) => {
                bail!("the sign-in was denied: run `braid login` again to start over")
            }
            Ok(DevicePoll::Expired) => return Err(expired()),
            Ok(DevicePoll::Ok {
                token,
                login,
                expires,
            }) => {
                let token = Token::parse(&token)
                    .map_err(|_| anyhow!("the hub sent something that is not a braid hub token"))?;
                return Ok(SignedIn {
                    token,
                    login,
                    expires,
                });
            }
            // Asked to wait: wait as long as the hub says, then poll again.
            Err(HubError::Api { status: 429, body }) => {
                interval = body.retry_after.unwrap_or(interval).max(interval);
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A hub that answers from a script.
    struct Script(RefCell<Vec<Result<DevicePoll, HubError>>>);

    impl DeviceFlow for Script {
        fn poll(&self, poll: &str) -> Result<DevicePoll, HubError> {
            assert_eq!(poll, "flow-1");
            self.0.borrow_mut().remove(0)
        }
    }

    fn start(interval: u64, expires_in: u64) -> DeviceStart {
        DeviceStart {
            poll: "flow-1".to_owned(),
            user_code: "ABCD-1234".to_owned(),
            verification_uri: "https://github.com/login/device".to_owned(),
            expires_in,
            interval,
        }
    }

    fn run(
        answers: Vec<Result<DevicePoll, HubError>>,
        start: &DeviceStart,
    ) -> (Result<SignedIn>, Vec<u64>) {
        let script = Script(RefCell::new(answers));
        let mut slept = Vec::new();
        let result = wait_for_approval(&script, start, |d| slept.push(d.as_secs()));
        (result, slept)
    }

    #[test]
    fn polls_at_the_interval_and_slows_down_when_told() {
        let token = Token::from_random([1; 5], [2; 32]);
        let mut limited = HubError::api(429, "rate_limited", "wait");
        if let HubError::Api { body, .. } = &mut limited {
            body.retry_after = Some(20);
        }
        let (result, slept) = run(
            vec![
                Ok(DevicePoll::Pending),
                Ok(DevicePoll::SlowDown { interval: 10 }),
                Ok(DevicePoll::Pending),
                Err(limited),
                Ok(DevicePoll::Ok {
                    token: token.expose(),
                    login: "ada".to_owned(),
                    expires: "2027-01-01T00:00:00Z".to_owned(),
                }),
            ],
            &start(5, 900),
        );
        let signed = result.unwrap();
        assert_eq!(signed.token, token);
        assert_eq!(signed.login, "ada");
        assert_eq!(slept, [5, 5, 10, 10, 20]);
    }

    #[test]
    fn denied_expired_and_out_of_time() {
        let (result, _) = run(vec![Ok(DevicePoll::Denied)], &start(1, 60));
        assert!(result.unwrap_err().to_string().contains("denied"));
        let (result, _) = run(
            vec![Ok(DevicePoll::Pending), Ok(DevicePoll::Expired)],
            &start(1, 60),
        );
        assert!(result.unwrap_err().to_string().contains("expired"));
        // The code's lifetime bounds the wait, whatever the hub says.
        let (result, slept) = run(
            vec![Ok(DevicePoll::Pending), Ok(DevicePoll::Pending)],
            &start(5, 10),
        );
        assert!(result.unwrap_err().to_string().contains("expired"));
        assert_eq!(slept, [5, 5]);
        // An interval of 0 still waits a second between polls.
        let (_, slept) = run(vec![Ok(DevicePoll::Denied)], &start(0, 60));
        assert_eq!(slept, [1]);
        // Other errors stop at once.
        let (result, _) = run(
            vec![Err(HubError::api(500, "internal", "broken"))],
            &start(1, 60),
        );
        assert!(result.unwrap_err().to_string().contains("broken"));
        // A malformed token is refused.
        let (result, _) = run(
            vec![Ok(DevicePoll::Ok {
                token: "nope".to_owned(),
                login: "ada".to_owned(),
                expires: String::new(),
            })],
            &start(1, 60),
        );
        assert!(result.is_err());
    }
}
