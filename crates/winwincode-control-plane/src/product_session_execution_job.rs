// SPDX-License-Identifier: Apache-2.0

//! Trusted `ProductSession` Chat `ExecutionJob` construction.

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{ModelRoute, RepositoryScope};
use winwincode_domain::{ExecutionJobId, ProductSessionId, Sha256Digest};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
};
use winwincode_storage::{ExecutionJobSubmission, ExecutionQueueScope};

use super::{
    ProductSessionCommandContext, ProductSessionServiceError, ProductSessionServiceErrorCode,
    service_error, storage_error,
};
use crate::{instant_from_millis, public_repository_scope, repository_scope_key};

const MAX_GOAL_BYTES: usize = 20_000;
const MAX_RUNTIME_SECONDS: i64 = 604_800;
const MAX_ARTIFACT_BYTES: i64 = 1_099_511_627_776;

/// Trusted repository snapshot and bounded execution policy installed by the
/// production composition root.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionExecutionConfig {
    repository_scope: RepositoryScope,
    checkout_revision: String,
    execution_profile: String,
    max_runtime_seconds: i64,
    max_artifact_bytes: i64,
}

impl ProductSessionExecutionConfig {
    /// Builds one immutable Chat execution policy.
    ///
    /// # Errors
    ///
    /// Rejects an invalid repository scope, unresolved revision, profile, or
    /// resource limits before any Chat command can be accepted.
    pub fn try_new(
        repository_scope: RepositoryScope,
        checkout_revision: impl Into<String>,
        execution_profile: impl Into<String>,
        max_runtime_seconds: i64,
        max_artifact_bytes: i64,
    ) -> Result<Self, ProductSessionServiceError> {
        repository_scope_key(&repository_scope).map_err(|error| storage_error(&error))?;
        let checkout_revision = checkout_revision.into();
        let execution_profile = execution_profile.into();
        if checkout_revision.is_empty()
            || checkout_revision.len() > 200
            || execution_profile.is_empty()
            || execution_profile.len() > 100
            || !(1..=MAX_RUNTIME_SECONDS).contains(&max_runtime_seconds)
            || !(0..=MAX_ARTIFACT_BYTES).contains(&max_artifact_bytes)
        {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidInput,
                "ProductSession execution configuration is invalid",
            ));
        }
        Ok(Self {
            repository_scope,
            checkout_revision,
            execution_profile,
            max_runtime_seconds,
            max_artifact_bytes,
        })
    }

    #[must_use]
    pub const fn repository_scope(&self) -> &RepositoryScope {
        &self.repository_scope
    }

    pub(super) fn prepare(
        &self,
        context: &ProductSessionCommandContext,
        product_session_id: &ProductSessionId,
        message: &str,
        model_route: &ModelRoute,
    ) -> Result<PreparedProductSessionExecution, ProductSessionServiceError> {
        if message.is_empty() || message.len() > MAX_GOAL_BYTES {
            return Err(service_error(
                ProductSessionServiceErrorCode::MessageLimitExceeded,
                "ProductSession Chat goal is outside the ExecutionJob bound",
            ));
        }
        let expected_scope = public_repository_scope(&self.repository_scope);
        if context.public_scope != expected_scope {
            return Err(service_error(
                ProductSessionServiceErrorCode::BindingIdentityMismatch,
                "ProductSession execution configuration belongs to another repository scope",
            ));
        }
        let start_millis = crate::session_binding_transaction::instant_millis(&context.occurred_at)
            .map_err(|error| storage_error(&error))?;
        let runtime_millis = u64::try_from(self.max_runtime_seconds)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1_000))
            .ok_or_else(|| {
                service_error(
                    ProductSessionServiceErrorCode::InvalidInput,
                    "ProductSession execution deadline is invalid",
                )
            })?;
        let deadline_millis = start_millis.checked_add(runtime_millis).ok_or_else(|| {
            service_error(
                ProductSessionServiceErrorCode::InvalidInput,
                "ProductSession execution deadline is out of range",
            )
        })?;
        let limits = ExecutionLimits {
            deadline_at: instant_from_millis(deadline_millis)
                .map_err(|error| storage_error(&error))?,
            max_artifact_bytes: self.max_artifact_bytes,
            max_runtime_seconds: self.max_runtime_seconds,
        };
        let workspace = ExecutionWorkspace {
            checkout_revision: self.checkout_revision.clone(),
            repository_id: self.repository_scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        };
        let job_id = deterministic_job_id(
            context.receipt_identity.request_id().0.as_bytes(),
            product_session_id,
        );
        let payload_digest = execution_payload_digest(&ExecutionPayloadDigestInput {
            request_id: &context.receipt_identity.request_id().0,
            product_session_id,
            message,
            model_route,
            execution_profile: &self.execution_profile,
            limits: &limits,
            workspace: &workspace,
        })?;
        let job = ExecutionJob {
            attempt: 1,
            execution_profile: self.execution_profile.clone(),
            goal: message.to_owned(),
            job_id: job_id.clone(),
            limits,
            payload_digest: payload_digest.clone(),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: product_session_id.clone(),
            }),
            stage_input: None,
            workspace,
        };
        let dispatch_payload = serde_json::to_vec(&job).map_err(|_| {
            service_error(
                ProductSessionServiceErrorCode::CorruptState,
                "ProductSession ExecutionJob cannot be encoded",
            )
        })?;
        let submission = ExecutionJobSubmission {
            scope: ExecutionQueueScope {
                organization_id: self.repository_scope.organization_id.clone(),
                workspace_id: self.repository_scope.workspace_id.clone(),
                project_id: self.repository_scope.project_id.clone(),
                repository_id: self.repository_scope.repository_id.clone(),
                product_session_id: product_session_id.clone(),
                delivery_id: None,
            },
            job_id,
            request_id: context.receipt_identity.request_id().clone(),
            payload_digest,
            dispatch_payload,
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: context.occurred_at.clone(),
        };
        Ok(PreparedProductSessionExecution { job, submission })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PreparedProductSessionExecution {
    pub(super) job: ExecutionJob,
    pub(super) submission: ExecutionJobSubmission,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionPayloadDigestInput<'input> {
    request_id: &'input str,
    product_session_id: &'input ProductSessionId,
    message: &'input str,
    model_route: &'input ModelRoute,
    execution_profile: &'input str,
    limits: &'input ExecutionLimits,
    workspace: &'input ExecutionWorkspace,
}

fn execution_payload_digest(
    input: &ExecutionPayloadDigestInput<'_>,
) -> Result<Sha256Digest, ProductSessionServiceError> {
    let payload =
        serde_json::to_vec(&("winwincode.product-session-execution.v1", input)).map_err(|_| {
            service_error(
                ProductSessionServiceErrorCode::CorruptState,
                "ProductSession ExecutionJob digest input cannot be encoded",
            )
        })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(payload)
    )))
}

fn deterministic_job_id(
    request_id: &[u8],
    product_session_id: &ProductSessionId,
) -> ExecutionJobId {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.product-session-execution-job.v1\0");
    hasher.update(request_id);
    hasher.update([0]);
    hasher.update(product_session_id.0.as_bytes());
    let encoded = format!("{:X}", hasher.finalize());
    ExecutionJobId(format!("job_{}", &encoded[..26]))
}
