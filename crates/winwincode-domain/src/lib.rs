// SPDX-License-Identifier: Apache-2.0

//! Shared identifiers and value objects for `WinWinCode`.
//!
//! The declarations are generated from `schema/winwincode/v1`; the canonical
//! JSON Schema remains their only source.

mod generated;

pub use generated::*;

/// Returns whether `value` is the canonical Delivery identifier defined by the
/// public schema.
#[must_use]
pub fn is_canonical_delivery_id(value: &str) -> bool {
    value.strip_prefix("dlv_").is_some_and(|identifier| {
        identifier.len() == 26
            && identifier.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    })
}
