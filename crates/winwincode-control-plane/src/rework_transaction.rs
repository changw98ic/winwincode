// SPDX-License-Identifier: Apache-2.0

//! Specialized atomic transaction for a bounded rework clarification.

use winwincode_api::generated::{CommandEnvelope, CommandName, DeliveryAdvancePayload};
use winwincode_delivery::{
    application::stage::{DeliveryReworkClarifiedEvent, StageAdvanceEffect, StageAdvanceResult},
    store::{ClarifyDeliveryRework, DeliveryCommand, DeliveryCommandPort, DeliveryStore},
};
use winwincode_storage::{CommitReceipt, NewOutboxEvent, ProductStateStorage, StorageError};

use crate::{
    StateChange,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit,
};

const REWORK_CLARIFIED_TOPIC: &str = "delivery.rework.clarified";

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    transition: &StageAdvanceResult,
) -> Result<CommitReceipt, StorageError> {
    let expected_revision = validate_command(command, transition)?;
    let delivery_id = transition.delivery.id().clone();
    let journal_key = delivery_journal_key(&delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);
    let stream_id = delivery_stream_id(&delivery_id);
    let event = clarification_event(transition)?;
    let event_id = clarification_event_id(&event);
    let mut commit = storage_commit(
        command,
        StateChange::new(
            &stream_id,
            transition.delivery.encode_json().map_err(storage_error)?,
            vec![NewOutboxEvent::new(
                &event_id,
                REWORK_CLARIFIED_TOPIC,
                serde_json::to_vec(&event).map_err(storage_error)?,
            )],
        ),
    )?;
    let request_digest = commit
        .command_digest
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("command digest is not canonical"))?
        .to_owned();
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::ClarifyRework(Box::new(
            ClarifyDeliveryRework {
                request_id: command.request_id.clone(),
                request_digest,
                expected_revision,
                transition: transition.clone(),
            },
        )))
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    commit.state = mutation.snapshot.encode_json().map_err(storage_error)?;
    if mutation.replayed {
        commit = commit.require_receipt_replay();
    }
    if let Some(publication) = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
    {
        if mutation.replayed {
            return Err(StorageError::invalid_input(
                "replayed rework clarification unexpectedly staged another Delivery journal record",
            ));
        }
        commit = commit.with_journal_publication(publication);
    } else if !mutation.replayed {
        return Err(StorageError::invalid_input(
            "new rework clarification did not stage a Delivery journal publication",
        ));
    }
    let receipt = storage.commit(&commit)?;
    if receipt.stream_id != stream_id || receipt.revision != mutation.snapshot.revision() {
        return Err(StorageError::invalid_input(
            "durable rework clarification receipt does not match its Delivery revision",
        ));
    }
    let [stored_event] = receipt.events.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable rework clarification receipt must contain exactly one event",
        ));
    };
    let stored: DeliveryReworkClarifiedEvent =
        serde_json::from_slice(&stored_event.payload).map_err(storage_error)?;
    if stored_event.topic != REWORK_CLARIFIED_TOPIC
        || stored_event.event_id != event_id
        || stored != event
        || serde_json::to_vec(&stored).map_err(storage_error)? != stored_event.payload
    {
        return Err(StorageError::invalid_input(
            "durable rework clarification event does not match the sealed transition",
        ));
    }
    Ok(receipt)
}

fn clarification_event(
    transition: &StageAdvanceResult,
) -> Result<DeliveryReworkClarifiedEvent, StorageError> {
    let StageAdvanceEffect::Clarify(reason) = transition.effect else {
        return Err(StorageError::invalid_input(
            "rework clarification transition has no clarification effect",
        ));
    };
    Ok(DeliveryReworkClarifiedEvent {
        schema_version: winwincode_delivery::domain::DELIVERY_SCHEMA_VERSION,
        delivery_id: transition.delivery.id().clone(),
        delivery_revision: transition.delivery.revision(),
        reason,
        occurred_at_millis: transition.delivery.snapshot().updated_at_millis,
    })
}

fn clarification_event_id(event: &DeliveryReworkClarifiedEvent) -> String {
    format!(
        "delivery-rework-clarified:{}:{}",
        event.delivery_id.0, event.delivery_revision
    )
}

fn validate_command(
    command: &CommandEnvelope,
    transition: &StageAdvanceResult,
) -> Result<u64, StorageError> {
    if command.command != CommandName::DeliveryAdvance {
        return Err(StorageError::invalid_input(
            "rework clarification transaction requires delivery.advance",
        ));
    }
    let payload: DeliveryAdvancePayload =
        serde_json::from_value(command.payload.clone()).map_err(|error| {
            StorageError::invalid_input(format!(
                "delivery.advance payload is not canonical: {error}"
            ))
        })?;
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        StorageError::invalid_input("Delivery expectedRevision must not be negative")
    })?;
    if serde_json::to_value(&payload).map_err(storage_error)? != command.payload
        || payload.delivery_id != *transition.delivery.id()
        || transition.delivery.revision() != expected_revision.saturating_add(1)
        || !matches!(transition.effect, StageAdvanceEffect::Clarify(_))
    {
        return Err(StorageError::invalid_input(
            "delivery.advance does not match the sealed rework clarification",
        ));
    }
    Ok(expected_revision)
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
