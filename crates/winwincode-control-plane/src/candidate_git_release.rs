// SPDX-License-Identifier: Apache-2.0

//! Durable Delivery authority for candidate Git reference release.
//!
//! A candidate reference is released only after two independent durable facts
//! exist in the Control Plane state store: the terminal Delivery mutation and
//! a read-closure mutation.  The public receipt below is constructed from
//! those stored facts; callers cannot manufacture a release authority by
//! supplying receipt digests alone.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_delivery::domain::{Delivery, DeliveryStatus, StageRunStatus};
use winwincode_domain::{DeliveryId, PublicationId, RequestId, Sha256Digest};
use winwincode_publication::{PublicationReadLedger, PublicationState};
use winwincode_storage::{
    CandidateGitPinReceipt, CandidateGitReleaseAuthority, CandidateGitTerminalOutcome,
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StateRevisionGuard, StorageError,
};

use crate::candidate_source::CandidateResolutionError;
use crate::delivery_transaction::delivery_stream_id;

const SCHEMA_VERSION: u8 = 1;
const READS_CLOSED_STREAM_PREFIX: &str = "delivery-candidate-reads-closed:";
const REQUEST_DOMAIN: &[u8] = b"winwincode.delivery-candidate-reads-closed-request.v1";
const COMMIT_DOMAIN: &[u8] = b"winwincode.delivery-candidate-reads-closed-commit.v1";
const PUBLICATION_STREAM_PREFIX: &str = "publication:";
const MAX_PUBLICATION_STREAMS: usize = 100_000;
const MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Durable receipt proving that a Delivery reached its terminal state and all
/// candidate readers have been closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateGitReadsClosedReceipt {
    delivery_id: DeliveryId,
    terminal_outcome: CandidateGitTerminalOutcome,
    terminal_receipt_digest: Sha256Digest,
    reads_closed_receipt_digest: Sha256Digest,
    delivery_revision: u64,
    terminal_receipt_identity: ReceiptIdentity,
    reads_closed_receipt_identity: ReceiptIdentity,
}

impl CandidateGitReadsClosedReceipt {
    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn terminal_outcome(&self) -> CandidateGitTerminalOutcome {
        self.terminal_outcome
    }

    #[must_use]
    pub const fn terminal_receipt_digest(&self) -> &Sha256Digest {
        &self.terminal_receipt_digest
    }

    #[must_use]
    pub const fn reads_closed_receipt_digest(&self) -> &Sha256Digest {
        &self.reads_closed_receipt_digest
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    pub(crate) fn release_authority(
        &self,
    ) -> Result<CandidateGitReleaseAuthority, CandidateResolutionError> {
        CandidateGitReleaseAuthority::delivery_final_without_future_reads(
            self.delivery_id.clone(),
            self.terminal_outcome,
            self.terminal_receipt_digest.clone(),
            self.reads_closed_receipt_digest.clone(),
        )
        .map_err(CandidateResolutionError::GitRetention)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReceiptIdentity {
    actor_key: Vec<u8>,
    scope_key: Vec<u8>,
    request_id: RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReadsClosed {
    schema_version: u8,
    delivery_id: DeliveryId,
    terminal_outcome: CandidateGitTerminalOutcome,
    terminal_receipt_digest: Sha256Digest,
    terminal_receipt_identity: StoredReceiptIdentity,
    reads_closed_receipt_digest: Sha256Digest,
    reads_closed_request_id: RequestId,
    delivery_revision: u64,
}

/// Commits or exactly replays the read-closure fact for one terminal Delivery.
///
/// The terminal receipt is checked against the canonical Delivery state and
/// the storage receipt table before the closure state is written.  The
/// resulting state and receipt are therefore the only release authority that
/// the Control Plane exposes.
pub(crate) fn commit(
    storage: &mut dyn ProductStateStorage,
    delivery_id: &DeliveryId,
    terminal_receipt: &CommitReceipt,
    terminal_outcome: CandidateGitTerminalOutcome,
    reader_guards: &[StateRevisionGuard],
) -> Result<CandidateGitReadsClosedReceipt, CandidateResolutionError> {
    let terminal_identity =
        verify_terminal_receipt(storage, delivery_id, terminal_receipt, terminal_outcome)?;
    let stored = StoredReadsClosed {
        schema_version: SCHEMA_VERSION,
        delivery_id: delivery_id.clone(),
        terminal_outcome,
        terminal_receipt_digest: terminal_receipt.command_digest.clone(),
        terminal_receipt_identity: StoredReceiptIdentity::from_identity(&terminal_identity),
        reads_closed_receipt_digest: Sha256Digest("sha256:".to_owned() + &"0".repeat(64)),
        reads_closed_request_id: RequestId("req_".to_owned() + &"0".repeat(26)),
        delivery_revision: terminal_receipt.revision,
    };
    let (payload_without_digest, reads_closed_request_id, reads_closed_digest) =
        canonical_closure_payload(stored)?;
    let payload = payload_without_digest;
    let stream_id = reads_closed_stream_id(delivery_id);
    let existing = storage.load_state(&stream_id)?;
    if let Some(existing) = existing {
        if existing.revision != 1 || existing.payload != payload {
            return Err(StorageError::invalid_input(
                "candidate read-closure state conflicts with its durable terminal receipt",
            )
            .into());
        }
        let receipt_identity =
            receipt_identity_for_closure(&terminal_identity, &reads_closed_request_id)?;
        let receipt = storage
            .load_receipt(&receipt_identity, &reads_closed_digest)?
            .ok_or_else(|| {
                StorageError::invalid_input(
                    "candidate read-closure state has no matching durable receipt",
                )
            })?;
        return receipt_from_stored(
            delivery_id,
            terminal_outcome,
            terminal_receipt,
            &terminal_identity,
            &reads_closed_digest,
            receipt_identity,
            &receipt,
        );
    }
    let receipt_identity =
        receipt_identity_for_closure(&terminal_identity, &reads_closed_request_id)?;
    let event_id = format!("candidate-git-reads-closed:{}", delivery_id.0);
    let event_payload = payload.clone();
    let mut commit = StateCommit::new(
        receipt_identity.clone(),
        reads_closed_digest.clone(),
        stream_id,
        0,
        payload,
        vec![NewOutboxEvent::internal(
            event_id,
            "delivery.candidate.git-reads-closed.v1",
            event_payload,
        )],
    );
    for guard in reader_guards {
        commit = commit.with_state_guard(guard.clone());
    }
    let receipt = storage.commit(&commit)?;
    if receipt.stream_id != commit.stream_id || receipt.revision != 1 {
        return Err(StorageError::adapter(
            "candidate read-closure receipt has an unexpected state revision",
        )
        .into());
    }
    receipt_from_stored(
        delivery_id,
        terminal_outcome,
        terminal_receipt,
        &terminal_identity,
        &reads_closed_digest,
        receipt_identity,
        &receipt,
    )
}

/// Verifies that every durable Publication reader for a Delivery is terminal.
///
/// Publication state is a separate aggregate from Delivery. A terminal
/// Delivery therefore does not by itself close a Publication read: a pending
/// or publishing Publication may still need the candidate's exact Git
/// objects. The production finalizer calls this check before minting the
/// read-closure authority. A Delivery with a configured publication target but
/// no Publication intent yet is deferred rather than released: the subsequent
/// Publication command still needs the candidate's stable Git reference.
/// Malformed or unavailable directory reads fail closed.
pub(crate) fn ensure_publication_readers_closed(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
) -> Result<Option<Vec<StateRevisionGuard>>, CandidateResolutionError> {
    let delivery_state = storage
        .load_state(&delivery_stream_id(delivery_id))?
        .ok_or_else(|| StorageError::invalid_input("terminal Delivery state is missing"))?;
    let delivery = Delivery::decode_json(&delivery_state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if delivery.id() != delivery_id || delivery.revision() != delivery_state.revision {
        return Err(StorageError::invalid_input(
            "terminal Delivery state identity is inconsistent",
        )
        .into());
    }
    let states = storage
        .load_bounded_state_directory(
            PUBLICATION_STREAM_PREFIX,
            MAX_PUBLICATION_STREAMS,
            MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES,
        )
        .map_err(CandidateResolutionError::Storage)?;
    let ledger = PublicationReadLedger::new(storage);
    let mut guards = Vec::new();
    let mut matching_publications = 0_usize;
    for state in states {
        let publication_id = state
            .stream_id
            .strip_prefix(PUBLICATION_STREAM_PREFIX)
            .filter(|value| !value.is_empty() && !value.contains(':'))
            .map(|value| PublicationId(value.to_owned()))
            .ok_or_else(|| {
                StorageError::invalid_input("Publication reader stream identity is invalid")
            })?;
        let publication = ledger.get(&publication_id).map_err(|error| {
            StorageError::invalid_input(format!("Publication reader state is invalid: {error}"))
        })?;
        if publication.revision() != state.revision {
            return Err(StorageError::invalid_input(
                "Publication reader state revision differs from its journal",
            )
            .into());
        }
        if publication.binding().delivery_id() != delivery_id {
            continue;
        }
        matching_publications = matching_publications.saturating_add(1);
        guards.push(
            StateRevisionGuard::new(state.stream_id.clone(), state.revision)
                .map_err(CandidateResolutionError::Storage)?,
        );
        if matches!(
            publication.state(),
            PublicationState::Pending | PublicationState::Publishing
        ) {
            return Err(StorageError::invalid_input(
                "candidate read-closure requires every Publication reader to be terminal",
            )
            .into());
        }
    }
    if delivery.snapshot().spec.publication_target.is_some() && matching_publications == 0 {
        return Ok(None);
    }
    Ok(Some(guards))
}

/// Validates that an externally supplied low-level release authority is backed
/// by the exact durable terminal/read-closure state.
pub(crate) fn validate_release_authority(
    storage: &dyn ProductStateStorage,
    pin: &CandidateGitPinReceipt,
    authority: &CandidateGitReleaseAuthority,
) -> Result<(), CandidateResolutionError> {
    if pin.delivery_id() != authority.delivery_id() {
        return Err(StorageError::invalid_input(
            "candidate release Delivery differs from the pinned Artifact authority",
        )
        .into());
    }
    let stream_id = reads_closed_stream_id(authority.delivery_id());
    let state = storage.load_state(&stream_id)?.ok_or_else(|| {
        StorageError::invalid_input("candidate read-closure authority is missing")
    })?;
    if state.revision != 1 {
        return Err(StorageError::invalid_input(
            "candidate read-closure authority revision is invalid",
        )
        .into());
    }
    let stored = decode_stored(&state.payload)?;
    let terminal_identity = stored.terminal_receipt_identity.to_identity()?;
    if stored.delivery_id != *authority.delivery_id()
        || stored.terminal_outcome != authority.terminal_outcome()
        || stored.terminal_receipt_digest != *authority.terminal_receipt_digest()
        || stored.reads_closed_receipt_digest != *authority.reads_closed_receipt_digest()
    {
        return Err(StorageError::invalid_input(
            "candidate release authority differs from durable read-closure state",
        )
        .into());
    }
    let delivery_state = storage
        .load_state(&delivery_stream_id(authority.delivery_id()))?
        .ok_or_else(|| StorageError::invalid_input("terminal Delivery state is missing"))?;
    if delivery_state.revision != stored.delivery_revision {
        return Err(StorageError::invalid_input(
            "terminal Delivery revision differs from read-closure authority",
        )
        .into());
    }
    let delivery = Delivery::decode_json(&delivery_state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if delivery.id() != authority.delivery_id()
        || delivery.revision() != delivery_state.revision
        || (authority.terminal_outcome() == CandidateGitTerminalOutcome::Delivered
            && delivery.snapshot().status != DeliveryStatus::Delivered)
    {
        return Err(StorageError::invalid_input(
            "candidate release authority is not bound to a terminal Delivery",
        )
        .into());
    }
    if delivery.snapshot().stage_runs.iter().any(|run| {
        matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    }) {
        return Err(StorageError::invalid_input(
            "candidate read-closure requires every Delivery StageRun to be terminal",
        )
        .into());
    }
    let terminal_receipt = storage
        .load_receipt(&terminal_identity, authority.terminal_receipt_digest())?
        .ok_or_else(|| StorageError::invalid_input("terminal Delivery receipt is missing"))?;
    if terminal_receipt.stream_id != delivery_stream_id(authority.delivery_id())
        || terminal_receipt.revision != stored.delivery_revision
    {
        return Err(StorageError::invalid_input(
            "terminal receipt is not the durable Delivery terminal mutation",
        )
        .into());
    }
    crate::validate_delivery_changed_receipt(
        &terminal_receipt,
        authority.delivery_id(),
        delivery_state.revision,
        crate::DeliveryChangeKind::Advanced,
    )?;
    let reads_closed_identity =
        receipt_identity_for_closure(&terminal_identity, &stored.reads_closed_request_id)?;
    let reads_closed_receipt = storage
        .load_receipt(
            &reads_closed_identity,
            authority.reads_closed_receipt_digest(),
        )?
        .ok_or_else(|| StorageError::invalid_input("read-closure receipt is missing"))?;
    if reads_closed_receipt.stream_id != stream_id || reads_closed_receipt.revision != 1 {
        return Err(StorageError::invalid_input(
            "read-closure receipt is not the durable closure mutation",
        )
        .into());
    }
    Ok(())
}

fn verify_terminal_receipt(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
    terminal_receipt: &CommitReceipt,
    terminal_outcome: CandidateGitTerminalOutcome,
) -> Result<ReceiptIdentity, CandidateResolutionError> {
    let stream_id = delivery_stream_id(delivery_id);
    if terminal_receipt.stream_id != stream_id {
        return Err(StorageError::invalid_input(
            "candidate release terminal receipt belongs to another stream",
        )
        .into());
    }
    let durable = storage
        .load_receipt(
            &terminal_receipt.receipt_identity,
            &terminal_receipt.command_digest,
        )?
        .ok_or_else(|| StorageError::invalid_input("candidate terminal receipt is not durable"))?;
    if durable.stream_id != terminal_receipt.stream_id
        || durable.revision != terminal_receipt.revision
        || durable.command_digest != terminal_receipt.command_digest
    {
        return Err(StorageError::invalid_input(
            "candidate terminal receipt differs from durable storage",
        )
        .into());
    }
    let state = storage
        .load_state(&stream_id)?
        .ok_or_else(|| StorageError::invalid_input("terminal Delivery state is missing"))?;
    if state.revision != terminal_receipt.revision {
        return Err(StorageError::invalid_input(
            "terminal receipt does not identify the current Delivery revision",
        )
        .into());
    }
    let delivery = Delivery::decode_json(&state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if delivery.id() != delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "terminal Delivery state identity is inconsistent",
        )
        .into());
    }
    crate::validate_delivery_changed_receipt(
        terminal_receipt,
        delivery_id,
        state.revision,
        crate::DeliveryChangeKind::Advanced,
    )?;
    if terminal_outcome == CandidateGitTerminalOutcome::Delivered
        && delivery.snapshot().status != DeliveryStatus::Delivered
    {
        return Err(StorageError::invalid_input(
            "Delivered candidate release requires a durable Delivered Delivery",
        )
        .into());
    }
    if delivery.snapshot().stage_runs.iter().any(|run| {
        matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    }) {
        return Err(StorageError::invalid_input(
            "candidate read-closure requires every Delivery StageRun to be terminal",
        )
        .into());
    }
    Ok(terminal_receipt.receipt_identity.clone())
}

fn receipt_from_stored(
    delivery_id: &DeliveryId,
    terminal_outcome: CandidateGitTerminalOutcome,
    terminal_receipt: &CommitReceipt,
    terminal_identity: &ReceiptIdentity,
    reads_closed_digest: &Sha256Digest,
    reads_closed_identity: ReceiptIdentity,
    reads_closed_receipt: &CommitReceipt,
) -> Result<CandidateGitReadsClosedReceipt, CandidateResolutionError> {
    if reads_closed_receipt.command_digest != *reads_closed_digest
        || reads_closed_receipt.stream_id != reads_closed_stream_id(delivery_id)
        || reads_closed_receipt.revision != 1
    {
        return Err(StorageError::invalid_input(
            "candidate read-closure receipt differs from durable state",
        )
        .into());
    }
    Ok(CandidateGitReadsClosedReceipt {
        delivery_id: delivery_id.clone(),
        terminal_outcome,
        terminal_receipt_digest: terminal_receipt.command_digest.clone(),
        reads_closed_receipt_digest: reads_closed_digest.clone(),
        delivery_revision: terminal_receipt.revision,
        terminal_receipt_identity: terminal_identity.clone(),
        reads_closed_receipt_identity: reads_closed_identity,
    })
}

fn canonical_closure_payload(
    mut stored: StoredReadsClosed,
) -> Result<(Vec<u8>, RequestId, Sha256Digest), CandidateResolutionError> {
    stored.reads_closed_receipt_digest = zero_digest();
    stored.reads_closed_request_id = zero_request_id();
    let seed = serde_json::to_vec(&stored).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure seed could not be encoded: {error}"
        )))
    })?;
    let request_id = RequestId(format!(
        "req_{}",
        hex_upper(&digest_bytes(REQUEST_DOMAIN, &seed))[..26].to_owned()
    ));
    stored.reads_closed_request_id = request_id.clone();
    let without_digest = serde_json::to_vec(&stored).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure payload could not be encoded: {error}"
        )))
    })?;
    let digest = Sha256Digest(format!(
        "sha256:{}",
        hex_lower(&digest_bytes(COMMIT_DOMAIN, &without_digest))
    ));
    stored.reads_closed_receipt_digest = digest.clone();
    let payload = serde_json::to_vec(&stored).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure payload could not be encoded: {error}"
        )))
    })?;
    Ok((payload, request_id, digest))
}

fn receipt_identity_for_closure(
    terminal_identity: &ReceiptIdentity,
    request_id: &RequestId,
) -> Result<ReceiptIdentity, CandidateResolutionError> {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(terminal_identity.actor_key().as_bytes().to_vec())?,
        ReceiptScopeKey::from_encoded(terminal_identity.scope_key().as_bytes().to_vec())?,
        request_id.clone(),
    )
    .map_err(Into::into)
}

fn decode_stored(payload: &[u8]) -> Result<StoredReadsClosed, CandidateResolutionError> {
    let stored: StoredReadsClosed = serde_json::from_slice(payload).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::invalid_input(format!(
            "candidate read-closure state is invalid: {error}"
        )))
    })?;
    if stored.schema_version != SCHEMA_VERSION {
        return Err(StorageError::invalid_input(
            "candidate read-closure schema version is unsupported",
        )
        .into());
    }
    let canonical = serde_json::to_vec(&stored).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure state could not be encoded: {error}"
        )))
    })?;
    if canonical != payload {
        return Err(
            StorageError::invalid_input("candidate read-closure state is not canonical").into(),
        );
    }
    let mut unsigned = stored.clone();
    unsigned.reads_closed_receipt_digest = zero_digest();
    unsigned.reads_closed_request_id = zero_request_id();
    let seed = serde_json::to_vec(&unsigned).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure state could not be encoded: {error}"
        )))
    })?;
    let expected_request_id = RequestId(format!(
        "req_{}",
        hex_upper(&digest_bytes(REQUEST_DOMAIN, &seed))[..26].to_owned()
    ));
    unsigned.reads_closed_request_id = expected_request_id.clone();
    let unsigned_payload = serde_json::to_vec(&unsigned).map_err(|error| {
        CandidateResolutionError::Storage(StorageError::adapter(format!(
            "candidate read-closure state could not be encoded: {error}"
        )))
    })?;
    let expected_digest = Sha256Digest(format!(
        "sha256:{}",
        hex_lower(&digest_bytes(COMMIT_DOMAIN, &unsigned_payload))
    ));
    if stored.reads_closed_request_id != expected_request_id
        || stored.reads_closed_receipt_digest != expected_digest
    {
        return Err(StorageError::invalid_input(
            "candidate read-closure state digest is inconsistent",
        )
        .into());
    }
    Ok(stored)
}

fn reads_closed_stream_id(delivery_id: &DeliveryId) -> String {
    format!("{READS_CLOSED_STREAM_PREFIX}{}", delivery_id.0)
}

impl StoredReceiptIdentity {
    fn from_identity(identity: &ReceiptIdentity) -> Self {
        Self {
            actor_key: identity.actor_key().as_bytes().to_vec(),
            scope_key: identity.scope_key().as_bytes().to_vec(),
            request_id: identity.request_id().clone(),
        }
    }

    fn to_identity(&self) -> Result<ReceiptIdentity, CandidateResolutionError> {
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(self.actor_key.clone())?,
            ReceiptScopeKey::from_encoded(self.scope_key.clone())?,
            self.request_id.clone(),
        )
        .map_err(Into::into)
    }
}

fn digest_bytes(domain: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(
        u64::try_from(payload.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(payload);
    digest.finalize().into()
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    result
}

fn hex_upper(bytes: &[u8; 32]) -> String {
    hex_lower(bytes).to_ascii_uppercase()
}

fn zero_digest() -> Sha256Digest {
    Sha256Digest("sha256:".to_owned() + &"0".repeat(64))
}

fn zero_request_id() -> RequestId {
    RequestId("req_".to_owned() + &"0".repeat(26))
}
