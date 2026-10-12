// SPDX-License-Identifier: Apache-2.0

mod generated;
mod git_candidate_artifact;
mod git_diff;
mod snapshot;
mod user_account;
mod verification_command;

pub use generated::*;
pub use git_candidate_artifact::{GitCandidateArtifactManifest, GitCandidateArtifactManifestError};
pub use git_diff::{GitHunkOriginError, rework_hunk_origins};
pub use snapshot::{
    CanonicalSnapshot, SnapshotBindingError, SnapshotIdentityError, SnapshotVerificationBinding,
    seal_snapshot, verify_snapshot_seal,
};
pub use user_account::{UserAccount, UserAccountError, UserAccountErrorKind};
pub use verification_command::{
    observed_verification_command_digest, observed_verification_command_is_check,
    observed_verification_command_is_test, verification_method_digest,
};

#[must_use]
pub fn is_canonical_delivery_id(value: &str) -> bool {
    is_canonical_prefixed_id(value, "dlv_")
}

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

mod chat_attachment;
pub use chat_attachment::validate_chat_attachments;
