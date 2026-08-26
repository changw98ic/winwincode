// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    AttentionItemId, DeliveryId, Instant, PublicationId, Revision, Sha256Digest,
};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DELIVERY_FACT_SCHEMA_VERSION: u8 = 3;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationTarget {
    schema_version: u8,
    provider: String,
    kind: String,
    repository: String,
    base_branch: String,
    head_repository: String,
    head_branch: String,
}

impl PublicationTarget {
    /// Builds one canonical GitHub pull-request target.
    ///
    /// # Errors
    ///
    /// Rejects malformed repository or branch identity.
    pub fn try_github(
        repository: impl Into<String>,
        base_branch: impl Into<String>,
        head_repository: impl Into<String>,
        head_branch: impl Into<String>,
    ) -> Result<Self, String> {
        let target = Self {
            schema_version: DELIVERY_FACT_SCHEMA_VERSION,
            provider: "github".to_owned(),
            kind: "pull-request".to_owned(),
            repository: repository.into(),
            base_branch: base_branch.into(),
            head_repository: head_repository.into(),
            head_branch: head_branch.into(),
        };
        target.validate()?;
        Ok(target)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != DELIVERY_FACT_SCHEMA_VERSION
            || self.provider != "github"
            || self.kind != "pull-request"
            || !repository_slug(&self.repository)
            || !repository_slug(&self.head_repository)
            || !git_branch(&self.base_branch)
            || !git_branch(&self.head_branch)
            || self.repository == self.head_repository && self.base_branch == self.head_branch
        {
            return Err("publication target is not canonical".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn schema_version(&self) -> u8 {
        self.schema_version
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    #[must_use]
    pub fn base_branch(&self) -> &str {
        &self.base_branch
    }

    #[must_use]
    pub fn head_repository(&self) -> &str {
        &self.head_repository
    }

    #[must_use]
    pub fn head_branch(&self) -> &str {
        &self.head_branch
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationSourceIssue {
    schema_version: u8,
    provider: String,
    kind: String,
    repository: String,
    number: u64,
}

impl PublicationSourceIssue {
    /// Builds one canonical GitHub issue source.
    ///
    /// # Errors
    ///
    /// Rejects a malformed repository or issue number.
    pub fn try_github(repository: impl Into<String>, number: u64) -> Result<Self, String> {
        let source = Self {
            schema_version: DELIVERY_FACT_SCHEMA_VERSION,
            provider: "github".to_owned(),
            kind: "issue".to_owned(),
            repository: repository.into(),
            number,
        };
        source.validate()?;
        Ok(source)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != DELIVERY_FACT_SCHEMA_VERSION
            || self.provider != "github"
            || self.kind != "issue"
            || !repository_slug(&self.repository)
            || self.number == 0
            || self.number > MAX_SAFE_INTEGER
        {
            return Err("publication source issue is not canonical".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn schema_version(&self) -> u8 {
        self.schema_version
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    #[must_use]
    pub const fn number(&self) -> u64 {
        self.number
    }
}

/// Immutable identity stored with one trusted publication intent/result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationFactBinding {
    delivery_id: DeliveryId,
    delivery_revision: u64,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    diff_sha256: String,
    verdict_id: String,
    approval_id: AttentionItemId,
    approval_review_set_sha256: String,
    target_sha256: String,
}

impl PublicationFactBinding {
    /// Builds the immutable identity shared by publication intent and result facts.
    ///
    /// # Errors
    ///
    /// Rejects incomplete or malformed Delivery, candidate, verdict, approval, or target facts.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        delivery_id: DeliveryId,
        delivery_revision: u64,
        delivery_spec_id: impl Into<String>,
        delivery_spec_revision: u64,
        candidate_ref: impl Into<String>,
        diff_sha256: impl Into<String>,
        verdict_id: impl Into<String>,
        approval_id: AttentionItemId,
        approval_review_set_sha256: impl Into<String>,
        target_sha256: impl Into<String>,
    ) -> Result<Self, String> {
        let fact = Self {
            delivery_id,
            delivery_revision,
            delivery_spec_id: delivery_spec_id.into(),
            delivery_spec_revision,
            candidate_ref: candidate_ref.into(),
            diff_sha256: diff_sha256.into(),
            verdict_id: verdict_id.into(),
            approval_id,
            approval_review_set_sha256: approval_review_set_sha256.into(),
            target_sha256: target_sha256.into(),
        };
        fact.validate()?;
        Ok(fact)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.delivery_revision == 0
            || self.delivery_revision > MAX_SAFE_INTEGER
            || self.delivery_spec_revision == 0
            || self.delivery_spec_revision > MAX_SAFE_INTEGER
            || !canonical_prefixed_id(&self.delivery_id.0, "dlv_")
            || !portable(&self.delivery_spec_id, 200)
            || !portable(&self.verdict_id, 200)
            || !canonical_prefixed_id(&self.approval_id.0, "att_")
            || candidate_digest(&self.candidate_ref).is_none()
            || !lowercase_sha256(&self.diff_sha256)
            || !lowercase_sha256(&self.approval_review_set_sha256)
            || !lowercase_sha256(&self.target_sha256)
        {
            return Err("publication fact binding is incomplete or malformed".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub fn delivery_spec_id(&self) -> &str {
        &self.delivery_spec_id
    }

    #[must_use]
    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    #[must_use]
    pub fn verdict_id(&self) -> &str {
        &self.verdict_id
    }

    #[must_use]
    pub const fn approval_id(&self) -> &AttentionItemId {
        &self.approval_id
    }

    #[must_use]
    pub fn approval_review_set_sha256(&self) -> &str {
        &self.approval_review_set_sha256
    }

    #[must_use]
    pub fn target_sha256(&self) -> &str {
        &self.target_sha256
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationResourceKind {
    GitHubIssue,
    GitHubPullRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationResourceFact {
    kind: PublicationResourceKind,
    repository: String,
    number: u64,
}

impl PublicationResourceFact {
    /// Builds one secret-safe provider resource identity.
    ///
    /// # Errors
    ///
    /// Rejects a malformed repository or resource number.
    pub fn try_new(
        kind: PublicationResourceKind,
        repository: impl Into<String>,
        number: u64,
    ) -> Result<Self, String> {
        let fact = Self {
            kind,
            repository: repository.into(),
            number,
        };
        fact.validate()?;
        Ok(fact)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !repository_slug(&self.repository) || self.number == 0 || self.number > MAX_SAFE_INTEGER
        {
            return Err("publication resource identity is invalid".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationResourceKind {
        self.kind
    }

    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    #[must_use]
    pub const fn number(&self) -> u64 {
        self.number
    }
}

/// Safe publication result fields supplied by the publication owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationResultFact {
    publication_id: PublicationId,
    revision: Revision,
    state: String,
    updated_at: Instant,
    binding: PublicationFactBinding,
    publication_set_sha256: Sha256Digest,
    resource: Option<PublicationResourceFact>,
}

impl PublicationResultFact {
    /// Builds one secret-safe publication projection fact.
    ///
    /// # Errors
    ///
    /// Rejects an invalid publication identity, state, revision, timestamp, binding, or resource.
    pub fn try_new(
        publication_id: PublicationId,
        revision: Revision,
        state: impl Into<String>,
        updated_at: Instant,
        binding: PublicationFactBinding,
        publication_set_sha256: Sha256Digest,
        resource: Option<PublicationResourceFact>,
    ) -> Result<Self, String> {
        let fact = Self {
            publication_id,
            revision,
            state: state.into(),
            updated_at,
            binding,
            publication_set_sha256,
            resource,
        };
        fact.validate()?;
        Ok(fact)
    }

    fn validate(&self) -> Result<(), String> {
        self.binding.validate()?;
        if !canonical_prefixed_id(&self.publication_id.0, "pub_")
            || self.revision.0 < 1
            || !matches!(
                self.state.as_str(),
                "pending" | "publishing" | "published" | "failed" | "cancelled"
            )
            || !canonical_instant(&self.updated_at.0)
            || !canonical_sha256(&self.publication_set_sha256)
            || self
                .resource
                .as_ref()
                .is_some_and(|value| value.validate().is_err())
        {
            return Err("publication result fact is invalid".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    #[must_use]
    pub const fn revision(&self) -> &Revision {
        &self.revision
    }

    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub const fn updated_at(&self) -> &Instant {
        &self.updated_at
    }

    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn publication_set_sha256(&self) -> &Sha256Digest {
        &self.publication_set_sha256
    }

    #[must_use]
    pub const fn resource(&self) -> Option<&PublicationResourceFact> {
        self.resource.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationAuthorization {
    pub(crate) binding: PublicationFactBinding,
    pub(crate) source: PublicationSourceIssue,
    pub(crate) target: PublicationTarget,
    pub(crate) candidate_digest: Sha256Digest,
    pub(crate) candidate_commit_id: String,
    pub(crate) artifact_id: String,
    pub(crate) artifact_digest: Sha256Digest,
    pub(crate) approved_by: String,
    pub(crate) approved_at_millis: u64,
    pub(crate) repository_scope_sha256: Sha256Digest,
    pub(crate) publication_set_sha256: Sha256Digest,
    pub(crate) provider_idempotency_key: String,
}

impl PublicationAuthorization {
    /// Seals one adapter-confirmed current Delivery, candidate, Artifact, approval, and target.
    ///
    /// This is a trusted Rust application seam, not a wire DTO: callers must derive every
    /// argument from current durable facts before invoking the publication coordinator.
    ///
    /// # Errors
    ///
    /// Rejects incomplete, stale, cross-target, or non-canonical fact sets.
    #[allow(clippy::too_many_arguments)]
    pub fn try_from_current_facts(
        binding: PublicationFactBinding,
        source: PublicationSourceIssue,
        target: PublicationTarget,
        candidate_commit_id: impl Into<String>,
        artifact_id: impl Into<String>,
        artifact_digest: Sha256Digest,
        approved_by: impl Into<String>,
        approved_at_millis: u64,
        repository_scope_sha256: Sha256Digest,
    ) -> Result<Self, String> {
        let candidate_digest = candidate_digest(binding.candidate_ref())
            .ok_or_else(|| "publication candidate reference is invalid".to_owned())?;
        let candidate_commit_id = candidate_commit_id.into();
        let artifact_id = artifact_id.into();
        let approved_by = approved_by.into();
        let provider_idempotency_key = format!(
            "github:pull-request:{}",
            canonical_sha256_json(&(binding.delivery_id(), &source, &target,)).0
        );
        let publication_set_sha256 = canonical_sha256_json(&(
            &binding,
            &source,
            &target,
            &candidate_digest,
            &candidate_commit_id,
            &artifact_id,
            &artifact_digest,
            &approved_by,
            approved_at_millis,
            &repository_scope_sha256,
        ));
        let authorization = Self {
            binding,
            source,
            target,
            candidate_digest,
            candidate_commit_id,
            artifact_id,
            artifact_digest,
            approved_by,
            approved_at_millis,
            repository_scope_sha256,
            publication_set_sha256,
            provider_idempotency_key,
        };
        authorization.validate()?;
        Ok(authorization)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.binding.validate()?;
        self.source.validate()?;
        self.target.validate()?;
        let expected_candidate_digest = candidate_digest(self.binding.candidate_ref())
            .ok_or_else(|| "publication candidate reference is invalid".to_owned())?;
        let expected_provider_key = format!(
            "github:pull-request:{}",
            canonical_sha256_json(&(self.binding.delivery_id(), &self.source, &self.target,)).0
        );
        let expected_publication_set = canonical_sha256_json(&(
            &self.binding,
            &self.source,
            &self.target,
            &self.candidate_digest,
            &self.candidate_commit_id,
            &self.artifact_id,
            &self.artifact_digest,
            &self.approved_by,
            self.approved_at_millis,
            &self.repository_scope_sha256,
        ));
        if self.binding.target_sha256() != raw_sha256_json(&self.target)
            || self.candidate_digest != expected_candidate_digest
            || !git_object(&self.candidate_commit_id)
            || !canonical_prefixed_id(&self.artifact_id, "art_")
            || !canonical_sha256(&self.artifact_digest)
            || !canonical_prefixed_id(&self.approved_by, "usr_")
            || self.approved_at_millis == 0
            || self.approved_at_millis > MAX_SAFE_INTEGER
            || !canonical_sha256(&self.repository_scope_sha256)
            || self.publication_set_sha256 != expected_publication_set
            || self.provider_idempotency_key != expected_provider_key
        {
            return Err("publication authorization is incomplete or stale".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn source(&self) -> &PublicationSourceIssue {
        &self.source
    }

    #[must_use]
    pub const fn target(&self) -> &PublicationTarget {
        &self.target
    }

    #[must_use]
    pub const fn candidate_digest(&self) -> &Sha256Digest {
        &self.candidate_digest
    }

    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    #[must_use]
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    #[must_use]
    pub const fn artifact_digest(&self) -> &Sha256Digest {
        &self.artifact_digest
    }

    #[must_use]
    pub fn approved_by(&self) -> &str {
        &self.approved_by
    }

    #[must_use]
    pub const fn approved_at_millis(&self) -> u64 {
        self.approved_at_millis
    }

    #[must_use]
    pub const fn repository_scope_sha256(&self) -> &Sha256Digest {
        &self.repository_scope_sha256
    }

    #[must_use]
    pub const fn publication_set_sha256(&self) -> &Sha256Digest {
        &self.publication_set_sha256
    }

    #[must_use]
    pub fn provider_idempotency_key(&self) -> &str {
        &self.provider_idempotency_key
    }
}

pub(crate) fn canonical_prefixed_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD_BASE32.contains(&byte))
    })
}

pub(crate) fn portable(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

pub(crate) fn repository_slug(value: &str) -> bool {
    let Some((owner, repository)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !repository.is_empty()
        && repository.len() <= 100
        && repository.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && !repository.contains('/')
}

pub(crate) fn git_branch(value: &str) -> bool {
    !value.is_empty()
        && value.encode_utf16().count() <= 255
        && value != "@"
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.chars().any(|character| {
            let code = u32::from(character);
            code <= 0x20
                || code == 0x7f
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '\\' | '[')
        })
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment.strip_suffix(".lock").is_none()
        })
}

pub(crate) fn git_object(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && lowercase_hex(value)
}

pub(crate) fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64 && lowercase_hex(value)
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(crate) fn canonical_sha256(value: &Sha256Digest) -> bool {
    value
        .0
        .strip_prefix("sha256:")
        .is_some_and(lowercase_sha256)
}

pub(crate) fn candidate_digest(value: &str) -> Option<Sha256Digest> {
    value
        .strip_prefix("git-candidate:sha256:")
        .filter(|digest| lowercase_sha256(digest))
        .map(|digest| Sha256Digest(format!("sha256:{digest}")))
}

pub(crate) fn raw_sha256_json(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("serializable publication fact");
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn canonical_sha256_json(value: &impl Serialize) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", raw_sha256_json(value)))
}

pub(crate) fn canonical_instant(value: &str) -> bool {
    value.len() == 24
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && value.as_bytes().get(10) == Some(&b'T')
        && value.as_bytes().get(13) == Some(&b':')
        && value.as_bytes().get(16) == Some(&b':')
        && value.as_bytes().get(19) == Some(&b'.')
        && value.ends_with('Z')
        && value.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7) && byte == b'-'
                || index == 10 && byte == b'T'
                || matches!(index, 13 | 16) && byte == b':'
                || index == 19 && byte == b'.'
                || index == 23 && byte == b'Z'
                || !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) && byte.is_ascii_digit()
        })
}
