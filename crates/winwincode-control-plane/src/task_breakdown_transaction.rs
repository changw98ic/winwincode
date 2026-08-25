// SPDX-License-Identifier: Apache-2.0

//! Receipt-first atomic transaction for an approved Delivery task graph.

use std::collections::HashSet;

use serde_json::Value;
use winwincode_api::generated::{
    CommandEnvelope, CommandName, DeliveryApproveTaskBreakdownPayload,
};
use winwincode_delivery::{
    application::task_breakdown::DeliveryTaskBreakdownApprovedEvent,
    domain::{DELIVERY_SCHEMA_VERSION, Delivery, DeliveryTaskStatus},
    projection::{ProjectionInput, SolutionReviewStatusProjection, project_delivery_detail},
    store::{
        ApproveDeliveryTaskBreakdown, DeliveryCommand, DeliveryCommandPort, DeliveryQuery,
        DeliveryQueryPort, DeliveryStore, DeliveryStoreError, DeliveryStoreErrorCode,
    },
};
use winwincode_domain::DeliveryId;
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StorageError,
};

use crate::{
    StateChange, command_receipt,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit,
};

const TASK_BREAKDOWN_APPROVED_TOPIC: &str = "delivery.task_breakdown.approved";

#[allow(
    clippy::too_many_lines,
    reason = "receipt-first replay and all four atomic members stay visibly ordered in one transaction seam"
)]
pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
) -> Result<CommitReceipt, StorageError> {
    if command.command != CommandName::DeliveryApproveTaskBreakdown {
        return Err(StorageError::invalid_input(
            "task-breakdown transaction requires delivery.approve_task_breakdown",
        ));
    }
    let (receipt_identity, command_digest) = command_receipt(command)?;
    let prior_receipt = storage.load_receipt(&receipt_identity, &command_digest)?;
    let payload = validate_payload(command)?;
    let expected_revision = expected_revision(command)?;
    let review_set_sha256 = transport_review_digest(&payload)?.to_owned();
    let delivery_id = payload.delivery_id.clone();
    let journal_key = delivery_journal_key(&delivery_id)?;

    if let Some(receipt) = prior_receipt {
        validate_receipt_projection(
            &receipt,
            &receipt_identity,
            &delivery_id,
            &review_set_sha256,
            true,
        )?;
        return Ok(receipt);
    }

    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);
    let source = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::GetRevision {
            delivery_id: delivery_id.clone(),
            revision: expected_revision,
        })
        .map_err(|error| delivery_store_error(&error))?;
    let request_digest = command_digest
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("command digest is not canonical"))?
        .to_owned();
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::ApproveTaskBreakdown(Box::new(
            ApproveDeliveryTaskBreakdown {
                delivery_id: delivery_id.clone(),
                request_id: command.request_id.clone(),
                request_digest,
                expected_revision,
                review_set_sha256: review_set_sha256.clone(),
            },
        )))
        .map_err(|error| delivery_store_error(&error))?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "task-breakdown journal replay is missing its scoped command receipt",
        ));
    }
    let event = mutation.task_breakdown_event.as_ref().ok_or_else(|| {
        StorageError::invalid_input("task-breakdown mutation did not produce its sealed event")
    })?;
    let event_payload = serde_json::to_vec(event).map_err(storage_error)?;
    let event_id = task_breakdown_event_id(event);
    let stream_id = delivery_stream_id(&delivery_id);
    let mut commit = storage_commit(
        command,
        StateChange::new(
            &stream_id,
            mutation.snapshot.encode_json().map_err(storage_error)?,
            vec![NewOutboxEvent::new(
                &event_id,
                TASK_BREAKDOWN_APPROVED_TOPIC,
                event_payload,
            )],
        ),
    )?;
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
        .ok_or_else(|| {
            StorageError::invalid_input(
                "new task-breakdown mutation did not stage a Delivery journal record",
            )
        })?;
    commit = commit.with_journal_publication(publication);

    let receipt = storage.commit(&commit)?;
    if receipt.idempotent_replay {
        validate_receipt_projection(
            &receipt,
            &commit.receipt_identity,
            &delivery_id,
            &review_set_sha256,
            true,
        )?;
        return Ok(receipt);
    }
    validate_committed_receipt(
        &receipt,
        &source,
        &mutation.snapshot,
        &commit.receipt_identity,
        &review_set_sha256,
        false,
    )?;
    Ok(receipt)
}

fn validate_payload(
    command: &CommandEnvelope,
) -> Result<DeliveryApproveTaskBreakdownPayload, StorageError> {
    let payload: DeliveryApproveTaskBreakdownPayload =
        serde_json::from_value(command.payload.clone()).map_err(|error| {
            StorageError::invalid_input(format!(
                "delivery.approve_task_breakdown payload is not canonical: {error}"
            ))
        })?;
    if serde_json::to_value(&payload).map_err(storage_error)? != command.payload {
        return Err(StorageError::invalid_input(
            "delivery.approve_task_breakdown payload is not canonical",
        ));
    }
    Ok(payload)
}

fn expected_revision(command: &CommandEnvelope) -> Result<u64, StorageError> {
    u64::try_from(command.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("Delivery expectedRevision must not be negative"))
}

fn transport_review_digest(
    payload: &DeliveryApproveTaskBreakdownPayload,
) -> Result<&str, StorageError> {
    let digest = payload
        .review_set_sha256
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("reviewSetSha256 is not canonical"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StorageError::invalid_input(
            "reviewSetSha256 is not canonical",
        ));
    }
    Ok(digest)
}

fn validate_committed_receipt(
    receipt: &CommitReceipt,
    source: &Delivery,
    committed: &Delivery,
    expected_identity: &ReceiptIdentity,
    review_set_sha256: &str,
    expected_replay: bool,
) -> Result<(), StorageError> {
    let event = validate_receipt_projection(
        receipt,
        expected_identity,
        committed.id(),
        review_set_sha256,
        expected_replay,
    )?;
    if event.delivery_spec_id != committed.snapshot().spec.id
        || event.delivery_spec_revision != committed.snapshot().spec.revision
        || event.tasks != committed.snapshot().tasks
        || committed.revision() != source.revision().saturating_add(1)
    {
        return Err(StorageError::invalid_input(
            "durable task-breakdown event does not match the committed Delivery",
        ));
    }

    let solution = project_delivery_detail(ProjectionInput::new(source))
        .map_err(storage_error)?
        .solution_review()
        .ok_or_else(|| StorageError::invalid_input("source solution review is missing"))?
        .clone();
    if solution.review_status() != SolutionReviewStatusProjection::Approved
        || solution.review_set_sha256() != review_set_sha256
    {
        return Err(StorageError::invalid_input(
            "source solution review does not authorize the durable task graph",
        ));
    }
    let mut expected_snapshot = source.clone().into_snapshot();
    expected_snapshot.tasks.clone_from(&event.tasks);
    expected_snapshot.revision = expected_snapshot.revision.saturating_add(1);
    let expected = Delivery::try_from_snapshot(expected_snapshot).map_err(storage_error)?;
    if expected != *committed {
        return Err(StorageError::invalid_input(
            "durable task-breakdown record changed facts outside the approved graph",
        ));
    }
    Ok(())
}

fn validate_receipt_projection(
    receipt: &CommitReceipt,
    expected_identity: &ReceiptIdentity,
    delivery_id: &DeliveryId,
    review_set_sha256: &str,
    expected_replay: bool,
) -> Result<DeliveryTaskBreakdownApprovedEvent, StorageError> {
    if receipt.stream_id != delivery_stream_id(delivery_id)
        || &receipt.receipt_identity != expected_identity
        || receipt.idempotent_replay != expected_replay
    {
        return Err(StorageError::invalid_input(
            "durable task-breakdown receipt does not match its scoped request",
        ));
    }
    let [stored_event] = receipt.events.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable task-breakdown receipt must contain exactly one event",
        ));
    };
    let event = strict_task_breakdown_event(&stored_event.payload)?;
    if stored_event.topic != TASK_BREAKDOWN_APPROVED_TOPIC
        || stored_event.event_id != task_breakdown_event_id(&event)
        || event.schema_version != DELIVERY_SCHEMA_VERSION
        || event.delivery_id != *delivery_id
        || event.delivery_revision != receipt.revision
        || event.review_set_sha256 != review_set_sha256
    {
        return Err(StorageError::invalid_input(
            "durable task-breakdown event does not match its original receipt",
        ));
    }
    validate_promoted_tasks(&event)?;
    Ok(event)
}

fn validate_promoted_tasks(event: &DeliveryTaskBreakdownApprovedEvent) -> Result<(), StorageError> {
    if event.tasks.is_empty() || event.tasks.len() > 200 {
        return Err(StorageError::invalid_input(
            "durable task-breakdown event has an invalid task count",
        ));
    }
    let mut ids = HashSet::with_capacity(event.tasks.len());
    for task in &event.tasks {
        if task.schema_version != DELIVERY_SCHEMA_VERSION
            || task.delivery_id != event.delivery_id
            || task.owner.is_some()
            || task.status != DeliveryTaskStatus::Pending
            || task.title.is_empty()
            || task.goal.is_empty()
            || task.acceptance_criterion_ids.is_empty()
            || !ids.insert(task.id.clone())
        {
            return Err(StorageError::invalid_input(
                "durable task-breakdown event has a changed task projection",
            ));
        }
        let criterion_ids = task.acceptance_criterion_ids.iter().collect::<HashSet<_>>();
        if criterion_ids.len() != task.acceptance_criterion_ids.len() {
            return Err(StorageError::invalid_input(
                "durable task-breakdown event has duplicate criterion ids",
            ));
        }
    }
    for task in &event.tasks {
        let dependencies = task.blocked_by_task_ids.iter().collect::<HashSet<_>>();
        if dependencies.len() != task.blocked_by_task_ids.len()
            || dependencies.contains(&task.id)
            || dependencies
                .iter()
                .any(|dependency| !ids.contains(*dependency))
        {
            return Err(StorageError::invalid_input(
                "durable task-breakdown event has invalid dependencies",
            ));
        }
    }
    let mut remaining = ids;
    while !remaining.is_empty() {
        let ready = event
            .tasks
            .iter()
            .filter(|task| remaining.contains(&task.id))
            .filter(|task| {
                task.blocked_by_task_ids
                    .iter()
                    .all(|dependency| !remaining.contains(dependency))
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(StorageError::invalid_input(
                "durable task-breakdown event has a dependency cycle",
            ));
        }
        for task_id in ready {
            remaining.remove(&task_id);
        }
    }
    Ok(())
}

fn strict_task_breakdown_event(
    payload: &[u8],
) -> Result<DeliveryTaskBreakdownApprovedEvent, StorageError> {
    let value: Value = serde_json::from_slice(payload).map_err(storage_error)?;
    let event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_value(value.clone()).map_err(storage_error)?;
    if serde_json::to_value(&event).map_err(storage_error)? != value {
        return Err(StorageError::invalid_input(
            "durable task-breakdown event has unknown or non-canonical fields",
        ));
    }
    Ok(event)
}

fn task_breakdown_event_id(event: &DeliveryTaskBreakdownApprovedEvent) -> String {
    format!(
        "delivery-task-breakdown:{}:{}",
        event.delivery_id.0, event.delivery_revision
    )
}

fn delivery_store_error(error: &DeliveryStoreError) -> StorageError {
    if error.code() == DeliveryStoreErrorCode::RevisionConflict
        && let (Some(expected), Some(current)) =
            (error.expected_revision(), error.current_revision())
    {
        return StorageError::revision_conflict(expected, current);
    }
    StorageError::invalid_input(error.to_string())
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
