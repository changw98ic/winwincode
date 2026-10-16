// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};

#[must_use]
pub fn candidate_application_digest(
    repository_binding_id: &str,
    base_commit: &str,
    candidate_commit: &str,
) -> String {
    let bytes = serde_json::to_vec(&[repository_binding_id, base_commit, candidate_commit])
        .expect("a string tuple always has a JSON encoding");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::candidate_application_digest;

    #[test]
    fn approval_digest_binds_repository_baseline_and_frozen_candidate() {
        let digest = candidate_application_digest("repo", "base", "candidate");
        for binding in [
            ["other", "base", "candidate"],
            ["repo", "other", "candidate"],
            ["repo", "base", "other"],
        ] {
            assert_ne!(
                digest,
                candidate_application_digest(binding[0], binding[1], binding[2])
            );
        }
        assert_ne!(
            candidate_application_digest("a,b", "c", "d"),
            candidate_application_digest("a", "b,c", "d")
        );
    }
}
