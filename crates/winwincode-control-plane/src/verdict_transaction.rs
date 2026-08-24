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
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StorageError,
};

use crate::{
    DeliveryVerdictCommitError, StateChange, command_receipt,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit,
};

const VERDICT_SUBMITTED_TOPIC: &str = "delivery.verdict.submitted";

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    facts: SubmitVerdictFacts<'_>,
) -> Result<CommitReceipt, DeliveryVerdictCommitError> {
    let payload =
        validate_command_envelope(command).map_err(DeliveryVerdictCommitError::Storage)?;
    let expected_revision =
        expected_revision(command).map_err(DeliveryVerdictCommitError::Storage)?;
    let (receipt_identity, command_digest) =
        command_receipt(command).map_err(DeliveryVerdictCommitError::Storage)?;
    let delivery_id = payload.delivery_id.clone();
    let journal_key =
        delivery_journal_key(&delivery_id).map_err(DeliveryVerdictCommitError::Storage)?;
    let loaded = storage
        .load_journal(&journal_key)
        .map_err(DeliveryVerdictCommitError::Storage)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);

    if let Some(receipt) = storage
        .load_receipt(&receipt_identity, &command_digest)
        .map_err(DeliveryVerdictCommitError::Storage)?
    {
        let source = DeliveryStore::borrowed(&journal)
            .query(DeliveryQuery::GetRevision {
                delivery_id: delivery_id.clone(),
                revision: expected_revision,
            })
            .map_err(delivery_store_error)?;
        let committed = DeliveryStore::borrowed(&journal)
            .query(DeliveryQuery::GetRevision {
                delivery_id,
                revision: receipt.revision,
            })
            .map_err(delivery_store_error)?;
        validate_receipt(&receipt, &source, &committed, &receipt_identity, true)
            .map_err(DeliveryVerdictCommitError::Storage)?;
        return Ok(receipt);
    }

    validate_command_facts(&payload, expected_revision, &facts)
        .map_err(DeliveryVerdictCommitError::Storage)?;
    let source = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::GetRevision {
            delivery_id: delivery_id.clone(),
            revision: expected_revision,
        })
        .map_err(delivery_store_error)?;
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
        .execute(DeliveryCommand::SubmitVerdict(Box::new(
            SubmitDeliveryVerdict {
                request_id: command.request_id.clone(),
                request_digest,
                expected_revision,
                transition,
            },
        )))
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
    validate_receipt(
        &receipt,
        &source,
        &mutation.snapshot,
        &commit.receipt_identity,
        mutation.replayed,
    )
    .map_err(DeliveryVerdictCommitError::Storage)?;
    Ok(receipt)
}

fn validate_command_envelope(
    command: &CommandEnvelope,
) -> Result<DeliverySubmitVerdictPayload, StorageError> {
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
    if serde_json::to_value(&payload).map_err(storage_error)? != command.payload {
        return Err(StorageError::invalid_input(
            "delivery.submit_verdict payload is not canonical",
        ));
    }
    Ok(payload)
}

fn validate_command_facts(
    payload: &DeliverySubmitVerdictPayload,
    expected_revision: u64,
    facts: &SubmitVerdictFacts<'_>,
) -> Result<(), StorageError> {
    if payload.delivery_id != *facts.candidate.delivery_id()
        || facts.expected_revision != expected_revision
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

fn delivery_store_error(error: impl std::fmt::Display) -> DeliveryVerdictCommitError {
    DeliveryVerdictCommitError::Storage(StorageError::invalid_input(error.to_string()))
}

fn expected_revision(command: &CommandEnvelope) -> Result<u64, StorageError> {
    u64::try_from(command.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("Delivery expectedRevision must not be negative"))
}

fn validate_receipt(
    receipt: &CommitReceipt,
    source: &Delivery,
    delivery: &Delivery,
    expected_identity: &ReceiptIdentity,
    expected_replay: bool,
) -> Result<(), StorageError> {
    if receipt.stream_id != delivery_stream_id(delivery.id())
        || receipt.revision != delivery.revision()
        || &receipt.receipt_identity != expected_identity
        || receipt.idempotent_replay != expected_replay
    {
        return Err(StorageError::invalid_input(
            "durable verdict receipt does not match its scoped request, replay state, or Delivery revision",
        ));
    }
    let [event] = receipt.events.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable verdict receipt must contain exactly one verdict event",
        ));
    };
    if event.topic != VERDICT_SUBMITTED_TOPIC {
        return Err(StorageError::invalid_input(
            "durable verdict receipt contains another event topic",
        ));
    }
    let submitted = strict_verdict_event(&event.payload)?;
    let expected = event_from_persisted_transition(source, delivery)?;
    if event.event_id != verdict_event_id(&submitted) || submitted != expected {
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

fn event_from_persisted_transition(
    source: &Delivery,
    delivery: &Delivery,
) -> Result<DeliveryVerdictSubmittedEvent, StorageError> {
    let before = source.snapshot();
    let after = delivery.snapshot();
    if after.revision != before.revision.saturating_add(1)
        || !after.attention_items.starts_with(&before.attention_items)
    {
        return Err(StorageError::invalid_input(
            "durable verdict snapshot is not the exact next Delivery transition",
        ));
    }
    let verdict = after.verdict.clone().ok_or_else(|| {
        StorageError::invalid_input("durable verdict snapshot has no computed verdict")
    })?;
    let evidence_ids = verdict
        .criteria
        .iter()
        .flat_map(|result| result.evidence_refs.iter())
        .map(|evidence_id| evidence_id.0.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let evidence = evidence_ids
        .into_iter()
        .map(|evidence_id| {
            after
                .evidence
                .iter()
                .find(|evidence| evidence.id.0 == evidence_id)
                .cloned()
                .ok_or_else(|| {
                    StorageError::invalid_input(
                        "durable verdict cites Evidence absent from its Delivery snapshot",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DeliveryVerdictSubmittedEvent {
        schema_version: 1,
        delivery_id: after.id.clone(),
        delivery_revision: after.revision,
        candidate_ref: verdict.candidate_ref.clone(),
        evidence,
        verdict,
        attention_items: after.attention_items[before.attention_items.len()..].to_vec(),
        task_statuses: after
            .tasks
            .iter()
            .map(
                |task| winwincode_delivery::application::verdict::DeliveryTaskStatusFact {
                    delivery_task_id: task.id.clone(),
                    status: task.status,
                },
            )
            .collect(),
        status: after.status,
        produced_at_millis: after.updated_at_millis,
    })
}

#[cfg(test)]
fn event_matches_delivery(
    event: &DeliveryVerdictSubmittedEvent,
    source: &Delivery,
    delivery: &Delivery,
) -> bool {
    event_from_persisted_transition(source, delivery).is_ok_and(|expected| expected == *event)
}

fn verdict_event_id(event: &DeliveryVerdictSubmittedEvent) -> String {
    format!("delivery-verdict:{}", event.verdict.id.0)
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}

#[cfg(test)]
mod tests {
    use winwincode_delivery::{
        application::verdict::{
            SubmitVerdictFacts, compute_verdict_transition,
            test_support::{VerdictFixtureOutcome, verdict_fixture},
        },
        domain::{Delivery, DeliveryTaskStatus},
    };
    use winwincode_domain::{DeliveryId, DeliveryTaskId};

    use super::event_matches_delivery;

    fn fail_transition() -> (
        winwincode_delivery::application::verdict::DeliveryVerdictSubmittedEvent,
        Delivery,
        Delivery,
    ) {
        let fixture = verdict_fixture(
            &DeliveryId("delivery-receipt-exactness".into()),
            VerdictFixtureOutcome::Fail,
        );
        let transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: 1_800_000_000_100,
            },
        )
        .expect("sealed failing transition");
        (
            transition.event().clone(),
            fixture.delivery,
            transition.delivery().clone(),
        )
    }

    #[test]
    fn durable_event_requires_the_complete_evidence_and_attention_sets() {
        let (event, source, delivery) = fail_transition();
        assert!(event_matches_delivery(&event, &source, &delivery));

        let mut missing_evidence = event.clone();
        missing_evidence.evidence.clear();
        assert!(!event_matches_delivery(
            &missing_evidence,
            &source,
            &delivery
        ));

        let mut missing_attention = event;
        missing_attention.attention_items.clear();
        assert!(!event_matches_delivery(
            &missing_attention,
            &source,
            &delivery
        ));
    }

    #[test]
    fn durable_event_rejects_repeated_task_entries_that_hide_another_task() {
        let (mut event, source, delivery) = fail_transition();
        let mut source_snapshot = source.into_snapshot();
        let mut source_second = source_snapshot.tasks[0].clone();
        source_second.id = DeliveryTaskId("task-receipt-second".into());
        source_second.status = DeliveryTaskStatus::Verifying;
        source_snapshot.tasks.push(source_second);
        let source = Delivery::try_from_snapshot(source_snapshot).expect("two-task source");
        let mut snapshot = delivery.into_snapshot();
        let mut second = snapshot.tasks[0].clone();
        second.id = DeliveryTaskId("task-receipt-second".into());
        second.status = DeliveryTaskStatus::Failed;
        snapshot.tasks.push(second);
        let delivery = Delivery::try_from_snapshot(snapshot).expect("two-task Delivery");

        event.task_statuses = vec![event.task_statuses[0].clone(); 2];
        assert!(!event_matches_delivery(&event, &source, &delivery));
    }
}
