// SPDX-License-Identifier: Apache-2.0

//! Current Delivery facts → secret-safe review-package Artifact → Publication authority.

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_delivery::{
    domain::{CandidatePathFact, Delivery, FrozenDeliveryCandidate},
    projection::{
        AttentionItemProjection, DeliveryProjection, DeliveryTaskProjection, EvidenceProjection,
        ProjectionInput, RequirementsProjection, SolutionReviewProjection, VerdictProjection,
        project_delivery_detail,
    },
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    ArtifactId, AttentionItemId, ExecutionMessageId, RequestId, Sha256Digest, StageRunId, UserId,
};
use winwincode_publication::{
    PublicationAuthorization, PublicationFactBinding, PublicationSourceIssue, PublicationTarget,
    RepositoryPolicyScope,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactError, ArtifactMeteringAttribution, ArtifactOpen,
    ArtifactProvenance, ArtifactRetention, ArtifactStore, ReceiptScopeKey, StorageError,
};

use crate::{
    ControlPlane, repository_scope_key,
    strongflow_projection::{
        StrongFlowProjectionError, current_publication_approval, derive_publication_binding,
        load_current,
    },
};

const REVIEW_PACKAGE_PROTOCOL: &str = "winwincode.github-review-package.v1";
const REVIEW_PACKAGE_MEDIA_TYPE: &str = "application/vnd.winwincode.github-review-package+json";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Failure before one exact Publication authority can be returned.
#[derive(Debug)]
pub enum PublicationPreparationError {
    Storage(StorageError),
    Artifact(ArtifactError),
    Projection(StrongFlowProjectionError),
    InvalidFacts(String),
}

impl fmt::Display for PublicationPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "publication state read failed: {error}"),
            Self::Artifact(error) => write!(formatter, "review package storage failed: {error}"),
            Self::Projection(error) => {
                write!(formatter, "publication facts are unavailable: {error}")
            }
            Self::InvalidFacts(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PublicationPreparationError {}

impl From<StorageError> for PublicationPreparationError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ArtifactError> for PublicationPreparationError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<StrongFlowProjectionError> for PublicationPreparationError {
    fn from(error: StrongFlowProjectionError) -> Self {
        Self::Projection(error)
    }
}

/// One deterministic review package and the Publication authority sealed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPublication {
    authorization: PublicationAuthorization,
    review_package_artifact_id: ArtifactId,
    review_package_digest: Sha256Digest,
    review_package_bytes: Vec<u8>,
}

impl PreparedPublication {
    #[must_use]
    pub const fn authorization(&self) -> &PublicationAuthorization {
        &self.authorization
    }

    #[must_use]
    pub const fn review_package_artifact_id(&self) -> &ArtifactId {
        &self.review_package_artifact_id
    }

    #[must_use]
    pub const fn review_package_digest(&self) -> &Sha256Digest {
        &self.review_package_digest
    }

    #[must_use]
    pub fn review_package_bytes(&self) -> &[u8] {
        &self.review_package_bytes
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPackage<'facts> {
    schema_version: u8,
    protocol: &'static str,
    prepared_at_millis: u64,
    delivery: ReviewPackageDelivery<'facts>,
    publication_binding: &'facts PublicationFactBinding,
    source: &'facts PublicationSourceIssue,
    target: &'facts PublicationTarget,
    candidate: ReviewPackageCandidate<'facts>,
    approval: ReviewPackageApproval<'facts>,
}

/// Publication-safe Delivery facts. Runtime stages and their worker/session
/// bindings deliberately remain outside the GitHub review package.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPackageDelivery<'facts> {
    delivery_id: &'facts winwincode_domain::DeliveryId,
    delivery_revision: u64,
    status: winwincode_delivery::domain::DeliveryStatus,
    requirements: &'facts RequirementsProjection,
    solution_review: Option<&'facts SolutionReviewProjection>,
    tasks: &'facts [DeliveryTaskProjection],
    attention: &'facts [AttentionItemProjection],
    evidence: &'facts [EvidenceProjection],
    verdict: Option<&'facts VerdictProjection>,
}

impl<'facts> From<&'facts DeliveryProjection> for ReviewPackageDelivery<'facts> {
    fn from(delivery: &'facts DeliveryProjection) -> Self {
        Self {
            delivery_id: delivery.delivery_id(),
            delivery_revision: delivery.delivery_revision(),
            status: delivery.status(),
            requirements: delivery.requirements(),
            solution_review: delivery.solution_review(),
            tasks: delivery.tasks(),
            attention: delivery.attention(),
            evidence: delivery.evidence(),
            verdict: delivery.verdict(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPackageCandidate<'facts> {
    candidate_ref: &'facts str,
    delivery_spec_id: &'facts str,
    delivery_spec_revision: u64,
    producer_stage_run_id: &'facts StageRunId,
    candidate_commit_id: &'facts str,
    candidate_tree_id: &'facts str,
    diff_sha256: &'facts str,
    changed_paths: &'facts [CandidatePathFact],
    source_artifact_id: &'facts str,
    source_artifact_digest: &'facts Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPackageApproval<'facts> {
    stage_run_id: &'facts StageRunId,
    attention_item_id: &'facts AttentionItemId,
    approved_by: &'facts str,
    approved_at_millis: u64,
    approval_review_set_sha256: &'facts str,
}

struct CurrentPublicationFacts {
    binding: PublicationFactBinding,
    source: PublicationSourceIssue,
    target: PublicationTarget,
    policy_scope: RepositoryPolicyScope,
    scope_key: ReceiptScopeKey,
    approval_stage_run_id: StageRunId,
    approval_attention_item_id: AttentionItemId,
    approved_by: String,
    approved_at_millis: u64,
}

impl CurrentPublicationFacts {
    fn resolve(
        scope: &RepositoryScope,
        delivery: &Delivery,
        detail: &DeliveryProjection,
    ) -> Result<Self, PublicationPreparationError> {
        let binding = derive_publication_binding(delivery, detail)?.ok_or_else(|| {
            PublicationPreparationError::InvalidFacts(
                "the current Delivery has no exact publishable approval".to_owned(),
            )
        })?;
        let approval = current_publication_approval(delivery)?.ok_or_else(|| {
            PublicationPreparationError::InvalidFacts(
                "the current Delivery has no exact human publication approval".to_owned(),
            )
        })?;
        let source_ref = detail.requirements().spec().source_ref().ok_or_else(|| {
            PublicationPreparationError::InvalidFacts(
                "publication requires one current GitHub issue source".to_owned(),
            )
        })?;
        let target_ref = detail
            .requirements()
            .spec()
            .publication_target()
            .ok_or_else(|| {
                PublicationPreparationError::InvalidFacts(
                    "publication requires one current GitHub pull-request target".to_owned(),
                )
            })?;
        Ok(Self {
            binding,
            source: PublicationSourceIssue::try_github(
                source_ref.repository.clone(),
                source_ref.number,
            )
            .map_err(PublicationPreparationError::InvalidFacts)?,
            target: PublicationTarget::try_github(
                target_ref.repository.clone(),
                target_ref.base_branch.clone(),
                target_ref.head_repository.clone(),
                target_ref.head_branch.clone(),
            )
            .map_err(PublicationPreparationError::InvalidFacts)?,
            policy_scope: RepositoryPolicyScope::try_new(
                scope.organization_id.clone(),
                scope.workspace_id.clone(),
                scope.project_id.clone(),
                scope.repository_id.clone(),
            )
            .map_err(PublicationPreparationError::InvalidFacts)?,
            scope_key: repository_scope_key(scope)?,
            approval_stage_run_id: approval.run.id.clone(),
            approval_attention_item_id: approval.attention.id.clone(),
            approved_by: approval.resolved_by.to_owned(),
            approved_at_millis: approval.resolved_at,
        })
    }

    fn review_package<'facts>(
        &'facts self,
        detail: &'facts DeliveryProjection,
        candidate: &'facts FrozenDeliveryCandidate,
    ) -> ReviewPackage<'facts> {
        ReviewPackage {
            schema_version: 1,
            protocol: REVIEW_PACKAGE_PROTOCOL,
            prepared_at_millis: self.approved_at_millis,
            delivery: ReviewPackageDelivery::from(detail),
            publication_binding: &self.binding,
            source: &self.source,
            target: &self.target,
            candidate: ReviewPackageCandidate {
                candidate_ref: candidate.candidate_ref(),
                delivery_spec_id: &candidate.delivery_spec_id().0,
                delivery_spec_revision: candidate.delivery_spec_revision(),
                producer_stage_run_id: candidate.producer_stage_run_id(),
                candidate_commit_id: candidate.candidate_commit_id(),
                candidate_tree_id: candidate.candidate_tree_id(),
                diff_sha256: candidate.diff_sha256(),
                changed_paths: candidate.changed_paths(),
                source_artifact_id: candidate.producer_artifact_ref(),
                source_artifact_digest: candidate.producer_artifact_digest(),
            },
            approval: ReviewPackageApproval {
                stage_run_id: &self.approval_stage_run_id,
                attention_item_id: &self.approval_attention_item_id,
                approved_by: &self.approved_by,
                approved_at_millis: self.approved_at_millis,
                approval_review_set_sha256: self.binding.approval_review_set_sha256(),
            },
        }
    }
}

struct StoredReviewPackage {
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    bytes: Vec<u8>,
}

impl ControlPlane {
    /// Rebuilds the current publishable Delivery facts, persists one exact
    /// secret-safe review package as an Artifact, and seals Publication
    /// authority to that package digest.
    ///
    /// Exact repeats replay the same Artifact metadata and bytes. No caller can
    /// supply an approval, verdict, target, package digest, or storage provenance.
    ///
    /// # Errors
    ///
    /// Rejects a missing or stale Delivery/candidate, non-passing verdict,
    /// absent exact human approval, invalid repository scope, or Artifact
    /// storage conflict/corruption.
    pub fn prepare_publication(
        &mut self,
        scope: &RepositoryScope,
        candidate: &FrozenDeliveryCandidate,
        requester: &UserId,
    ) -> Result<PreparedPublication, PublicationPreparationError> {
        let delivery = load_current(self, candidate.delivery_id())?;
        let detail =
            project_delivery_detail(ProjectionInput::new(&delivery).with_candidate(candidate))
                .map_err(StrongFlowProjectionError::from)?;
        let facts = CurrentPublicationFacts::resolve(scope, &delivery, &detail)?;
        let package_bytes =
            serde_json::to_vec(&facts.review_package(&detail, candidate)).map_err(|error| {
                PublicationPreparationError::InvalidFacts(format!(
                    "review package encoding failed: {error}"
                ))
            })?;
        let artifacts = self.artifact_store.as_mut().ok_or_else(|| {
            PublicationPreparationError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        let stored = store_review_package(
            artifacts,
            facts.scope_key,
            candidate,
            ArtifactMeteringAttribution {
                organization_id: scope.organization_id.clone(),
                workspace_id: scope.workspace_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
                delivery_id: Some(candidate.delivery_id().clone()),
                product_session_id: Some(candidate.producer_product_session_id().clone()),
                user_id: requester.clone(),
            },
            facts.approved_at_millis,
            package_bytes,
        )?;
        let authorization = PublicationAuthorization::try_from_current_facts(
            facts.binding,
            facts.source,
            facts.target,
            candidate.candidate_commit_id(),
            stored.artifact_id.0.clone(),
            stored.digest.clone(),
            &facts.approved_by,
            facts.approved_at_millis,
            facts.policy_scope.sha256(),
        )
        .map_err(PublicationPreparationError::InvalidFacts)?;
        Ok(PreparedPublication {
            authorization,
            review_package_artifact_id: stored.artifact_id,
            review_package_digest: stored.digest,
            review_package_bytes: stored.bytes,
        })
    }
}

fn store_review_package(
    artifacts: &mut ArtifactStore,
    scope_key: ReceiptScopeKey,
    candidate: &FrozenDeliveryCandidate,
    metering_attribution: ArtifactMeteringAttribution,
    prepared_at_millis: u64,
    package_bytes: Vec<u8>,
) -> Result<StoredReviewPackage, PublicationPreparationError> {
    let package_digest = sha256_digest(&package_bytes);
    let artifact_id = ArtifactId(derived_id(
        "art",
        "winwincode.github-review-package-artifact.v1",
        &package_digest,
    ));
    let open_message_id = ExecutionMessageId(derived_id(
        "xmsg",
        "winwincode.github-review-package-open.v1",
        &package_digest,
    ));
    let chunk_message_id = ExecutionMessageId(derived_id(
        "xmsg",
        "winwincode.github-review-package-chunk.v1",
        &package_digest,
    ));
    let request_id = RequestId(derived_id(
        "req",
        "winwincode.github-review-package-request.v1",
        &package_digest,
    ));
    let provenance = ArtifactProvenance::execution_job(
        candidate.producer_execution_job_id().clone(),
        candidate.producer_attempt(),
        candidate.producer_lease_id().clone(),
        candidate.producer_fencing_token().clone(),
        candidate.producer_worker_id().clone(),
        candidate.producer_worker_instance_id().clone(),
        candidate.producer_worker_session_id().clone(),
    )?;
    let size_bytes = u64::try_from(package_bytes.len()).map_err(|_| {
        PublicationPreparationError::InvalidFacts(
            "review package size is outside the supported range".to_owned(),
        )
    })?;
    artifacts.open_artifact(ArtifactOpen::new(
        scope_key.clone(),
        open_message_id,
        request_id,
        artifact_id.clone(),
        "report",
        REVIEW_PACKAGE_MEDIA_TYPE,
        package_digest.clone(),
        size_bytes,
        Some("winwincode-review-package.json".into()),
        provenance.clone(),
        metering_attribution,
        ArtifactRetention::Indefinite,
        prepared_at_millis,
    ))?;
    artifacts.append_chunk(&ArtifactChunk::new(
        scope_key.clone(),
        chunk_message_id,
        artifact_id.clone(),
        provenance.clone(),
        prepared_at_millis,
        1,
        REVIEW_PACKAGE_MEDIA_TYPE,
        package_digest.clone(),
        package_bytes.clone(),
        true,
    ))?;
    let stored = artifacts.read_exact(&ArtifactAccess::new(
        scope_key,
        artifact_id.clone(),
        package_digest.clone(),
        provenance,
    ))?;
    if stored.bytes() != package_bytes
        || stored.metadata().kind() != "report"
        || stored.metadata().media_type() != REVIEW_PACKAGE_MEDIA_TYPE
        || !stored.metadata().is_complete()
    {
        return Err(PublicationPreparationError::InvalidFacts(
            "stored review package does not match its exact prepared facts".to_owned(),
        ));
    }
    Ok(StoredReviewPackage {
        artifact_id,
        digest: package_digest,
        bytes: package_bytes,
    })
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn derived_id(prefix: &str, namespace: &str, digest: &Sha256Digest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(digest.0.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(bytes);
    let mut encoded = [b'0'; 26];
    for byte in encoded.iter_mut().rev() {
        *byte = CROCKFORD_BASE32[(value & 31) as usize];
        value >>= 5;
    }
    format!(
        "{prefix}_{}",
        String::from_utf8(encoded.to_vec()).expect("base32 ASCII")
    )
}
