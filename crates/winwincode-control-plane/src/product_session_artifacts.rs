// SPDX-License-Identifier: Apache-2.0

//! Chat output reads are authorized by the retained turn, never by an Artifact id alone.

use crate::{
    ControlPlane, ProductSessionPersistence, ProductSessionService, ProductSessionServiceError,
    ProductSessionServiceErrorCode, repository_scope_key,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use winwincode_api::generated::{
    PageInfo, SessionArtifactGetQuery, SessionArtifactGetResult, SessionArtifactGetResultResponse,
    SessionArtifactGetResultResponseQuery,
};
use winwincode_domain::{RepositoryScope, SchemaVersion};
use winwincode_execution_port::generated::ArtifactReference;
use winwincode_storage::{
    ArtifactAccess, ArtifactProvenance, ReceiptScopeKey, WorkerSlotAuthority,
};

fn invalid() -> ProductSessionServiceError {
    crate::product_session_service::service_error(
        ProductSessionServiceErrorCode::InvalidInput,
        "Chat artifact request is invalid",
    )
}

fn unavailable() -> ProductSessionServiceError {
    crate::product_session_service::service_error(
        ProductSessionServiceErrorCode::NotFound,
        "Chat artifact is unavailable",
    )
}

fn access(
    scope: ReceiptScopeKey,
    runtime: &WorkerSlotAuthority,
    artifact: &ArtifactReference,
) -> Result<ArtifactAccess, ProductSessionServiceError> {
    let provenance = ArtifactProvenance::execution_job(
        runtime.job_id.clone(),
        runtime.attempt,
        runtime.lease_id.clone(),
        runtime.fencing_token.clone(),
        runtime.worker_id.clone(),
        runtime.worker_instance_id.clone(),
        runtime.worker_session_id.clone(),
    )
    .map_err(|_| unavailable())?;
    Ok(ArtifactAccess::new(
        scope,
        artifact.artifact_id.clone(),
        artifact.digest.clone(),
        provenance,
    ))
}

impl ControlPlane {
    pub(crate) fn validate_chat_artifacts(
        &self,
        scope: &RepositoryScope,
        runtime: &WorkerSlotAuthority,
        artifacts: &[ArtifactReference],
    ) -> Result<(), ProductSessionServiceError> {
        if artifacts.is_empty() {
            return Ok(());
        }
        if artifacts.len() > 16 {
            return Err(invalid());
        }
        let scope = repository_scope_key(scope).map_err(|_| invalid())?;
        let store = self.artifact_store.as_ref().ok_or_else(unavailable)?;
        for artifact in artifacts {
            let record = store
                .describe_exact(&access(scope.clone(), runtime, artifact)?)
                .map_err(|_| unavailable())?;
            if record.media_type() != "application/zip" || record.file_name() != Some("project.zip")
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// Reads a bounded portion of an acknowledged project from its owning Chat turn.
    ///
    /// # Errors
    /// Rejects an invalid range, foreign session, missing terminal link, or changed bytes.
    pub fn session_artifact_get(
        &self,
        storage: &mut dyn ProductSessionPersistence,
        query: &SessionArtifactGetQuery,
    ) -> Result<SessionArtifactGetResultResponse, ProductSessionServiceError> {
        if query.page.cursor.is_some()
            || query.parameters.offset < 0
            || !(1..=262_144).contains(&query.parameters.length)
        {
            return Err(invalid());
        }
        let scope = repository_scope_key(&query.scope).map_err(|_| invalid())?;
        let record = ProductSessionService::new(storage)
            .get(&scope, &query.parameters.product_session_id)?
            .ok_or_else(unavailable)?;
        let (turn, artifact) = record
            .turn_intents()
            .iter()
            .find_map(|turn| {
                turn.terminal_outcome
                    .as_ref()?
                    .artifact_refs
                    .as_ref()?
                    .iter()
                    .find(|artifact| artifact.artifact_id == query.parameters.artifact_id)
                    .map(|artifact| (turn, artifact))
            })
            .ok_or_else(unavailable)?;
        let binding = record
            .bindings()
            .iter()
            .find(|binding| {
                binding.binding().identity().execution_job_id() == &turn.execution_job_id
            })
            .ok_or_else(unavailable)?;
        let artifact = ArtifactReference {
            artifact_id: artifact.artifact_id.clone(),
            digest: artifact.digest.clone(),
        };
        let object = self
            .artifact_store
            .as_ref()
            .ok_or_else(unavailable)?
            .read_exact_range(
                &access(scope, &binding.slot().authority, &artifact)?,
                u64::try_from(query.parameters.offset).map_err(|_| invalid())?,
                u64::try_from(query.parameters.length).map_err(|_| invalid())?,
            )
            .map_err(|_| unavailable())?;
        Ok(SessionArtifactGetResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: SessionArtifactGetResultResponseQuery::SessionArtifactGet,
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            result: SessionArtifactGetResult {
                artifact_id: artifact.artifact_id,
                digest: artifact.digest,
                offset: query.parameters.offset,
                total_size: i64::try_from(object.range().total_size())
                    .map_err(|_| unavailable())?,
                data_base64: STANDARD.encode(object.bytes()),
            },
        })
    }
}
