// SPDX-License-Identifier: Apache-2.0

//! Control Plane join from durable Artifact/source authority to Delivery candidate.

use std::fmt;

use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::{
    application::stage::DeliveryTerminalOutcomeFacts,
    domain::{
        Delivery, DeliveryValidationError, FrozenDeliveryCandidate,
        candidate::freeze_delivery_candidate_from_source,
    },
};
use winwincode_domain::{ArtifactId, DeliveryId, Sha256Digest};
use winwincode_storage::{
    ArtifactAccess, ArtifactError, ArtifactProvenance, ArtifactStore, GitSourceResolver,
    ProductStateStorage, StorageError,
};

use crate::delivery_transaction::delivery_stream_id;
use crate::repository_scope_key;

/// Failure while deriving an immutable candidate from durable source facts.
#[derive(Debug)]
pub enum CandidateResolutionError {
    Storage(StorageError),
    Artifact(ArtifactError),
    Delivery(DeliveryValidationError),
}

impl fmt::Display for CandidateResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "candidate state read failed: {error}"),
            Self::Artifact(error) => write!(formatter, "candidate source read failed: {error}"),
            Self::Delivery(error) => write!(formatter, "candidate authority failed: {error}"),
        }
    }
}

impl std::error::Error for CandidateResolutionError {}

impl From<StorageError> for CandidateResolutionError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ArtifactError> for CandidateResolutionError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<DeliveryValidationError> for CandidateResolutionError {
    fn from(error: DeliveryValidationError) -> Self {
        Self::Delivery(error)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve(
    storage: &dyn ProductStateStorage,
    artifacts: &ArtifactStore,
    source_resolver: &dyn GitSourceResolver,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    artifact_id: &ArtifactId,
    artifact_digest: &Sha256Digest,
    terminal_facts: &DeliveryTerminalOutcomeFacts,
) -> Result<FrozenDeliveryCandidate, CandidateResolutionError> {
    let state = storage
        .load_state(&delivery_stream_id(delivery_id))?
        .ok_or_else(|| StorageError::invalid_input("candidate Delivery state does not exist"))?;
    let delivery = Delivery::decode_json(&state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if delivery.id() != delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "candidate Delivery state identity or revision is inconsistent",
        )
        .into());
    }
    let active = terminal_facts.authority().active_lease();
    let provenance = ArtifactProvenance::execution_job(
        active.execution_job_id().clone(),
        active.attempt(),
        active.lease_id().clone(),
        active.fencing_token().clone(),
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.worker_session_id().clone(),
    )?;
    let object = artifacts.read_exact(&ArtifactAccess::new(
        repository_scope_key(scope)?,
        artifact_id.clone(),
        artifact_digest.clone(),
        provenance,
    ))?;
    let source = source_resolver.resolve_candidate(
        &object,
        &delivery.snapshot().spec.repository.locator,
        &delivery.snapshot().spec.base_revision,
    )?;
    freeze_delivery_candidate_from_source(&delivery, &source, terminal_facts).map_err(Into::into)
}
