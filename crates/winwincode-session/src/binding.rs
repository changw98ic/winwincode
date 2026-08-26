// SPDX-License-Identifier: Apache-2.0

//! Exact identity binding for ProductSession-backed execution.
//!
//! Binding values are internal domain facts. They are built by validated
//! constructors or by the Control Plane's generated-message adapter; they do
//! not deserialize directly from wire JSON.
//!
//! ```compile_fail
//! use serde::Deserialize;
//! use winwincode_session::SessionBinding;
//!
//! let _binding: SessionBinding = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use serde::Deserialize;
//! use winwincode_session::SessionBindingIdentity;
//!
//! let _identity: SessionBindingIdentity = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use serde::Serialize;
//! use winwincode_session::SessionBinding;
//!
//! fn serialize_binding(binding: &SessionBinding) {
//!     let _ = serde_json::to_string(binding).unwrap();
//! }
//! ```
//!
//! ```compile_fail
//! use winwincode_session::SessionBinding;
//!
//! let _binding = SessionBinding::default();
//! ```
//!
//! ```compile_fail
//! use winwincode_session::BindingAuthority;
//! use winwincode_domain::{FencingToken, LeaseId, WorkerId, WorkerInstanceId};
//!
//! let _authority = BindingAuthority::new(
//!     WorkerId("wrk_00000000000000000000000001".to_owned()),
//!     WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
//!     LeaseId("lse_00000000000000000000000001".to_owned()),
//!     1,
//!     FencingToken("1".to_owned()),
//! );
//! ```

use std::fmt;

use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, LeaseId, ProductSessionId,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

/// Execution scope carried by one binding. Chat has no hidden Delivery or
/// `StageRun` identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingScope {
    ProductSession,
    DeliveryStage {
        delivery_id: DeliveryId,
        delivery_task_id: Option<DeliveryTaskId>,
        stage_run_id: StageRunId,
    },
}

/// Immutable ProductSession/Delivery/StageRun/Job identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingIdentity {
    scope: BindingScope,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
}

/// Runtime source identity copied from a validated generated source identity.
///
/// This value has no wire representation and carries no authority. Lease
/// validity remains owned by the scheduler's sealed authority at the Control
/// Plane seam.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSourceIdentity {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    lease_id: LeaseId,
}

/// Exact binding joining independent session identities. The scheduler lease
/// authority is deliberately not stored here: callers retain the one sealed
/// authority supplied by the Control Plane adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    identity: SessionBindingIdentity,
    worker_session_id: Option<WorkerSessionId>,
    codex_thread_id: Option<CodexThreadId>,
    source_identity: Option<RuntimeSourceIdentity>,
}

/// Binding construction and matching failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBindingError {
    InvalidIdentity(&'static str),
    InvalidSourceIdentity(&'static str),
    InvalidScope(&'static str),
    WorkerSessionRequired,
    ConflictingIdentity(&'static str),
}

impl fmt::Display for SessionBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(field) => write!(formatter, "invalid binding identity: {field}"),
            Self::InvalidSourceIdentity(field) => {
                write!(formatter, "invalid runtime source identity: {field}")
            }
            Self::InvalidScope(field) => write!(formatter, "invalid binding scope: {field}"),
            Self::WorkerSessionRequired => {
                formatter.write_str("CodexThread requires an accepted WorkerSession")
            }
            Self::ConflictingIdentity(field) => {
                write!(formatter, "conflicting binding identity: {field}")
            }
        }
    }
}

impl std::error::Error for SessionBindingError {}

impl BindingScope {
    fn validate(&self) -> Result<(), SessionBindingError> {
        match self {
            Self::ProductSession => Ok(()),
            Self::DeliveryStage {
                delivery_id,
                delivery_task_id,
                stage_run_id,
            } => {
                validate_id(&delivery_id.0, "deliveryId", "dlv_")?;
                if let Some(task_id) = delivery_task_id {
                    validate_id(&task_id.0, "deliveryTaskId", "dtk_")?;
                }
                validate_id(&stage_run_id.0, "stageRunId", "run_")
            }
        }
    }

    #[must_use]
    pub fn delivery_id(&self) -> Option<&DeliveryId> {
        match self {
            Self::ProductSession => None,
            Self::DeliveryStage { delivery_id, .. } => Some(delivery_id),
        }
    }

    #[must_use]
    pub fn delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        match self {
            Self::ProductSession => None,
            Self::DeliveryStage {
                delivery_task_id, ..
            } => delivery_task_id.as_ref(),
        }
    }

    #[must_use]
    pub fn stage_run_id(&self) -> Option<&StageRunId> {
        match self {
            Self::ProductSession => None,
            Self::DeliveryStage { stage_run_id, .. } => Some(stage_run_id),
        }
    }
}

#[allow(clippy::missing_errors_doc)]
impl SessionBindingIdentity {
    /// Constructs the Chat/ProductSession scope without creating Delivery facts.
    pub fn product_session(
        product_session_id: ProductSessionId,
        execution_job_id: ExecutionJobId,
    ) -> Result<Self, SessionBindingError> {
        Self::try_new(
            BindingScope::ProductSession,
            product_session_id,
            execution_job_id,
        )
    }

    /// Constructs a Delivery stage scope. A Delivery-level task is represented by `None`.
    pub fn delivery_stage(
        delivery_id: DeliveryId,
        delivery_task_id: Option<DeliveryTaskId>,
        stage_run_id: StageRunId,
        product_session_id: ProductSessionId,
        execution_job_id: ExecutionJobId,
    ) -> Result<Self, SessionBindingError> {
        Self::try_new(
            BindingScope::DeliveryStage {
                delivery_id,
                delivery_task_id,
                stage_run_id,
            },
            product_session_id,
            execution_job_id,
        )
    }

    /// Constructs and validates an identity for either supported scope.
    pub fn try_new(
        scope: BindingScope,
        product_session_id: ProductSessionId,
        execution_job_id: ExecutionJobId,
    ) -> Result<Self, SessionBindingError> {
        scope.validate()?;
        validate_id(&product_session_id.0, "productSessionId", "psn_")?;
        validate_id(&execution_job_id.0, "executionJobId", "job_")?;
        Ok(Self {
            scope,
            product_session_id,
            execution_job_id,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> &BindingScope {
        &self.scope
    }

    #[must_use]
    pub const fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    #[must_use]
    pub const fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    #[must_use]
    pub fn delivery_id(&self) -> Option<&DeliveryId> {
        self.scope.delivery_id()
    }

    #[must_use]
    pub fn delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.scope.delivery_task_id()
    }

    #[must_use]
    pub fn stage_run_id(&self) -> Option<&StageRunId> {
        self.scope.stage_run_id()
    }
}

#[allow(clippy::missing_errors_doc)]
impl RuntimeSourceIdentity {
    /// Creates the source facts emitted by one accepted execution worker.
    pub fn execution_worker(
        lease_id: LeaseId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_session_id: WorkerSessionId,
    ) -> Result<Self, SessionBindingError> {
        validate_id(&lease_id.0, "leaseId", "lse_")?;
        validate_id(&worker_id.0, "workerId", "wrk_")?;
        validate_id(&worker_instance_id.0, "workerInstanceId", "wki_")?;
        validate_id(&worker_session_id.0, "workerSessionId", "wsn_")?;
        Ok(Self {
            worker_id,
            worker_instance_id,
            worker_session_id,
            lease_id,
        })
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub const fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    #[must_use]
    pub const fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }
}

#[allow(clippy::missing_errors_doc)]
impl SessionBinding {
    /// Creates a binding with pending `WorkerSession` and `CodexThread` attachments.
    pub fn pending(identity: SessionBindingIdentity) -> Result<Self, SessionBindingError> {
        Self::try_new(identity, None, None, None)
    }

    /// Creates and validates a binding with optional owner-reported attachments.
    pub fn try_new(
        identity: SessionBindingIdentity,
        worker_session_id: Option<WorkerSessionId>,
        codex_thread_id: Option<CodexThreadId>,
        source_identity: Option<RuntimeSourceIdentity>,
    ) -> Result<Self, SessionBindingError> {
        if codex_thread_id.is_some() && worker_session_id.is_none() {
            return Err(SessionBindingError::WorkerSessionRequired);
        }
        if let Some(worker_session_id) = &worker_session_id {
            validate_id(&worker_session_id.0, "workerSessionId", "wsn_")?;
        }
        if let Some(codex_thread_id) = &codex_thread_id {
            validate_id(&codex_thread_id.0, "codexThreadId", "cdx_")?;
        }
        if let Some(source_identity) = &source_identity
            && source_identity.worker_session_id()
                != worker_session_id
                    .as_ref()
                    .unwrap_or(source_identity.worker_session_id())
        {
            return Err(SessionBindingError::ConflictingIdentity(
                "sourceIdentity.workerSessionId",
            ));
        }
        Ok(Self {
            identity,
            worker_session_id,
            codex_thread_id,
            source_identity,
        })
    }

    /// Adds or confirms the `WorkerSession` reported by the Worker.
    pub fn accept_worker_session(
        &self,
        worker_session_id: WorkerSessionId,
    ) -> Result<Self, SessionBindingError> {
        validate_id(&worker_session_id.0, "workerSessionId", "wsn_")?;
        if self
            .worker_session_id
            .as_ref()
            .is_some_and(|current| current != &worker_session_id)
        {
            return Err(SessionBindingError::ConflictingIdentity("workerSessionId"));
        }
        if self
            .source_identity
            .as_ref()
            .is_some_and(|source| source.worker_session_id() != &worker_session_id)
        {
            return Err(SessionBindingError::ConflictingIdentity(
                "sourceIdentity.workerSessionId",
            ));
        }
        let mut next = self.clone();
        next.worker_session_id = Some(worker_session_id);
        Ok(next)
    }

    /// Adds or confirms the `CodexThread` reported by the Codex adapter.
    pub fn accept_codex_thread(
        &self,
        codex_thread_id: CodexThreadId,
    ) -> Result<Self, SessionBindingError> {
        if self.worker_session_id.is_none() {
            return Err(SessionBindingError::WorkerSessionRequired);
        }
        validate_id(&codex_thread_id.0, "codexThreadId", "cdx_")?;
        if self
            .codex_thread_id
            .as_ref()
            .is_some_and(|current| current != &codex_thread_id)
        {
            return Err(SessionBindingError::ConflictingIdentity("codexThreadId"));
        }
        let mut next = self.clone();
        next.codex_thread_id = Some(codex_thread_id);
        Ok(next)
    }

    /// Adds or confirms the accepted runtime source identity.
    pub fn with_source_identity(
        &self,
        source_identity: RuntimeSourceIdentity,
    ) -> Result<Self, SessionBindingError> {
        if let Some(worker_session_id) = &self.worker_session_id
            && source_identity.worker_session_id() != worker_session_id
        {
            return Err(SessionBindingError::ConflictingIdentity(
                "sourceIdentity.workerSessionId",
            ));
        }
        if self
            .source_identity
            .as_ref()
            .is_some_and(|current| current != &source_identity)
        {
            return Err(SessionBindingError::ConflictingIdentity("sourceIdentity"));
        }
        let mut next = self.clone();
        next.source_identity = Some(source_identity);
        Ok(next)
    }

    #[must_use]
    pub const fn identity(&self) -> &SessionBindingIdentity {
        &self.identity
    }

    #[must_use]
    pub fn delivery_id(&self) -> Option<&DeliveryId> {
        self.identity.delivery_id()
    }

    #[must_use]
    pub fn delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.identity.delivery_task_id()
    }

    #[must_use]
    pub fn stage_run_id(&self) -> Option<&StageRunId> {
        self.identity.stage_run_id()
    }

    #[must_use]
    pub const fn product_session_id(&self) -> &ProductSessionId {
        self.identity.product_session_id()
    }

    #[must_use]
    pub const fn execution_job_id(&self) -> &ExecutionJobId {
        self.identity.execution_job_id()
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> Option<&WorkerSessionId> {
        self.worker_session_id.as_ref()
    }

    #[must_use]
    pub const fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.codex_thread_id.as_ref()
    }

    #[must_use]
    pub const fn source_identity(&self) -> Option<&RuntimeSourceIdentity> {
        self.source_identity.as_ref()
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.worker_session_id.is_some()
            && self.codex_thread_id.is_some()
            && self.source_identity.is_some()
    }

    #[must_use]
    pub fn matches_identity(&self, identity: &SessionBindingIdentity) -> bool {
        &self.identity == identity
    }
}

fn validate_id(value: &str, field: &'static str, prefix: &str) -> Result<(), SessionBindingError> {
    if !canonical_id(value, prefix) {
        return Err(SessionBindingError::InvalidIdentity(field));
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H'
                            | b'J'..=b'K'
                            | b'M'..=b'N'
                            | b'P'..=b'T'
                            | b'V'..=b'Z'
                    )
            })
    })
}
