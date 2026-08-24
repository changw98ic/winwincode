// SPDX-License-Identifier: Apache-2.0

//! Specialized atomic transaction for one computed Delivery verdict.

use serde_json::Value;
use winwincode_api::generated::{CommandEnvelope, CommandName, DeliverySubmitVerdictPayload};
use winwincode_delivery::{
    application::verdict::{
        DeliveryVerdictSubmittedEvent, SubmitVerdictFacts, compute_verdict_transition,
    },
    domain::Delivery,
    store::{
        DeliveryCommand, DeliveryCommandPort, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
        SubmitDeliveryVerdict,
    },
};
use winwincode_storage::{CommitReceipt, NewOutboxEvent, ProductStateStorage, StorageError};

use crate::{
    DeliveryVerdictCommitError, StateChange,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit,
};

const VERDICT_SUBMITTED_TOPIC: &str = "delivery.verdict.submitted";

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    facts: SubmitVerdictFacts<'_>,
) -> Result<CommitReceipt, DeliveryVerdictCommitError> {
    validate_command(command, &facts).map_err(DeliveryVerdictCommitError::Storage)?;
    let expected_revision =
        expected_revision(command).map_err(DeliveryVerdictCommitError::Storage)?;
    let delivery_id = facts.candidate.delivery_id().clone();
    let journal_key =
        delivery_journal_key(&delivery_id).map_err(DeliveryVerdictCommitError::Storage)?;
    let loaded = storage
        .load_journal(&journal_key)
        .map_err(DeliveryVerdictCommitError::Storage)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);
    let source = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::GetRevision {
            delivery_id: delivery_id.clone(),
            revision: expected_revision,
        })
        .map_err(|error| {
            DeliveryVerdictCommitError::Storage(StorageError::invalid_input(error.to_string()))
        })?;
    let transition = compute_verdict_transition(&source, facts)
        .map_err(DeliveryVerdictCommitError::Coordination)?;
    let event_id = verdict_event_id(transition.event());
    let event_payload = serde_json::to_vec(transition.event()).map_err(storage_error)?;
    let stream_id = delivery_stream_id(&delivery_id);
    let mut commit = storage_commit(
        command,
        StateChange::new(
            &stream_id,
            transition.delivery().encode_json().map_err(storage_error)?,
            vec![NewOutboxEvent::new(
                &event_id,
                VERDICT_SUBMITTED_TOPIC,
                event_payload,
            )],
        ),
    )
    .map_err(DeliveryVerdictCommitError::Storage)?;
    let request_digest = commit
        .command_digest
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("command digest is not canonical"))?
        .to_owned();
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SubmitVerdict(SubmitDeliveryVerdict {
            request_id: command.request_id.clone(),
            request_digest,
            expected_revision,
            transition,
        }))
        .map_err(|error| {
            DeliveryVerdictCommitError::Storage(StorageError::invalid_input(error.to_string()))
        })?;
    commit.state = mutation.snapshot.encode_json().map_err(storage_error)?;
    if mutation.replayed {
        commit = commit.require_receipt_replay();
    }
    if let Some(publication) = journal.into_publication().map_err(|error| {
        DeliveryVerdictCommitError::Storage(StorageError::adapter(error.to_string()))
    })? {
        if mutation.replayed {
            return Err(DeliveryVerdictCommitError::Storage(
                StorageError::invalid_input(
                    "replayed verdict unexpectedly staged another Delivery journal record",
                ),
            ));
        }
        commit = commit.with_journal_publication(publication);
    } else if !mutation.replayed {
        return Err(DeliveryVerdictCommitError::Storage(
            StorageError::invalid_input("new verdict did not stage a Delivery journal publication"),
        ));
    }

    let receipt = storage
        .commit(&commit)
        .map_err(DeliveryVerdictCommitError::Storage)?;
    validate_receipt(&receipt, &mutation.snapshot).map_err(DeliveryVerdictCommitError::Storage)?;
    Ok(receipt)
}

fn validate_command(
    command: &CommandEnvelope,
    facts: &SubmitVerdictFacts<'_>,
) -> Result<(), StorageError> {
    if command.command != CommandName::DeliverySubmitVerdict {
        return Err(StorageError::invalid_input(
            "Delivery verdict transaction requires delivery.submit_verdict",
        ));
    }
    let payload: DeliverySubmitVerdictPayload = serde_json::from_value(command.payload.clone())
        .map_err(|error| {
            StorageError::invalid_input(format!(
                "delivery.submit_verdict payload is not canonical: {error}"
            ))
        })?;
    if serde_json::to_value(&payload).map_err(storage_error)? != command.payload
        || payload.delivery_id != *facts.candidate.delivery_id()
        || facts.expected_revision != expected_revision(command)?
        || facts
            .candidate
            .candidate_ref()
            .strip_prefix("git-candidate:")
            != Some(payload.candidate_digest.0.as_str())
    {
        return Err(StorageError::invalid_input(
            "delivery.submit_verdict does not match the sealed candidate or expected revision",
        ));
    }
    Ok(())
}

fn expected_revision(command: &CommandEnvelope) -> Result<u64, StorageError> {
    u64::try_from(command.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("Delivery expectedRevision must not be negative"))
}

fn validate_receipt(receipt: &CommitReceipt, delivery: &Delivery) -> Result<(), StorageError> {
    if receipt.stream_id != delivery_stream_id(delivery.id())
        || receipt.revision != delivery.revision()
    {
        return Err(StorageError::invalid_input(
            "durable verdict receipt does not match its Delivery revision",
        ));
    }
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == VERDICT_SUBMITTED_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable verdict receipt must contain exactly one verdict event",
        ));
    };
    let submitted = strict_verdict_event(&event.payload)?;
    if event.event_id != verdict_event_id(&submitted)
        || !event_matches_delivery(&submitted, delivery)
    {
        return Err(StorageError::invalid_input(
            "durable verdict event does not match the committed Delivery facts",
        ));
    }
    Ok(())
}

fn strict_verdict_event(payload: &[u8]) -> Result<DeliveryVerdictSubmittedEvent, StorageError> {
    let value: Value = serde_json::from_slice(payload).map_err(storage_error)?;
    let event: DeliveryVerdictSubmittedEvent =
        serde_json::from_value(value.clone()).map_err(storage_error)?;
    if serde_json::to_value(&event).map_err(storage_error)? != value {
        return Err(StorageError::invalid_input(
            "durable verdict event has unknown or non-canonical fields",
        ));
    }
    Ok(event)
}

fn event_matches_delivery(event: &DeliveryVerdictSubmittedEvent, delivery: &Delivery) -> bool {
    let snapshot = delivery.snapshot();
    event.schema_version == 1
        && event.delivery_id == snapshot.id
        && event.delivery_revision == snapshot.revision
        && event.candidate_ref == event.verdict.candidate_ref
        && snapshot.verdict.as_ref() == Some(&event.verdict)
        && event.status == snapshot.status
        && event.produced_at_millis == snapshot.updated_at_millis
        && event
            .evidence
            .iter()
            .all(|evidence| snapshot.evidence.contains(evidence))
        && event
            .attention_items
            .iter()
            .all(|attention| snapshot.attention_items.contains(attention))
        && event.task_statuses.len() == snapshot.tasks.len()
        && event.task_statuses.iter().all(|fact| {
            snapshot
                .tasks
                .iter()
                .any(|task| task.id == fact.delivery_task_id && task.status == fact.status)
        })
}

fn verdict_event_id(event: &DeliveryVerdictSubmittedEvent) -> String {
    format!("delivery-verdict:{}", event.verdict.id.0)
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
