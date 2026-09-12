// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{PublicationId, Sha256Digest};
use winwincode_publication::{
    PublicationError, PublicationJournalMutation, PublicationJournalRecord,
    PublicationReceiptIdentity, PublicationStorage, PublicationStorageCommit,
    PublicationStorageError, PublicationStorageErrorKind, PublicationStorageEvent,
    PublicationStorageReceipt, PublicationStoredJournal, PublicationStoredState,
};

use crate::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, CommitReceipt,
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    StateCommit, StorageError, StorageErrorKind,
};

const PUBLICATION_AGGREGATE_TYPE: &str = "publication";

pub struct PublicationStorageAdapter<'storage> {
    storage: &'storage mut dyn ProductStateStorage,
}

impl<'storage> PublicationStorageAdapter<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }
}

pub struct PublicationReadStorageAdapter<'storage> {
    storage: &'storage dyn ProductStateStorage,
}

impl<'storage> PublicationReadStorageAdapter<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage dyn ProductStateStorage) -> Self {
        Self { storage }
    }
}

/// Copies an already canonical product-state receipt into the Publication port.
///
/// # Errors
/// Rejects an incomplete receipt identity.
pub fn publication_receipt_identity(
    identity: &ReceiptIdentity,
) -> Result<PublicationReceiptIdentity, PublicationStorageError> {
    PublicationReceiptIdentity::try_new(
        identity.actor_key().as_bytes().to_vec(),
        identity.scope_key().as_bytes().to_vec(),
        identity.request_id().clone(),
    )
}

#[must_use]
pub fn publication_error(error: &StorageError) -> PublicationError {
    PublicationError::from(storage_error(error))
}

fn storage_identity(
    identity: &PublicationReceiptIdentity,
) -> Result<ReceiptIdentity, PublicationStorageError> {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(identity.actor_key().to_vec())
            .map_err(|error| storage_error(&error))?,
        ReceiptScopeKey::from_encoded(identity.scope_key().to_vec())
            .map_err(|error| storage_error(&error))?,
        identity.request_id().clone(),
    )
    .map_err(|error| storage_error(&error))
}

fn stream_id(publication_id: &PublicationId) -> String {
    format!("publication:{}", publication_id.0)
}

fn journal_key(
    publication_id: &PublicationId,
) -> Result<AggregateJournalKey, PublicationStorageError> {
    AggregateJournalKey::new(PUBLICATION_AGGREGATE_TYPE, publication_id.0.clone())
        .map_err(|error| storage_error(&error))
}

fn storage_error(error: &StorageError) -> PublicationStorageError {
    let kind = match error.kind() {
        StorageErrorKind::InvalidInput => PublicationStorageErrorKind::InvalidInput,
        StorageErrorKind::RevisionConflict => PublicationStorageErrorKind::RevisionConflict,
        StorageErrorKind::RequestConflict => PublicationStorageErrorKind::RequestConflict,
        StorageErrorKind::RequestReplayMissing => PublicationStorageErrorKind::RequestReplayMissing,
        StorageErrorKind::JournalAlreadyExists => PublicationStorageErrorKind::AlreadyExists,
        StorageErrorKind::JournalNotFound => PublicationStorageErrorKind::NotFound,
        StorageErrorKind::JournalConflict => PublicationStorageErrorKind::JournalConflict,
        StorageErrorKind::EventCursorExpired | StorageErrorKind::Adapter => {
            PublicationStorageErrorKind::Adapter
        }
        StorageErrorKind::Closed => PublicationStorageErrorKind::Closed,
    };
    PublicationStorageError::new(kind, error.to_string())
}

fn publication_receipt(
    receipt: CommitReceipt,
    expected_id: &PublicationId,
) -> Result<PublicationStorageReceipt, PublicationStorageError> {
    if receipt.stream_id != stream_id(expected_id) {
        return Err(PublicationStorageError::new(
            PublicationStorageErrorKind::Adapter,
            "publication receipt stream differs from its requested identity",
        ));
    }
    Ok(PublicationStorageReceipt {
        publication_id: expected_id.clone(),
        revision: receipt.revision,
        events: receipt
            .events
            .into_iter()
            .map(|event| PublicationStorageEvent {
                event_id: event.event_id,
                topic: event.topic,
                payload: event.payload,
            })
            .collect(),
    })
}

fn load_receipt(
    storage: &dyn ProductStateStorage,
    identity: &PublicationReceiptIdentity,
    digest: &Sha256Digest,
) -> Result<Option<PublicationStorageReceipt>, PublicationStorageError> {
    let identity = storage_identity(identity)?;
    storage
        .load_receipt(&identity, digest)
        .map_err(|error| storage_error(&error))?
        .map(|receipt| {
            let publication_id = receipt
                .stream_id
                .strip_prefix("publication:")
                .filter(|value| !value.is_empty() && !value.contains(':'))
                .map(|value| PublicationId(value.to_owned()))
                .ok_or_else(|| {
                    PublicationStorageError::new(
                        PublicationStorageErrorKind::Adapter,
                        "publication receipt stream identity is invalid",
                    )
                })?;
            publication_receipt(receipt, &publication_id)
        })
        .transpose()
}

fn load_state(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
) -> Result<Option<PublicationStoredState>, PublicationStorageError> {
    let expected_stream = stream_id(publication_id);
    storage
        .load_state(&expected_stream)
        .map_err(|error| storage_error(&error))?
        .map(|state| {
            if state.stream_id != expected_stream {
                return Err(PublicationStorageError::new(
                    PublicationStorageErrorKind::Adapter,
                    "publication state stream identity is invalid",
                ));
            }
            Ok(PublicationStoredState {
                revision: state.revision,
                payload: state.payload,
            })
        })
        .transpose()
}

fn load_journal(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
) -> Result<Option<PublicationStoredJournal>, PublicationStorageError> {
    storage
        .load_journal(&journal_key(publication_id)?)
        .map_err(|error| storage_error(&error))
        .map(|journal| {
            journal.map(|journal| PublicationStoredJournal {
                manifest: journal.manifest,
                records: journal
                    .records
                    .into_iter()
                    .map(|record| PublicationJournalRecord {
                        sequence: record.sequence,
                        digest: record.digest,
                        payload: record.payload,
                    })
                    .collect(),
            })
        })
}

impl PublicationStorage for PublicationReadStorageAdapter<'_> {
    fn load_receipt(
        &self,
        identity: &PublicationReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<PublicationStorageReceipt>, PublicationStorageError> {
        load_receipt(self.storage, identity, digest)
    }

    fn load_state(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredState>, PublicationStorageError> {
        load_state(self.storage, publication_id)
    }

    fn load_journal(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredJournal>, PublicationStorageError> {
        load_journal(self.storage, publication_id)
    }

    fn commit(
        &mut self,
        _commit: &PublicationStorageCommit,
    ) -> Result<PublicationStorageReceipt, PublicationStorageError> {
        Err(PublicationStorageError::new(
            PublicationStorageErrorKind::Closed,
            "read-only publication storage cannot commit",
        ))
    }
}

impl PublicationStorage for PublicationStorageAdapter<'_> {
    fn load_receipt(
        &self,
        identity: &PublicationReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<PublicationStorageReceipt>, PublicationStorageError> {
        load_receipt(self.storage, identity, digest)
    }

    fn load_state(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredState>, PublicationStorageError> {
        load_state(self.storage, publication_id)
    }

    fn load_journal(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredJournal>, PublicationStorageError> {
        load_journal(self.storage, publication_id)
    }

    fn commit(
        &mut self,
        commit: &PublicationStorageCommit,
    ) -> Result<PublicationStorageReceipt, PublicationStorageError> {
        let journal = match commit.journal() {
            PublicationJournalMutation::Create {
                manifest,
                first_record,
            } => AggregateJournalPublication::Create {
                key: journal_key(commit.publication_id())?,
                manifest: manifest.clone(),
                first_record: AggregateJournalRecord::new(
                    first_record.sequence,
                    first_record.digest.clone(),
                    first_record.payload.clone(),
                ),
            },
            PublicationJournalMutation::Append {
                expected_tail_sequence,
                expected_tail_digest,
                record,
            } => AggregateJournalPublication::Append {
                key: journal_key(commit.publication_id())?,
                expected_tail_sequence: *expected_tail_sequence,
                expected_tail_digest: expected_tail_digest.clone(),
                record: AggregateJournalRecord::new(
                    record.sequence,
                    record.digest.clone(),
                    record.payload.clone(),
                ),
            },
        };
        let event = commit.event();
        let state = StateCommit::new(
            storage_identity(commit.receipt_identity())?,
            commit.command_digest().clone(),
            stream_id(commit.publication_id()),
            commit.expected_revision(),
            commit.state().to_vec(),
            vec![NewOutboxEvent::internal(
                event.event_id.clone(),
                event.topic.clone(),
                event.payload.clone(),
            )],
        )
        .with_journal_publication(journal);
        let receipt = self
            .storage
            .commit(&state)
            .map_err(|error| storage_error(&error))?;
        publication_receipt(receipt, commit.publication_id())
    }
}
