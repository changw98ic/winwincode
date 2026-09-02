// SPDX-License-Identifier: Apache-2.0

//! Durable candidate/runtime/Evidence join for production Delivery verdicts.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::from_slice;
use sha2::{Digest, Sha256};
use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::{
    application::stage::DeliveryTerminalOutcomeFacts,
    domain::{
        Delivery, DeliveryStage, SessionBinding, StageRun, StageRunActorType, StageRunStatus,
        candidate::{
            ProductionRuntimeEvent, ProductionRuntimeEventCategory, ProductionRuntimePayload,
            ProductionVerificationRuntime, freeze_delivery_candidate_from_source,
            resolve_production_verdict,
        },
    },
};
use winwincode_domain::{ExecutionJobId, Sha256Digest};
use winwincode_execution_port::generated::{EncodedPayload, ExecutionEventCategory};
use winwincode_storage::{
    ArtifactAccess, ArtifactProvenance, ArtifactStore, GitSourceResolver, ProductStateStorage,
    StorageError, ValidatedGitSourceArtifact,
};

use crate::runtime_event_transaction::{RuntimeLedgerState, runtime_stream_id_for_projection};
use crate::session_binding_transaction::instant_millis;
use crate::{
    DeliveryAuthorityError, DeliveryVerdictAuthority, repository_scope_key,
    terminal_outcome_transaction::load_settled_terminal_authority,
};

const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";

pub(crate) fn resolve(
    storage: &dyn ProductStateStorage,
    artifacts: &ArtifactStore,
    source_resolver: &dyn GitSourceResolver,
    scope: &RepositoryScope,
    delivery: &Delivery,
) -> Result<DeliveryVerdictAuthority, DeliveryAuthorityError> {
    let writer = current_writer(delivery)?;
    let writer_terminal = load_terminal(storage, delivery, &writer.execution_job_id)?;
    let writer_source = source_for_terminal(
        artifacts,
        source_resolver,
        scope,
        delivery,
        &writer_terminal,
    )?;
    let mut verification = Vec::new();
    for run in current_verification_runs(delivery)? {
        let binding = exact_binding(delivery, run)?;
        let terminal = load_terminal(storage, delivery, &binding.execution_job_id)?;
        let events = runtime_events(storage, scope, delivery, run, &terminal)?;
        verification.push(ProductionVerificationRuntime::from_durable_read_only(
            terminal,
            writer_source.clone(),
            events,
        ));
    }
    let resolved =
        resolve_production_verdict(delivery, &writer_source, &writer_terminal, verification)
            .map_err(|error| authority_error(&error))?;
    let (candidate, verification, evidence, produced_at_millis) = resolved.into_parts();
    Ok(DeliveryVerdictAuthority {
        candidate,
        verification,
        evidence,
        produced_at_millis,
    })
}

/// Rebuilds the current successful writer candidate from the same durable
/// terminal and Artifact authorities used by production verdict resolution.
///
/// A Delivery without a successful current writer has no candidate. Once a
/// writer succeeds, every missing, corrupt, ambiguous, or stale source fact is
/// rejected rather than falling back to an older writer.
pub(crate) fn resolve_current_candidate(
    storage: &dyn ProductStateStorage,
    artifacts: &ArtifactStore,
    source_resolver: &dyn GitSourceResolver,
    scope: &RepositoryScope,
    delivery: &Delivery,
) -> Result<Option<winwincode_delivery::domain::FrozenDeliveryCandidate>, DeliveryAuthorityError> {
    let Some(writer) = selected_current_writer(delivery)? else {
        return Ok(None);
    };
    let terminal = load_terminal(storage, delivery, &writer.execution_job_id)?;
    let source = source_for_terminal(artifacts, source_resolver, scope, delivery, &terminal)?;
    freeze_delivery_candidate_from_source(delivery, &source, &terminal)
        .map(Some)
        .map_err(|error| authority_error(&error))
}

/// Rebuilds the executor candidate from the terminal handoff that the same
/// `delivery.advance` transaction will consume.
///
/// The transition Delivery has already applied the successful executor facts,
/// so the sealed handoff can be revalidated there without reading a terminal
/// authority that is intentionally still pending in storage.
pub(crate) fn resolve_pending_executor_candidate(
    artifacts: &ArtifactStore,
    source_resolver: &dyn GitSourceResolver,
    scope: &RepositoryScope,
    delivery: &Delivery,
    terminal: &DeliveryTerminalOutcomeFacts,
) -> Result<winwincode_delivery::domain::FrozenDeliveryCandidate, DeliveryAuthorityError> {
    let source = source_for_terminal(artifacts, source_resolver, scope, delivery, terminal)?;
    freeze_delivery_candidate_from_source(delivery, &source, terminal)
        .map_err(|error| authority_error(&error))
}

fn current_writer(delivery: &Delivery) -> Result<&SessionBinding, DeliveryAuthorityError> {
    selected_current_writer(delivery)?.ok_or_else(|| {
        DeliveryAuthorityError::new("current writer is missing or has not succeeded")
    })
}

fn selected_current_writer(
    delivery: &Delivery,
) -> Result<Option<&SessionBinding>, DeliveryAuthorityError> {
    let writers = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.actor_type == StageRunActorType::Codex
                && matches!(
                    run.stage,
                    DeliveryStage::Executing | DeliveryStage::Reworking
                )
                && matches!(run.role.as_str(), "executor" | "remediator")
        })
        .collect::<Vec<_>>();
    let Some(current_key) = writers
        .iter()
        .map(|run| (run.started_at_millis, run.attempt))
        .max()
    else {
        return Ok(None);
    };
    let current = writers
        .into_iter()
        .filter(|run| (run.started_at_millis, run.attempt) == current_key)
        .collect::<Vec<_>>();
    let [candidate] = current.as_slice() else {
        return Err(DeliveryAuthorityError::new(
            "current writer attempt is ambiguous",
        ));
    };
    if candidate.status != StageRunStatus::Succeeded {
        return Ok(None);
    }
    exact_binding(delivery, candidate).map(Some)
}

fn current_verification_runs(
    delivery: &Delivery,
) -> Result<Vec<&StageRun>, DeliveryAuthorityError> {
    let mut ordered = Vec::new();
    for role in ["reviewer", "verifier", "adversarial-verifier"] {
        let role_runs = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| {
                run.stage == DeliveryStage::Verifying
                    && run.actor_type == StageRunActorType::Codex
                    && run.role == role
            })
            .collect::<Vec<_>>();
        let Some(current_attempt) = role_runs.iter().map(|run| run.attempt).max() else {
            continue;
        };
        let current = role_runs
            .into_iter()
            .filter(|run| run.attempt == current_attempt)
            .collect::<Vec<_>>();
        let [run] = current.as_slice() else {
            return Err(DeliveryAuthorityError::new(
                "current verification role attempt is ambiguous",
            ));
        };
        if run.status != StageRunStatus::Succeeded {
            return Err(DeliveryAuthorityError::new(
                "current independent verification role has not succeeded",
            ));
        }
        ordered.push(*run);
    }
    if !matches!(
        ordered.as_slice(),
        [reviewer, verifier]
            if reviewer.role == "reviewer" && verifier.role == "verifier"
    ) && !matches!(
        ordered.as_slice(),
        [reviewer, verifier, adversarial]
            if reviewer.role == "reviewer"
                && verifier.role == "verifier"
                && adversarial.role == "adversarial-verifier"
    ) {
        return Err(DeliveryAuthorityError::new(
            "current independent verification roles are incomplete",
        ));
    }
    Ok(ordered)
}

fn exact_binding<'delivery>(
    delivery: &'delivery Delivery,
    run: &StageRun,
) -> Result<&'delivery SessionBinding, DeliveryAuthorityError> {
    let matching = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id)
        .collect::<Vec<_>>();
    let [binding] = matching.as_slice() else {
        return Err(DeliveryAuthorityError::new(
            "current StageRun does not have one exact SessionBinding",
        ));
    };
    Ok(*binding)
}

fn load_terminal(
    storage: &dyn ProductStateStorage,
    delivery: &Delivery,
    job_id: &ExecutionJobId,
) -> Result<DeliveryTerminalOutcomeFacts, DeliveryAuthorityError> {
    load_settled_terminal_authority(storage, delivery, job_id)
        .map_err(|error| storage_error(&error))
}

fn source_for_terminal(
    artifacts: &ArtifactStore,
    source_resolver: &dyn GitSourceResolver,
    scope: &RepositoryScope,
    delivery: &Delivery,
    terminal: &DeliveryTerminalOutcomeFacts,
) -> Result<ValidatedGitSourceArtifact, DeliveryAuthorityError> {
    let active = terminal.authority().active_lease();
    let provenance = ArtifactProvenance::execution_job(
        active.execution_job_id().clone(),
        active.attempt(),
        active.lease_id().clone(),
        active.fencing_token().clone(),
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.worker_session_id().clone(),
    )
    .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
    let scope_key = repository_scope_key(scope).map_err(|error| storage_error(&error))?;
    let mut candidate = None;
    for artifact in terminal.metadata().artifacts() {
        let object = artifacts
            .read_exact(&ArtifactAccess::new(
                scope_key.clone(),
                artifact.artifact_id.clone(),
                artifact.digest.clone(),
                provenance.clone(),
            ))
            .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
        if object.metadata().media_type() != CANDIDATE_MEDIA_TYPE {
            continue;
        }
        let resolved = source_resolver
            .resolve_candidate(
                &object,
                &delivery.snapshot().spec.repository.locator,
                &delivery.snapshot().spec.base_revision,
            )
            .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
        if candidate.replace(resolved).is_some() {
            return Err(DeliveryAuthorityError::new(
                "settled Worker outcome names more than one candidate source Artifact",
            ));
        }
    }
    candidate.ok_or_else(|| {
        DeliveryAuthorityError::new("settled Worker outcome has no exact candidate source Artifact")
    })
}

fn runtime_events(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery: &Delivery,
    run: &StageRun,
    terminal: &DeliveryTerminalOutcomeFacts,
) -> Result<Vec<ProductionRuntimeEvent>, DeliveryAuthorityError> {
    let binding = exact_binding(delivery, run)?;
    let scope_key = repository_scope_key(scope).map_err(|error| storage_error(&error))?;
    let stream_id = runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id);
    let stored = storage
        .load_state(&stream_id)
        .map_err(|error| storage_error(&error))?
        .ok_or_else(|| DeliveryAuthorityError::new("verification runtime ledger is missing"))?;
    let ledger: RuntimeLedgerState = from_slice(&stored.payload).map_err(|error| {
        DeliveryAuthorityError::new(format!("runtime ledger is invalid: {error}"))
    })?;
    let canonical = serde_json::to_vec(&ledger).map_err(|error| {
        DeliveryAuthorityError::new(format!("runtime ledger encode failed: {error}"))
    })?;
    if canonical != stored.payload
        || stored.revision != ledger.highest_sequence
        || ledger.events.len() as u64 != ledger.highest_sequence
    {
        return Err(DeliveryAuthorityError::new(
            "verification runtime ledger is non-canonical or truncated",
        ));
    }
    validate_runtime_identity(&ledger, delivery, run, binding, terminal)?;
    ledger
        .events
        .into_iter()
        .map(|entry| {
            let encoded = serde_json::to_vec(&entry.event).map_err(|error| {
                DeliveryAuthorityError::new(format!("runtime event encode failed: {error}"))
            })?;
            let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(encoded)));
            if digest != entry.event_digest {
                return Err(DeliveryAuthorityError::new("runtime event digest changed"));
            }
            let occurred_at_millis =
                instant_millis(&entry.event.occurred_at).map_err(|error| storage_error(&error))?;
            let category = runtime_category(&entry.event.category);
            let event_id = entry.event.event_id;
            let sequence = u64::try_from(entry.event.sequence.0)
                .map_err(|_| DeliveryAuthorityError::new("runtime event sequence is invalid"))?;
            let payload = entry
                .event
                .payload
                .as_ref()
                .map(validated_runtime_payload)
                .transpose()?;
            Ok(ProductionRuntimeEvent::from_durable_ledger(
                category,
                event_id,
                sequence,
                occurred_at_millis,
                payload,
            ))
        })
        .collect()
}

fn runtime_category(category: &ExecutionEventCategory) -> ProductionRuntimeEventCategory {
    match category {
        ExecutionEventCategory::Lifecycle => ProductionRuntimeEventCategory::Lifecycle,
        ExecutionEventCategory::Activity => ProductionRuntimeEventCategory::Activity,
        ExecutionEventCategory::Command => ProductionRuntimeEventCategory::Command,
        ExecutionEventCategory::Test => ProductionRuntimeEventCategory::Test,
        ExecutionEventCategory::Diff => ProductionRuntimeEventCategory::Diff,
        ExecutionEventCategory::Usage => ProductionRuntimeEventCategory::Usage,
        ExecutionEventCategory::Attention => ProductionRuntimeEventCategory::Attention,
        ExecutionEventCategory::Diagnostic => ProductionRuntimeEventCategory::Diagnostic,
    }
}

fn validated_runtime_payload(
    payload: &EncodedPayload,
) -> Result<ProductionRuntimePayload, DeliveryAuthorityError> {
    let bytes = STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| DeliveryAuthorityError::new("runtime payload is not canonical base64"))?;
    if STANDARD.encode(&bytes) != payload.data_base64 {
        return Err(DeliveryAuthorityError::new(
            "runtime payload is not canonical base64",
        ));
    }
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    if digest != payload.payload_digest {
        return Err(DeliveryAuthorityError::new(
            "runtime payload digest changed",
        ));
    }
    Ok(ProductionRuntimePayload::from_validated_bytes(
        payload.content_type.clone(),
        bytes,
    ))
}

fn validate_runtime_identity(
    ledger: &RuntimeLedgerState,
    delivery: &Delivery,
    run: &StageRun,
    binding: &SessionBinding,
    terminal: &DeliveryTerminalOutcomeFacts,
) -> Result<(), DeliveryAuthorityError> {
    let active = terminal.authority().active_lease();
    let terminal_sequence = u64::try_from(terminal.metadata().last_event_sequence().0)
        .map_err(|_| DeliveryAuthorityError::new("terminal runtime sequence is invalid"))?;
    let exact = ledger.delivery_id.as_ref() == Some(delivery.id())
        && ledger.delivery_task_id == run.delivery_task_id
        && ledger.stage_run_id.as_ref() == Some(&run.id)
        && ledger.product_session_id == binding.product_session_id
        && ledger.execution_job_id == binding.execution_job_id
        && binding.worker_session_id.as_ref() == Some(&ledger.worker_session_id)
        && binding.codex_thread_id.as_ref() == Some(&ledger.codex_thread_id)
        && terminal.metadata().codex_thread_id() == Some(&ledger.codex_thread_id)
        && ledger.lease_id == *active.lease_id()
        && ledger.attempt == active.attempt()
        && ledger.fencing_token == *active.fencing_token()
        && ledger.worker_id == *active.worker_id()
        && ledger.worker_instance_id == *active.worker_instance_id()
        && ledger.worker_session_id == *active.worker_session_id()
        && ledger.highest_sequence == terminal_sequence;
    if exact {
        Ok(())
    } else {
        Err(DeliveryAuthorityError::new(
            "verification runtime ledger identity is stale or foreign",
        ))
    }
}

fn storage_error(error: &StorageError) -> DeliveryAuthorityError {
    DeliveryAuthorityError::new(error.to_string())
}

fn authority_error(error: &impl ToString) -> DeliveryAuthorityError {
    DeliveryAuthorityError::new(error.to_string())
}
