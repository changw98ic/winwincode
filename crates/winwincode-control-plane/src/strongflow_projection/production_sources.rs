// SPDX-License-Identifier: Apache-2.0

//! Production Publication projection reads over canonical durable facts.

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_delivery::domain::{Delivery, FrozenDeliveryCandidate};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{DeliveryId, PublicationId, Revision, Sha256Digest};
use winwincode_publication::{
    Publication, PublicationError, PublicationErrorKind, PublicationReadLedger,
    RepositoryPolicyScope,
};
use winwincode_storage::{
    ArtifactStore, GitSourceResolver, ProductStateStorage, StorageError, StorageErrorKind,
    StoredStateDirectoryEntry,
};

use super::{
    SqliteTrustedRuntimeProjectionAdapter, StrongFlowProjectionSources, TrustedProjectionReadError,
    TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
};

const PUBLICATION_STREAM_PREFIX: &str = "publication:";
const MAX_PUBLICATION_STREAMS: usize = 100_000;
const MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Production adapter that validates the bounded Publication directory and
/// reconstructs the current candidate from durable terminal and Artifact
/// authority on every read.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteTrustedPublicationProjectionAdapter;

impl TrustedPublicationProjectionAdapter for SqliteTrustedPublicationProjectionAdapter {
    #[allow(
        clippy::too_many_arguments,
        reason = "the trusted read binds every independent durable authority explicitly"
    )]
    fn read_current_with_storage(
        &self,
        storage: &dyn ProductStateStorage,
        artifacts: Option<&ArtifactStore>,
        source_resolver: Option<&dyn GitSourceResolver>,
        delivery: &Delivery,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        if delivery.id() != delivery_id || delivery.revision() != delivery_revision {
            return Err(TrustedProjectionReadError::Stale);
        }
        let artifacts = artifacts.ok_or(TrustedProjectionReadError::Unavailable)?;
        let source_resolver = source_resolver.ok_or(TrustedProjectionReadError::Unavailable)?;
        let candidate = crate::delivery_verdict_authority::resolve_current_candidate(
            storage,
            artifacts,
            source_resolver,
            scope,
            delivery,
        )
        .map_err(|_| TrustedProjectionReadError::Invalid)?;
        let directory = load_publication_directory(storage, scope, delivery_id, delivery_revision)?;
        let publication_revision = match directory.publication.as_ref() {
            Some(publication) => Revision(
                i64::try_from(publication.revision())
                    .map_err(|_| TrustedProjectionReadError::Invalid)?,
            ),
            None => Revision(0),
        };
        if expected_publication_revision.is_some_and(|expected| expected != &publication_revision) {
            return Err(TrustedProjectionReadError::Stale);
        }
        let result = directory
            .publication
            .as_ref()
            .map(Publication::result_fact)
            .transpose()
            .map_err(|error| publication_error(&error))?;
        if let Some(result) = result.as_ref() {
            let candidate = candidate
                .as_ref()
                .ok_or(TrustedProjectionReadError::Invalid)?;
            validate_candidate_result(candidate, result)?;
        }
        let source_seal = publication_source_seal(
            scope,
            delivery,
            candidate.as_ref(),
            &directory,
            result.as_ref(),
        )?;
        TrustedPublicationProjectionRead::try_new(
            scope.clone(),
            delivery_id.clone(),
            delivery_revision,
            publication_revision,
            candidate,
            result,
            source_seal,
        )
    }

    fn read_current(
        &self,
        _scope: &RepositoryScope,
        _delivery_id: &DeliveryId,
        _delivery_revision: u64,
        _expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        Err(TrustedProjectionReadError::Unavailable)
    }
}

fn validate_candidate_result(
    candidate: &FrozenDeliveryCandidate,
    result: &winwincode_publication::PublicationResultFact,
) -> Result<(), TrustedProjectionReadError> {
    let binding = result.binding();
    if binding.delivery_id() != candidate.delivery_id()
        || binding.delivery_spec_id() != candidate.delivery_spec_id().0.as_str()
        || binding.delivery_spec_revision() != candidate.delivery_spec_revision()
        || binding.candidate_ref() != candidate.candidate_ref()
        || binding.diff_sha256() != candidate.diff_sha256()
    {
        return Err(TrustedProjectionReadError::Invalid);
    }
    Ok(())
}

pub(crate) fn production_sources() -> StrongFlowProjectionSources {
    StrongFlowProjectionSources::new(
        Box::new(SqliteTrustedRuntimeProjectionAdapter::from_sqlite_storage()),
        Box::new(SqliteTrustedPublicationProjectionAdapter),
    )
}

#[derive(Debug)]
struct LoadedPublicationDirectory {
    directory_sha256: Sha256Digest,
    matched_state: Option<StoredStateDirectoryEntry>,
    publication: Option<Publication>,
}

fn load_publication_directory(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
) -> Result<LoadedPublicationDirectory, TrustedProjectionReadError> {
    let scope_sha256 = RepositoryPolicyScope::try_new(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .map_err(|_| TrustedProjectionReadError::Invalid)?
    .sha256();
    let states = storage
        .load_bounded_state_directory(
            PUBLICATION_STREAM_PREFIX,
            MAX_PUBLICATION_STREAMS,
            MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES,
        )
        .map_err(|error| storage_error(&error))?;
    let directory_sha256 = publication_directory_sha256(&states)?;
    let ledger = PublicationReadLedger::new(storage);
    let mut selected = None;
    for state in &states {
        let publication_id = publication_id_from_stream(&state.stream_id)?;
        let publication = ledger
            .get(&publication_id)
            .map_err(|error| publication_error(&error))?;
        if publication.revision() != state.revision {
            return Err(TrustedProjectionReadError::Stale);
        }
        if publication.repository_scope_sha256() == &scope_sha256
            && publication.binding().delivery_id() == delivery_id
            && publication.binding().delivery_revision() == delivery_revision
            && selected.replace((state.clone(), publication)).is_some()
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
    }
    let (matched_state, publication) = selected.map_or((None, None), |(state, publication)| {
        (Some(state), Some(publication))
    });
    Ok(LoadedPublicationDirectory {
        directory_sha256,
        matched_state,
        publication,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationDirectoryEntrySeal<'entry> {
    stream_id: &'entry str,
    revision: u64,
    payload_sha256: Sha256Digest,
}

fn publication_directory_sha256(
    states: &[StoredStateDirectoryEntry],
) -> Result<Sha256Digest, TrustedProjectionReadError> {
    let entries = states
        .iter()
        .map(|state| PublicationDirectoryEntrySeal {
            stream_id: &state.stream_id,
            revision: state.revision,
            payload_sha256: state.payload_sha256.clone(),
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(&entries).map_err(|_| TrustedProjectionReadError::Invalid)?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn publication_id_from_stream(
    stream_id: &str,
) -> Result<PublicationId, TrustedProjectionReadError> {
    stream_id
        .strip_prefix(PUBLICATION_STREAM_PREFIX)
        .filter(|value| !value.is_empty() && !value.contains(':'))
        .map(|value| PublicationId(value.to_owned()))
        .ok_or(TrustedProjectionReadError::Invalid)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationSourceSeal<'facts> {
    schema: &'static str,
    scope: &'facts RepositoryScope,
    delivery: &'facts Delivery,
    candidate: Option<&'facts FrozenDeliveryCandidate>,
    directory_sha256: &'facts Sha256Digest,
    publication_stream_id: Option<&'facts str>,
    publication_state_revision: Option<u64>,
    publication_state_sha256: Option<Sha256Digest>,
    result: Option<&'facts winwincode_publication::PublicationResultFact>,
}

fn publication_source_seal(
    scope: &RepositoryScope,
    delivery: &Delivery,
    candidate: Option<&FrozenDeliveryCandidate>,
    directory: &LoadedPublicationDirectory,
    result: Option<&winwincode_publication::PublicationResultFact>,
) -> Result<Sha256Digest, TrustedProjectionReadError> {
    let publication_state_sha256 = directory
        .matched_state
        .as_ref()
        .map(|state| state.payload_sha256.clone());
    let seal = PublicationSourceSeal {
        schema: "winwincode.strongflow-publication-source.v1",
        scope,
        delivery,
        candidate,
        directory_sha256: &directory.directory_sha256,
        publication_stream_id: directory
            .matched_state
            .as_ref()
            .map(|state| state.stream_id.as_str()),
        publication_state_revision: directory.matched_state.as_ref().map(|state| state.revision),
        publication_state_sha256,
        result,
    };
    let encoded = serde_json::to_vec(&seal).map_err(|_| TrustedProjectionReadError::Invalid)?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn publication_error(error: &PublicationError) -> TrustedProjectionReadError {
    match error.kind() {
        PublicationErrorKind::NotFound | PublicationErrorKind::RevisionConflict => {
            TrustedProjectionReadError::Stale
        }
        PublicationErrorKind::Storage | PublicationErrorKind::AuditUnavailable => {
            TrustedProjectionReadError::TemporarilyUnavailable
        }
        PublicationErrorKind::InvalidInput
        | PublicationErrorKind::StaleAuthority
        | PublicationErrorKind::PolicyDenied
        | PublicationErrorKind::RequestConflict
        | PublicationErrorKind::AlreadyExists
        | PublicationErrorKind::WrongState
        | PublicationErrorKind::PortContract
        | PublicationErrorKind::Corrupt => TrustedProjectionReadError::Invalid,
    }
}

fn storage_error(error: &StorageError) -> TrustedProjectionReadError {
    match error.kind() {
        StorageErrorKind::EventCursorExpired => TrustedProjectionReadError::ExactCutNotRetained,
        StorageErrorKind::InvalidInput
        | StorageErrorKind::RevisionConflict
        | StorageErrorKind::RequestConflict
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::RequestReplayMissing => TrustedProjectionReadError::Invalid,
        StorageErrorKind::Adapter | StorageErrorKind::Closed => {
            TrustedProjectionReadError::TemporarilyUnavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(feature = "test-support")]
    use std::sync::atomic::AtomicU64;

    #[cfg(feature = "test-support")]
    use rusqlite::{Connection, params};
    use winwincode_delivery::application::verdict::test_support::{
        VerdictFixtureOutcome, verdict_fixture,
    };
    #[cfg(feature = "test-support")]
    use winwincode_delivery::domain::{Delivery, DeliveryStatus};
    #[cfg(feature = "test-support")]
    use winwincode_domain::RequestId;
    use winwincode_domain::{
        AttentionItemId, Instant, OrganizationId, ProjectId, RepositoryId, Sha256Digest,
        WorkspaceId,
    };
    #[cfg(feature = "test-support")]
    use winwincode_publication::{
        PublicationCommandContext, PublicationOperation, PublicationPort, PublicationPortError,
        PublicationPortMutation, PublicationPortObservation, PublicationPublishCommand,
        test_support::{current_policy_coordinator, current_publication_fixture},
    };
    use winwincode_publication::{PublicationFactBinding, PublicationResultFact};
    use winwincode_storage::{
        AggregateJournalKey, CommitReceipt, LoadedAggregateJournal, OutboxEvent,
        ProductStateStorage, ProjectionEventStreamKey, ProjectionReadCut, ReceiptIdentity,
        StoredState,
    };
    #[cfg(feature = "test-support")]
    use winwincode_storage::{
        ArtifactError, ArtifactObject, LocalArtifactObjectStore, ReceiptActorKey, ReceiptScopeKey,
        SqliteStorage, ValidatedGitSourceArtifact,
    };

    use super::*;

    #[cfg(feature = "test-support")]
    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    #[cfg(feature = "test-support")]
    struct UnusedProvider;

    #[cfg(feature = "test-support")]
    struct UnusedGitResolver;

    #[cfg(feature = "test-support")]
    impl GitSourceResolver for UnusedGitResolver {
        fn resolve_candidate(
            &self,
            _artifact: &ArtifactObject,
            _repository_locator: &str,
            _base_revision: &str,
        ) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
            panic!("a Delivery without writer StageRuns must not resolve a candidate")
        }
    }

    #[cfg(feature = "test-support")]
    impl PublicationPort for UnusedProvider {
        fn lookup(
            &mut self,
            _operation: &PublicationOperation,
        ) -> Result<PublicationPortObservation, PublicationPortError> {
            panic!("intent persistence must not read the provider")
        }

        fn apply(
            &mut self,
            _operation: &PublicationOperation,
        ) -> Result<PublicationPortMutation, PublicationPortError> {
            panic!("intent persistence must not write the provider")
        }
    }

    struct ReadBoundaryStorage {
        proves_empty: bool,
        directory_read: AtomicBool,
    }

    impl ReadBoundaryStorage {
        fn empty() -> Self {
            Self {
                proves_empty: true,
                directory_read: AtomicBool::new(false),
            }
        }

        fn unavailable() -> Self {
            Self {
                proves_empty: false,
                directory_read: AtomicBool::new(false),
            }
        }
    }

    impl ProductStateStorage for ReadBoundaryStorage {
        fn load_receipt(
            &self,
            _identity: &ReceiptIdentity,
            _command_digest: &Sha256Digest,
        ) -> Result<Option<CommitReceipt>, StorageError> {
            Err(StorageError::adapter(
                "receipt storage is outside this read fixture",
            ))
        }

        fn load_state(&self, _stream_id: &str) -> Result<Option<StoredState>, StorageError> {
            Err(StorageError::adapter(
                "state storage is outside this read fixture",
            ))
        }

        fn load_bounded_state_directory(
            &self,
            prefix: &str,
            max_entries: usize,
            max_payload_bytes: usize,
        ) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
            assert_eq!(prefix, PUBLICATION_STREAM_PREFIX);
            assert_eq!(max_entries, MAX_PUBLICATION_STREAMS);
            assert_eq!(max_payload_bytes, MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES);
            self.directory_read.store(true, Ordering::SeqCst);
            if self.proves_empty {
                Ok(Vec::new())
            } else {
                Err(StorageError::adapter(
                    "publication directory is unavailable",
                ))
            }
        }

        fn load_projection_read_cut(
            &self,
            _state_stream_ids: &[String],
            _key: &ProjectionEventStreamKey,
            _expected: Option<&winwincode_storage::ProjectionEventCursor>,
        ) -> Result<ProjectionReadCut, StorageError> {
            Err(StorageError::adapter(
                "projection storage is outside this read fixture",
            ))
        }

        fn load_journal(
            &self,
            _key: &AggregateJournalKey,
        ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
            Err(StorageError::adapter(
                "journal storage is outside this read fixture",
            ))
        }

        fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
            Ok(Vec::new())
        }

        fn mark_published(&mut self, _event_id: &str) -> Result<(), StorageError> {
            Err(StorageError::adapter(
                "outbox storage is outside this read fixture",
            ))
        }

        fn close(self: Box<Self>) -> Result<(), StorageError> {
            Ok(())
        }
    }

    #[test]
    fn empty_publication_requires_a_successful_atomic_directory_read() {
        let storage = ReadBoundaryStorage::empty();
        let directory = load_publication_directory(
            &storage,
            &scope(),
            &DeliveryId("dlv_01J00000000000000000000000".to_owned()),
            1,
        )
        .expect("sealed empty directory");
        assert!(storage.directory_read.load(Ordering::SeqCst));
        assert!(directory.publication.is_none());
    }

    #[test]
    fn unavailable_publication_directory_is_never_an_empty_cut() {
        let storage = ReadBoundaryStorage::unavailable();
        assert_eq!(
            load_publication_directory(
                &storage,
                &scope(),
                &DeliveryId("dlv_01J00000000000000000000000".to_owned()),
                1,
            )
            .expect_err("directory failure must remain visible"),
            TrustedProjectionReadError::TemporarilyUnavailable
        );
        assert!(storage.directory_read.load(Ordering::SeqCst));
    }

    #[test]
    fn publication_result_must_bind_the_exact_current_candidate() {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000000".to_owned()),
            VerdictFixtureOutcome::Pass,
        );
        let binding = PublicationFactBinding::try_new(
            fixture.candidate.delivery_id().clone(),
            fixture.delivery.revision(),
            fixture.candidate.delivery_spec_id().0.clone(),
            fixture.candidate.delivery_spec_revision(),
            fixture.candidate.candidate_ref(),
            "f".repeat(64),
            "verdict:fixture:pass",
            AttentionItemId("att_01J00000000000000000000000".to_owned()),
            "d".repeat(64),
            "e".repeat(64),
        )
        .expect("mismatched but well-formed binding");
        let result = PublicationResultFact::try_new(
            PublicationId("pub_01J00000000000000000000000".to_owned()),
            Revision(1),
            "pending",
            Instant("2026-08-27T00:00:00.000Z".to_owned()),
            binding,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            None,
        )
        .expect("well-formed Publication result");

        assert_eq!(
            validate_candidate_result(&fixture.candidate, &result),
            Err(TrustedProjectionReadError::Invalid)
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn two_publications_for_one_delivery_revision_are_rejected() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-strongflow-publications-{}-{}",
            std::process::id(),
            NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut storage = SqliteStorage::open(&root).expect("open fixture storage");
        let fixture = current_publication_fixture();
        persist_publication(&mut storage, &fixture, 1);
        persist_publication(&mut storage, &fixture, 2);

        assert_eq!(
            load_publication_directory(
                &storage,
                &publication_scope(),
                fixture.authorization().binding().delivery_id(),
                fixture.authorization().binding().delivery_revision(),
            )
            .expect_err("duplicate current Publication facts must fail closed"),
            TrustedProjectionReadError::Invalid
        );
        drop(storage);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn publication_inserted_after_empty_cut_rejects_exact_revision_zero() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-strongflow-publication-race-{}-{}",
            std::process::id(),
            NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut storage = SqliteStorage::open(root.join("state")).expect("open fixture storage");
        let object_store =
            LocalArtifactObjectStore::open(root.join("objects")).expect("open Artifact objects");
        let artifacts = ArtifactStore::open(root.join("artifacts"), Box::new(object_store))
            .expect("open Artifact catalog");
        let fixture = current_publication_fixture();
        let delivery = delivery_without_candidate(fixture.authorization().binding());
        let adapter = SqliteTrustedPublicationProjectionAdapter;
        let first = adapter
            .read_current_with_storage(
                &storage,
                Some(&artifacts),
                Some(&UnusedGitResolver),
                &delivery,
                &publication_scope(),
                delivery.id(),
                delivery.revision(),
                None,
            )
            .expect("first bounded empty Publication read");
        assert_eq!(first.publication_revision(), &Revision(0));

        persist_publication(&mut storage, &fixture, 1);

        assert_eq!(
            adapter.read_current_with_storage(
                &storage,
                Some(&artifacts),
                Some(&UnusedGitResolver),
                &delivery,
                &publication_scope(),
                delivery.id(),
                delivery.revision(),
                Some(first.publication_revision()),
            ),
            Err(TrustedProjectionReadError::Stale)
        );
        drop(artifacts);
        drop(storage);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn atomic_directory_detects_low_id_inserted_after_a_legacy_page() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-strongflow-publication-page-race-{}-{}",
            std::process::id(),
            NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut storage = SqliteStorage::open(&root).expect("open fixture storage");
        let fixture = current_publication_fixture();
        for seed in 100..=356 {
            persist_publication(&mut storage, &fixture, seed);
        }
        let initial = storage
            .load_bounded_state_directory(
                PUBLICATION_STREAM_PREFIX,
                MAX_PUBLICATION_STREAMS,
                MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES,
            )
            .expect("initial atomic directory");
        let initial_sha256 =
            publication_directory_sha256(&initial).expect("initial directory seal");
        let upper_bound = storage
            .last_state_stream_id(PUBLICATION_STREAM_PREFIX)
            .expect("legacy upper bound")
            .expect("non-empty legacy directory");
        let first_page = storage
            .scan_state_streams(PUBLICATION_STREAM_PREFIX, "", &upper_bound, 256)
            .expect("legacy first page");
        let after = first_page
            .last()
            .expect("full legacy first page")
            .stream_id
            .clone();

        persist_publication(&mut storage, &fixture, 50);

        let legacy_remainder = storage
            .scan_state_streams(PUBLICATION_STREAM_PREFIX, &after, &upper_bound, 256)
            .expect("legacy remainder");
        assert!(
            legacy_remainder
                .iter()
                .all(|state| state.stream_id != "publication:pub_00000000000000000000000050")
        );
        let exact = storage
            .load_bounded_state_directory(
                PUBLICATION_STREAM_PREFIX,
                MAX_PUBLICATION_STREAMS,
                MAX_PUBLICATION_DIRECTORY_PAYLOAD_BYTES,
            )
            .expect("exact atomic directory");
        assert!(
            exact
                .iter()
                .any(|state| state.stream_id == "publication:pub_00000000000000000000000050")
        );
        assert_ne!(
            publication_directory_sha256(&exact).expect("exact directory seal"),
            initial_sha256
        );
        drop(storage);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn atomic_directory_rejects_a_total_payload_over_its_bound() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-strongflow-publication-bytes-{}-{}",
            std::process::id(),
            NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let storage = SqliteStorage::open(&root).expect("open fixture storage");
        let connection =
            Connection::open(root.join("control-plane.sqlite3")).expect("open raw fixture SQLite");
        for seed in 1..=2 {
            connection
                .execute(
                    "INSERT INTO product_state (stream_id, revision, payload) VALUES (?1, 1, ?2)",
                    params![format!("publication:pub_{seed:026}"), vec![0_u8; 5]],
                )
                .expect("insert bounded payload fixture");
        }

        let error = storage
            .load_bounded_state_directory(PUBLICATION_STREAM_PREFIX, 10, 8)
            .expect_err("the combined payload exceeds eight bytes");
        assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
        drop(storage);
        drop(connection);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    fn scope() -> RepositoryScope {
        RepositoryScope {
            kind: winwincode_domain::RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_01J00000000000000000000000".to_owned()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000000".to_owned()),
            project_id: ProjectId("prj_01J00000000000000000000000".to_owned()),
            repository_id: RepositoryId("rep_01J00000000000000000000000".to_owned()),
        }
    }

    #[cfg(feature = "test-support")]
    fn publication_scope() -> RepositoryScope {
        RepositoryScope {
            kind: winwincode_domain::RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
            workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
            project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
            repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
        }
    }

    #[cfg(feature = "test-support")]
    fn persist_publication(
        storage: &mut dyn ProductStateStorage,
        fixture: &winwincode_publication::test_support::CurrentPublicationFixture,
        seed: u64,
    ) {
        let command = PublicationPublishCommand::try_new(
            PublicationId(format!("pub_{seed:026}")),
            fixture.authorization().binding().delivery_id().clone(),
            fixture.authorization().candidate_digest().clone(),
            fixture.authorization().target().clone(),
        )
        .expect("canonical Publication command");
        let command_digest = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(format!("publication-{seed}").as_bytes())
        ));
        let context = PublicationCommandContext::try_new(
            ReceiptIdentity::new(
                ReceiptActorKey::from_encoded(b"fixture-publication-actor".to_vec())
                    .expect("actor key"),
                ReceiptScopeKey::from_encoded(b"fixture-publication-repository-scope".to_vec())
                    .expect("scope key"),
                RequestId(format!("req_{seed:026}")),
            )
            .expect("Publication receipt identity"),
            command_digest,
            0,
            1_100 + seed,
        )
        .expect("Publication command context");
        let mut provider = UnusedProvider;
        current_policy_coordinator(storage, &mut provider)
            .publish(&context, &command, fixture.authorization())
            .expect("persist Publication intent");
    }

    #[cfg(feature = "test-support")]
    fn delivery_without_candidate(binding: &PublicationFactBinding) -> Delivery {
        let fixture = verdict_fixture(binding.delivery_id(), VerdictFixtureOutcome::Pass);
        let mut snapshot = fixture.delivery.into_snapshot();
        snapshot.revision = binding.delivery_revision();
        snapshot.status = DeliveryStatus::Draft;
        snapshot.stage_runs.clear();
        snapshot.session_bindings.clear();
        snapshot.attention_items.clear();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        Delivery::try_from_snapshot(snapshot).expect("candidate-free Delivery")
    }
}
