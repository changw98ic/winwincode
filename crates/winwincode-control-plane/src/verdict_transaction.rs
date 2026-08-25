// SPDX-License-Identifier: Apache-2.0

//! Specialized atomic transaction for one computed Delivery verdict.

use std::collections::HashSet;

use serde_json::Value;
use winwincode_api::generated::{CommandEnvelope, CommandName, DeliverySubmitVerdictPayload};
use winwincode_delivery::{
    application::verdict::{
        DeliveryVerdictSubmittedEvent, SubmitVerdictFacts, compute_verdict_transition,
    },
    domain::{
        AttentionItemStatus, CriterionVerdict, DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus,
        DeliveryTaskStatus,
    },
    store::{
        DeliveryCommand, DeliveryCommandPort, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
        SubmitDeliveryVerdict,
    },
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StorageError,
};

use crate::{
    DeliveryChangeKind, DeliveryVerdictCommitError, StateChange, command_receipt,
    delivery_changed_event,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit, validate_delivery_changed_receipt,
};

const VERDICT_SUBMITTED_TOPIC: &str = "delivery.verdict.submitted";
const VERDICT_EVENT_SCHEMA_VERSION: u8 = 1;

#[allow(
    clippy::too_many_lines,
    reason = "receipt replay and all four atomic Verdict members stay visibly ordered in one transaction seam"
)]
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
    let prior_receipt = storage
        .load_receipt(&receipt_identity, &command_digest)
        .map_err(DeliveryVerdictCommitError::Storage)?;

    if let Some(receipt) = prior_receipt {
        validate_replayed_receipt(&receipt, &payload, expected_revision, &receipt_identity)
            .map_err(DeliveryVerdictCommitError::Storage)?;
        return Ok(receipt);
    }

    let journal_key =
        delivery_journal_key(&delivery_id).map_err(DeliveryVerdictCommitError::Storage)?;
    let loaded = storage
        .load_journal(&journal_key)
        .map_err(DeliveryVerdictCommitError::Storage)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);

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
    let changed_event = delivery_changed_event(
        command,
        &delivery_id,
        transition.delivery().revision(),
        DeliveryChangeKind::Advanced,
    )
    .map_err(DeliveryVerdictCommitError::Storage)?;
    let stream_id = delivery_stream_id(&delivery_id);
    let mut commit = storage_commit(
        command,
        StateChange::new(
            &stream_id,
            transition.delivery().encode_json().map_err(storage_error)?,
            vec![
                NewOutboxEvent::internal(&event_id, VERDICT_SUBMITTED_TOPIC, event_payload),
                changed_event,
            ],
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

fn validate_replayed_receipt(
    receipt: &CommitReceipt,
    payload: &DeliverySubmitVerdictPayload,
    expected_revision: u64,
    expected_identity: &ReceiptIdentity,
) -> Result<(), StorageError> {
    let committed_revision = expected_revision.checked_add(1).ok_or_else(|| {
        StorageError::invalid_input("Delivery verdict revision exceeds the durable range")
    })?;
    if receipt.stream_id != delivery_stream_id(&payload.delivery_id)
        || receipt.revision != committed_revision
        || &receipt.receipt_identity != expected_identity
        || !receipt.idempotent_replay
    {
        return Err(StorageError::invalid_input(
            "durable verdict replay does not match its scoped request, stream, or revision",
        ));
    }
    let submitted = strict_receipt_event(receipt)?;
    let candidate_ref = format!("git-candidate:{}", payload.candidate_digest.0);
    validate_replayed_event(&submitted, payload, &candidate_ref, receipt.revision)?;
    validate_delivery_changed_receipt(
        receipt,
        &payload.delivery_id,
        receipt.revision,
        DeliveryChangeKind::Advanced,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "a replay validates every self-contained verdict event relationship without consulting mutable Delivery state"
)]
fn validate_replayed_event(
    event: &DeliveryVerdictSubmittedEvent,
    payload: &DeliverySubmitVerdictPayload,
    candidate_ref: &str,
    committed_revision: u64,
) -> Result<(), StorageError> {
    let verdict = &event.verdict;
    if event.schema_version != VERDICT_EVENT_SCHEMA_VERSION
        || event.delivery_id != payload.delivery_id
        || event.delivery_revision != committed_revision
        || event.candidate_ref != candidate_ref
        || event.candidate_ref != verdict.candidate_ref
        || event.produced_at_millis != verdict.produced_at_millis
        || verdict.schema_version != DELIVERY_SCHEMA_VERSION
        || verdict.delivery_id != event.delivery_id
        || verdict.criteria.is_empty()
    {
        return Err(StorageError::invalid_input(
            "durable verdict replay event does not match its command or verdict identity",
        ));
    }

    let mut evidence_ids = HashSet::with_capacity(event.evidence.len());
    let mut evidence_spec_revision = None;
    for evidence in &event.evidence {
        let valid = evidence.schema_version == DELIVERY_SCHEMA_VERSION
            && evidence.delivery_id == event.delivery_id
            && evidence.delivery_spec_id == verdict.delivery_spec_id
            && evidence.delivery_spec_revision > 0
            && evidence.candidate_ref == candidate_ref
            && evidence.created_at_millis <= event.produced_at_millis
            && evidence_ids.insert(evidence.id.0.as_str());
        if !valid
            || evidence_spec_revision
                .replace(evidence.delivery_spec_revision)
                .is_some_and(|prior| prior != evidence.delivery_spec_revision)
        {
            return Err(StorageError::invalid_input(
                "durable verdict replay contains foreign, repeated, or inconsistent Evidence",
            ));
        }
    }

    let mut result_ids = HashSet::with_capacity(verdict.criteria.len());
    let mut criterion_ids = HashSet::with_capacity(verdict.criteria.len());
    let mut cited_evidence = HashSet::new();
    for result in &verdict.criteria {
        let valid = result.schema_version == DELIVERY_SCHEMA_VERSION
            && result.delivery_id == event.delivery_id
            && result.delivery_spec_id == verdict.delivery_spec_id
            && result.candidate_ref == candidate_ref
            && result.evaluated_at_millis == event.produced_at_millis
            && result_ids.insert(result.id.0.as_str())
            && criterion_ids.insert(result.criterion_id.0.as_str());
        if !valid
            || (matches!(
                result.verdict,
                CriterionVerdict::Pass | CriterionVerdict::Fail
            ) && result.evidence_refs.is_empty())
        {
            return Err(StorageError::invalid_input(
                "durable verdict replay contains a foreign or repeated criterion result",
            ));
        }
        let mut result_evidence = HashSet::with_capacity(result.evidence_refs.len());
        for evidence_id in &result.evidence_refs {
            if !result_evidence.insert(evidence_id.0.as_str())
                || !evidence_ids.contains(evidence_id.0.as_str())
            {
                return Err(StorageError::invalid_input(
                    "durable verdict replay criterion cites missing or repeated Evidence",
                ));
            }
            cited_evidence.insert(evidence_id.0.as_str());
        }
    }
    if cited_evidence != evidence_ids
        || verdict
            .unresolved_findings
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return Err(StorageError::invalid_input(
            "durable verdict replay does not contain the exact canonical Evidence or finding set",
        ));
    }

    let mut attention_ids = HashSet::with_capacity(event.attention_items.len());
    for item in &event.attention_items {
        if item.schema_version != DELIVERY_SCHEMA_VERSION
            || item.delivery_id != event.delivery_id
            || item.delivery_spec_id != verdict.delivery_spec_id
            || item.stage_run_id.is_none()
            || !item.blocking
            || item.status != AttentionItemStatus::Open
            || item.resolution.is_some()
            || item.resolved_by.is_some()
            || item.resolved_at_millis.is_some()
            || item.created_at_millis != event.produced_at_millis
            || !attention_ids.insert(item.id.0.as_str())
            || item.options.is_empty()
            || item
                .options
                .iter()
                .any(|option| option.schema_version != DELIVERY_SCHEMA_VERSION)
        {
            return Err(StorageError::invalid_input(
                "durable verdict replay contains a non-canonical Attention set",
            ));
        }
    }

    let mut task_ids = HashSet::with_capacity(event.task_statuses.len());
    if event
        .task_statuses
        .iter()
        .any(|task| !task_ids.insert(task.delivery_task_id.0.as_str()))
        || !matches!(
            event.status,
            DeliveryStatus::Verifying
                | DeliveryStatus::NeedsAttention
                | DeliveryStatus::ReadyToDeliver
        )
        || (event.status == DeliveryStatus::NeedsAttention && event.attention_items.is_empty())
        || (event.status == DeliveryStatus::ReadyToDeliver
            && (verdict.status != CriterionVerdict::Pass
                || event
                    .task_statuses
                    .iter()
                    .any(|task| task.status != DeliveryTaskStatus::Completed)))
    {
        return Err(StorageError::invalid_input(
            "durable verdict replay contains a non-canonical task or Delivery status",
        ));
    }
    Ok(())
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
    let submitted = strict_receipt_event(receipt)?;
    let expected = event_from_persisted_transition(source, delivery)?;
    if submitted != expected {
        return Err(StorageError::invalid_input(
            "durable verdict event does not match the committed Delivery facts",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        delivery.id(),
        delivery.revision(),
        DeliveryChangeKind::Advanced,
    )?;
    Ok(())
}

fn strict_receipt_event(
    receipt: &CommitReceipt,
) -> Result<DeliveryVerdictSubmittedEvent, StorageError> {
    let [event, changed] = receipt.events.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable verdict receipt must contain exactly one verdict event and one Delivery change event",
        ));
    };
    if event.topic != VERDICT_SUBMITTED_TOPIC
        || event.projection_cursor.is_some()
        || event.sequence == 0
        || changed.sequence <= event.sequence
    {
        return Err(StorageError::invalid_input(
            "durable verdict receipt event ordering or topic is not canonical",
        ));
    }
    let submitted = strict_verdict_event(&event.payload)?;
    if event.event_id != verdict_event_id(&submitted) {
        return Err(StorageError::invalid_input(
            "durable verdict receipt event id does not match its sealed Verdict",
        ));
    }
    Ok(submitted)
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
            &DeliveryId("dlv_5K2F6D4ZBGXG691EQ8HJXJACA1".into()),
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
