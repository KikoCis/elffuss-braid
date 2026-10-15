// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The time a changeset is made, in the local time zone.
//!
//! The time zone is the system's, as [jiff](https://docs.rs/jiff) finds it
//! without calling into the C library: the `TZ` environment variable, a
//! POSIX rule or a name in the system's time zone database, or else the
//! system setting (on Unix, `/etc/localtime`). When it cannot be determined,
//! the offset is zero.

use braid_format::Signature;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The current time: seconds since the Unix epoch, and the offset of the
/// local time zone from UTC at that instant, in minutes.
pub fn local_now() -> (i64, i64) {
    let now = Timestamp::now();
    let offset =
        TimeZone::try_system().map_or(0, |zone| i64::from(zone.to_offset(now).seconds()) / 60);
    (now.as_second(), offset)
}

/// A signature by `name` and `email`, made now, in local time.
pub fn signature_now(name: &[u8], email: &[u8]) -> Signature {
    let (seconds, offset_minutes) = local_now();
    Signature {
        name: name.to_vec(),
        email: email.to_vec(),
        seconds,
        offset_minutes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_has_a_real_offset() {
        let (seconds, offset) = local_now();
        assert!(seconds > 1_700_000_000);
        // Every time zone in use is within a day of UTC.
        assert!(offset.abs() < 24 * 60, "{offset}");
        let signature = signature_now(b"Ada", b"ada@example.org");
        assert_eq!(signature.name, b"Ada");
        assert!(signature.seconds >= seconds);
    }
}
