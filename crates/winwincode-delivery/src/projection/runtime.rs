// SPDX-License-Identifier: Apache-2.0

//! Exact-bound runtime projection over accepted, sealed Worker/Codex facts.

use std::{error::Error, fmt};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionEventId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::domain::{Delivery, SessionBindingId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProjectionErrorCode {
    InvalidBinding,
    AmbiguousBinding,
    UnboundEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProjectionError {
    code: RuntimeProjectionErrorCode,
    message: String,
}

impl RuntimeProjectionError {
    pub const fn code(&self) -> RuntimeProjectionErrorCode {
        self.code
    }
}

impl fmt::Display for RuntimeProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeIdentity {
    delivery_id: DeliveryId,
    stage_run_id: StageRunId,
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    execution_job_id: ExecutionJobId,
    lease_id: LeaseId,
    attempt: u64,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRuntimeBinding {
    session_binding_id: SessionBindingId,
    identity: RuntimeIdentity,
    settled_last_sequence: Option<u64>,
    seal: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRuntimeEvent {
    identity: RuntimeIdentity,
    sequence: u64,
    event_id: ExecutionEventId,
    seal: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSessionProjection {
    pub session_binding_id: SessionBindingId,
    pub as_of_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProjectionSnapshot {
    pub delivery_id: DeliveryId,
    pub sessions: Vec<RuntimeSessionProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeApplyOutcome {
    Applied {
        session_binding_id: SessionBindingId,
        sequence: u64,
    },
}

#[derive(Debug, Clone)]
pub struct RuntimeProjection {
    snapshot: RuntimeProjectionSnapshot,
    bindings: Vec<AcceptedRuntimeBinding>,
}

impl RuntimeProjection {
    /// Opens a read-only projection over exact accepted runtime bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when a sealed binding is not current and exact for the
    /// supplied canonical Delivery.
    pub fn new(
        delivery: &Delivery,
        bindings: Vec<AcceptedRuntimeBinding>,
    ) -> Result<Self, RuntimeProjectionError> {
        for (index, binding) in bindings.iter().enumerate() {
            validate_binding(delivery, binding)?;
            if bindings[index + 1..].iter().any(|other| {
                other.session_binding_id == binding.session_binding_id
                    || other.identity == binding.identity
            }) {
                return Err(projection_error(
                    RuntimeProjectionErrorCode::AmbiguousBinding,
                    "accepted runtime bindings repeat one SessionBinding or execution identity",
                ));
            }
        }
        let mut sessions = bindings
            .iter()
            .map(|binding| RuntimeSessionProjection {
                session_binding_id: binding.session_binding_id.clone(),
                as_of_sequence: 0,
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session_binding_id.0.cmp(&right.session_binding_id.0));
        Ok(Self {
            snapshot: RuntimeProjectionSnapshot {
                delivery_id: delivery.id().clone(),
                sessions,
            },
            bindings,
        })
    }

    pub fn snapshot(&self) -> &RuntimeProjectionSnapshot {
        &self.snapshot
    }

    /// Folds one already accepted and sealed runtime fact.
    ///
    /// # Errors
    ///
    /// Returns an error before mutation when the event is not exact for one
    /// accepted binding.
    pub fn apply(
        &mut self,
        event: &AcceptedRuntimeEvent,
    ) -> Result<RuntimeApplyOutcome, RuntimeProjectionError> {
        if event.seal != seal_event(event)? {
            return Err(projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event seal is invalid",
            ));
        }
        let mut matching = self
            .bindings
            .iter()
            .filter(|binding| binding.identity == event.identity);
        let binding = matching.next().ok_or_else(|| {
            projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event does not match an accepted binding",
            )
        })?;
        if matching.next().is_some()
            || binding
                .settled_last_sequence
                .is_some_and(|last| event.sequence > last)
        {
            return Err(projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event is ambiguous or follows its settled binding",
            ));
        }
        let session = self
            .snapshot
            .sessions
            .iter_mut()
            .find(|session| session.session_binding_id == binding.session_binding_id)
            .ok_or_else(|| {
                projection_error(
                    RuntimeProjectionErrorCode::InvalidBinding,
                    "runtime projection lost its accepted SessionBinding",
                )
            })?;
        if event.sequence != session.as_of_sequence.saturating_add(1) {
            return Err(projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event is not the next contiguous sequence",
            ));
        }
        session.as_of_sequence = event.sequence;
        Ok(RuntimeApplyOutcome::Applied {
            session_binding_id: binding.session_binding_id.clone(),
            sequence: event.sequence,
        })
    }
}

fn validate_binding(
    delivery: &Delivery,
    accepted: &AcceptedRuntimeBinding,
) -> Result<(), RuntimeProjectionError> {
    if accepted.seal != seal_binding(accepted)? {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "accepted runtime binding seal is invalid",
        ));
    }
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.id == accepted.session_binding_id
                && binding.delivery_id == accepted.identity.delivery_id
                && binding.stage_run_id == accepted.identity.stage_run_id
                && binding.product_session_id == accepted.identity.product_session_id
                && binding.execution_job_id == accepted.identity.execution_job_id
                && binding.worker_session_id.as_ref() == Some(&accepted.identity.worker_session_id)
                && binding.codex_thread_id.as_ref() == Some(&accepted.identity.codex_thread_id)
        });
    let binding = bindings.next();
    let mut runs = delivery.snapshot().stage_runs.iter().filter(|run| {
        run.id == accepted.identity.stage_run_id
            && run.delivery_id == accepted.identity.delivery_id
            && run.attempt == accepted.identity.attempt
    });
    let run = runs.next();
    let settled_matches = run.is_some_and(|run| {
        let settled = run.finished_at_millis.is_some();
        settled == accepted.settled_last_sequence.is_some()
            && accepted.settled_last_sequence.is_none_or(|last| last > 0)
    });
    if accepted.identity.delivery_id != *delivery.id()
        || binding.is_none()
        || bindings.next().is_some()
        || run.is_none()
        || runs.next().is_some()
        || !settled_matches
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "accepted runtime binding does not match one current SessionBinding and StageRun",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BindingSeal<'binding> {
    session_binding_id: &'binding SessionBindingId,
    identity: &'binding RuntimeIdentity,
    settled_last_sequence: Option<u64>,
}

fn seal_binding(binding: &AcceptedRuntimeBinding) -> Result<Sha256Digest, RuntimeProjectionError> {
    seal(&BindingSeal {
        session_binding_id: &binding.session_binding_id,
        identity: &binding.identity,
        settled_last_sequence: binding.settled_last_sequence,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventSeal<'event> {
    identity: &'event RuntimeIdentity,
    sequence: u64,
    event_id: &'event ExecutionEventId,
}

fn seal_event(event: &AcceptedRuntimeEvent) -> Result<Sha256Digest, RuntimeProjectionError> {
    seal(&EventSeal {
        identity: &event.identity,
        sequence: event.sequence,
        event_id: &event.event_id,
    })
}

fn seal(value: &impl Serialize) -> Result<Sha256Digest, RuntimeProjectionError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            format!("runtime fact seal cannot be encoded: {error}"),
        )
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn projection_error(
    code: RuntimeProjectionErrorCode,
    message: impl Into<String>,
) -> RuntimeProjectionError {
    RuntimeProjectionError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Delivery, test_fixture};

    fn fixture() -> (Delivery, AcceptedRuntimeBinding, AcceptedRuntimeEvent) {
        let delivery = Delivery::try_from_snapshot(test_fixture()).expect("canonical Delivery");
        let session = &delivery.snapshot().session_bindings[0];
        let run = &delivery.snapshot().stage_runs[0];
        let identity = RuntimeIdentity {
            delivery_id: delivery.id().clone(),
            stage_run_id: run.id.clone(),
            product_session_id: session.product_session_id.clone(),
            worker_session_id: session
                .worker_session_id
                .clone()
                .expect("accepted WorkerSession"),
            codex_thread_id: session
                .codex_thread_id
                .clone()
                .expect("accepted CodexThread"),
            execution_job_id: session.execution_job_id.clone(),
            lease_id: LeaseId("lease-runtime-projection".into()),
            attempt: run.attempt,
            fencing_token: FencingToken("7".into()),
            worker_id: WorkerId("worker-runtime-projection".into()),
            worker_instance_id: WorkerInstanceId("worker-instance-runtime-projection".into()),
        };
        let mut binding = AcceptedRuntimeBinding {
            session_binding_id: session.id.clone(),
            identity: identity.clone(),
            settled_last_sequence: Some(1),
            seal: Sha256Digest(String::new()),
        };
        binding.seal = seal_binding(&binding).expect("binding seal");
        let mut event = AcceptedRuntimeEvent {
            identity,
            sequence: 1,
            event_id: ExecutionEventId("runtime-event-1".into()),
            seal: Sha256Digest(String::new()),
        };
        event.seal = seal_event(&event).expect("event seal");
        (delivery, binding, event)
    }

    #[test]
    fn runtime_event_requires_one_exact_session_binding() {
        let (delivery, binding, event) = fixture();
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding]).expect("exact runtime binding");
        assert_eq!(
            projection.apply(&event).expect("exact bound event"),
            RuntimeApplyOutcome::Applied {
                session_binding_id: delivery.snapshot().session_bindings[0].id.clone(),
                sequence: 1,
            }
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);
    }

    #[test]
    fn runtime_projection_rejects_unbound_or_ambiguous_event() {
        let (delivery, binding, event) = fixture();
        let duplicate = RuntimeProjection::new(&delivery, vec![binding.clone(), binding.clone()])
            .expect_err("duplicate accepted authority must be ambiguous");
        assert_eq!(
            duplicate.code(),
            RuntimeProjectionErrorCode::AmbiguousBinding
        );

        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding]).expect("exact binding");
        let mut foreign = event;
        foreign.identity.delivery_id = DeliveryId("foreign-delivery".into());
        foreign.seal = seal_event(&foreign).expect("foreign sealed event");
        let error = projection
            .apply(&foreign)
            .expect_err("unbound event must fail before projection");
        assert_eq!(error.code(), RuntimeProjectionErrorCode::UnboundEvent);
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 0);
    }
}
