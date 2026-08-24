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
    validate_receipt(&receipt, &source, &mutation.snapshot)
        .map_err(DeliveryVerdictCommitError::Storage)?;
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

fn validate_receipt(
    receipt: &CommitReceipt,
    source: &Delivery,
    delivery: &Delivery,
) -> Result<(), StorageError> {
    if receipt.stream_id != delivery_stream_id(delivery.id())
        || receipt.revision != delivery.revision()
    {
        return Err(StorageError::invalid_input(
            "durable verdict receipt does not match its Delivery revision",
        ));
    }
    let [event] = receipt.events.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable verdict receipt must contain exactly one verdict event",
        ));
    };
    if event.topic != VERDICT_SUBMITTED_TOPIC {
        return Err(StorageError::invalid_input(
            "durable verdict receipt has the wrong event topic",
        ));
    }
    let submitted = strict_verdict_event(&event.payload)?;
    if event.event_id != verdict_event_id(&submitted)
        || !event_matches_delivery(&submitted, source, delivery)
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

fn event_matches_delivery(
    event: &DeliveryVerdictSubmittedEvent,
    source: &Delivery,
    delivery: &Delivery,
) -> bool {
    let before = source.snapshot();
    let snapshot = delivery.snapshot();
    let mut cited_ids = event
        .verdict
        .criteria
        .iter()
        .flat_map(|result| result.evidence_refs.iter())
        .collect::<Vec<_>>();
    cited_ids.sort_by(|left, right| left.0.cmp(&right.0));
    cited_ids.dedup();
    let mut expected_evidence = snapshot
        .evidence
        .iter()
        .filter(|evidence| {
            evidence.candidate_ref == event.candidate_ref
                && cited_ids
                    .binary_search_by(|id| id.0.cmp(&evidence.id.0))
                    .is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    expected_evidence.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    let mut actual_evidence = event.evidence.clone();
    actual_evidence.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    let attention_prefix_is_exact = snapshot.attention_items.get(..before.attention_items.len())
        == Some(before.attention_items.as_slice());
    let expected_attention = snapshot
        .attention_items
        .get(before.attention_items.len()..)
        .unwrap_or_default();
    let expected_task_statuses = snapshot
        .tasks
        .iter()
        .map(
            |task| winwincode_delivery::application::verdict::DeliveryTaskStatusFact {
                delivery_task_id: task.id.clone(),
                status: task.status,
            },
        )
        .collect::<Vec<_>>();
    event.schema_version == 1
        && source.id() == delivery.id()
        && source.revision().checked_add(1) == Some(delivery.revision())
        && event.delivery_id == snapshot.id
        && event.delivery_revision == snapshot.revision
        && event.candidate_ref == event.verdict.candidate_ref
        && snapshot.verdict.as_ref() == Some(&event.verdict)
        && event.status == snapshot.status
        && event.produced_at_millis == snapshot.updated_at_millis
        && cited_ids.len() == expected_evidence.len()
        && actual_evidence == expected_evidence
        && attention_prefix_is_exact
        && event.attention_items == expected_attention
        && event.task_statuses == expected_task_statuses
}

fn verdict_event_id(event: &DeliveryVerdictSubmittedEvent) -> String {
    format!("delivery-verdict:{}", event.verdict.id.0)
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}

#[cfg(test)]
mod tests {
    use winwincode_delivery::application::verdict::{
        SubmitVerdictFacts, compute_verdict_transition,
        test_support::{VerdictFixtureOutcome, verdict_fixture},
    };
    use winwincode_domain::DeliveryId;

    use super::event_matches_delivery;

    #[test]
    fn durable_verdict_event_requires_the_exact_computed_projection() {
        let fixture = verdict_fixture(
            &DeliveryId("delivery-verdict-receipt-exact".into()),
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
        .expect("computed verdict transition");
        let exact = transition.event().clone();
        assert!(event_matches_delivery(
            &exact,
            &fixture.delivery,
            transition.delivery()
        ));

        let mut missing_evidence = exact.clone();
        missing_evidence.evidence.pop();
        assert!(!event_matches_delivery(
            &missing_evidence,
            &fixture.delivery,
            transition.delivery()
        ));

        let mut missing_attention = exact;
        missing_attention.attention_items.clear();
        assert!(!event_matches_delivery(
            &missing_attention,
            &fixture.delivery,
            transition.delivery()
        ));
    }
}
