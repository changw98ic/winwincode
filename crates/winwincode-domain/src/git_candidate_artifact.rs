// SPDX-License-Identifier: Apache-2.0

use std::{error::Error, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Sha256Digest;

const SCHEMA_VERSION: u8 = 2;
const MAX_BUNDLE_BYTES: usize = 512 * 1024 * 1024;

/// Canonical candidate Artifact carrying the Git objects missing from an
/// independently deployed Control Plane repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCandidateArtifactManifest {
    candidate_commit_id: String,
    bundle: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestWire {
    schema_version: u8,
    candidate_commit_id: String,
    bundle_base64: String,
    bundle_digest: Sha256Digest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitCandidateArtifactManifestError;

impl fmt::Display for GitCandidateArtifactManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("candidate Artifact manifest is invalid")
    }
}

impl Error for GitCandidateArtifactManifestError {}

impl GitCandidateArtifactManifest {
    /// Creates the one current canonical manifest.
    ///
    /// # Errors
    ///
    /// Rejects a malformed commit id or an empty/oversized Git bundle.
    pub fn new(
        candidate_commit_id: impl Into<String>,
        bundle: Vec<u8>,
    ) -> Result<Self, GitCandidateArtifactManifestError> {
        let candidate_commit_id = candidate_commit_id.into();
        if !git_object_id(&candidate_commit_id)
            || bundle.is_empty()
            || bundle.len() > MAX_BUNDLE_BYTES
        {
            return Err(GitCandidateArtifactManifestError);
        }
        Ok(Self {
            candidate_commit_id,
            bundle,
        })
    }

    /// Decodes and verifies exact canonical JSON, bundle encoding, and digest.
    ///
    /// # Errors
    ///
    /// Rejects old versions, unknown/reordered fields, invalid base64, or a
    /// bundle whose bytes do not match the sealed digest.
    pub fn decode(bytes: &[u8]) -> Result<Self, GitCandidateArtifactManifestError> {
        let wire: ManifestWire =
            serde_json::from_slice(bytes).map_err(|_| GitCandidateArtifactManifestError)?;
        if wire.schema_version != SCHEMA_VERSION || !git_object_id(&wire.candidate_commit_id) {
            return Err(GitCandidateArtifactManifestError);
        }
        let bundle = STANDARD
            .decode(&wire.bundle_base64)
            .map_err(|_| GitCandidateArtifactManifestError)?;
        let manifest = Self::new(wire.candidate_commit_id, bundle)?;
        if manifest.bundle_digest() != wire.bundle_digest || manifest.encode()? != bytes {
            return Err(GitCandidateArtifactManifestError);
        }
        Ok(manifest)
    }

    /// Encodes the strict canonical JSON retained as Artifact bytes.
    ///
    /// # Errors
    ///
    /// Returns an error only if serialization of the closed wire shape fails.
    pub fn encode(&self) -> Result<Vec<u8>, GitCandidateArtifactManifestError> {
        serde_json::to_vec(&ManifestWire {
            schema_version: SCHEMA_VERSION,
            candidate_commit_id: self.candidate_commit_id.clone(),
            bundle_base64: STANDARD.encode(&self.bundle),
            bundle_digest: self.bundle_digest(),
        })
        .map_err(|_| GitCandidateArtifactManifestError)
    }

    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    #[must_use]
    pub fn bundle(&self) -> &[u8] {
        &self.bundle
    }

    fn bundle_digest(&self) -> Sha256Digest {
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(&self.bundle)))
    }
}

fn git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_and_rejects_changed_bundle() {
        let manifest = GitCandidateArtifactManifest::new("a".repeat(40), b"bundle".to_vec())
            .expect("manifest");
        let bytes = manifest.encode().expect("manifest encoding");
        assert_eq!(
            GitCandidateArtifactManifest::decode(&bytes).expect("canonical manifest"),
            manifest
        );

        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        changed["bundleBase64"] = serde_json::Value::String(STANDARD.encode(b"changed"));
        assert!(
            GitCandidateArtifactManifest::decode(
                &serde_json::to_vec(&changed).expect("changed JSON")
            )
            .is_err()
        );
    }
}
