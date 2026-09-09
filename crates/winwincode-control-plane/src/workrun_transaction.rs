// SPDX-License-Identifier: Apache-2.0

//! Durable Delivery append for an accepted `WorkRun` dispatch.
//!
//! The scheduler first accepts the Worker dispatch and seals its opaque lease
//! authority. This transaction is the only seam that turns that authority into
//! the canonical Delivery `WorkRun` and its pending `SessionBinding`.

use sha2::{Digest, Sha256};
use winwincode_delivery::domain::Delivery;
use winwincode_delivery::store::{
    AppendDeliveryWorkRun, DeliveryCommand, DeliveryCommandPort, DeliveryStore,
};
use winwincode_domain::{DeliveryId, Revision, SchemaVersion, WorkRun, WorkRunState};
use winwincode_execution_port::generated::{ExecutionJob, ExecutionScope};
use winwincode_storage::{
    DurableOutboxEvent, ExecutionDispatchAuthority, ExecutionDispatchWorkRunProof,
    ProductStateStorage, PublicEventActor, PublicEventSource, ReceiptIdentity, StateCommit,
    StorageError, receipt_actor_key,
};

use crate::delivery_transaction::{delivery_journal_key, delivery_stream_id};
use crate::{
    DeliveryChangeKind, delivery_changed_event_for_scope, instant_from_millis,
    public_repository_scope, repository_scope_from_receipt_key,
};

const EVENT_COMPONENT: &str = "execution-dispatch-workrun";

/// Commits the `WorkRun` and pending binding produced by one accepted dispatch.
#[allow(
    clippy::too_many_lines,
    reason = "Keep accepted dispatch proof and its atomic WorkRun publication together"
)]
pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    durable: &DurableOutboxEvent,
    job: &ExecutionJob,
    authority: &ExecutionDispatchAuthority,
    proof: &ExecutionDispatchWorkRunProof,
    now_millis: u64,
) -> Result<winwincode_storage::CommitReceipt, StorageError> {
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        return Err(StorageError::invalid_input(
            "accepted dispatch is not a Delivery WorkRun",
        ));
    };
    let delivery_id = DeliveryId(
        durable
            .stream_id()
            .strip_prefix("delivery:")
            .ok_or_else(|| StorageError::invalid_input("WorkRun dispatch has no Delivery stream"))?
            .to_owned(),
    );
    if durable.stream_id() != delivery_stream_id(&delivery_id)
        || durable.revision() == 0
        || job.job_id != authority.lease().job_id
    {
        return Err(StorageError::invalid_input(
            "accepted WorkRun dispatch intent is foreign",
        ));
    }
    let run = WorkRun {
        attempt: job.attempt,
        candidate_digest: None,
        codex_thread_id: None,
        contract_revision: scope.work_contract_revision.clone(),
        execution_job_id: job.job_id.clone(),
        fencing_token: authority.lease().fencing_token.0.clone(),
        id: scope.work_run_id.clone(),
        lease_id: authority.lease().lease_id.clone(),
        product_session_id: Some(scope.product_session_id.clone()),
        revision: Revision(1),
        schema_version: SchemaVersion::WinwincodeV1,
        state: WorkRunState::Leased,
        work_contract_id: scope.work_contract_id.clone(),
        worker_id: authority.lease().worker_id.clone(),
        worker_instance_id: authority.lease().worker_instance_id.clone(),
        worker_session_id: authority.worker_session_id().clone(),
        work_item_id: scope.work_item_id.clone(),
        work_item_revision: scope.work_item_revision.clone(),
    };
    proof.verify_work_run(&run, &delivery_id.0)?;

    let request_id = authority.dispatch_request_id().clone();
    let request_digest = request_digest(job, &run)?;
    let scope = repository_scope_from_receipt_key(durable.receipt_identity().scope_key())?;
    let actor = PublicEventActor::System {
        id: winwincode_domain::SystemActorId("sys_00000000000000000000000000".to_owned()),
    };
    let receipt_identity = ReceiptIdentity::new(
        receipt_actor_key(&actor)?,
        durable.receipt_identity().scope_key().clone(),
        request_id.clone(),
    )?;
    let command_digest = winwincode_domain::Sha256Digest(format!("sha256:{request_digest}"));

    // Replay is keyed by the original dispatch identity, not by the current
    // Delivery revision. A later SessionBinding/terminal commit may have
    // advanced the aggregate, but must not make the original append appear
    // stale or manufacture a second WorkRun receipt.
    if let Some(receipt) = storage.load_receipt(&receipt_identity, &command_digest)? {
        return Ok(receipt);
    }

    let state = storage
        .load_state(&delivery_stream_id(&delivery_id))?
        .ok_or_else(|| StorageError::invalid_input("Delivery state is missing for WorkRun"))?;
    let delivery = Delivery::decode_json(&state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if delivery.id() != &delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "Delivery state is inconsistent for WorkRun append",
        ));
    }

    let journal_key = delivery_journal_key(&delivery_id)?;
    let journal = crate::delivery_transaction::StagedDeliveryJournal::new(
        delivery_id.clone(),
        storage.load_journal(&journal_key)?,
    );
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::AppendWorkRun(Box::new(
            AppendDeliveryWorkRun {
                delivery_id: delivery_id.clone(),
                request_id: request_id.clone(),
                request_digest: request_digest.clone(),
                expected_revision: delivery.revision(),
                run,
                authority: authority.clone(),
                proof: proof.clone(),
                now_millis,
            },
        )))
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    let events = if mutation.replayed {
        Vec::new()
    } else {
        vec![delivery_changed_event_for_scope(
            public_repository_scope(&scope),
            &delivery_id,
            mutation.snapshot.revision(),
            DeliveryChangeKind::Advanced,
            instant_from_millis(now_millis)?,
            PublicEventSource::ControlPlane {
                actor,
                component: EVENT_COMPONENT.to_owned(),
            },
        )?]
    };
    let mut commit = StateCommit::new(
        receipt_identity,
        command_digest,
        delivery_stream_id(&delivery_id),
        delivery.revision(),
        mutation
            .snapshot
            .encode_json()
            .map_err(|error| StorageError::invalid_input(error.to_string()))?,
        events,
    );
    if let Some(publication) = publication {
        if mutation.replayed {
            return Err(StorageError::invalid_input(
                "replayed WorkRun append staged a journal publication",
            ));
        }
        commit = commit.with_journal_publication(publication);
    } else if !mutation.replayed {
        return Err(StorageError::invalid_input(
            "new WorkRun append did not stage a journal publication",
        ));
    }
    storage.commit(&commit)
}

fn request_digest(job: &ExecutionJob, run: &WorkRun) -> Result<String, StorageError> {
    let job = serde_json::to_vec(job).map_err(|error| StorageError::adapter(error.to_string()))?;
    let run = serde_json::to_vec(run).map_err(|error| StorageError::adapter(error.to_string()))?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.delivery.workrun.append.v1\0");
    digest.update(job);
    digest.update([0]);
    digest.update(run);
    Ok(format!("{:x}", digest.finalize()))
}
