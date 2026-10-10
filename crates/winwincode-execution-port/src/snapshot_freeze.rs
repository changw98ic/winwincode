use sha2::{Digest, Sha256};
use winwincode_domain::{SchemaVersion, Sha256Digest, is_canonical_prefixed_id};

use crate::generated::{
    ExecutionScope, ExecutionWorkspaceWriteMode, SnapshotFreezeReceipt,
    SnapshotFreezeReceiptMessage, SnapshotFreezeRequestMessage,
};

#[must_use]
pub fn snapshot_role_binding_valid(
    profile: &str,
    snapshot_id: Option<&winwincode_domain::SnapshotId>,
) -> bool {
    let verification = matches!(profile, "reviewer" | "verifier" | "adversarial-verifier");
    match snapshot_id {
        Some(id) => verification && is_canonical_prefixed_id(&id.0, "snap_"),
        None => !verification,
    }
}

pub fn validate_freeze_request(request: &SnapshotFreezeRequestMessage) -> Result<(), &'static str> {
    let dispatch = &request.dispatch;
    let job = &dispatch.job;
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        return Err("Snapshot freeze requires a verification WorkRun");
    };
    let input = job
        .work_input
        .as_ref()
        .ok_or("Snapshot freeze requires WorkRun input")?;
    if request.schema_version != SchemaVersion::WinwincodeV1
        || request.candidate.schema_version != SchemaVersion::WinwincodeV1
        || !is_canonical_prefixed_id(&request.request_id.0, "req_")
        || !is_canonical_prefixed_id(&request.candidate.id.0, "cnd_")
        || request.candidate.attempt < 1
        || [
            &request.candidate.base_commit,
            &request.candidate.candidate_commit,
            &request.candidate.candidate_tree,
            &request.base_tree_id.0,
        ]
        .into_iter()
        .any(|id| {
            !matches!(id.len(), 40 | 64)
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || request.lease.attempt < 1
        || dispatch.lease != request.lease
        || job.job_id != request.lease.job_id
        || job.attempt != request.lease.attempt
        || scope.attempt != job.attempt
        || request.repository_id != job.workspace.repository_id
        || job.workspace.checkout_revision != request.candidate.candidate_commit
        || job.workspace.write_mode != ExecutionWorkspaceWriteMode::ReadOnly
        || dispatch.snapshot_id.is_some()
        || input.snapshot_id.is_some()
        || input.candidate_ref.as_ref() != Some(&request.candidate.candidate_ref)
        || request.candidate.candidate_ref
            != format!(
                "refs/winwincode/candidates/{}",
                request.candidate.candidate_commit
            )
        || scope.work_contract_id != request.candidate.work_contract_id
        || scope.work_contract_revision != request.candidate.contract_revision
        || input.work_contract.id != scope.work_contract_id
        || input.work_contract.revision != scope.work_contract_revision
        || input.work_item.id != scope.work_item_id
        || input.work_item.revision != scope.work_item_revision
        || !matches!(
            job.execution_profile.as_str(),
            "reviewer" | "verifier" | "adversarial-verifier"
        )
    {
        return Err("Snapshot freeze request does not match its planned verification");
    }
    Ok(())
}

pub fn seal_freeze_receipt(
    receipt: &SnapshotFreezeReceipt,
) -> Result<Sha256Digest, serde_json::Error> {
    let mut value: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_value(serde_json::to_value(receipt)?)?;
    value.remove("validationSeal");
    let mut hash = Sha256::new();
    hash.update(b"winwincode.snapshot.freeze-receipt.v1\0");
    hash.update(serde_json::to_vec(&value)?);
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

pub fn validate_freeze_receipt(
    request: &SnapshotFreezeRequestMessage,
    message: &SnapshotFreezeReceiptMessage,
) -> Result<(), &'static str> {
    validate_freeze_request(request)?;
    let receipt = &message.receipt;
    if request.schema_version != SchemaVersion::WinwincodeV1
        || request.candidate.schema_version != SchemaVersion::WinwincodeV1
        || message.schema_version != SchemaVersion::WinwincodeV1
        || receipt.schema_version != SchemaVersion::WinwincodeV1
        || !is_canonical_prefixed_id(&request.request_id.0, "req_")
        || !is_canonical_prefixed_id(&receipt.receipt_id.0, "req_")
        || request.candidate.attempt < 1
        || request.lease.attempt < 1
        || receipt.request_id != request.request_id
        || receipt.candidate_id != request.candidate.id
        || receipt.work_run_id != request.candidate.work_run_id
        || receipt.repository_id != request.repository_id
        || message.lease != request.lease
        || receipt.worker_id != request.lease.worker_id
        || receipt.worker_instance_id != request.lease.worker_instance_id
        || receipt.lease_id != request.lease.lease_id
        || receipt.attempt != request.lease.attempt
        || receipt.fencing_token != request.lease.fencing_token
        || receipt.base_commit_id.0 != request.candidate.base_commit
        || receipt.base_tree_id != request.base_tree_id
        || receipt.candidate_commit_id.0 != request.candidate.candidate_commit
        || receipt.candidate_tree_id.0 != request.candidate.candidate_tree
        || receipt.diff_sha256 != request.candidate.diff_digest
        || receipt.content_digest != request.content_digest
    {
        return Err("Snapshot freeze receipt does not match the durable request");
    }
    if seal_freeze_receipt(receipt).map_err(|_| "Snapshot receipt cannot be encoded")?
        != receipt.validation_seal
    {
        return Err("Snapshot freeze receipt seal changed");
    }
    Ok(())
}
