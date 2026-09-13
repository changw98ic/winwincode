// SPDX-License-Identifier: Apache-2.0

//! Shared identifiers and value objects for `WinWinCode`.
//!
//! The declarations are generated from `schema/winwincode/v1`; the canonical
//! JSON Schema remains their only source. Hand-written domain modules add
//! objects whose lifecycle lives in code rather than in the schema.

mod generated;
mod git_candidate_artifact;
mod user_account;
mod verification_command;

pub use generated::*;
pub use git_candidate_artifact::{GitCandidateArtifactManifest, GitCandidateArtifactManifestError};
pub use user_account::{
    UserAccount, UserAccountError, UserAccountErrorKind, UserAccountRole, UserAccountState,
};
pub use verification_command::{
    observed_verification_command_digest, observed_verification_command_is_test,
    verification_method_digest,
};

/// Returns whether `value` is the canonical Delivery identifier defined by the
/// public schema.
#[must_use]
pub fn is_canonical_delivery_id(value: &str) -> bool {
    is_canonical_prefixed_id(value, "dlv_")
}

/// Checks the schema's uppercase Crockford identifier under an expected prefix.
/// Callers supply the fixed prefix of the identifier type they accept.
#[must_use]
pub fn is_canonical_prefixed_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|identifier| {
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
