// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Name rules never panic, and agree with each other: a valid name is a valid
//! one-component path, and an alias of a metadata directory is never valid.

#![no_main]

use braid_format::name::{fold_for_aliases, is_reserved, validate_name, validate_path};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let folded = fold_for_aliases(data);
    let valid = validate_name(data).is_ok();
    if valid {
        assert!(validate_path(data).is_ok());
        assert!(!is_reserved(data));
        let mut nested = b"dir/".to_vec();
        nested.extend_from_slice(data);
        assert!(validate_path(&nested).is_ok());
    }
    if folded == b".git" || folded == b".braid" {
        assert!(!valid);
    }
});
