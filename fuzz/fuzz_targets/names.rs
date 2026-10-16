// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Name rules never panic, and agree with each other: a valid name is a valid
//! one-component path, and an alias of a metadata directory is never valid.
//!
//! The alias checks take a fast path for names that are all ASCII; it must
//! give the same fold and the same answers as the full Unicode fold. Most
//! inputs are not ASCII, so each one is also checked as ASCII, and as text
//! made of the characters of the special names.

#![no_main]

use braid_format::name::{
    fold_for_aliases, forbidden_as_symlink, is_reserved, reference, validate_name, validate_path,
};
use libfuzzer_sys::fuzz_target;

/// The characters of `.git`, `.braid`, their short names and the files that
/// must not be symlinks, in both cases, and what the fold cuts or trims.
const SPECIAL: &[u8] = b".gitbraGITBRAD~19mdulensoMUL: ";

fn agrees(name: &[u8]) {
    assert_eq!(fold_for_aliases(name), reference::fold_for_aliases(name));
    assert_eq!(is_reserved(name), reference::is_reserved(name));
    assert_eq!(
        forbidden_as_symlink(name),
        reference::forbidden_as_symlink(name)
    );
}

fuzz_target!(|data: &[u8]| {
    agrees(data);
    let ascii: Vec<u8> = data.iter().map(|byte| byte & 0x7f).collect();
    agrees(&ascii);
    let special: Vec<u8> = data
        .iter()
        .map(|byte| SPECIAL[usize::from(*byte) % SPECIAL.len()])
        .collect();
    agrees(&special);

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
