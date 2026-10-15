// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A git map either fails to decode or re-encodes to exactly its input.
//!
//! Each input is tried as given and behind a valid header, so the fuzzer
//! spends its time on the tables rather than on finding the magic string.

#![no_main]

use braid_git::GitMap;
use libfuzzer_sys::fuzz_target;

/// The magic string and version of a version 1 map.
const HEADER: &[u8] = b"braid-git-map\x01";

fn check(bytes: &[u8]) {
    if let Ok(map) = GitMap::from_bytes(bytes) {
        assert_eq!(map.to_bytes(), bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    assert!(GitMap::new().to_bytes().starts_with(HEADER));
    check(data);
    let mut framed = HEADER.to_vec();
    framed.extend_from_slice(data);
    check(&framed);
});
