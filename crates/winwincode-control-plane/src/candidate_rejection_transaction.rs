// SPDX-License-Identifier: Apache-2.0

//! Atomic CP refusal of a candidate after the immutable Worker terminal.

use sha2::{Digest, Sha256};
use winwincode_delivery::{
    application::candidate_rejection::{CandidateRejectedTransition, CandidateRejectionFact},
    domain::Delivery,
    store::{DeliveryCommand, DeliveryCommandPort, DeliveryStore, RejectDeliveryCandidate},
};
use winwincode_domain::{RepositoryScope, RequestId, Sha256Digest};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventSource,
    ReceiptIdentity, StateCommit, StorageError,
};

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    scope: &RepositoryScope,
    actor: &PublicEventActor,
    current: &Delivery,
    fact: CandidateRejectionFact,
) -> Result<CommitReceipt, StorageError> {
    let attention_id = fact.attention_id();
    let request_id = RequestId(format!("req_{}", &attention_id.0[4..]));
    let payload = serde_json::to_vec(&fact).map_err(storage_error)?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload)));
    let identity = ReceiptIdentity::new(
        winwincode_storage::receipt_actor_key(actor)?,
        crate::repository_scope_key(scope)?,
        request_id.clone(),
    )?;
    if let Some(receipt) = storage.load_receipt(&identity, &digest)? {
        if receipt.stream_id != crate::delivery_transaction::delivery_stream_id(current.id()) {
            return Err(StorageError::invalid_input(
                "candidate refusal receipt is foreign",
            ));
        }
        return Ok(receipt);
    }
    let transition = CandidateRejectedTransition::new(current, fact).map_err(storage_error)?;
    let key = crate::delivery_transaction::delivery_journal_key(current.id())?;
    let journal = crate::delivery_transaction::StagedDeliveryJournal::new(
        current.id().clone(),
        storage.load_journal(&key)?,
    );
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::RejectCandidate(Box::new(
            RejectDeliveryCandidate {
                request_id: request_id.clone(),
                request_digest: digest.0[7..].into(),
                expected_revision: current.revision(),
                transition,
            },
        )))
        .map_err(storage_error)?;
    let publication = journal
        .into_publication()
        .map_err(storage_error)?
        .ok_or_else(|| {
            StorageError::invalid_input("candidate refusal has no journal publication")
        })?;
    let changed = crate::delivery_changed_event_for_scope(
        crate::public_repository_scope(scope),
        current.id(),
        mutation.snapshot.revision(),
        crate::DeliveryChangeKind::Advanced,
        crate::instant_from_millis(mutation.snapshot.snapshot().updated_at_millis)?,
        PublicEventSource::ControlPlane {
            actor: actor.clone(),
            component: "delivery-candidate-acceptance".into(),
        },
    )?;
    storage.commit(
        &StateCommit::new(
            identity,
            digest,
            crate::delivery_transaction::delivery_stream_id(current.id()),
            current.revision(),
            mutation.snapshot.encode_json().map_err(storage_error)?,
            vec![
                NewOutboxEvent::internal(
                    format!("candidate-rejection:{}", request_id.0),
                    "delivery.candidate.rejected",
                    payload,
                ),
                changed,
            ],
        )
        .with_journal_publication(publication),
    )
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
