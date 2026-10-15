// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A changeset, or an external change list, either fails to decode or
//! re-encodes to exactly its input.

#![no_main]

use braid_format::Changeset;
use braid_format::changeset::{decode_change_list, encode_change_list};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(changeset) = Changeset::decode(data) {
        assert_eq!(changeset.encode(), data);
    }
    if let Ok(changes) = decode_change_list(data) {
        assert_eq!(encode_change_list(&changes), data);
    }
});
