// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId, FencingToken,
    LeaseId, ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use super::{
    DeliveryValidationError, DeliveryValidationErrorCode, SessionBindingId, portable_identifier,
    positive, safe_non_negative, schema_version, validation_error,
};

/// Exact link between a Codex-backed Delivery stage and separately owned sessions.
///
/// Product, Delivery, task, `StageRun`, and `ExecutionJob` identities are immutable.
/// `WorkerSession` and `CodexThread` are filled only when their respective owners
/// report them. There is deliberately no generic `sessionId` or legacy DSH
/// session field in the canonical Rust model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionBindingSourceKind {
    /// The Control Plane created the pending binding with delivery.advance.
    DeliveryAdvance,
    /// A Worker reported the binding through the typed `ExecutionPort`.
    ExecutionPort,
    /// A one-time migration converted a legacy Delivery snapshot.
    LegacyMigration,
}

/// Immutable provenance for the source that established the binding facts.
///
/// The source is deliberately separate from the four session identities. A
/// binding can be created by a Delivery dispatch and later completed by a
/// Worker message, but it must retain which typed seam supplied its current
/// authority instead of accepting an unqualified string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBindingSourceProvenance {
    kind: SessionBindingSourceKind,
    reference: String,
}

impl SessionBindingSourceProvenance {
    #[must_use]
    pub const fn kind(&self) -> SessionBindingSourceKind {
        self.kind
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub(crate) fn pending_delivery_advance() -> Self {
        Self {
            kind: SessionBindingSourceKind::DeliveryAdvance,
            reference: String::from("delivery.advance"),
        }
    }

    pub(crate) fn from_execution_port(message_id: ExecutionMessageId) -> Self {
        Self {
            kind: SessionBindingSourceKind::ExecutionPort,
            reference: message_id.0,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn delivery_advance(reference: impl Into<String>) -> Self {
        Self {
            kind: SessionBindingSourceKind::DeliveryAdvance,
            reference: reference.into(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn execution_port(message_id: ExecutionMessageId) -> Self {
        Self::from_execution_port(message_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn legacy_migration(reference: impl Into<String>) -> Self {
        Self {
            kind: SessionBindingSourceKind::LegacyMigration,
            reference: reference.into(),
        }
    }

    fn validate(&self, path: &str) -> Result<(), DeliveryValidationError> {
        portable_identifier(&self.reference, &format!("{path}.reference"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBinding {
    pub schema_version: u8,
    pub id: SessionBindingId,
    pub delivery_id: DeliveryId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage_run_id: StageRunId,
    pub product_session_id: ProductSessionId,
    pub execution_job_id: ExecutionJobId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub worker_session_id: Option<WorkerSessionId>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub codex_thread_id: Option<CodexThreadId>,
    /// Scheduler Worker identity associated with the current lease. It is
    /// pending until a Worker binding is accepted.
    pub worker_id: Option<WorkerId>,
    /// Worker process boot identity associated with the current lease.
    pub worker_instance_id: Option<WorkerInstanceId>,
    /// Lease that fenced the Worker session and Codex thread.
    pub lease_id: Option<LeaseId>,
    /// Stage attempt that owns this binding. Every persisted snapshot carries
    /// the explicit `StageRun` attempt.
    pub attempt: u64,
    /// Monotonic scheduler fencing token for the lease.
    pub fencing_token: Option<FencingToken>,
    /// Typed source and reference for the binding facts.
    pub source_provenance: SessionBindingSourceProvenance,
    pub bound_at_millis: u64,
}

fn default_attempt() -> u64 {
    1
}

impl Default for SessionBinding {
    fn default() -> Self {
        Self {
            schema_version: super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId(String::new()),
            delivery_id: DeliveryId(String::new()),
            delivery_task_id: None,
            stage_run_id: StageRunId(String::new()),
            product_session_id: ProductSessionId(String::new()),
            execution_job_id: ExecutionJobId(String::new()),
            worker_session_id: None,
            codex_thread_id: None,
            worker_id: None,
            worker_instance_id: None,
            lease_id: None,
            attempt: default_attempt(),
            fencing_token: None,
            source_provenance: SessionBindingSourceProvenance::pending_delivery_advance(),
            bound_at_millis: 0,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SessionBinding {
    /// Builds a deterministic complete authority for a test-only fixture.
    /// Production callers receive authority from the typed Control Plane
    /// message path instead of constructing persisted binding facts.
    #[must_use]
    pub fn with_test_authority(mut self, seed: &str, attempt: u64) -> Self {
        let mut fence = 0_u64;
        for byte in seed.bytes() {
            fence = fence.wrapping_mul(31).wrapping_add(u64::from(byte));
        }
        let fence = (fence % 9_999_999).saturating_add(1);
        self.worker_id = Some(WorkerId(format!("wrk-test-{seed}")));
        self.worker_instance_id = Some(WorkerInstanceId(format!("wki-test-{seed}")));
        self.lease_id = Some(LeaseId(format!("lse-test-{seed}")));
        self.attempt = attempt;
        self.fencing_token = Some(FencingToken(fence.to_string()));
        self.source_provenance = SessionBindingSourceProvenance::from_execution_port(
            ExecutionMessageId(format!("msg-test-{seed}")),
        );
        self
    }
}

pub(crate) fn validate(
    binding: &SessionBinding,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(binding.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&binding.id.0, &format!("{path}.id"))?;
    portable_identifier(&binding.delivery_id.0, &format!("{path}.deliveryId"))?;
    if let Some(task_id) = &binding.delivery_task_id {
        portable_identifier(&task_id.0, &format!("{path}.deliveryTaskId"))?;
    }
    portable_identifier(&binding.stage_run_id.0, &format!("{path}.stageRunId"))?;
    portable_identifier(
        &binding.product_session_id.0,
        &format!("{path}.productSessionId"),
    )?;
    portable_identifier(
        &binding.execution_job_id.0,
        &format!("{path}.executionJobId"),
    )?;
    if let Some(session_id) = &binding.worker_session_id {
        portable_identifier(&session_id.0, &format!("{path}.workerSessionId"))?;
    }
    if let Some(thread_id) = &binding.codex_thread_id {
        portable_identifier(&thread_id.0, &format!("{path}.codexThreadId"))?;
        if binding.worker_session_id.is_none() {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("{path}.codexThreadId"),
                "CodexThread requires an accepted WorkerSession",
            ));
        }
    }
    validate_execution_authority(binding, path)?;
    safe_non_negative(binding.bound_at_millis, &format!("{path}.boundAtMillis"))
}

fn validate_execution_authority(
    binding: &SessionBinding,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let authority_fields = [
        binding.worker_id.is_some(),
        binding.worker_instance_id.is_some(),
        binding.lease_id.is_some(),
        binding.fencing_token.is_some(),
    ];
    if authority_fields.iter().any(|present| *present)
        && !authority_fields.iter().all(|present| *present)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            format!("{path}.workerId"),
            "Worker, instance, lease, and fencing identities must be persisted together",
        ));
    }
    if authority_fields.iter().all(|present| *present) && binding.worker_session_id.is_none() {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            format!("{path}.workerSessionId"),
            "fenced execution authority requires an accepted WorkerSession",
        ));
    }
    if binding.worker_session_id.is_some() && !authority_fields.iter().all(|present| *present) {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            format!("{path}.workerId"),
            "an accepted WorkerSession requires complete persisted lease authority",
        ));
    }
    if let Some(worker_id) = &binding.worker_id {
        portable_identifier(&worker_id.0, &format!("{path}.workerId"))?;
    }
    if let Some(worker_instance_id) = &binding.worker_instance_id {
        portable_identifier(&worker_instance_id.0, &format!("{path}.workerInstanceId"))?;
    }
    if let Some(lease_id) = &binding.lease_id {
        portable_identifier(&lease_id.0, &format!("{path}.leaseId"))?;
    }
    positive(binding.attempt, &format!("{path}.attempt"))?;
    if let Some(fencing_token) = &binding.fencing_token
        && (fencing_token.0.is_empty()
            || fencing_token.0.len() > 20
            || fencing_token.0.starts_with('0')
            || !fencing_token.0.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.fencingToken"),
            "must be a decimal fencing token without leading zeroes",
        ));
    }
    binding
        .source_provenance
        .validate(&format!("{path}.sourceProvenance"))?;
    match (
        binding.worker_session_id.is_some(),
        binding.source_provenance.kind(),
    ) {
        (
            false,
            SessionBindingSourceKind::DeliveryAdvance | SessionBindingSourceKind::LegacyMigration,
        )
        | (true, SessionBindingSourceKind::ExecutionPort) => {}
        (false, SessionBindingSourceKind::ExecutionPort) => {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("{path}.sourceProvenance.kind"),
                "pending bindings cannot claim an ExecutionPort source",
            ));
        }
        (
            true,
            SessionBindingSourceKind::DeliveryAdvance | SessionBindingSourceKind::LegacyMigration,
        ) => {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("{path}.sourceProvenance.kind"),
                "a complete binding requires a typed ExecutionPort source",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId,
        FencingToken, LeaseId, ProductSessionId, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

    use crate::domain::{
        Delivery, DeliveryValidationErrorCode, SessionBindingSourceKind,
        SessionBindingSourceProvenance, test_fixture,
    };

    #[test]
    fn session_binding_round_trips_the_complete_execution_authority() {
        let mut fixture = test_fixture();
        let binding = &mut fixture.session_bindings[0];
        binding.worker_id = Some(WorkerId("wrk_01J00000000000000000000000".into()));
        binding.worker_instance_id =
            Some(WorkerInstanceId("wki_01J00000000000000000000000".into()));
        binding.lease_id = Some(LeaseId("lse_01J00000000000000000000000".into()));
        binding.attempt = 1;
        binding.fencing_token = Some(FencingToken("7".into()));
        binding.source_provenance = SessionBindingSourceProvenance::execution_port(
            ExecutionMessageId("msg_01J00000000000000000000000".into()),
        );

        let delivery = Delivery::try_from_snapshot(fixture).expect("complete binding");
        let encoded: serde_json::Value =
            serde_json::from_slice(&delivery.encode_json().expect("encoded Delivery"))
                .expect("Delivery JSON");
        let persisted = &encoded["sessionBindings"][0];
        assert_eq!(
            persisted["workerId"],
            serde_json::json!("wrk_01J00000000000000000000000")
        );
        assert_eq!(
            persisted["workerInstanceId"],
            serde_json::json!("wki_01J00000000000000000000000")
        );
        assert_eq!(
            persisted["leaseId"],
            serde_json::json!("lse_01J00000000000000000000000")
        );
        assert_eq!(persisted["attempt"], serde_json::json!(1));
        assert_eq!(persisted["fencingToken"], serde_json::json!("7"));
        assert_eq!(
            persisted["sourceProvenance"]["kind"],
            serde_json::json!("execution-port")
        );
        assert_eq!(
            persisted["sourceProvenance"]["reference"],
            serde_json::json!("msg_01J00000000000000000000000")
        );

        let restored = Delivery::decode_json(&delivery.encode_json().expect("encoded Delivery"))
            .expect("round-trip Delivery");
        assert_eq!(restored.snapshot(), delivery.snapshot());
        assert_eq!(
            restored.snapshot().session_bindings[0]
                .source_provenance
                .kind,
            SessionBindingSourceKind::ExecutionPort
        );
    }

    #[test]
    fn session_binding_rejects_partial_fenced_authority() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].worker_instance_id = None;

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("partial lease authority must fail before persistence");
        assert_eq!(
            error.code(),
            DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn session_binding_requires_product_and_execution_job_identities() {
        for field in ["productSessionId", "executionJobId"] {
            let mut fixture = serde_json::to_value(test_fixture()).expect("fixture json");
            fixture["sessionBindings"][0]
                .as_object_mut()
                .expect("SessionBinding object")
                .remove(field);
            let bytes = serde_json::to_vec(&fixture).expect("fixture bytes");
            let error = Delivery::decode_json(&bytes)
                .expect_err("a canonical SessionBinding requires both owner identities");
            assert_eq!(
                error.code(),
                DeliveryValidationErrorCode::InvalidShape,
                "{field}"
            );
        }
    }

    #[test]
    fn codex_thread_requires_an_accepted_worker_session() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].worker_session_id = None;
        assert!(fixture.session_bindings[0].codex_thread_id.is_some());

        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn session_binding_matches_delivery_stage_run_and_task() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].delivery_task_id = Some(DeliveryTaskId("foreign-task".into()));
        assert!(Delivery::try_from_snapshot(fixture).is_err());

        let mut fixture = test_fixture();
        fixture.session_bindings[0].delivery_id =
            DeliveryId("dlv_01J00000000000000000000009".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn complete_binding_requires_persisted_authority() {
        let mut fixture = test_fixture();
        let binding = &mut fixture.session_bindings[0];
        binding.worker_id = None;
        binding.worker_instance_id = None;
        binding.lease_id = None;
        binding.fencing_token = None;

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("a complete binding without lease authority must be rejected");

        assert_eq!(
            error.code(),
            DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn pending_binding_cannot_claim_execution_port_provenance() {
        let mut fixture = test_fixture();
        let binding = &mut fixture.session_bindings[0];
        binding.worker_session_id = None;
        binding.codex_thread_id = None;
        binding.worker_id = None;
        binding.worker_instance_id = None;
        binding.lease_id = None;
        binding.fencing_token = None;
        binding.source_provenance = SessionBindingSourceProvenance::execution_port(
            ExecutionMessageId("msg_01J00000000000000000000001".into()),
        );

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("pending binding must use pending provenance");
        assert_eq!(
            error.code(),
            DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn complete_binding_cannot_claim_delivery_advance_provenance() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].source_provenance =
            SessionBindingSourceProvenance::delivery_advance("delivery.advance");

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("complete binding must use typed execution provenance");
        assert_eq!(
            error.code(),
            DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn binding_attempt_must_match_its_stage_run() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].attempt = 2;

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("a binding from another StageRun attempt must be rejected");

        assert_eq!(
            error.code(),
            DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn fenced_authority_must_be_unique_across_bindings() {
        let mut fixture = test_fixture();
        let mut run = fixture.stage_runs[0].clone();
        run.id = crate::domain::StageRunId("stage-verification-2".into());
        fixture.stage_runs.push(run.clone());
        let mut binding = fixture.session_bindings[0].clone();
        binding.id = crate::domain::SessionBindingId("binding-verifier-2".into());
        binding.stage_run_id = run.id;
        binding.product_session_id = ProductSessionId("product-session-verifier-2".into());
        binding.execution_job_id = ExecutionJobId("execution-job-verifier-2".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-session-verifier-2".into()));
        binding.codex_thread_id = Some(CodexThreadId("codex-thread-verifier-2".into()));
        fixture.session_bindings.push(binding);

        let error = Delivery::try_from_snapshot(fixture)
            .expect_err("the same lease authority cannot own two bindings");

        assert_eq!(error.code(), DeliveryValidationErrorCode::DuplicateId);
    }
}
