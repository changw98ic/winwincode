// SPDX-License-Identifier: Apache-2.0

//! Publication-owned persistence contract.

use std::{error::Error, fmt};

use winwincode_domain::{PublicationId, RequestId, Sha256Digest};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationStorageErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    RequestReplayMissing,
    AlreadyExists,
    NotFound,
    JournalConflict,
    Adapter,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStorageError {
    kind: PublicationStorageErrorKind,
    message: String,
}

impl PublicationStorageError {
    #[must_use]
    pub fn new(kind: PublicationStorageErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationStorageErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PublicationStorageError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationReceiptIdentity {
    actor_key: Vec<u8>,
    scope_key: Vec<u8>,
    request_id: RequestId,
}

impl PublicationReceiptIdentity {
    /// Builds one opaque request identity from canonical adapter-owned encodings.
    ///
    /// # Errors
    /// Rejects empty actor, scope, or request identities.
    pub fn try_new(
        actor_key: impl Into<Vec<u8>>,
        scope_key: impl Into<Vec<u8>>,
        request_id: RequestId,
    ) -> Result<Self, PublicationStorageError> {
        let value = Self {
            actor_key: actor_key.into(),
            scope_key: scope_key.into(),
            request_id,
        };
        if value.actor_key.is_empty() || value.scope_key.is_empty() || value.request_id.0.is_empty()
        {
            return Err(PublicationStorageError::new(
                PublicationStorageErrorKind::InvalidInput,
                "publication receipt identity is incomplete",
            ));
        }
        Ok(value)
    }

    #[must_use]
    pub fn actor_key(&self) -> &[u8] {
        &self.actor_key
    }

    #[must_use]
    pub fn scope_key(&self) -> &[u8] {
        &self.scope_key
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStorageEvent {
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStoredState {
    pub revision: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationJournalRecord {
    pub sequence: u64,
    pub digest: String,
    pub payload: Vec<u8>,
}

impl PublicationJournalRecord {
    #[must_use]
    pub fn new(sequence: u64, digest: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            sequence,
            digest: digest.into(),
            payload: payload.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStoredJournal {
    pub manifest: Vec<u8>,
    pub records: Vec<PublicationJournalRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationJournalMutation {
    Create {
        manifest: Vec<u8>,
        first_record: PublicationJournalRecord,
    },
    Append {
        expected_tail_sequence: u64,
        expected_tail_digest: String,
        record: PublicationJournalRecord,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStorageCommit {
    receipt_identity: PublicationReceiptIdentity,
    command_digest: Sha256Digest,
    publication_id: PublicationId,
    expected_revision: u64,
    state: Vec<u8>,
    event: PublicationStorageEvent,
    journal: PublicationJournalMutation,
}

impl PublicationStorageCommit {
    /// Builds the one atomic state, receipt, outbox, and journal mutation.
    ///
    /// # Errors
    /// Rejects incomplete bytes or a journal sequence inconsistent with the state revision.
    pub fn try_new(
        receipt_identity: PublicationReceiptIdentity,
        command_digest: Sha256Digest,
        publication_id: PublicationId,
        expected_revision: u64,
        state: Vec<u8>,
        event: PublicationStorageEvent,
        journal: PublicationJournalMutation,
    ) -> Result<Self, PublicationStorageError> {
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            PublicationStorageError::new(
                PublicationStorageErrorKind::InvalidInput,
                "publication revision overflow",
            )
        })?;
        let journal_sequence = match &journal {
            PublicationJournalMutation::Create {
                manifest,
                first_record,
            } if expected_revision == 0 && !manifest.is_empty() => first_record.sequence,
            PublicationJournalMutation::Append {
                expected_tail_sequence,
                expected_tail_digest,
                record,
            } if *expected_tail_sequence == expected_revision
                && !expected_tail_digest.is_empty() =>
            {
                record.sequence
            }
            _ => 0,
        };
        if publication_id.0.is_empty()
            || state.is_empty()
            || event.event_id.is_empty()
            || event.topic.is_empty()
            || event.payload.is_empty()
            || event.payload != state
            || journal_sequence != next_revision
        {
            return Err(PublicationStorageError::new(
                PublicationStorageErrorKind::InvalidInput,
                "publication storage commit is incomplete or inconsistent",
            ));
        }
        Ok(Self {
            receipt_identity,
            command_digest,
            publication_id,
            expected_revision,
            state,
            event,
            journal,
        })
    }

    #[must_use]
    pub const fn receipt_identity(&self) -> &PublicationReceiptIdentity {
        &self.receipt_identity
    }

    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    #[must_use]
    pub fn state(&self) -> &[u8] {
        &self.state
    }

    #[must_use]
    pub const fn event(&self) -> &PublicationStorageEvent {
        &self.event
    }

    #[must_use]
    pub const fn journal(&self) -> &PublicationJournalMutation {
        &self.journal
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationStorageReceipt {
    pub publication_id: PublicationId,
    pub revision: u64,
    pub events: Vec<PublicationStorageEvent>,
}

/// Atomic durable operations required by the Publication coordinator.
pub trait PublicationStorage {
    /// Returns the exact result retained for one request identity and digest.
    ///
    /// # Errors
    /// Returns a typed adapter, validation, or receipt conflict failure.
    fn load_receipt(
        &self,
        identity: &PublicationReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<PublicationStorageReceipt>, PublicationStorageError>;

    /// Returns the current encoded Publication state.
    ///
    /// # Errors
    /// Returns a typed adapter or validation failure.
    fn load_state(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredState>, PublicationStorageError>;

    /// Returns the complete append-only Publication journal.
    ///
    /// # Errors
    /// Returns a typed adapter or validation failure.
    fn load_journal(
        &self,
        publication_id: &PublicationId,
    ) -> Result<Option<PublicationStoredJournal>, PublicationStorageError>;

    /// Atomically persists one Publication state, receipt, event, and journal record.
    ///
    /// # Errors
    /// Returns a typed adapter, validation, idempotency, or revision failure.
    fn commit(
        &mut self,
        commit: &PublicationStorageCommit,
    ) -> Result<PublicationStorageReceipt, PublicationStorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_rejects_an_event_that_differs_from_state() {
        let identity = PublicationReceiptIdentity::try_new(
            b"actor".to_vec(),
            b"scope".to_vec(),
            RequestId("request".to_owned()),
        )
        .expect("receipt identity");
        let error = PublicationStorageCommit::try_new(
            identity,
            Sha256Digest("digest".to_owned()),
            PublicationId("publication".to_owned()),
            0,
            vec![1],
            PublicationStorageEvent {
                event_id: "event".to_owned(),
                topic: "topic".to_owned(),
                payload: vec![2],
            },
            PublicationJournalMutation::Create {
                manifest: vec![1],
                first_record: PublicationJournalRecord::new(1, "digest", vec![1]),
            },
        )
        .expect_err("event and state must match");

        assert_eq!(error.kind(), PublicationStorageErrorKind::InvalidInput);
    }
}
