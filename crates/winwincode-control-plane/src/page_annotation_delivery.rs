// SPDX-License-Identifier: Apache-2.0

//! Canonical Delivery journal projection for page annotations.
//!
//! The page annotation payload remains owned by the Collaboration Inbox. This
//! module only appends the corresponding canonical Attention and Evidence
//! facts to the same Delivery journal publication, so Delivery projections and
//! rework can consume the existing types.

use winwincode_delivery::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, DELIVERY_SCHEMA_VERSION, Delivery,
    DeliverySpecId, EvidenceRef, EvidenceRefType, SessionBindingId,
};
use winwincode_delivery::store::{
    AppendDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryMutationOperation,
    DeliveryQueryPort, DeliveryStore,
};
use winwincode_domain::{ArtifactId, FencingToken, RequestId, Sha256Digest};
use winwincode_storage::{
    AggregateJournalPublication, ArtifactAccess, ArtifactProvenance, ArtifactStore,
    ProductStateStorage, StorageError,
};

use crate::{
    collaboration_inbox::{PageAnnotation, PageAnnotationArtifactRef},
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key},
};

/// Stages the canonical Delivery append for one page annotation.
///
/// A page annotation is committed only when its owning Delivery journal is
/// present and can receive the derived Attention/Evidence append.
#[allow(clippy::too_many_lines)]
pub(crate) fn stage(
    storage: &dyn ProductStateStorage,
    scope: &winwincode_domain::RepositoryScope,
    annotation: &PageAnnotation,
    request_id: &RequestId,
    request_digest: &Sha256Digest,
    expected_revision: u64,
    artifacts: Option<&ArtifactStore>,
) -> Result<Option<AggregateJournalPublication>, StorageError> {
    let key = delivery_journal_key(&annotation.candidate.delivery_id)?;
    let Some(loaded) = storage.load_journal(&key)? else {
        return Err(StorageError::invalid_input(
            "page annotation Delivery journal is unavailable",
        ));
    };
    let journal =
        StagedDeliveryJournal::new(annotation.candidate.delivery_id.clone(), Some(loaded));
    let store = DeliveryStore::borrowed(&journal);
    let current = store
        .query(winwincode_delivery::store::DeliveryQuery::Get(
            annotation.candidate.delivery_id.clone(),
        ))
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    if current.revision() != expected_revision {
        return Err(StorageError::invalid_input(
            "page annotation Delivery revision changed",
        ));
    }
    if current.snapshot().spec.id.0 != annotation.candidate.delivery_spec_id
        || current.snapshot().spec.revision != annotation.candidate.delivery_spec_revision
    {
        return Err(StorageError::invalid_input(
            "page annotation Delivery specification changed",
        ));
    }
    let run = current
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| {
            run.id == annotation.candidate.work_run_id
                && u64::try_from(run.attempt).ok() == Some(annotation.candidate.attempt)
                && matches!(run.state, winwincode_domain::WorkRunState::CandidateReady)
                && run
                    .candidate_digest
                    .as_ref()
                    .is_some_and(|digest| digest.0 == annotation.candidate.candidate_digest.0)
        })
        .ok_or_else(|| StorageError::invalid_input("page annotation WorkRun is not current"))?;
    let binding = current
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| {
            binding.id.0 == annotation.candidate.session_binding_id
                && binding.delivery_id == annotation.candidate.delivery_id
                && binding.work_run_id == run.id
                && binding.attempt == annotation.candidate.attempt
                && binding.execution_job_id == run.execution_job_id
                && binding.lease_id.as_ref() == Some(&run.lease_id)
                && binding.worker_id.as_ref() == Some(&run.worker_id)
                && binding.worker_instance_id.as_ref() == Some(&run.worker_instance_id)
                && binding.worker_session_id.as_ref() == Some(&run.worker_session_id)
        })
        .ok_or_else(|| {
            StorageError::invalid_input("page annotation SessionBinding is not current")
        })?;
    let candidate_evidence = current
        .snapshot()
        .evidence
        .iter()
        .find(|evidence| {
            evidence.delivery_id == annotation.candidate.delivery_id
                && evidence.delivery_spec_id.0 == annotation.candidate.delivery_spec_id
                && evidence.delivery_spec_revision == annotation.candidate.delivery_spec_revision
                && evidence.work_run_id == run.id
                && evidence.session_binding_id == binding.id
                && evidence.candidate_ref == annotation.candidate.candidate_ref
        })
        .ok_or_else(|| {
            StorageError::invalid_input("page annotation candidate Evidence is not current")
        })?;
    if !matches!(
        candidate_evidence.evidence_type,
        EvidenceRefType::Commit | EvidenceRefType::Diff | EvidenceRefType::File
    ) {
        return Err(StorageError::invalid_input(
            "page annotation candidate Evidence type is not a Git candidate fact",
        ));
    }
    let diff_source = format!("git_diff:{}", annotation.candidate.diff_sha256.0);
    let tree_source = format!("git_file:{}:", annotation.candidate.candidate_tree_id);
    let has_current_diff = current.snapshot().evidence.iter().any(|evidence| {
        evidence.work_run_id == run.id
            && evidence.candidate_ref == annotation.candidate.candidate_ref
            && evidence.evidence_type == EvidenceRefType::Diff
            && evidence.source_ref == diff_source
    });
    let has_current_tree = current.snapshot().evidence.iter().any(|evidence| {
        evidence.work_run_id == run.id
            && evidence.candidate_ref == annotation.candidate.candidate_ref
            && evidence.evidence_type == EvidenceRefType::File
            && evidence.source_ref.starts_with(&tree_source)
    });
    if !has_current_diff || !has_current_tree {
        return Err(StorageError::invalid_input(
            "page annotation candidate tree or diff is not current",
        ));
    }

    if let Some(artifact) = annotation.screenshot_artifact.as_ref() {
        verify_screenshot_artifact(
            artifacts.ok_or_else(|| {
                StorageError::invalid_input(
                    "page annotation screenshot Artifact authority is unavailable",
                )
            })?,
            scope,
            artifact,
            annotation.candidate.attempt,
            run,
        )?;
    }

    let mut snapshot = current.clone().into_snapshot();
    snapshot.revision = current.revision().saturating_add(1);
    snapshot.updated_at_millis = annotation.updated_at_millis;
    snapshot.attention_items.push(AttentionItem {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: annotation.derived_attention_id.clone(),
        delivery_id: annotation.candidate.delivery_id.clone(),
        delivery_spec_id: DeliverySpecId(annotation.candidate.delivery_spec_id.clone()),
        work_run_id: Some(annotation.candidate.work_run_id.clone()),
        item_type: AttentionItemType::RequirementQuestion,
        title: "页面批注".to_owned(),
        context: annotation.body.clone(),
        options: Vec::new(),
        assigned_to: None,
        blocking: false,
        status: AttentionItemStatus::Open,
        resolution: None,
        resolved_by: None,
        created_at_millis: annotation.updated_at_millis,
        resolved_at_millis: None,
    });
    snapshot.evidence.push(EvidenceRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: annotation.evidence_id.clone(),
        delivery_id: annotation.candidate.delivery_id.clone(),
        delivery_spec_id: DeliverySpecId(annotation.candidate.delivery_spec_id.clone()),
        delivery_spec_revision: annotation.candidate.delivery_spec_revision,
        work_run_id: annotation.candidate.work_run_id.clone(),
        session_binding_id: SessionBindingId(annotation.candidate.session_binding_id.clone()),
        candidate_ref: annotation.candidate.candidate_ref.clone(),
        evidence_type: EvidenceRefType::ReviewFinding,
        source_ref: format!("page-annotation:{}", annotation.id.0),
        created_at_millis: annotation.updated_at_millis,
    });
    let next = Delivery::try_from_snapshot(snapshot)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let request_digest = request_digest
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("page annotation request digest is invalid"))?;
    let mutation = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: annotation.candidate.delivery_id.clone(),
            request_id: request_id.clone(),
            request_digest: request_digest.to_owned(),
            operation: DeliveryMutationOperation::PageAnnotationRecorded,
            expected_revision,
            snapshot: next,
        }))
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    if mutation.replayed {
        return Ok(None);
    }
    journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))
}

fn verify_screenshot_artifact(
    artifacts: &ArtifactStore,
    scope: &winwincode_domain::RepositoryScope,
    artifact: &PageAnnotationArtifactRef,
    attempt: u64,
    run: &winwincode_domain::WorkRun,
) -> Result<(), StorageError> {
    let provenance = ArtifactProvenance::execution_job(
        run.execution_job_id.clone(),
        attempt,
        run.lease_id.clone(),
        FencingToken(run.fencing_token.clone()),
        run.worker_id.clone(),
        run.worker_instance_id.clone(),
        run.worker_session_id.clone(),
    )
    .map_err(|_| StorageError::invalid_input("page annotation Artifact provenance is invalid"))?;
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            crate::repository_scope_key(scope)?,
            ArtifactId(artifact.artifact_id.clone()),
            artifact.digest.clone(),
            provenance,
        ))
        .map_err(|_| {
            StorageError::invalid_input("page annotation screenshot Artifact is not authorized")
        })?;
    if !matches!(
        object.metadata().media_type(),
        "image/png" | "image/jpeg" | "image/webp"
    ) || object.metadata().digest() != &artifact.digest
        || object.bytes().is_empty()
    {
        return Err(StorageError::invalid_input(
            "page annotation screenshot Artifact descriptor is invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PageAnnotationArtifactRef;
    use sha2::{Digest as _, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};
    use winwincode_domain::{
        OrganizationId, ProjectId, RepositoryId, RepositoryScopeKind, RequestId, UserId,
        WorkspaceId,
    };
    use winwincode_storage::{
        ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactRetention,
        FakeArtifactObjectStore,
    };

    fn scope() -> winwincode_domain::RepositoryScope {
        winwincode_domain::RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_01J00000000000000000000001".into()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000001".into()),
            project_id: ProjectId("prj_01J00000000000000000000001".into()),
            repository_id: RepositoryId("rep_01J00000000000000000000001".into()),
        }
    }

    fn temporary_directory() -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("winwincode-page-artifact-{suffix}"));
        std::fs::create_dir_all(&path).expect("temporary directory");
        path
    }

    fn fixture_run() -> winwincode_domain::WorkRun {
        let delivery: Delivery = serde_json::from_slice(include_bytes!(
            "../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("Delivery fixture");
        delivery
            .snapshot()
            .work_run_aggregate
            .runs
            .first()
            .cloned()
            .expect("fixture WorkRun")
    }

    #[test]
    fn screenshot_artifact_requires_exact_descriptor_and_fenced_provenance() {
        let root = temporary_directory();
        let mut store = ArtifactStore::open(
            root.join("catalog"),
            Box::new(FakeArtifactObjectStore::new()),
        )
        .expect("ArtifactStore");
        let run = fixture_run();
        let artifact_id = ArtifactId("art_01J00000000000000000000001".into());
        let bytes = b"png-fixture".to_vec();
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
        let provenance = ArtifactProvenance::execution_job(
            run.execution_job_id.clone(),
            1,
            run.lease_id.clone(),
            FencingToken(run.fencing_token.clone()),
            run.worker_id.clone(),
            run.worker_instance_id.clone(),
            run.worker_session_id.clone(),
        )
        .expect("provenance");
        let scope_key = crate::repository_scope_key(&scope()).expect("scope key");
        let attribution = ArtifactMeteringAttribution {
            organization_id: OrganizationId("org_01J00000000000000000000001".into()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000001".into()),
            project_id: ProjectId("prj_01J00000000000000000000001".into()),
            repository_id: RepositoryId("rep_01J00000000000000000000001".into()),
            delivery_id: None,
            product_session_id: None,
            user_id: UserId("usr_01J00000000000000000000001".into()),
        };
        store
            .open_artifact(ArtifactOpen::new(
                scope_key.clone(),
                winwincode_domain::ExecutionMessageId("xmsg_01J00000000000000000000001".into()),
                RequestId("req_01J00000000000000000000001".into()),
                artifact_id.clone(),
                "screenshot",
                "image/png",
                digest.clone(),
                bytes.len() as u64,
                None,
                provenance.clone(),
                attribution,
                ArtifactRetention::Indefinite,
                1,
            ))
            .expect("Artifact open");
        store
            .append_chunk(&ArtifactChunk::new(
                scope_key,
                winwincode_domain::ExecutionMessageId("xmsg_01J00000000000000000000002".into()),
                artifact_id.clone(),
                provenance,
                2,
                1,
                "image/png",
                digest.clone(),
                bytes,
                true,
            ))
            .expect("Artifact chunk");
        let reference = PageAnnotationArtifactRef {
            artifact_id: artifact_id.0,
            digest,
        };
        verify_screenshot_artifact(&store, &scope(), &reference, 1, &run)
            .expect("exact Artifact authority");

        let mut foreign = run.clone();
        foreign.lease_id.0 = "lse_01J00000000000000000000009".into();
        assert!(verify_screenshot_artifact(&store, &scope(), &reference, 1, &foreign).is_err());
        store.close().expect("Artifact store close");
        std::fs::remove_dir_all(root).expect("temporary directory cleanup");
    }
}
