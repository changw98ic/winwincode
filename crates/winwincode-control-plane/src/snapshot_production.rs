// SPDX-License-Identifier: Apache-2.0

//! Control Plane ownership of the Candidate → freeze → Snapshot transaction.

use sha2::{Digest, Sha256};
use winwincode_delivery::domain::Delivery;
use winwincode_domain::{
    Candidate, CandidateDigest, CandidateId, CanonicalSnapshot, GitObjectId, Instant, RequestId,
    SchemaVersion, Sha256Digest, Snapshot, SnapshotId, seal_snapshot,
};
use winwincode_execution_port::{
    execution_identity::canonical_dispatch_session_identity,
    generated::{
        ExecutionPortMessage, ExecutionScope, JobDispatchMessage, SnapshotFreezeReceiptMessage,
        SnapshotFreezeRequestMessage, SnapshotFreezeRequestMessageKind,
        SnapshotVerificationDispatchMessage, SnapshotVerificationDispatchMessageKind,
    },
    snapshot_freeze::{validate_freeze_receipt, validate_freeze_request},
};
use winwincode_storage::{
    CandidateGitRetentionState, NewOutboxEvent, ReceiptIdentity, SnapshotProductStaged,
    SnapshotVerificationBinding, SqliteStorage, StateCommit, StateRevisionGuard, StorageError,
    commit_snapshot_product,
};

use crate::{
    ControlPlane,
    delivery_transaction::load_durable_execution_job,
    delivery_verdict_authority::{current_writer, load_terminal, source_for_terminal},
    publication_preparation::derived_id,
    repository_scope_from_receipt_key,
    session_binding_transaction::instant_millis,
};

pub(crate) const FREEZE_TOPIC: &str = "verification.snapshot.freeze";
pub(crate) const DISPATCH_TOPIC: &str = "verification.snapshot.dispatch";

impl ControlPlane {
    /// Seals the current Candidate and persists the freeze command before it
    /// leaves Control Plane. Ordinary jobs use the existing dispatch path.
    ///
    /// # Errors
    /// Rejects a foreign lease/job, missing source or pin, and changed replay.
    pub fn prepare_snapshot_freeze(
        &mut self,
        registry: &mut SqliteStorage,
        dispatch: &JobDispatchMessage,
        now: &Instant,
    ) -> Result<ExecutionPortMessage, StorageError> {
        if !is_verification(dispatch) {
            return Ok(ExecutionPortMessage::JobDispatchMessage(dispatch.clone()));
        }
        self.require_snapshot_lease(registry, dispatch, now)?;
        let stream = freeze_stream(dispatch);
        if let Some(stored) = self.storage_ref()?.load_state(&stream)? {
            let request: SnapshotFreezeRequestMessage = decode(&stored.payload)?;
            if request.dispatch != *dispatch {
                return Err(StorageError::invalid_input(
                    "Snapshot freeze dispatch changed",
                ));
            }
            validate_freeze_request(&request).map_err(StorageError::invalid_input)?;
            return Ok(ExecutionPortMessage::SnapshotFreezeRequestMessage(request));
        }
        let (durable, job) = load_durable_execution_job(self.storage_ref()?, &dispatch.job.job_id)?;
        if job != dispatch.job {
            return Err(StorageError::invalid_input(
                "Snapshot planned Job differs from durable intent",
            ));
        }
        let state = self
            .storage_ref()?
            .load_state(durable.stream_id())?
            .ok_or_else(|| StorageError::invalid_input("Snapshot Delivery is missing"))?;
        let delivery = Delivery::decode_json(&state.payload).map_err(invalid)?;
        let scope = repository_scope_from_receipt_key(durable.receipt_identity().scope_key())?;
        let (candidate, base_tree_id, content_digest) =
            self.sealed_snapshot_candidate(registry, &scope, &delivery)?;
        let seed = digest(&serde_json::to_vec(&dispatch.lease).map_err(invalid)?);
        let request = SnapshotFreezeRequestMessage {
            base_tree_id,
            candidate,
            content_digest,
            dispatch: dispatch.clone(),
            kind: SnapshotFreezeRequestMessageKind::SnapshotFreezeRequest,
            lease: dispatch.lease.clone(),
            message_id: winwincode_domain::ExecutionMessageId(derived_id(
                "xmsg",
                "snapshot-freeze",
                &seed,
            )),
            repository_id: scope.repository_id,
            request_id: RequestId(derived_id("req", "snapshot-freeze", &seed)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now.clone(),
        };
        validate_freeze_request(&request).map_err(StorageError::invalid_input)?;
        let payload = serde_json::to_vec(&request).map_err(invalid)?;
        let identity = ReceiptIdentity::new(
            durable.receipt_identity().actor_key().clone(),
            durable.receipt_identity().scope_key().clone(),
            request.request_id.clone(),
        )?;
        let commit = StateCommit::new(
            identity,
            digest(&payload),
            stream,
            0,
            payload.clone(),
            vec![NewOutboxEvent::internal(
                freeze_event(&request),
                FREEZE_TOPIC,
                payload,
            )],
        )
        .with_state_guard(StateRevisionGuard::new(
            durable.stream_id(),
            state.revision,
        )?);
        self.storage_mut()?.commit(&commit)?;
        Ok(ExecutionPortMessage::SnapshotFreezeRequestMessage(request))
    }

    /// Validates Worker facts against the durable request and current producer,
    /// then atomically creates Snapshot, runtime binding and dispatch intent.
    ///
    /// # Errors
    /// Rejects missing, stale or foreign authority and any changed code facts.
    #[allow(clippy::too_many_lines)]
    pub fn accept_snapshot_freeze(
        &mut self,
        registry: &mut SqliteStorage,
        message: &SnapshotFreezeReceiptMessage,
        now: &Instant,
    ) -> Result<SnapshotVerificationDispatchMessage, StorageError> {
        let stream = format!(
            "snapshot-freeze:v1:{}:{}",
            message.lease.job_id.0, message.lease.attempt
        );
        let stored = self
            .storage_ref()?
            .load_state(&stream)?
            .ok_or_else(|| StorageError::invalid_input("Snapshot freeze request is missing"))?;
        let request: SnapshotFreezeRequestMessage = decode(&stored.payload)?;
        validate_freeze_receipt(&request, message).map_err(StorageError::invalid_input)?;
        self.require_snapshot_lease(registry, &request.dispatch, now)?;
        let (durable, _) = load_durable_execution_job(self.storage_ref()?, &message.lease.job_id)?;
        let state = self
            .storage_ref()?
            .load_state(durable.stream_id())?
            .ok_or_else(|| StorageError::invalid_input("Snapshot Delivery is missing"))?;
        let delivery = Delivery::decode_json(&state.payload).map_err(invalid)?;
        let scope = repository_scope_from_receipt_key(durable.receipt_identity().scope_key())?;
        let (candidate, base_tree, content) =
            self.sealed_snapshot_candidate(registry, &scope, &delivery)?;
        if candidate != request.candidate
            || base_tree != request.base_tree_id
            || content != request.content_digest
        {
            return Err(StorageError::invalid_input(
                "Snapshot receipt no longer names the current sealed Candidate",
            ));
        }
        let seed = digest(candidate.id.0.as_bytes());
        let snapshot_id = SnapshotId(derived_id("snap", "candidate-snapshot", &seed));
        let snapshot: CanonicalSnapshot = if let Some(stored) = self
            .storage_ref()?
            .load_state(&format!("snapshot-product:v1:{}", snapshot_id.0))?
        {
            decode(&stored.payload)?
        } else {
            let receipt = &message.receipt;
            let mut snapshot = Snapshot {
                schema_version: SchemaVersion::WinwincodeV1,
                snapshot_id,
                candidate_id: candidate.id.clone(),
                work_run_id: candidate.work_run_id.clone(),
                repository_id: receipt.repository_id.clone(),
                base_commit_id: receipt.base_commit_id.clone(),
                base_tree_id: receipt.base_tree_id.clone(),
                candidate_commit_id: receipt.candidate_commit_id.clone(),
                candidate_tree_id: receipt.candidate_tree_id.clone(),
                diff_sha256: receipt.diff_sha256.clone(),
                content_digest: receipt.content_digest.clone(),
                created_at_millis: i64::try_from(instant_millis(&message.receipt.frozen_at)?)
                    .map_err(invalid)?,
                immutable: true,
                validation_seal: Sha256Digest(String::new()),
            };
            snapshot.validation_seal = seal_snapshot(&snapshot);
            snapshot.try_into().map_err(invalid)?
        };
        if snapshot.as_contract().base_tree_id != request.base_tree_id
            || snapshot.as_contract().content_digest != request.content_digest
            || snapshot.as_contract().repository_id != request.repository_id
        {
            return Err(StorageError::invalid_input(
                "Snapshot retry changed protected input",
            ));
        }
        let dispatch = &request.dispatch;
        let ExecutionScope::WorkRunExecutionScope(scope) = &dispatch.job.scope else {
            return Err(StorageError::invalid_input(
                "Snapshot WorkRun scope is missing",
            ));
        };
        let (worker_session_id, codex_thread_id) = canonical_dispatch_session_identity(
            &dispatch.lease.worker_id,
            &dispatch.lease.worker_instance_id,
            dispatch,
        )
        .map_err(invalid)?;
        let seed = digest(&serde_json::to_vec(&dispatch.lease).map_err(invalid)?);
        let input = dispatch
            .job
            .work_input
            .as_ref()
            .ok_or_else(|| StorageError::invalid_input("verification input missing"))?;
        let plan_id = winwincode_domain::VerificationPlanId(derived_id(
            "vpl",
            "snapshot-verification-plan",
            &seed,
        ));
        let session_id = winwincode_domain::VerificationSessionId(derived_id(
            "vsn",
            "snapshot-verification-session",
            &seed,
        ));
        let binding = SnapshotVerificationBinding {
            verification_plan: winwincode_domain::VerificationPlan {
                schema_version: SchemaVersion::WinwincodeV1,
                id: plan_id.clone(),
                candidate_digest: candidate.candidate_digest.clone(),
                work_contract_id: scope.work_contract_id.clone(),
                contract_revision: scope.work_contract_revision.clone(),
                work_item_id: scope.work_item_id.clone(),
                work_item_revision: scope.work_item_revision.clone(),
                work_run_id: scope.work_run_id.clone(),
                plan_revision: winwincode_domain::Revision(1),
                criterion_ids: input.work_item.criterion_ids.clone(),
                commands: input
                    .work_contract
                    .criteria
                    .iter()
                    .filter(|criterion| input.work_item.criterion_ids.contains(&criterion.id))
                    .filter_map(|criterion| criterion.verification_method.clone())
                    .collect(),
                permission_profile: "candidate-read-only".into(),
                required_roles: vec![dispatch.job.execution_profile.clone()],
            },
            verification_session: winwincode_domain::VerificationSession {
                schema_version: SchemaVersion::WinwincodeV1,
                id: session_id.clone(),
                verification_session_id: session_id,
                verification_plan_id: plan_id,
                snapshot_id: snapshot.snapshot_id().clone(),
                candidate_id: candidate.id.clone(),
                work_run_id: scope.work_run_id.clone(),
                attempt: dispatch.lease.attempt,
                created_at: message.receipt.frozen_at.clone(),
                session_identity: winwincode_domain::SessionIdentity {
                    work_run_id: Some(scope.work_run_id.clone()),
                    product_session_id: scope.product_session_id.clone(),
                    worker_session_id: worker_session_id.clone(),
                    codex_thread_id: codex_thread_id.clone(),
                },
            },
            session_binding_id: derived_id("sbn", "snapshot-binding", &seed),
            work_run_id: scope.work_run_id.clone(),
            execution_job_id: dispatch.job.job_id.clone(),
            product_session_id: scope.product_session_id.clone(),
            worker_session_id: Some(worker_session_id),
            codex_thread_id: Some(codex_thread_id),
            verification_role: dispatch.job.execution_profile.clone(),
            attempt: dispatch.lease.attempt,
        };
        let staged =
            SnapshotProductStaged::new(candidate, snapshot.clone(), binding)?.with_state_guard(
                StateRevisionGuard::new(durable.stream_id(), state.revision)?,
            );
        let identity = ReceiptIdentity::new(
            durable.receipt_identity().actor_key().clone(),
            durable.receipt_identity().scope_key().clone(),
            RequestId(derived_id("req", "snapshot-product", &seed)),
        )?;
        commit_snapshot_product(
            self.storage_mut()?,
            identity,
            digest(&serde_json::to_vec(message).map_err(invalid)?),
            &staged,
        )?;
        self.storage_mut()?
            .mark_published(&freeze_event(&request))?;
        let mut dispatch = dispatch.clone();
        dispatch.snapshot_id = Some(snapshot.snapshot_id().clone());
        Ok(SnapshotVerificationDispatchMessage {
            kind: SnapshotVerificationDispatchMessageKind::SnapshotVerify,
            schema_version: SchemaVersion::WinwincodeV1,
            message_id: dispatch.message_id.clone(),
            sent_at: dispatch.sent_at.clone(),
            dispatch,
            snapshot: snapshot.into_contract(),
        })
    }

    /// Replays committed transport intents until dispatch acceptance. Publication
    /// to UI subscribers never acknowledges these internal Worker commands.
    ///
    /// # Errors
    /// Rejects corrupted intents or bindings; obsolete leases are never sent.
    pub fn pending_snapshot_commands(
        &mut self,
        registry: &mut SqliteStorage,
        worker_id: &winwincode_domain::WorkerId,
        worker_instance_id: &winwincode_domain::WorkerInstanceId,
        now: &Instant,
    ) -> Result<Vec<ExecutionPortMessage>, StorageError> {
        let mut commands = Vec::new();
        for event in self.storage_ref()?.pending_events()? {
            let (request, snapshot_id) = match event.topic.as_str() {
                FREEZE_TOPIC => (
                    decode::<SnapshotFreezeRequestMessage>(&event.payload)?,
                    None,
                ),
                DISPATCH_TOPIC => {
                    #[derive(serde::Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct DispatchKey {
                        execution_job_id: winwincode_domain::ExecutionJobId,
                        snapshot_id: SnapshotId,
                        attempt: i64,
                    }
                    let key: DispatchKey = decode(&event.payload)?;
                    let stream = format!(
                        "snapshot-freeze:v1:{}:{}",
                        key.execution_job_id.0, key.attempt
                    );
                    let stored = self.storage_ref()?.load_state(&stream)?.ok_or_else(|| {
                        StorageError::invalid_input("Snapshot dispatch has no freeze request")
                    })?;
                    (decode(&stored.payload)?, Some(key.snapshot_id))
                }
                _ => continue,
            };
            let live = registry
                .execution_registry()?
                .load_live_lease(&request.lease.job_id, now)?;
            if live.as_ref().is_none_or(|lease| {
                crate::execution_port_service::lease_stamp(lease) != request.lease
            }) {
                self.storage_mut()?.mark_published(&event.event_id)?;
                continue;
            }
            if &request.lease.worker_id != worker_id
                || &request.lease.worker_instance_id != worker_instance_id
            {
                continue;
            }
            self.require_snapshot_lease(registry, &request.dispatch, now)?;
            if let Some(snapshot_id) = snapshot_id {
                let snapshot: CanonicalSnapshot = decode(
                    &self
                        .storage_ref()?
                        .load_state(&format!("snapshot-product:v1:{}", snapshot_id.0))?
                        .ok_or_else(|| {
                            StorageError::invalid_input("Snapshot dispatch product is missing")
                        })?
                        .payload,
                )?;
                let ExecutionScope::WorkRunExecutionScope(scope) = &request.dispatch.job.scope
                else {
                    return Err(StorageError::invalid_input(
                        "Snapshot dispatch WorkRun is missing",
                    ));
                };
                winwincode_storage::validate_snapshot_binding(
                    self.storage_ref()?,
                    &winwincode_storage::SnapshotBindingCheck::new(
                        snapshot_id.clone(),
                        request.dispatch.job.job_id.clone(),
                        scope.work_run_id.clone(),
                        scope.product_session_id.clone(),
                        request.lease.attempt,
                    ),
                )?;
                let mut dispatch = request.dispatch;
                dispatch.snapshot_id = Some(snapshot_id);
                commands.push(ExecutionPortMessage::SnapshotVerificationDispatchMessage(
                    SnapshotVerificationDispatchMessage {
                        kind: SnapshotVerificationDispatchMessageKind::SnapshotVerify,
                        schema_version: SchemaVersion::WinwincodeV1,
                        message_id: dispatch.message_id.clone(),
                        sent_at: dispatch.sent_at.clone(),
                        dispatch,
                        snapshot: snapshot.into_contract(),
                    },
                ));
            } else {
                commands.push(ExecutionPortMessage::SnapshotFreezeRequestMessage(request));
            }
        }
        Ok(commands)
    }

    pub(crate) fn validate_supplied_snapshot(
        &self,
        job_id: &winwincode_domain::ExecutionJobId,
        supplied: Option<&SnapshotId>,
    ) -> Result<(), StorageError> {
        let (_, job) = load_durable_execution_job(self.storage_ref()?, job_id)?;
        let snapshot = self.snapshot_for_job(&job)?;
        if supplied != snapshot.as_ref().map(CanonicalSnapshot::snapshot_id) {
            return Err(StorageError::invalid_input(
                "Snapshot binding is missing or foreign",
            ));
        }
        Ok(())
    }

    pub(crate) fn snapshot_for_job(
        &self,
        job: &winwincode_execution_port::generated::ExecutionJob,
    ) -> Result<Option<CanonicalSnapshot>, StorageError> {
        snapshot_for_job(self.storage_ref()?, job)
    }

    fn require_snapshot_lease(
        &self,
        registry: &mut SqliteStorage,
        dispatch: &JobDispatchMessage,
        now: &Instant,
    ) -> Result<(), StorageError> {
        if self.local_database_path() != Some(registry.database_path()) {
            return Err(StorageError::invalid_input(
                "Snapshot registry is not the product database",
            ));
        }
        let lease = registry
            .execution_registry()?
            .load_live_lease(&dispatch.job.job_id, now)?
            .ok_or_else(|| StorageError::invalid_input("Snapshot lease is missing"))?;
        if crate::execution_port_service::lease_stamp(&lease) != dispatch.lease
            || now.0 < dispatch.lease.issued_at.0
            || now.0 >= dispatch.lease.expires_at.0
        {
            return Err(StorageError::invalid_input(
                "Snapshot lease is foreign or expired",
            ));
        }
        Ok(())
    }

    fn sealed_snapshot_candidate(
        &self,
        registry: &mut SqliteStorage,
        scope: &winwincode_domain::RepositoryScope,
        delivery: &Delivery,
    ) -> Result<(Candidate, GitObjectId, Sha256Digest), StorageError> {
        let writer = current_writer(delivery).map_err(invalid)?;
        let terminal = load_terminal(self.storage_ref()?, delivery, &writer.execution_job_id)
            .map_err(invalid)?;
        let artifacts = self
            .artifact_store
            .as_ref()
            .ok_or_else(|| StorageError::invalid_input("Snapshot Artifact store is unavailable"))?;
        let resolver = self.git_source_resolver.as_deref().ok_or_else(|| {
            StorageError::invalid_input("Snapshot Git source resolver is unavailable")
        })?;
        let source = source_for_terminal(
            artifacts,
            resolver,
            scope,
            delivery,
            &terminal,
            &delivery.snapshot().spec.base_revision,
        )
        .map_err(invalid)?;
        // Apply the same current-writer and authorized rework delta checks used
        // by candidate review and verdict resolution before requesting a freeze.
        let frozen = crate::delivery_verdict_authority::freeze_source(
            self.storage_ref()?,
            artifacts,
            resolver,
            scope,
            delivery,
            &source,
            &terminal,
        )
        .map_err(invalid)?;
        let root = self.git_repository_root.as_ref().ok_or_else(|| {
            StorageError::invalid_input("Snapshot controlled Git root is unavailable")
        })?;
        let pin = registry
            .git_candidate_retention(root)
            .map_err(invalid)?
            .load_by_artifact(source.artifact().artifact_id())
            .map_err(invalid)?
            .ok_or_else(|| StorageError::invalid_input("Snapshot Candidate pin is missing"))?;
        if pin.state() != CandidateGitRetentionState::Pinned
            || pin.delivery_id() != delivery.id()
            || pin.artifact_digest() != source.artifact().digest()
            || pin.candidate_commit_id() != source.candidate_commit_id()
            || pin.candidate_tree_id() != source.candidate_tree_id()
            || pin.repository_locator() != source.repository_locator()
        {
            return Err(StorageError::invalid_input(
                "Snapshot Candidate pin differs from source",
            ));
        }
        let candidate = Candidate {
            schema_version: SchemaVersion::WinwincodeV1,
            id: candidate_id_for_writer(writer)?,
            work_contract_id: writer.work_contract_id.clone(),
            contract_revision: writer.work_contract_revision.clone(),
            work_item_id: writer.work_item_id.clone(),
            work_run_id: writer.work_run_id.clone(),
            attempt: i64::try_from(writer.attempt).map_err(invalid)?,
            producer_worker_session_id: writer.worker_session_id.clone().ok_or_else(|| {
                StorageError::invalid_input("Candidate producer session is missing")
            })?,
            candidate_ref: format!(
                "refs/winwincode/candidates/{}",
                source.candidate_commit_id()
            ),
            candidate_digest: CandidateDigest(frozen.candidate_digest().0.clone()),
            base_commit: source.base_commit_id().to_owned(),
            candidate_commit: source.candidate_commit_id().to_owned(),
            candidate_tree: source.candidate_tree_id().to_owned(),
            diff_digest: Sha256Digest(format!("sha256:{}", source.diff_sha256())),
        };
        Ok((
            candidate,
            GitObjectId(source.base_tree_id().to_owned()),
            source.content_digest().clone(),
        ))
    }
}

fn is_verification(dispatch: &JobDispatchMessage) -> bool {
    matches!(
        dispatch.job.execution_profile.as_str(),
        "reviewer" | "verifier" | "adversarial-verifier"
    )
}
fn freeze_stream(dispatch: &JobDispatchMessage) -> String {
    format!(
        "snapshot-freeze:v1:{}:{}",
        dispatch.job.job_id.0, dispatch.lease.attempt
    )
}
fn freeze_event(request: &SnapshotFreezeRequestMessage) -> String {
    format!("snapshot-freeze:{}", request.request_id.0)
}
fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
fn invalid(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
    serde_json::from_slice(bytes).map_err(invalid)
}

pub(crate) fn snapshot_for_job(
    storage: &dyn winwincode_storage::ProductStateStorage,
    job: &winwincode_execution_port::generated::ExecutionJob,
) -> Result<Option<CanonicalSnapshot>, StorageError> {
    if !matches!(
        job.execution_profile.as_str(),
        "reviewer" | "verifier" | "adversarial-verifier"
    ) {
        return Ok(None);
    }
    let stored = storage
        .load_state(&format!(
            "snapshot-freeze:v1:{}:{}",
            job.job_id.0, job.attempt
        ))?
        .ok_or_else(|| {
            StorageError::invalid_input("verification has no Snapshot freeze request")
        })?;
    let request: SnapshotFreezeRequestMessage = decode(&stored.payload)?;
    if request.dispatch.job != *job {
        return Err(StorageError::invalid_input(
            "verification Job changed after freeze",
        ));
    }
    let snapshot_id = SnapshotId(derived_id(
        "snap",
        "candidate-snapshot",
        &digest(request.candidate.id.0.as_bytes()),
    ));
    let snapshot: CanonicalSnapshot = decode(
        &storage
            .load_state(&format!("snapshot-product:v1:{}", snapshot_id.0))?
            .ok_or_else(|| StorageError::invalid_input("verification Snapshot has not committed"))?
            .payload,
    )?;
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        return Err(StorageError::invalid_input("verification has no WorkRun"));
    };
    winwincode_storage::validate_snapshot_binding(
        storage,
        &winwincode_storage::SnapshotBindingCheck::new(
            snapshot_id,
            job.job_id.clone(),
            scope.work_run_id.clone(),
            scope.product_session_id.clone(),
            job.attempt,
        ),
    )?;
    Ok(Some(snapshot))
}

pub(crate) fn candidate_id_for_writer(
    writer: &winwincode_delivery::domain::SessionBinding,
) -> Result<CandidateId, StorageError> {
    let seed = digest(
        &serde_json::to_vec(&(
            &writer.work_contract_id,
            &writer.work_contract_revision,
            &writer.work_run_id,
            &writer.execution_job_id,
            &writer.lease_id,
        ))
        .map_err(invalid)?,
    );
    Ok(CandidateId(derived_id("cnd", "sealed-candidate", &seed)))
}
