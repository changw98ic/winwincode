// SPDX-License-Identifier: Apache-2.0

//! Receipt-first atomic transaction for an approved Delivery task graph.

use std::time::{SystemTime, UNIX_EPOCH};

use winwincode_api::generated::{CommandEnvelope, CommandName, Scope, WorkItemsCreatePayload};
use winwincode_delivery::store::{
    DeliveryCommand, DeliveryCommandPort, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
    DeliveryStoreError, DeliveryStoreErrorCode,
};
use winwincode_domain::DeliveryId;
use winwincode_storage::{CommitReceipt, ProductStateStorage, ReceiptIdentity, StorageError};

use crate::{
    DeliveryChangeKind, StateChange, command_receipt, delivery_changed_event,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit, validate_delivery_changed_receipt,
};

/// Receipt-first creation of the canonical `WorkItem` graph.
#[allow(
    clippy::too_many_lines,
    reason = "Keep receipt-first WorkItem creation and its atomic journal publication together"
)]
pub(crate) fn execute_create(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
) -> Result<CommitReceipt, StorageError> {
    if command.command != CommandName::WorkitemsCreate {
        return Err(StorageError::invalid_input("wrong command"));
    }
    if !matches!(command.scope, Scope::RepositoryScope(_)) {
        return Err(StorageError::invalid_input(
            "workitems.create requires repository scope",
        ));
    }
    let payload: WorkItemsCreatePayload = serde_json::from_value(command.payload.clone())
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if command.expected_revision != payload.expected_revision {
        return Err(StorageError::invalid_input(
            "command and payload expectedRevision must match",
        ));
    }
    let (receipt_identity, command_digest) = command_receipt(command)?;
    if let Some(receipt) = storage.load_receipt(&receipt_identity, &command_digest)? {
        validate_created_receipt(&receipt, &receipt_identity, &payload.delivery_id, true)?;
        return Ok(receipt);
    }
    let expected_revision = u64::try_from(payload.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("expectedRevision must not be negative"))?;
    let request_digest = command_digest
        .0
        .strip_prefix("sha256:")
        .ok_or_else(|| StorageError::invalid_input("command digest is not canonical"))?
        .to_owned();
    let delivery_id = payload.delivery_id.clone();
    let journal = StagedDeliveryJournal::new(
        delivery_id.clone(),
        storage.load_journal(&delivery_journal_key(&delivery_id)?)?,
    );
    let source = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::Get(delivery_id.clone()))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    let contract = source.snapshot().work_run_aggregate.contract.clone();
    if contract.revision != payload.contract_revision {
        return Err(StorageError::invalid_input("contract revision mismatch"));
    }
    if payload.items.is_empty() {
        return Err(StorageError::invalid_input("items must not be empty"));
    }
    let items = payload
        .items
        .into_iter()
        .map(|item| {
            let state = if item.depends_on.is_empty() {
                winwincode_domain::WorkItemState::Ready
            } else {
                winwincode_domain::WorkItemState::WaitingDependency
            };
            winwincode_domain::WorkItem {
                criterion_ids: item.criterion_ids,
                depends_on: item.depends_on,
                goal: item.goal,
                id: item.id,
                revision: winwincode_domain::Revision(1),
                schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
                state,
                title: item.title,
                work_contract_id: contract.id.clone(),
                work_contract_revision: contract.revision.clone(),
            }
        })
        .collect();
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::invalid_input("system clock before unix epoch"))?
        .as_millis();
    let now_millis = u64::try_from(now_millis)
        .map_err(|_| StorageError::invalid_input("system clock value overflow"))?;
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::CreateWorkItems(Box::new(
            winwincode_delivery::store::CreateDeliveryWorkItems {
                delivery_id: delivery_id.clone(),
                request_id: command.request_id.clone(),
                request_digest,
                expected_revision,
                contract_revision: payload.contract_revision.0,
                items,
                now_millis,
            },
        )))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "WorkItem creation replay is missing its scoped command receipt",
        ));
    }
    let stream_id = delivery_stream_id(&delivery_id);
    let changed = delivery_changed_event(
        command,
        &delivery_id,
        mutation.snapshot.revision(),
        DeliveryChangeKind::Advanced,
        crate::instant_from_millis(mutation.snapshot.snapshot().updated_at_millis)?,
        "delivery-workitems-create",
    )?;
    let mut commit = storage_commit(
        command,
        StateChange::new(
            &stream_id,
            mutation.snapshot.encode_json().map_err(storage_error)?,
            vec![changed],
        ),
    )?;
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
        .ok_or_else(|| StorageError::invalid_input("missing journal publication"))?;
    commit = commit.with_journal_publication(publication);
    let receipt = storage.commit(&commit)?;
    validate_created_receipt(
        &receipt,
        &receipt_identity,
        &delivery_id,
        receipt.idempotent_replay,
    )?;
    Ok(receipt)
}

fn validate_created_receipt(
    receipt: &CommitReceipt,
    identity: &ReceiptIdentity,
    delivery_id: &DeliveryId,
    replayed: bool,
) -> Result<(), StorageError> {
    if &receipt.receipt_identity != identity
        || receipt.idempotent_replay != replayed
        || receipt.stream_id != delivery_stream_id(delivery_id)
        || receipt.events.len() != 1
    {
        return Err(StorageError::invalid_input(
            "WorkItem creation receipt does not match its scoped request",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        delivery_id,
        receipt.revision,
        DeliveryChangeKind::Advanced,
    )
}

fn delivery_store_error(
    error: &DeliveryStoreError,
    request_id: &winwincode_domain::RequestId,
) -> StorageError {
    match error.code() {
        DeliveryStoreErrorCode::RevisionConflict => {
            if let (Some(expected), Some(current)) =
                (error.expected_revision(), error.current_revision())
            {
                return StorageError::revision_conflict(expected, current);
            }
        }
        DeliveryStoreErrorCode::RequestConflict => {
            return StorageError::request_conflict(request_id);
        }
        DeliveryStoreErrorCode::InvalidStoreOptions
        | DeliveryStoreErrorCode::DeliveryAlreadyExists
        | DeliveryStoreErrorCode::DeliveryNotFound
        | DeliveryStoreErrorCode::StoreCorrupt
        | DeliveryStoreErrorCode::DeliveryIdMismatch
        | DeliveryStoreErrorCode::StoreIoError
        | DeliveryStoreErrorCode::ReviewSetStale => {}
    }
    StorageError::invalid_input(error.to_string())
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
