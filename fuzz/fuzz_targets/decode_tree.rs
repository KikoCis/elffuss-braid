// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A tree node either fails to decode or re-encodes to exactly its input.

#![no_main]

use braid_format::Node;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(node) = Node::decode(data) {
        assert_eq!(node.encode(), data);
    }
});
