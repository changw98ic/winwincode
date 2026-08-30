// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId, OrganizationId, ProductSessionId,
    ProjectId, PublicationId, RepositoryId, RequestId, ServiceAccountId, Sha256Digest, StageRunId,
    SystemActorId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};

use crate::store::AuditError;

const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable identity for one immutable audit event.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AuditEventId(String);

impl AuditEventId {
    /// Creates the canonical `aud_` identity used by the audit ledger.
    ///
    /// # Errors
    ///
    /// Rejects values outside the canonical Crockford identifier format.
    pub fn try_new(value: impl Into<String>) -> Result<Self, AuditError> {
        let value = value.into();
        if !canonical_id(&value, "aud") {
            return Err(AuditError::invalid(
                "audit event id must be aud_ followed by 26 Crockford characters",
            ));
        }
        Ok(Self(value))
    }

    /// Derives one stable audit identity from an already-canonical SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Rejects a malformed digest. The digest bytes themselves are not retained.
    pub fn from_digest(digest: &Sha256Digest) -> Result<Self, AuditError> {
        validate_digest(digest, "audit event identity digest")?;
        let hex = digest
            .0
            .strip_prefix("sha256:")
            .ok_or_else(|| AuditError::invalid("audit event identity digest is invalid"))?;
        let mut first_128_bits = [0_u8; 16];
        for (index, byte) in first_128_bits.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&hex[offset..offset + 2], 16)
                .map_err(|_| AuditError::invalid("audit event identity digest is invalid"))?;
        }
        let mut value = u128::from_be_bytes(first_128_bits);
        let mut encoded = [b'0'; 26];
        for byte in encoded.iter_mut().rev() {
            *byte = CROCKFORD_BASE32[(value & 31) as usize];
            value >>= 5;
        }
        let suffix = encoded.into_iter().map(char::from).collect::<String>();
        Ok(Self(format!("aud_{suffix}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable authenticated actor identity. Authentication proof is deliberately
/// outside the audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditActor {
    User(UserId),
    ServiceAccount(ServiceAccountId),
    System(SystemActorId),
}

/// Exact tenant scope attached to one audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditScope {
    Organization {
        organization_id: OrganizationId,
    },
    Workspace {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    Project {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
}

impl AuditScope {
    /// Builds an organization scope.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical organization identity.
    pub fn organization(organization_id: OrganizationId) -> Result<Self, AuditError> {
        let scope = Self::Organization { organization_id };
        scope.validate()?;
        Ok(scope)
    }

    /// Builds a workspace scope.
    ///
    /// # Errors
    ///
    /// Rejects any non-canonical identity.
    pub fn workspace(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Self, AuditError> {
        let scope = Self::Workspace {
            organization_id,
            workspace_id,
        };
        scope.validate()?;
        Ok(scope)
    }

    /// Builds a project scope.
    ///
    /// # Errors
    ///
    /// Rejects any non-canonical identity.
    pub fn project(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> Result<Self, AuditError> {
        let scope = Self::Project {
            organization_id,
            workspace_id,
            project_id,
        };
        scope.validate()?;
        Ok(scope)
    }

    /// Builds a repository scope.
    ///
    /// # Errors
    ///
    /// Rejects any non-canonical identity.
    pub fn repository(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    ) -> Result<Self, AuditError> {
        let scope = Self::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        };
        scope.validate()?;
        Ok(scope)
    }

    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        match self {
            Self::Organization { organization_id }
            | Self::Workspace {
                organization_id, ..
            }
            | Self::Project {
                organization_id, ..
            }
            | Self::Repository {
                organization_id, ..
            } => organization_id,
        }
    }

    #[must_use]
    /// Converts a scope that the policy layer already authorized into read
    /// access. This conversion does not authenticate or authorize a caller.
    pub fn into_access(self) -> AuditAccess {
        AuditAccess { scope: self }
    }

    pub(crate) fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            Self::Organization { .. } => None,
            Self::Workspace { workspace_id, .. }
            | Self::Project { workspace_id, .. }
            | Self::Repository { workspace_id, .. } => Some(workspace_id),
        }
    }

    pub(crate) fn project_id(&self) -> Option<&ProjectId> {
        match self {
            Self::Organization { .. } | Self::Workspace { .. } => None,
            Self::Project { project_id, .. } | Self::Repository { project_id, .. } => {
                Some(project_id)
            }
        }
    }

    pub(crate) fn repository_id(&self) -> Option<&RepositoryId> {
        match self {
            Self::Repository { repository_id, .. } => Some(repository_id),
            Self::Organization { .. } | Self::Workspace { .. } | Self::Project { .. } => None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), AuditError> {
        if !canonical_id(&self.organization_id().0, "org")
            || self
                .workspace_id()
                .is_some_and(|id| !canonical_id(&id.0, "wsp"))
            || self
                .project_id()
                .is_some_and(|id| !canonical_id(&id.0, "prj"))
            || self
                .repository_id()
                .is_some_and(|id| !canonical_id(&id.0, "rep"))
        {
            return Err(AuditError::invalid(
                "audit scope contains a non-canonical identity",
            ));
        }
        Ok(())
    }
}

/// Scope authority supplied by the policy layer for one audit read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditAccess {
    pub(crate) scope: AuditScope,
}

impl AuditAccess {
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
}

/// Closed action category plus one stable, non-sensitive operation name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditAction {
    kind: AuditActionKind,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_invocation: Option<AuditModelInvocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_reference_id: Option<CredentialReferenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditActionKind {
    Business,
    Administration,
    Command,
    Approval,
    Policy,
    Credential,
    WorkerLease,
    Provider,
    ModelInvocation,
    DeliveryState,
    Publication,
}

/// Secret-safe summary of one model call. Raw prompts, responses, provider
/// requests, and credentials have no representation in this type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditModelInvocation {
    provider_id: String,
    model_id: String,
    input_digest: Sha256Digest,
    output_digest: Sha256Digest,
    input_tokens: u64,
    output_tokens: u64,
}

impl AuditModelInvocation {
    /// Builds one bounded model-call summary.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary provider/model text, malformed digests, or usage
    /// counters outside the exact JSON integer range.
    pub fn try_new(
        provider_id: &str,
        model_id: &str,
        input_digest: Sha256Digest,
        output_digest: Sha256Digest,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<Self, AuditError> {
        let summary = Self {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            input_digest,
            output_digest,
            input_tokens,
            output_tokens,
        };
        summary.validate()?;
        Ok(summary)
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[must_use]
    pub const fn input_digest(&self) -> &Sha256Digest {
        &self.input_digest
    }

    #[must_use]
    pub const fn output_digest(&self) -> &Sha256Digest {
        &self.output_digest
    }

    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    fn validate(&self) -> Result<(), AuditError> {
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
        validate_token(&self.provider_id, "audit model provider")?;
        validate_token(&self.model_id, "audit model identity")?;
        validate_digest(&self.input_digest, "model input digest")?;
        validate_digest(&self.output_digest, "model output digest")?;
        if self.input_tokens > MAX_SAFE_INTEGER || self.output_tokens > MAX_SAFE_INTEGER {
            return Err(AuditError::invalid(
                "audit model usage exceeds the exact JSON integer range",
            ));
        }
        Ok(())
    }
}

impl AuditAction {
    /// Builds a business operation from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn business(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Business, name)
    }

    /// Builds an administrative operation from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn administration(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Administration, name)
    }

    /// Builds a command action from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn command(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Command, name)
    }

    /// Builds an approval action from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn approval(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Approval, name)
    }

    /// Builds a policy action from one stable canonical name or rule ID.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn policy(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Policy, name)
    }

    /// Builds a secret-safe Credential operation from one stable canonical
    /// name. Credential material has no representation in an audit action.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn credential(
        name: &str,
        credential_reference_id: CredentialReferenceId,
    ) -> Result<Self, AuditError> {
        validate_token(name, "audit action name")?;
        if !canonical_id(&credential_reference_id.0, "crd") {
            return Err(AuditError::invalid(
                "credential audit action identity is not canonical",
            ));
        }
        Ok(Self {
            kind: AuditActionKind::Credential,
            name: name.to_owned(),
            model_invocation: None,
            credential_reference_id: Some(credential_reference_id),
            provider_id: None,
        })
    }

    /// Builds a Worker lease action from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn worker_lease(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::WorkerLease, name)
    }

    /// Builds a secret-safe Provider operation from one stable canonical
    /// name. Provider requests and responses remain outside the audit event.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn provider(provider_id: &str, name: &str) -> Result<Self, AuditError> {
        validate_token(provider_id, "audit Provider identity")?;
        validate_token(name, "audit action name")?;
        Ok(Self {
            kind: AuditActionKind::Provider,
            name: name.to_owned(),
            model_invocation: None,
            credential_reference_id: None,
            provider_id: Some(provider_id.to_owned()),
        })
    }

    /// Builds a Delivery state action from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn delivery_state(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::DeliveryState, name)
    }

    /// Builds a Publication action from one stable canonical name.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded text.
    pub fn publication(name: &str) -> Result<Self, AuditError> {
        Self::new(AuditActionKind::Publication, name)
    }

    fn new(kind: AuditActionKind, name: &str) -> Result<Self, AuditError> {
        validate_token(name, "audit action name")?;
        Ok(Self {
            kind,
            name: name.to_owned(),
            model_invocation: None,
            credential_reference_id: None,
            provider_id: None,
        })
    }

    /// Binds a model invocation action to its exact secret-safe summary.
    ///
    /// # Errors
    ///
    /// Rejects an invalid model summary.
    pub fn model_invocation(summary: AuditModelInvocation) -> Result<Self, AuditError> {
        summary.validate()?;
        Ok(Self {
            kind: AuditActionKind::ModelInvocation,
            name: "model.invoke".to_owned(),
            model_invocation: Some(summary),
            credential_reference_id: None,
            provider_id: None,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> AuditActionKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn model_summary(&self) -> Option<&AuditModelInvocation> {
        self.model_invocation.as_ref()
    }

    #[must_use]
    pub const fn credential_reference_id(&self) -> Option<&CredentialReferenceId> {
        self.credential_reference_id.as_ref()
    }

    #[must_use]
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    fn validate(&self) -> Result<(), AuditError> {
        validate_token(&self.name, "audit action name")?;
        match (
            self.kind,
            &self.model_invocation,
            &self.credential_reference_id,
            &self.provider_id,
        ) {
            (AuditActionKind::ModelInvocation, Some(summary), None, None) => summary.validate(),
            (AuditActionKind::Credential, None, Some(id), None) if canonical_id(&id.0, "crd") => {
                Ok(())
            }
            (AuditActionKind::Provider, None, None, Some(provider_id)) => {
                validate_token(provider_id, "audit Provider identity")
            }
            (AuditActionKind::ModelInvocation, None, None, None) => Err(AuditError::invalid(
                "model invocation audit action requires its sealed summary",
            )),
            (AuditActionKind::Credential, None, None, None) => Err(AuditError::invalid(
                "Credential audit action requires its stable reference identity",
            )),
            (AuditActionKind::Provider, None, None, None) => Err(AuditError::invalid(
                "Provider audit action requires its stable Provider identity",
            )),
            (_, None, None, None) => Ok(()),
            _ => Err(AuditError::invalid(
                "audit action carries facts from a different action category",
            )),
        }
    }
}

/// Whether the audited operation changed canonical state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditState {
    Changed {
        before: Option<Sha256Digest>,
        after: Sha256Digest,
    },
    Unchanged {
        current: Option<Sha256Digest>,
    },
}

impl AuditState {
    /// Builds one exact state transition.
    ///
    /// # Errors
    ///
    /// Rejects malformed or identical before/after digests.
    pub fn changed(before: Option<Sha256Digest>, after: Sha256Digest) -> Result<Self, AuditError> {
        if before.as_ref().is_some_and(|value| value == &after) {
            return Err(AuditError::invalid(
                "changed audit state must have different before and after digests",
            ));
        }
        let state = Self::Changed { before, after };
        state.validate()?;
        Ok(state)
    }

    /// Builds an operation that left canonical state unchanged.
    ///
    /// # Errors
    ///
    /// Rejects a malformed current digest.
    pub fn unchanged(current: Option<Sha256Digest>) -> Result<Self, AuditError> {
        let state = Self::Unchanged { current };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), AuditError> {
        match self {
            Self::Changed { before, after } => {
                if before.as_ref().is_some_and(|value| value == after) {
                    return Err(AuditError::invalid(
                        "changed audit state must have different before and after digests",
                    ));
                }
                if let Some(before) = before {
                    validate_digest(before, "before state digest")?;
                }
                validate_digest(after, "after state digest")
            }
            Self::Unchanged { current } => {
                if let Some(current) = current {
                    validate_digest(current, "current state digest")?;
                }
                Ok(())
            }
        }
    }
}

/// Closed request origin. Forwarded headers, user agents, and arbitrary source
/// text never enter the event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditOrigin {
    Local { component: String },
    Network { source_ip: IpAddr },
}

impl AuditOrigin {
    /// Builds a local component origin.
    ///
    /// # Errors
    ///
    /// Rejects arbitrary or unbounded component text.
    pub fn local(component: &str) -> Result<Self, AuditError> {
        validate_token(component, "audit local component")?;
        Ok(Self::Local {
            component: component.to_owned(),
        })
    }

    #[must_use]
    pub const fn network(source_ip: IpAddr) -> Self {
        Self::Network { source_ip }
    }

    fn validate(&self) -> Result<(), AuditError> {
        match self {
            Self::Local { component } => validate_token(component, "audit local component"),
            Self::Network { .. } => Ok(()),
        }
    }
}

/// The phase that supplied the accepted binding source fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditBindingPhase {
    WorkerSession,
    CodexThread,
}

/// The narrow source fact for an accepted binding. Binding messages do not
/// carry an execution-event sequence, so the audit record names the exact
/// message and binding phase instead of inventing a zero sequence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AuditBindingSource {
    message_id: ExecutionMessageId,
    phase: AuditBindingPhase,
}

impl AuditBindingSource {
    /// Builds the source fact from the exact accepted `ExecutionPort` message.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError`] when the message identity is not canonical.
    pub fn try_new(
        message_id: ExecutionMessageId,
        phase: AuditBindingPhase,
    ) -> Result<Self, AuditError> {
        if !canonical_id(&message_id.0, "xmsg") {
            return Err(AuditError::invalid(
                "binding audit source message id is not canonical",
            ));
        }
        Ok(Self { message_id, phase })
    }

    #[must_use]
    pub const fn message_id(&self) -> &ExecutionMessageId {
        &self.message_id
    }

    #[must_use]
    pub const fn phase(&self) -> AuditBindingPhase {
        self.phase
    }

    fn validate(&self) -> Result<(), AuditError> {
        if !canonical_id(&self.message_id.0, "xmsg") {
            return Err(AuditError::invalid(
                "binding audit source message id is not canonical",
            ));
        }
        Ok(())
    }
}

/// The closed execution identity carried by an accepted binding, runtime, or
/// terminal audit subject. Every field is required except the Delivery task;
/// callers cannot represent a partially joined execution identity. Runtime
/// and terminal facts carry a positive execution acknowledgement sequence.
/// An accepted binding instead carries [`AuditBindingSource`] because its wire
/// message has no execution-event sequence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AuditExecutionIdentity {
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    stage_run_id: StageRunId,
    execution_job_id: ExecutionJobId,
    delivery_id: DeliveryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery_task_id: Option<DeliveryTaskId>,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    lease_id: LeaseId,
    attempt: u64,
    fencing_token: FencingToken,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_sequence: Option<ExecutionAckSequence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding_source: Option<AuditBindingSource>,
}

impl AuditExecutionIdentity {
    /// Builds one complete runtime or terminal execution identity from the
    /// canonical domain IDs and a proven execution acknowledgement sequence.
    ///
    /// The returned value is suitable for all three execution subject
    /// branches. It does not mint or persist an identity and does not call the
    /// Control Plane.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical IDs, a zero or out-of-range attempt, an invalid
    /// fencing token, or a non-positive/out-of-range source sequence.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        product_session_id: ProductSessionId,
        worker_session_id: WorkerSessionId,
        codex_thread_id: CodexThreadId,
        stage_run_id: StageRunId,
        execution_job_id: ExecutionJobId,
        delivery_id: DeliveryId,
        delivery_task_id: Option<DeliveryTaskId>,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lease_id: LeaseId,
        attempt: u64,
        fencing_token: FencingToken,
        source_sequence: ExecutionAckSequence,
    ) -> Result<Self, AuditError> {
        let identity = Self {
            product_session_id,
            worker_session_id,
            codex_thread_id,
            stage_run_id,
            execution_job_id,
            delivery_id,
            delivery_task_id,
            worker_id,
            worker_instance_id,
            lease_id,
            attempt,
            fencing_token,
            source_sequence: Some(source_sequence),
            binding_source: None,
        };
        identity.validate()?;
        Ok(identity)
    }

    /// Builds one complete accepted-binding identity from its exact source
    /// message and phase. No synthetic execution sequence is accepted.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError`] when an identity is not canonical, the attempt
    /// or fencing token is invalid, or the binding source is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_binding(
        product_session_id: ProductSessionId,
        worker_session_id: WorkerSessionId,
        codex_thread_id: CodexThreadId,
        stage_run_id: StageRunId,
        execution_job_id: ExecutionJobId,
        delivery_id: DeliveryId,
        delivery_task_id: Option<DeliveryTaskId>,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lease_id: LeaseId,
        attempt: u64,
        fencing_token: FencingToken,
        binding_source: AuditBindingSource,
    ) -> Result<Self, AuditError> {
        let identity = Self {
            product_session_id,
            worker_session_id,
            codex_thread_id,
            stage_run_id,
            execution_job_id,
            delivery_id,
            delivery_task_id,
            worker_id,
            worker_instance_id,
            lease_id,
            attempt,
            fencing_token,
            source_sequence: None,
            binding_source: Some(binding_source),
        };
        identity.validate()?;
        Ok(identity)
    }

    #[must_use]
    pub const fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    #[must_use]
    pub const fn codex_thread_id(&self) -> &CodexThreadId {
        &self.codex_thread_id
    }

    #[must_use]
    pub const fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    #[must_use]
    pub const fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.delivery_task_id.as_ref()
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
    pub const fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    #[must_use]
    pub const fn fencing_token(&self) -> &FencingToken {
        &self.fencing_token
    }

    #[must_use]
    pub const fn source_sequence(&self) -> Option<&ExecutionAckSequence> {
        self.source_sequence.as_ref()
    }

    #[must_use]
    pub const fn binding_source(&self) -> Option<&AuditBindingSource> {
        self.binding_source.as_ref()
    }

    fn validate(&self) -> Result<(), AuditError> {
        let identities = [
            (&self.product_session_id.0, "psn", "ProductSession"),
            (&self.worker_session_id.0, "wsn", "WorkerSession"),
            (&self.codex_thread_id.0, "cdx", "CodexThread"),
            (&self.stage_run_id.0, "run", "StageRun"),
            (&self.execution_job_id.0, "job", "ExecutionJob"),
            (&self.delivery_id.0, "dlv", "Delivery"),
            (&self.worker_id.0, "wrk", "Worker"),
            (&self.worker_instance_id.0, "wki", "WorkerInstance"),
            (&self.lease_id.0, "lse", "Lease"),
        ];
        if identities
            .iter()
            .any(|(value, prefix, _)| !canonical_id(value, prefix))
            || self
                .delivery_task_id
                .as_ref()
                .is_some_and(|id| !canonical_id(&id.0, "dtk"))
        {
            return Err(AuditError::invalid(
                "execution audit subject contains a non-canonical identity",
            ));
        }
        if self.attempt == 0 || self.attempt > MAX_SAFE_INTEGER {
            return Err(AuditError::invalid(
                "execution audit attempt is outside the supported range",
            ));
        }
        match (&self.source_sequence, &self.binding_source) {
            (Some(source_sequence), None)
                if source_sequence.0 > 0
                    && source_sequence.0.cast_unsigned() <= MAX_SAFE_INTEGER => {}
            (None, Some(binding_source)) => binding_source.validate()?,
            (Some(_), None) => {
                return Err(AuditError::invalid(
                    "execution audit source sequence is outside the supported range",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(AuditError::invalid(
                    "execution audit identity cannot carry both sequence and binding source",
                ));
            }
            (None, None) => {
                return Err(AuditError::invalid(
                    "execution audit identity requires a sequence or binding source",
                ));
            }
        }
        if !canonical_fencing_token(&self.fencing_token.0) {
            return Err(AuditError::invalid(
                "execution audit fencing token is not canonical",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct AuditExecutionIdentityWire {
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    stage_run_id: StageRunId,
    execution_job_id: ExecutionJobId,
    delivery_id: DeliveryId,
    #[serde(default)]
    delivery_task_id: Option<DeliveryTaskId>,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    lease_id: LeaseId,
    attempt: u64,
    fencing_token: FencingToken,
    #[serde(default)]
    source_sequence: Option<ExecutionAckSequence>,
    #[serde(default)]
    binding_source: Option<AuditBindingSource>,
}

impl<'de> Deserialize<'de> for AuditExecutionIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = AuditExecutionIdentityWire::deserialize(deserializer)?;
        let AuditExecutionIdentityWire {
            product_session_id,
            worker_session_id,
            codex_thread_id,
            stage_run_id,
            execution_job_id,
            delivery_id,
            delivery_task_id,
            worker_id,
            worker_instance_id,
            lease_id,
            attempt,
            fencing_token,
            source_sequence,
            binding_source,
        } = wire;
        let identity = match (source_sequence, binding_source) {
            (Some(source_sequence), None) => Self::try_new(
                product_session_id,
                worker_session_id,
                codex_thread_id,
                stage_run_id,
                execution_job_id,
                delivery_id,
                delivery_task_id,
                worker_id,
                worker_instance_id,
                lease_id,
                attempt,
                fencing_token,
                source_sequence,
            ),
            (None, Some(binding_source)) => Self::try_new_binding(
                product_session_id,
                worker_session_id,
                codex_thread_id,
                stage_run_id,
                execution_job_id,
                delivery_id,
                delivery_task_id,
                worker_id,
                worker_instance_id,
                lease_id,
                attempt,
                fencing_token,
                binding_source,
            ),
            (Some(_), Some(_)) => Err(AuditError::invalid(
                "execution audit identity cannot carry both sequence and binding source",
            )),
            (None, None) => Err(AuditError::invalid(
                "execution audit identity requires a sequence or binding source",
            )),
        };
        identity.map_err(serde::de::Error::custom)
    }
}

/// The execution record branch. Keeping this discriminant separate prevents
/// an accepted binding, runtime event, and terminal outcome from being
/// represented as an untyped or partially populated subject.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditExecutionSubjectKind {
    AcceptedBinding,
    Runtime,
    Terminal,
}

/// Closed top-level subject categories. Publication subjects remain on their
/// own branch; execution subjects never share optional publication fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditSubjectKind {
    References,
    Publication,
    Execution(AuditExecutionSubjectKind),
}

impl AuditSubjectKind {
    #[must_use]
    pub const fn execution_kind(self) -> Option<AuditExecutionSubjectKind> {
        match self {
            Self::Execution(kind) => Some(kind),
            Self::References | Self::Publication => None,
        }
    }
}

/// Product identities joined to an audit event. No raw model or publication
/// body is accepted. The legacy reference builder remains the publication
/// branch; execution records use complete typed identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuditSubject {
    References {
        delivery: Option<DeliveryId>,
        product_session: Option<ProductSessionId>,
        lease: Option<LeaseId>,
        publication: Option<PublicationId>,
    },
    Publication {
        delivery: DeliveryId,
        publication: PublicationId,
    },
    AcceptedBinding(AuditExecutionIdentity),
    Runtime(AuditExecutionIdentity),
    Terminal(AuditExecutionIdentity),
}

impl Default for AuditSubject {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditSubject {
    #[must_use]
    pub const fn new() -> Self {
        Self::References {
            delivery: None,
            product_session: None,
            lease: None,
            publication: None,
        }
    }

    #[must_use]
    pub fn accepted_binding(identity: AuditExecutionIdentity) -> Self {
        Self::AcceptedBinding(identity)
    }

    #[must_use]
    pub fn runtime(identity: AuditExecutionIdentity) -> Self {
        Self::Runtime(identity)
    }

    #[must_use]
    pub fn terminal(identity: AuditExecutionIdentity) -> Self {
        Self::Terminal(identity)
    }

    #[must_use]
    pub const fn kind(&self) -> AuditSubjectKind {
        match self {
            Self::References { .. } => AuditSubjectKind::References,
            Self::Publication { .. } => AuditSubjectKind::Publication,
            Self::AcceptedBinding(_) => {
                AuditSubjectKind::Execution(AuditExecutionSubjectKind::AcceptedBinding)
            }
            Self::Runtime(_) => AuditSubjectKind::Execution(AuditExecutionSubjectKind::Runtime),
            Self::Terminal(_) => AuditSubjectKind::Execution(AuditExecutionSubjectKind::Terminal),
        }
    }

    #[must_use]
    pub const fn execution_kind(&self) -> Option<AuditExecutionSubjectKind> {
        match self.kind() {
            AuditSubjectKind::Execution(kind) => Some(kind),
            AuditSubjectKind::References | AuditSubjectKind::Publication => None,
        }
    }

    #[must_use]
    pub const fn execution(&self) -> Option<&AuditExecutionIdentity> {
        match self {
            Self::AcceptedBinding(identity)
            | Self::Runtime(identity)
            | Self::Terminal(identity) => Some(identity),
            Self::References { .. } | Self::Publication { .. } => None,
        }
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when called on a closed execution subject. Execution subjects
    /// must be constructed with a complete execution identity.
    pub fn with_delivery(self, delivery_id: DeliveryId) -> Self {
        match self {
            Self::References {
                product_session,
                lease,
                publication,
                ..
            } => {
                if let Some(publication_id) = publication {
                    if product_session.is_none() && lease.is_none() {
                        return Self::Publication {
                            delivery: delivery_id,
                            publication: publication_id,
                        };
                    }
                    return Self::References {
                        delivery: Some(delivery_id),
                        product_session,
                        lease,
                        publication: Some(publication_id),
                    };
                }
                Self::References {
                    delivery: Some(delivery_id),
                    product_session,
                    lease,
                    publication,
                }
            }
            Self::Publication { publication, .. } => Self::Publication {
                delivery: delivery_id,
                publication,
            },
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => {
                panic!("execution audit subjects cannot be rebuilt with a partial Delivery field")
            }
        }
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when called on a closed execution subject. Execution subjects
    /// must be constructed with a complete execution identity.
    pub fn with_product_session(self, product_session_id: ProductSessionId) -> Self {
        match self {
            Self::References {
                delivery,
                lease,
                publication,
                ..
            } => Self::References {
                delivery,
                product_session: Some(product_session_id),
                lease,
                publication,
            },
            Self::Publication {
                delivery,
                publication,
            } => Self::References {
                delivery: Some(delivery),
                product_session: Some(product_session_id),
                lease: None,
                publication: Some(publication),
            },
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => {
                panic!(
                    "execution audit subjects cannot be rebuilt with a partial ProductSession field"
                )
            }
        }
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when called on a closed execution subject. Execution subjects
    /// must be constructed with a complete execution identity.
    pub fn with_lease(self, lease_id: LeaseId) -> Self {
        match self {
            Self::References {
                delivery,
                product_session,
                publication,
                ..
            } => Self::References {
                delivery,
                product_session,
                lease: Some(lease_id),
                publication,
            },
            Self::Publication {
                delivery,
                publication,
            } => Self::References {
                delivery: Some(delivery),
                product_session: None,
                lease: Some(lease_id),
                publication: Some(publication),
            },
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => {
                panic!("execution audit subjects cannot be rebuilt with a partial Lease field")
            }
        }
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when called on a closed execution subject. Execution subjects
    /// must be constructed with a complete execution identity.
    pub fn with_publication(self, publication_id: PublicationId) -> Self {
        match self {
            Self::References {
                delivery: Some(delivery),
                product_session: None,
                lease: None,
                ..
            }
            | Self::Publication { delivery, .. } => Self::Publication {
                delivery,
                publication: publication_id,
            },
            Self::References {
                delivery,
                product_session,
                lease,
                ..
            } => Self::References {
                delivery,
                product_session,
                lease,
                publication: Some(publication_id),
            },
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => {
                panic!("execution audit subjects cannot be rebuilt with a Publication field")
            }
        }
    }

    #[must_use]
    pub const fn delivery_id(&self) -> Option<&DeliveryId> {
        match self {
            Self::References { delivery, .. } => delivery.as_ref(),
            Self::Publication { delivery, .. } => Some(delivery),
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => None,
        }
    }

    #[must_use]
    pub const fn product_session_id(&self) -> Option<&ProductSessionId> {
        match self {
            Self::References {
                product_session, ..
            } => product_session.as_ref(),
            Self::Publication { .. }
            | Self::AcceptedBinding(_)
            | Self::Runtime(_)
            | Self::Terminal(_) => None,
        }
    }

    #[must_use]
    pub const fn lease_id(&self) -> Option<&LeaseId> {
        match self {
            Self::References { lease, .. } => lease.as_ref(),
            Self::Publication { .. }
            | Self::AcceptedBinding(_)
            | Self::Runtime(_)
            | Self::Terminal(_) => None,
        }
    }

    #[must_use]
    pub const fn publication_id(&self) -> Option<&PublicationId> {
        match self {
            Self::References { publication, .. } => publication.as_ref(),
            Self::Publication { publication, .. } => Some(publication),
            Self::AcceptedBinding(_) | Self::Runtime(_) | Self::Terminal(_) => None,
        }
    }

    fn validate(&self) -> Result<(), AuditError> {
        match self {
            Self::References {
                delivery,
                product_session,
                lease,
                publication,
            } => {
                if delivery
                    .as_ref()
                    .is_some_and(|id| !canonical_id(&id.0, "dlv"))
                    || product_session
                        .as_ref()
                        .is_some_and(|id| !canonical_id(&id.0, "psn"))
                    || lease.as_ref().is_some_and(|id| !canonical_id(&id.0, "lse"))
                    || publication
                        .as_ref()
                        .is_some_and(|id| !canonical_id(&id.0, "pub"))
                {
                    return Err(AuditError::invalid(
                        "audit subject contains a non-canonical identity",
                    ));
                }
                Ok(())
            }
            Self::Publication {
                delivery,
                publication,
            } => {
                if !canonical_id(&delivery.0, "dlv") || !canonical_id(&publication.0, "pub") {
                    return Err(AuditError::invalid(
                        "publication audit subject contains a non-canonical identity",
                    ));
                }
                Ok(())
            }
            Self::AcceptedBinding(identity) => {
                identity.validate()?;
                if identity.source_sequence().is_some() || identity.binding_source().is_none() {
                    return Err(AuditError::invalid(
                        "accepted binding audit subject requires its typed binding source",
                    ));
                }
                Ok(())
            }
            Self::Runtime(identity) | Self::Terminal(identity) => {
                identity.validate()?;
                if identity.source_sequence().is_none() || identity.binding_source().is_some() {
                    return Err(AuditError::invalid(
                        "runtime and terminal audit subjects require a proven source sequence",
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Serialize)]
struct LegacyAuditSubjectWire<'subject> {
    delivery: Option<&'subject DeliveryId>,
    product_session: Option<&'subject ProductSessionId>,
    lease: Option<&'subject LeaseId>,
    publication: Option<&'subject PublicationId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAuditSubjectWireOwned {
    delivery: Option<DeliveryId>,
    product_session: Option<ProductSessionId>,
    lease: Option<LeaseId>,
    publication: Option<PublicationId>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecutionAuditSubjectWire<'subject> {
    AcceptedBinding {
        #[serde(flatten)]
        identity: &'subject AuditExecutionIdentity,
    },
    Runtime {
        #[serde(flatten)]
        identity: &'subject AuditExecutionIdentity,
    },
    Terminal {
        #[serde(flatten)]
        identity: &'subject AuditExecutionIdentity,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExecutionAuditSubjectWireOwned {
    AcceptedBinding {
        #[serde(flatten)]
        identity: AuditExecutionIdentity,
    },
    Runtime {
        #[serde(flatten)]
        identity: AuditExecutionIdentity,
    },
    Terminal {
        #[serde(flatten)]
        identity: AuditExecutionIdentity,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AuditSubjectWireOwned {
    Execution(Box<ExecutionAuditSubjectWireOwned>),
    Legacy(LegacyAuditSubjectWireOwned),
}

impl Serialize for AuditSubject {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        match self {
            Self::References {
                delivery,
                product_session,
                lease,
                publication,
            } => LegacyAuditSubjectWire {
                delivery: delivery.as_ref(),
                product_session: product_session.as_ref(),
                lease: lease.as_ref(),
                publication: publication.as_ref(),
            }
            .serialize(serializer),
            Self::Publication {
                delivery,
                publication,
            } => LegacyAuditSubjectWire {
                delivery: Some(delivery),
                product_session: None,
                lease: None,
                publication: Some(publication),
            }
            .serialize(serializer),
            Self::AcceptedBinding(identity) => {
                ExecutionAuditSubjectWire::AcceptedBinding { identity }.serialize(serializer)
            }
            Self::Runtime(identity) => {
                ExecutionAuditSubjectWire::Runtime { identity }.serialize(serializer)
            }
            Self::Terminal(identity) => {
                ExecutionAuditSubjectWire::Terminal { identity }.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for AuditSubject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let subject = match AuditSubjectWireOwned::deserialize(deserializer)? {
            AuditSubjectWireOwned::Execution(subject) => match *subject {
                ExecutionAuditSubjectWireOwned::AcceptedBinding { identity } => {
                    Self::AcceptedBinding(identity)
                }
                ExecutionAuditSubjectWireOwned::Runtime { identity } => Self::Runtime(identity),
                ExecutionAuditSubjectWireOwned::Terminal { identity } => Self::Terminal(identity),
            },
            AuditSubjectWireOwned::Legacy(subject) => {
                let LegacyAuditSubjectWireOwned {
                    delivery,
                    product_session,
                    lease,
                    publication,
                } = subject;
                match (delivery, publication, product_session, lease) {
                    (Some(delivery), Some(publication), None, None) => Self::Publication {
                        delivery,
                        publication,
                    },
                    (delivery, publication, product_session, lease) => Self::References {
                        delivery,
                        product_session,
                        lease,
                        publication,
                    },
                }
            }
        };
        subject.validate().map_err(serde::de::Error::custom)?;
        Ok(subject)
    }
}

/// Stable result category. Human-readable errors and remote diagnostics stay
/// outside the durable event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Succeeded,
    Rejected,
    Failed,
}

/// Minimum payload retention. The immutable header and its chain digest remain
/// after a finite payload expires.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "until_millis", rename_all = "snake_case")]
pub enum AuditRetention {
    UntilMillis(u64),
    Indefinite,
}

/// One validated immutable audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    event_id: AuditEventId,
    occurred_at_millis: u64,
    actor: AuditActor,
    scope: AuditScope,
    request_id: RequestId,
    action: AuditAction,
    state: AuditState,
    origin: AuditOrigin,
    subject: AuditSubject,
    outcome: AuditOutcome,
    result_code: String,
    retention: AuditRetention,
}

impl AuditEvent {
    /// Builds a successful business state change.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, unsafe action/result text, unchanged
    /// digests, or a retention deadline that precedes the event.
    #[allow(clippy::too_many_arguments)]
    pub fn state_change(
        event_id: AuditEventId,
        occurred_at_millis: u64,
        actor: AuditActor,
        scope: AuditScope,
        request_id: RequestId,
        action: AuditAction,
        state: AuditState,
        origin: AuditOrigin,
        subject: AuditSubject,
        result_code: &str,
        retention: AuditRetention,
    ) -> Result<Self, AuditError> {
        if !matches!(state, AuditState::Changed { .. }) {
            return Err(AuditError::invalid(
                "state-change audit event requires changed state digests",
            ));
        }
        let event = Self {
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            outcome: AuditOutcome::Succeeded,
            result_code: result_code.to_owned(),
            retention,
        };
        event.validate()?;
        Ok(event)
    }

    /// Builds one successful operation that did not change canonical state,
    /// such as an idempotent replay or a model call observation.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, unsafe action/result text, changed state,
    /// or an invalid retention deadline.
    #[allow(clippy::too_many_arguments)]
    pub fn succeeded(
        event_id: AuditEventId,
        occurred_at_millis: u64,
        actor: AuditActor,
        scope: AuditScope,
        request_id: RequestId,
        action: AuditAction,
        state: AuditState,
        origin: AuditOrigin,
        subject: AuditSubject,
        result_code: &str,
        retention: AuditRetention,
    ) -> Result<Self, AuditError> {
        Self::unchanged_result(
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            AuditOutcome::Succeeded,
            result_code,
            retention,
        )
    }

    /// Builds one rejected operation that left canonical state unchanged.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, unsafe action/result text, changed state,
    /// or an invalid retention deadline.
    #[allow(clippy::too_many_arguments)]
    pub fn rejected(
        event_id: AuditEventId,
        occurred_at_millis: u64,
        actor: AuditActor,
        scope: AuditScope,
        request_id: RequestId,
        action: AuditAction,
        state: AuditState,
        origin: AuditOrigin,
        subject: AuditSubject,
        result_code: &str,
        retention: AuditRetention,
    ) -> Result<Self, AuditError> {
        Self::unchanged_result(
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            AuditOutcome::Rejected,
            result_code,
            retention,
        )
    }

    /// Builds one failed operation that left canonical state unchanged.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, unsafe action/result text, changed state,
    /// or an invalid retention deadline.
    #[allow(clippy::too_many_arguments)]
    pub fn failed(
        event_id: AuditEventId,
        occurred_at_millis: u64,
        actor: AuditActor,
        scope: AuditScope,
        request_id: RequestId,
        action: AuditAction,
        state: AuditState,
        origin: AuditOrigin,
        subject: AuditSubject,
        result_code: &str,
        retention: AuditRetention,
    ) -> Result<Self, AuditError> {
        Self::unchanged_result(
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            AuditOutcome::Failed,
            result_code,
            retention,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn unchanged_result(
        event_id: AuditEventId,
        occurred_at_millis: u64,
        actor: AuditActor,
        scope: AuditScope,
        request_id: RequestId,
        action: AuditAction,
        state: AuditState,
        origin: AuditOrigin,
        subject: AuditSubject,
        outcome: AuditOutcome,
        result_code: &str,
        retention: AuditRetention,
    ) -> Result<Self, AuditError> {
        if !matches!(state, AuditState::Unchanged { .. }) {
            return Err(AuditError::invalid(
                "rejected or failed audit event requires unchanged state",
            ));
        }
        let event = Self {
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            outcome,
            result_code: result_code.to_owned(),
            retention,
        };
        event.validate()?;
        Ok(event)
    }

    #[must_use]
    pub const fn event_id(&self) -> &AuditEventId {
        &self.event_id
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn actor(&self) -> &AuditActor {
        &self.actor
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[must_use]
    pub const fn action(&self) -> &AuditAction {
        &self.action
    }

    #[must_use]
    pub const fn state(&self) -> &AuditState {
        &self.state
    }

    #[must_use]
    pub const fn origin(&self) -> &AuditOrigin {
        &self.origin
    }

    #[must_use]
    pub const fn subject(&self) -> &AuditSubject {
        &self.subject
    }

    #[must_use]
    pub const fn retention(&self) -> AuditRetention {
        self.retention
    }

    #[must_use]
    pub const fn outcome(&self) -> AuditOutcome {
        self.outcome
    }

    #[must_use]
    pub fn result_code(&self) -> &str {
        &self.result_code
    }

    pub(crate) fn validate(&self) -> Result<(), AuditError> {
        if !canonical_id(self.event_id.as_str(), "aud") {
            return Err(AuditError::invalid("stored audit event id is invalid"));
        }
        if self.occurred_at_millis == 0 || self.occurred_at_millis > i64::MAX as u64 {
            return Err(AuditError::invalid(
                "audit occurrence timestamp is outside the SQLite range",
            ));
        }
        validate_actor(&self.actor)?;
        self.scope.validate()?;
        if !canonical_id(&self.request_id.0, "req") {
            return Err(AuditError::invalid("audit request id is not canonical"));
        }
        self.action.validate()?;
        self.state.validate()?;
        self.origin.validate()?;
        self.subject.validate()?;
        validate_token(&self.result_code, "audit result code")?;
        if matches!(self.outcome, AuditOutcome::Rejected | AuditOutcome::Failed)
            && matches!(self.state, AuditState::Changed { .. })
        {
            return Err(AuditError::invalid(
                "rejected or failed audit result cannot change state",
            ));
        }
        if let AuditRetention::UntilMillis(until) = self.retention
            && until <= self.occurred_at_millis
        {
            return Err(AuditError::invalid(
                "audit retention deadline must follow the event timestamp",
            ));
        }
        Ok(())
    }
}

fn validate_actor(actor: &AuditActor) -> Result<(), AuditError> {
    let valid = match actor {
        AuditActor::User(id) => canonical_id(&id.0, "usr"),
        AuditActor::ServiceAccount(id) => canonical_id(&id.0, "svc"),
        AuditActor::System(id) => canonical_id(&id.0, "sys"),
    };
    if valid {
        Ok(())
    } else {
        Err(AuditError::invalid("audit actor identity is not canonical"))
    }
}

pub(crate) fn validate_digest(
    digest: &Sha256Digest,
    field: &'static str,
) -> Result<(), AuditError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(AuditError::invalid(format!(
            "{field} is not a SHA-256 digest"
        )));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(AuditError::invalid(format!(
            "{field} must contain 64 lowercase hexadecimal digits"
        )));
    }
    Ok(())
}

fn validate_token(value: &str, field: &'static str) -> Result<(), AuditError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
    {
        return Err(AuditError::invalid(format!(
            "{field} must be a bounded portable token"
        )));
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
        .is_some_and(|identifier| {
            identifier.len() == 26
                && identifier.bytes().all(|byte| {
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

fn canonical_fencing_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}
