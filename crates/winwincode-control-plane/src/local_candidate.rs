// SPDX-License-Identifier: Apache-2.0

//! `LocalCandidateReceipt` / `LocalApplyReceipt` application service over the
//! durable Server-side candidate receipt ledger.
//!
//! The Device Client owns the local git facts of candidate delivery (plan
//! 5.6): it freezes a candidate commit under a stable local ref and reports
//! `client.candidate.retained`, then answers every create-branch / apply /
//! discard attempt with `client.candidate.apply_result`. This service is the
//! Control Plane's audit authority over those client-issued receipts: it
//! appends retentions idempotently, settles every apply attempt exactly once
//! behind the frozen contract 6/8 state machine, and keeps the apply history
//! immutable so retries append new receipts instead of rewriting old ones.
//! Absolute filesystem paths never enter this surface.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    LocalApplySettlement, LocalCandidateRetained, LocalCandidateStoreError,
    LocalCandidateStoreErrorKind, SqliteStorage,
};

/// Re-exported so service consumers can name the frozen receipt records and
/// result vocabulary without importing the storage crate directly.
pub use winwincode_storage::{LocalApplyReceiptRecord, LocalCandidateReceiptRecord};
pub use winwincode_storage::{LocalApplyResult, LocalApplyStrategy, LocalCandidateReceiptState};

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidateServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No repository binding matches the requested identity.
    UnknownRepositoryBinding,
    /// No local candidate receipt matches the requested identity.
    UnknownLocalCandidate,
    /// No local apply receipt matches the requested identity.
    UnknownLocalApplyReceipt,
    /// The candidate is already retained under a different identity or with
    /// different facts.
    LocalCandidateConflict,
    /// The apply receipt id is already used with different fields.
    ApplyReceiptConflict,
    /// The candidate already reached its `applied` or `discarded` terminal.
    TerminalCandidateConflict,
    /// The settlement identity does not match the retained candidate.
    CandidateIdentityMismatch,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost an impossible race.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free local candidate service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCandidateServiceError {
    kind: LocalCandidateServiceErrorKind,
    message: String,
}

impl LocalCandidateServiceError {
    #[must_use]
    pub const fn kind(&self) -> LocalCandidateServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for LocalCandidateServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalCandidateServiceError {}

impl From<LocalCandidateStoreError> for LocalCandidateServiceError {
    fn from(source: LocalCandidateStoreError) -> Self {
        Self {
            kind: match source.kind() {
                LocalCandidateStoreErrorKind::InvalidInput => {
                    LocalCandidateServiceErrorKind::InvalidInput
                }
                LocalCandidateStoreErrorKind::UnknownClientNode => {
                    LocalCandidateServiceErrorKind::UnknownClientNode
                }
                LocalCandidateStoreErrorKind::UnknownRepositoryBinding => {
                    LocalCandidateServiceErrorKind::UnknownRepositoryBinding
                }
                LocalCandidateStoreErrorKind::UnknownLocalCandidate => {
                    LocalCandidateServiceErrorKind::UnknownLocalCandidate
                }
                LocalCandidateStoreErrorKind::UnknownLocalApplyReceipt => {
                    LocalCandidateServiceErrorKind::UnknownLocalApplyReceipt
                }
                LocalCandidateStoreErrorKind::LocalCandidateConflict => {
                    LocalCandidateServiceErrorKind::LocalCandidateConflict
                }
                LocalCandidateStoreErrorKind::ApplyReceiptConflict => {
                    LocalCandidateServiceErrorKind::ApplyReceiptConflict
                }
                LocalCandidateStoreErrorKind::TerminalCandidateConflict => {
                    LocalCandidateServiceErrorKind::TerminalCandidateConflict
                }
                LocalCandidateStoreErrorKind::CandidateIdentityMismatch => {
                    LocalCandidateServiceErrorKind::CandidateIdentityMismatch
                }
                LocalCandidateStoreErrorKind::IllegalStateTransition => {
                    LocalCandidateServiceErrorKind::IllegalStateTransition
                }
                LocalCandidateStoreErrorKind::RevisionConflict => {
                    LocalCandidateServiceErrorKind::RevisionConflict
                }
                LocalCandidateStoreErrorKind::CorruptState => {
                    LocalCandidateServiceErrorKind::CorruptState
                }
                LocalCandidateStoreErrorKind::Storage => LocalCandidateServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

/// Local candidate receipt application service over one storage connection.
pub struct LocalCandidateService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> LocalCandidateService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Records a `client.candidate.retained` report idempotently (plan 5.6,
    /// contract 6): replays of the same receipt and duplicate reports of an
    /// already retained candidate ref return the original row unchanged;
    /// any fact disagreement fails closed.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical command, an unknown client node or repository
    /// binding, a candidate ref retained with different facts, a receipt id
    /// reused for a different candidate, or storage failure.
    pub fn record_retained(
        &mut self,
        retained: &LocalCandidateRetained,
        now: &Instant,
    ) -> Result<LocalCandidateReceiptRecord, LocalCandidateServiceError> {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .record_retained(retained, now)?)
    }

    /// Records a `client.candidate.apply_result` settlement (contract 8):
    /// appends exactly one immutable receipt, projects the frozen result
    /// code onto the candidate state machine, and returns both rows.
    /// Replays of a settled receipt id are accepted idempotent no-ops;
    /// terminal candidates refuse further settlements.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical command, an unknown candidate, an identity
    /// mismatch against the retained candidate, a receipt id reused with
    /// different fields, a terminal candidate, an illegal result projection,
    /// or storage failure.
    pub fn record_apply_result(
        &mut self,
        settlement: &LocalApplySettlement,
        now: &Instant,
    ) -> Result<(LocalApplyReceiptRecord, LocalCandidateReceiptRecord), LocalCandidateServiceError>
    {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .record_apply_result(settlement, now)?)
    }

    /// Returns one durable local candidate receipt projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity, corrupt stored rows, or
    /// storage failure.
    pub fn candidate_snapshot(
        &mut self,
        local_candidate_receipt_id: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateServiceError> {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .candidate_snapshot(local_candidate_receipt_id)?)
    }

    /// Returns the candidate receipt for one client-local candidate ref, if
    /// any.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or candidate ref, or
    /// storage failure.
    pub fn candidate_for_ref(
        &mut self,
        client_node_id: &str,
        candidate_ref: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateServiceError> {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .candidate_for_ref(client_node_id, candidate_ref)?)
    }

    /// Returns one immutable local apply receipt.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity or storage failure.
    pub fn apply_receipt(
        &mut self,
        local_apply_receipt_id: &str,
    ) -> Result<Option<LocalApplyReceiptRecord>, LocalCandidateServiceError> {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .apply_receipt(local_apply_receipt_id)?)
    }

    /// Returns the full immutable apply history of one candidate, oldest
    /// first.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity or storage failure.
    pub fn apply_history_for_candidate(
        &mut self,
        local_candidate_receipt_id: &str,
    ) -> Result<Vec<LocalApplyReceiptRecord>, LocalCandidateServiceError> {
        Ok(self
            .storage
            .local_candidate_ledger()?
            .apply_history_for_candidate(local_candidate_receipt_id)?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_storage::{
        RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-local-candidate-service-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn crockford(seed: u64) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut identity = String::with_capacity(26);
        let mut value = seed;
        for _ in 0..26 {
            let digit = usize::try_from(value % 32).expect("digit fits");
            identity.push(ALPHABET[digit] as char);
            value /= 32;
        }
        identity
    }

    const COMMIT: &str = "00112233445566778899aabbccddeeff00112233";
    const RESULTING: &str = "1234567890abcdef1234567890abcdef12345678";

    /// Seeds one registered client node and one repository binding and
    /// returns their identities.
    fn seed_fixture(storage: &mut SqliteStorage, seed: u64) -> (String, String) {
        let node = format!("cnd_{}", crockford(seed));
        let registration = winwincode_storage::ClientNodeRegistration::try_new(
            node.clone(),
            format!("{seed:010}"),
            "Service Test Device".to_owned(),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(format!("cix_{}", crockford(seed + 1))),
            2,
        )
        .expect("registration");
        {
            let mut registry = storage.client_node_registry().expect("registry");
            registry
                .register(&registration, 0, &instant("2026-01-01T00:00:00.000Z"))
                .expect("register");
        }
        let binding = format!("rbd_{}", crockford(seed + 2));
        let mut ledger = storage.repository_binding_ledger().expect("binding ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
            node.as_str(),
            "winwincode",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
            .expect("upsert");
        (node, binding)
    }

    fn retained(
        receipt_seed: u64,
        node: &str,
        binding: &str,
        ref_seed: u64,
    ) -> LocalCandidateRetained {
        LocalCandidateRetained::try_new(
            format!("lcr_{}", crockford(receipt_seed)),
            node,
            binding,
            format!("refs/winwincode/candidates/candidate-{ref_seed}"),
            COMMIT,
            format!("refs/winwincode/candidates/candidate-{ref_seed}"),
        )
        .expect("retained command")
    }

    fn settlement(
        apply_seed: u64,
        receipt: &str,
        node: &str,
        binding: &str,
        ref_seed: u64,
        result: LocalApplyResult,
        resulting_commit: Option<String>,
    ) -> LocalApplySettlement {
        LocalApplySettlement::try_new(
            format!("lar_{}", crockford(apply_seed)),
            receipt,
            node,
            binding,
            format!("refs/winwincode/candidates/candidate-{ref_seed}"),
            "winwincode/main-branch".to_owned(),
            COMMIT.to_owned(),
            LocalApplyStrategy::Merge,
            result,
            resulting_commit,
            None,
        )
        .expect("settlement command")
    }

    #[test]
    fn service_round_trips_retention_settlement_and_history() {
        let mut storage = SqliteStorage::open(temporary_directory("round-trip")).expect("storage");
        let (node, binding) = seed_fixture(&mut storage, 1);
        let command = retained(10, node.as_str(), binding.as_str(), 20);
        let receipt_id = command.local_candidate_receipt_id().to_owned();
        let mut service = LocalCandidateService::new(&mut storage);

        let retained_record = service
            .record_retained(&command, &instant("2026-01-01T01:00:00.000Z"))
            .expect("retained");
        assert_eq!(retained_record.state, LocalCandidateReceiptState::Retained);
        assert_eq!(retained_record.revision, 1);

        // A failure keeps the candidate retryable...
        let failure = settlement(
            11,
            receipt_id.as_str(),
            node.as_str(),
            binding.as_str(),
            20,
            LocalApplyResult::WorkingTreeDirty,
            None,
        );
        let (_, failed) = service
            .record_apply_result(&failure, &instant("2026-01-01T01:01:00.000Z"))
            .expect("failure settlement");
        assert_eq!(failed.state, LocalCandidateReceiptState::Failed);

        // ...and the retry applies.
        let retry = settlement(
            12,
            receipt_id.as_str(),
            node.as_str(),
            binding.as_str(),
            20,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
        );
        let (apply_receipt, applied) = service
            .record_apply_result(&retry, &instant("2026-01-01T01:02:00.000Z"))
            .expect("retry settlement");
        assert_eq!(apply_receipt.result, LocalApplyResult::Applied);
        assert_eq!(apply_receipt.resulting_commit.as_deref(), Some(RESULTING));
        assert_eq!(applied.state, LocalCandidateReceiptState::Applied);

        // Read projections agree with the settled facts.
        let snapshot = service
            .candidate_snapshot(receipt_id.as_str())
            .expect("snapshot")
            .expect("candidate");
        assert_eq!(snapshot, applied);
        let by_ref = service
            .candidate_for_ref(node.as_str(), "refs/winwincode/candidates/candidate-20")
            .expect("by ref")
            .expect("candidate by ref");
        assert_eq!(by_ref, applied);
        let apply = service
            .apply_receipt(apply_receipt.local_apply_receipt_id.as_str())
            .expect("apply receipt")
            .expect("apply receipt row");
        assert_eq!(apply, apply_receipt);
        let history = service
            .apply_history_for_candidate(receipt_id.as_str())
            .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].result, LocalApplyResult::WorkingTreeDirty);
        assert_eq!(history[1].result, LocalApplyResult::Applied);
    }

    #[test]
    fn service_maps_the_ledger_taxonomy_one_to_one() {
        let mut storage = SqliteStorage::open(temporary_directory("taxonomy")).expect("storage");
        let (node, binding) = seed_fixture(&mut storage, 30);
        let mut service = LocalCandidateService::new(&mut storage);

        // An unknown candidate identity maps to the precise service kind.
        let unknown = settlement(
            41,
            "lcr_00000000000000000000000000",
            node.as_str(),
            binding.as_str(),
            50,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
        );
        let error = service
            .record_apply_result(&unknown, &instant("2026-01-01T01:01:00.000Z"))
            .expect_err("unknown candidate must fail");
        assert_eq!(
            error.kind(),
            LocalCandidateServiceErrorKind::UnknownLocalCandidate
        );

        // Non-canonical command inputs map to InvalidInput; a local
        // filesystem path can never pass as a candidate ref.
        let error = LocalCandidateRetained::try_new(
            "lcr_not-canonical",
            node.as_str(),
            binding.as_str(),
            "refs/winwincode/candidates/candidate-51",
            COMMIT,
            "refs/winwincode/candidates/candidate-51",
        )
        .expect_err("non-canonical id must fail");
        assert_eq!(error.kind(), LocalCandidateStoreErrorKind::InvalidInput);

        let error = LocalCandidateRetained::try_new(
            format!("lcr_{}", crockford(52)),
            node.as_str(),
            binding.as_str(),
            "/absolute/path".to_owned(),
            COMMIT,
            "refs/winwincode/candidates/candidate-52",
        )
        .expect_err("a path can never be a candidate ref");
        assert_eq!(error.kind(), LocalCandidateStoreErrorKind::InvalidInput);
    }
}
