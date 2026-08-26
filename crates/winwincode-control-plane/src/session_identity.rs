// SPDX-License-Identifier: Apache-2.0

//! Control Plane seam for accepting the generated `session.binding` message.

use winwincode_delivery::application::{
    session_binding::SessionBindingIdentity as DeliverySessionBindingIdentity,
    stage::SessionBindingAuthority,
};
use winwincode_domain::Instant;
use winwincode_execution_port::generated::{DeliveryStageExecutionScope, SessionBindingMessage};
use winwincode_session::{
    BindingScope, RuntimeSourceIdentity, SessionBinding, SessionBindingError,
    SessionBindingIdentity,
};

/// A validated message/authority pair ready for canonical session and Delivery writes.
#[derive(Debug)]
pub struct SessionBindingAcceptance<'a> {
    binding: SessionBinding,
    delivery_identity: DeliverySessionBindingIdentity,
    message: &'a SessionBindingMessage,
    authority: &'a SessionBindingAuthority,
}

/// Failure at the generated-message/session identity seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIdentityAdapterError {
    InvalidMessage(&'static str),
    ForeignAuthority(&'static str),
    InvalidLeaseWindow(&'static str),
    InvalidSessionBinding(String),
}

impl std::fmt::Display for SessionIdentityAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMessage(field) => {
                write!(formatter, "invalid SessionBinding message: {field}")
            }
            Self::ForeignAuthority(field) => {
                write!(
                    formatter,
                    "SessionBinding does not match scheduler authority: {field}"
                )
            }
            Self::InvalidLeaseWindow(field) => {
                write!(formatter, "invalid SessionBinding lease window: {field}")
            }
            Self::InvalidSessionBinding(error) => {
                write!(formatter, "invalid canonical SessionBinding: {error}")
            }
        }
    }
}

impl std::error::Error for SessionIdentityAdapterError {}

impl From<SessionBindingError> for SessionIdentityAdapterError {
    fn from(error: SessionBindingError) -> Self {
        Self::InvalidSessionBinding(error.to_string())
    }
}

/// Accept one generated `session.binding` only when it matches the scheduler-owned authority and
/// the durable Delivery execution scope. The scope is supplied separately because the
/// `session.binding` wire message carries `stageRunId` and `ProductSessionId`, but deliberately
/// does not repeat the Delivery and optional task identifiers.
///
/// # Errors
///
/// Returns a typed error when the message discriminator, identity joins, lease window, scheduler
/// authority, or durable Delivery scope is not exact.
pub fn validate_session_binding<'a>(
    message: &'a SessionBindingMessage,
    authority: &'a SessionBindingAuthority,
    scope: &DeliveryStageExecutionScope,
) -> Result<SessionBindingAcceptance<'a>, SessionIdentityAdapterError> {
    validate_message_discriminator(message)?;
    validate_message_identifiers(message)?;
    validate_message_authority_fields(message)?;
    validate_message_lease_window(message, authority)?;
    validate_scheduler_authority(message, authority)?;
    validate_delivery_scope(message, authority, scope)?;

    let identity = SessionBindingIdentity::try_new(
        BindingScope::DeliveryStage {
            delivery_id: scope.delivery_id.clone(),
            delivery_task_id: scope.delivery_task_id.clone(),
            stage_run_id: scope.stage_run_id.clone(),
        },
        message.product_session_id.clone(),
        message.lease.job_id.clone(),
    )?;
    let source = RuntimeSourceIdentity::execution_worker(
        message.source_identity.lease_id.clone(),
        message.source_identity.worker_id.clone(),
        message.source_identity.worker_instance_id.clone(),
        message.source_identity.worker_session_id.clone(),
    )?;
    let binding = SessionBinding::try_new(
        identity,
        Some(message.worker_session_id.clone()),
        Some(message.codex_thread_id.clone()),
        Some(source),
    )?;
    let delivery_identity = DeliverySessionBindingIdentity {
        delivery_id: scope.delivery_id.clone(),
        delivery_task_id: scope.delivery_task_id.clone(),
        stage_run_id: scope.stage_run_id.clone(),
        product_session_id: message.product_session_id.clone(),
        execution_job_id: message.lease.job_id.clone(),
    };
    Ok(SessionBindingAcceptance {
        binding,
        delivery_identity,
        message,
        authority,
    })
}

fn validate_message_discriminator(
    message: &SessionBindingMessage,
) -> Result<(), SessionIdentityAdapterError> {
    if message.kind
        != winwincode_execution_port::generated::SessionBindingMessageKind::SessionBinding
    {
        return Err(SessionIdentityAdapterError::InvalidMessage("kind"));
    }
    if message.schema_version != winwincode_domain::SchemaVersion::WinwincodeV1 {
        return Err(SessionIdentityAdapterError::InvalidMessage("schemaVersion"));
    }
    Ok(())
}

fn validate_message_identifiers(
    message: &SessionBindingMessage,
) -> Result<(), SessionIdentityAdapterError> {
    require_id(&message.message_id.0, "messageId", "xmsg_")?;
    require_id(&message.product_session_id.0, "productSessionId", "psn_")?;
    require_id(&message.stage_run_id.0, "stageRunId", "run_")?;
    require_id(&message.worker_session_id.0, "workerSessionId", "wsn_")?;
    require_id(&message.codex_thread_id.0, "codexThreadId", "cdx_")?;
    require_id(&message.worker_id.0, "workerId", "wrk_")?;
    require_id(&message.lease.job_id.0, "lease.jobId", "job_")?;
    require_id(&message.lease.lease_id.0, "lease.leaseId", "lse_")?;
    require_id(&message.lease.worker_id.0, "lease.workerId", "wrk_")?;
    require_id(
        &message.lease.worker_instance_id.0,
        "lease.workerInstanceId",
        "wki_",
    )?;
    require_id(&message.lease_id.0, "leaseId", "lse_")?;
    require_id(
        &message.source_identity.lease_id.0,
        "sourceIdentity.leaseId",
        "lse_",
    )?;
    require_id(
        &message.source_identity.worker_id.0,
        "sourceIdentity.workerId",
        "wrk_",
    )?;
    require_id(
        &message.source_identity.worker_instance_id.0,
        "sourceIdentity.workerInstanceId",
        "wki_",
    )?;
    require_id(
        &message.source_identity.worker_session_id.0,
        "sourceIdentity.workerSessionId",
        "wsn_",
    )?;
    Ok(())
}

fn validate_message_authority_fields(
    message: &SessionBindingMessage,
) -> Result<(), SessionIdentityAdapterError> {
    if message.lease.attempt <= 0 || message.lease.attempt > 1_000 {
        return Err(SessionIdentityAdapterError::InvalidMessage("lease.attempt"));
    }
    if message.attempt != message.lease.attempt {
        return Err(SessionIdentityAdapterError::InvalidMessage("attempt"));
    }
    if message.fencing_token != message.lease.fencing_token {
        return Err(SessionIdentityAdapterError::InvalidMessage("fencingToken"));
    }
    if message.lease_id != message.lease.lease_id {
        return Err(SessionIdentityAdapterError::InvalidMessage("leaseId"));
    }
    if message.worker_id != message.lease.worker_id {
        return Err(SessionIdentityAdapterError::InvalidMessage("workerId"));
    }
    if message.source_identity.kind
        != winwincode_domain::SessionBindingSourceIdentityKind::ExecutionWorker
    {
        return Err(SessionIdentityAdapterError::InvalidMessage(
            "sourceIdentity.kind",
        ));
    }
    if message.source_identity.lease_id != message.lease.lease_id
        || message.source_identity.worker_id != message.lease.worker_id
        || message.source_identity.worker_instance_id != message.lease.worker_instance_id
        || message.source_identity.worker_session_id != message.worker_session_id
    {
        return Err(SessionIdentityAdapterError::InvalidMessage(
            "sourceIdentity",
        ));
    }
    if message.session_identity.product_session_id != message.product_session_id
        || message.session_identity.worker_session_id != message.worker_session_id
        || message.session_identity.codex_thread_id != message.codex_thread_id
        || message.session_identity.stage_run_id.as_ref() != Some(&message.stage_run_id)
    {
        return Err(SessionIdentityAdapterError::InvalidMessage(
            "sessionIdentity",
        ));
    }
    validate_fencing_token(&message.fencing_token)?;
    Ok(())
}

fn validate_message_lease_window(
    message: &SessionBindingMessage,
    authority: &SessionBindingAuthority,
) -> Result<(), SessionIdentityAdapterError> {
    validate_instant(&message.lease.issued_at, "issuedAt")?;
    validate_instant(&message.bound_at, "boundAt")?;
    validate_instant(&message.sent_at, "sentAt")?;
    validate_instant(&message.lease.expires_at, "expiresAt")?;
    if message.lease.issued_at.0 > message.bound_at.0
        || message.bound_at.0 >= message.lease.expires_at.0
        || message.sent_at.0 < message.bound_at.0
        || message.sent_at.0 > message.lease.expires_at.0
    {
        return Err(SessionIdentityAdapterError::InvalidLeaseWindow(
            "message timestamp is outside lease",
        ));
    }
    if authority.issued_at() != &message.lease.issued_at
        || authority.expires_at() != &message.lease.expires_at
    {
        return Err(SessionIdentityAdapterError::InvalidLeaseWindow(
            "message changed scheduler-owned lease window",
        ));
    }
    Ok(())
}

fn validate_scheduler_authority(
    message: &SessionBindingMessage,
    authority: &SessionBindingAuthority,
) -> Result<(), SessionIdentityAdapterError> {
    let active = authority.active_lease();
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| SessionIdentityAdapterError::ForeignAuthority("attempt"))?;
    if active.execution_job_id() != &message.lease.job_id
        || active.attempt() != attempt
        || active.lease_id() != &message.lease.lease_id
        || active.fencing_token() != &message.lease.fencing_token
        || active.worker_id() != &message.lease.worker_id
        || active.worker_instance_id() != &message.lease.worker_instance_id
        || active.worker_session_id() != &message.worker_session_id
    {
        return Err(SessionIdentityAdapterError::ForeignAuthority("lease"));
    }
    Ok(())
}

fn validate_delivery_scope(
    message: &SessionBindingMessage,
    authority: &SessionBindingAuthority,
    scope: &DeliveryStageExecutionScope,
) -> Result<(), SessionIdentityAdapterError> {
    if scope.kind
        != winwincode_execution_port::generated::DeliveryStageExecutionScopeKind::DeliveryStage
    {
        return Err(SessionIdentityAdapterError::InvalidMessage("scope.kind"));
    }
    require_id(&scope.delivery_id.0, "scope.deliveryId", "dlv_")?;
    if let Some(task_id) = &scope.delivery_task_id {
        require_id(&task_id.0, "scope.deliveryTaskId", "dtk_")?;
    }
    require_id(&scope.stage_run_id.0, "scope.stageRunId", "run_")?;
    require_id(
        &scope.product_session_id.0,
        "scope.productSessionId",
        "psn_",
    )?;
    if scope.product_session_id != message.product_session_id
        || scope.stage_run_id != message.stage_run_id
        || authority.active_lease().execution_job_id() != &message.lease.job_id
    {
        return Err(SessionIdentityAdapterError::ForeignAuthority(
            "execution scope",
        ));
    }
    Ok(())
}

fn validate_fencing_token(
    token: &winwincode_domain::FencingToken,
) -> Result<(), SessionIdentityAdapterError> {
    if token.0.is_empty()
        || token.0.len() > 20
        || token.0.starts_with('0')
        || !token.0.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(SessionIdentityAdapterError::InvalidMessage("fencingToken"));
    }
    Ok(())
}

fn validate_instant(
    instant: &Instant,
    field: &'static str,
) -> Result<(), SessionIdentityAdapterError> {
    let value = instant.0.as_bytes();
    let valid_shape = value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !valid_shape {
        return Err(SessionIdentityAdapterError::InvalidLeaseWindow(field));
    }
    Ok(())
}

fn require_id(
    value: &str,
    field: &'static str,
    prefix: &str,
) -> Result<(), SessionIdentityAdapterError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
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
    });
    if !valid {
        return Err(SessionIdentityAdapterError::InvalidMessage(field));
    }
    Ok(())
}

impl SessionBindingAcceptance<'_> {
    /// Returns the canonical session binding value.
    #[must_use]
    pub const fn binding(&self) -> &SessionBinding {
        &self.binding
    }

    /// Returns the Delivery identity required by its canonical write methods.
    #[must_use]
    pub const fn delivery_identity(&self) -> &DeliverySessionBindingIdentity {
        &self.delivery_identity
    }

    /// Returns the generated message retained for Delivery provenance.
    #[must_use]
    pub const fn message(&self) -> &SessionBindingMessage {
        self.message
    }

    /// Returns the one scheduler-owned authority used for acceptance.
    #[must_use]
    pub const fn authority(&self) -> &SessionBindingAuthority {
        self.authority
    }
}

#[cfg(test)]
mod tests {
    use winwincode_delivery::application::stage::test_support::{
        active_lease_identity, session_binding_authority,
    };
    use winwincode_domain::{
        CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId,
        FencingToken, Instant, LeaseId, ProductSessionId, StageRunId, WorkerId, WorkerInstanceId,
        WorkerSessionId,
    };
    use winwincode_domain::{
        SchemaVersion, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind,
        SessionIdentity,
    };
    use winwincode_execution_port::generated::{
        DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, ExecutionLeaseStamp,
        SessionBindingMessage, SessionBindingMessageKind,
    };

    use super::{SessionIdentityAdapterError, validate_session_binding};

    fn id(prefix: &str, value: u64) -> String {
        format!("{prefix}_{value:026}")
    }

    fn fixture(
        seed: u64,
    ) -> (
        winwincode_delivery::application::stage::SessionBindingAuthority,
        SessionBindingMessage,
        DeliveryStageExecutionScope,
    ) {
        let job_id = ExecutionJobId(id("job", seed));
        let lease_id = LeaseId(id("lse", seed));
        let worker_id = WorkerId(id("wrk", seed));
        let worker_instance_id = WorkerInstanceId(id("wki", seed));
        let worker_session_id = WorkerSessionId(id("wsn", seed));
        let product_session_id = ProductSessionId(id("psn", seed));
        let stage_run_id = StageRunId(id("run", seed));
        let fencing_token = FencingToken(seed.to_string());
        let issued_at = Instant("2027-01-15T08:00:00.200Z".into());
        let bound_at = Instant("2027-01-15T08:00:01.000Z".into());
        let sent_at = Instant("2027-01-15T08:00:01.100Z".into());
        let expires_at = Instant("2027-01-15T08:05:00.000Z".into());
        let authority = session_binding_authority(
            active_lease_identity(
                job_id.clone(),
                1,
                lease_id.clone(),
                fencing_token.clone(),
                worker_id.clone(),
                worker_instance_id.clone(),
                worker_session_id.clone(),
            ),
            issued_at.clone(),
            expires_at.clone(),
        );
        let message = SessionBindingMessage {
            attempt: 1,
            bound_at,
            codex_thread_id: CodexThreadId(id("cdx", seed)),
            fencing_token: fencing_token.clone(),
            kind: SessionBindingMessageKind::SessionBinding,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at,
                fencing_token,
                issued_at,
                job_id,
                lease_id: lease_id.clone(),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            },
            lease_id: lease_id.clone(),
            message_id: ExecutionMessageId(id("xmsg", seed)),
            product_session_id: product_session_id.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at,
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId(id("cdx", seed)),
                product_session_id: product_session_id.clone(),
                stage_run_id: Some(stage_run_id.clone()),
                worker_session_id: worker_session_id.clone(),
            },
            source_identity: SessionBindingSourceIdentity {
                kind: SessionBindingSourceIdentityKind::ExecutionWorker,
                lease_id,
                worker_id: worker_id.clone(),
                worker_instance_id,
                worker_session_id: worker_session_id.clone(),
            },
            stage_run_id: stage_run_id.clone(),
            worker_id,
            worker_session_id,
        };
        let scope = DeliveryStageExecutionScope {
            delivery_id: DeliveryId(id("dlv", seed)),
            delivery_task_id: Some(DeliveryTaskId(id("dtk", seed))),
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id,
            rework_authorization: None,
            stage_run_id,
        };
        (authority, message, scope)
    }

    #[test]
    fn generated_message_and_sealed_authority_produce_one_canonical_binding() {
        let (authority, message, scope) = fixture(1);
        let accepted = validate_session_binding(&message, &authority, &scope)
            .expect("matching generated message is accepted");

        assert_eq!(
            accepted.binding().product_session_id(),
            &scope.product_session_id
        );
        assert_eq!(
            accepted.binding().execution_job_id(),
            authority.active_lease().execution_job_id()
        );
        assert_eq!(accepted.binding().stage_run_id(), Some(&scope.stage_run_id));
        assert_eq!(
            accepted.binding().worker_session_id(),
            Some(&message.worker_session_id)
        );
        assert_eq!(
            accepted.binding().codex_thread_id(),
            Some(&message.codex_thread_id)
        );
        assert!(accepted.binding().is_complete());
        assert_eq!(accepted.delivery_identity().delivery_id, scope.delivery_id);
        assert_eq!(
            accepted.delivery_identity().delivery_task_id,
            scope.delivery_task_id
        );
        assert_eq!(
            accepted.delivery_identity().execution_job_id,
            authority.active_lease().execution_job_id().clone()
        );
        assert_eq!(accepted.message(), &message);
        assert_eq!(accepted.authority(), &authority);
    }

    #[test]
    fn foreign_job_or_stage_identity_is_rejected() {
        let (authority, mut message, scope) = fixture(2);
        message.lease.job_id = ExecutionJobId(id("job", 99));
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::ForeignAuthority("lease"))
        ));

        let (authority, mut message, scope) = fixture(3);
        message.stage_run_id = StageRunId(id("run", 99));
        message.session_identity.stage_run_id = Some(message.stage_run_id.clone());
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::ForeignAuthority(
                "execution scope"
            ))
        ));

        let (authority, mut message, scope) = fixture(7);
        message.product_session_id = ProductSessionId(id("psn", 99));
        message.session_identity.product_session_id = message.product_session_id.clone();
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::ForeignAuthority(
                "execution scope"
            ))
        ));
    }

    #[test]
    fn stale_authority_and_message_are_rejected() {
        let (authority, mut message, scope) = fixture(4);
        message.fencing_token = FencingToken("3".into());
        message.lease.fencing_token = message.fencing_token.clone();
        message.source_identity.lease_id = message.lease.lease_id.clone();
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::ForeignAuthority("lease"))
        ));
    }

    #[test]
    fn message_outside_scheduler_lease_window_is_rejected() {
        let (authority, mut message, scope) = fixture(5);
        message.bound_at = Instant("2027-01-15T08:05:00.001Z".into());
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::InvalidLeaseWindow(_))
        ));

        let (authority, mut message, scope) = fixture(6);
        message.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());
        assert!(matches!(
            validate_session_binding(&message, &authority, &scope),
            Err(SessionIdentityAdapterError::InvalidLeaseWindow(_))
        ));
    }
}
