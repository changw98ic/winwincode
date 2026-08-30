// SPDX-License-Identifier: Apache-2.0

//! Immutable dispatch-payload rotation for scheduler-owned Worker replacement.

use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;

use crate::{ExecutionJobRecord, StorageError};

const MAX_EXECUTION_ATTEMPT: u64 = 1_000;

pub(crate) struct ReplacementDispatchPayload {
    pub attempt: u64,
    pub bytes: Vec<u8>,
}

pub(crate) fn replacement_dispatch_payload(
    job: &ExecutionJobRecord,
) -> Result<ReplacementDispatchPayload, StorageError> {
    let attempt = job
        .attempt
        .checked_add(1)
        .filter(|attempt| *attempt <= MAX_EXECUTION_ATTEMPT)
        .ok_or_else(|| StorageError::invalid_input("execution replacement attempt is exhausted"))?;
    let mut decoded: Value = serde_json::from_slice(&job.dispatch_payload).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload is not valid JSON")
    })?;
    let object = decoded.as_object_mut().ok_or_else(|| {
        StorageError::adapter("execution replacement dispatch payload is not an object")
    })?;
    let stored_attempt = object
        .get("attempt")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            StorageError::adapter("execution replacement dispatch attempt is missing")
        })?;
    if stored_attempt != job.attempt {
        return Err(StorageError::adapter(
            "execution replacement dispatch attempt differs from its queue record",
        ));
    }
    object.insert("attempt".to_owned(), Value::Number(Number::from(attempt)));
    let bytes = serde_json::to_vec(&decoded).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload cannot be encoded")
    })?;
    Ok(ReplacementDispatchPayload { attempt, bytes })
}

pub(crate) fn logical_dispatch_digest(
    dispatch_payload: &[u8],
) -> Result<Sha256Digest, StorageError> {
    let mut decoded: Value = serde_json::from_slice(dispatch_payload).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload is not valid JSON")
    })?;
    let object = decoded.as_object_mut().ok_or_else(|| {
        StorageError::adapter("execution replacement dispatch payload is not an object")
    })?;
    if object.remove("attempt").is_none() {
        return Err(StorageError::adapter(
            "execution replacement dispatch attempt is missing",
        ));
    }
    let encoded = serde_json::to_vec(&decoded).map_err(|_| {
        StorageError::adapter("logical execution replacement payload cannot be encoded")
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
        RequestId, Sha256Digest, WorkspaceId,
    };

    use super::*;
    use crate::{ExecutionJobState, ExecutionQueueScope};

    fn record(payload: &Value) -> ExecutionJobRecord {
        ExecutionJobRecord {
            scope: ExecutionQueueScope {
                organization_id: OrganizationId("org_00000000000000000000000001".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000002".into()),
                project_id: ProjectId("prj_00000000000000000000000003".into()),
                repository_id: RepositoryId("rep_00000000000000000000000004".into()),
                product_session_id: ProductSessionId("psn_00000000000000000000000005".into()),
                delivery_id: None,
            },
            job_id: ExecutionJobId("job_00000000000000000000000006".into()),
            submission_request_id: RequestId("req_00000000000000000000000007".into()),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            dispatch_payload: serde_json::to_vec(payload).expect("payload"),
            state: ExecutionJobState::Running,
            attempt: 1,
            revision: 3,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: Instant("2027-10-01T10:00:01.000Z".into()),
            updated_at: Instant("2027-10-01T10:00:04.000Z".into()),
            cancellation: None,
        }
    }

    #[test]
    fn replacement_changes_only_the_attempt() {
        let original = serde_json::json!({
            "attempt": 1,
            "executionProfile": "local-codex",
            "goal": "keep every immutable field",
            "jobId": "job_00000000000000000000000006",
            "payloadDigest": format!("sha256:{}", "a".repeat(64)),
        });
        let replacement =
            replacement_dispatch_payload(&record(&original)).expect("replacement payload");
        assert_eq!(replacement.attempt, 2);
        let mut expected = original;
        expected["attempt"] = Value::from(2);
        assert_eq!(
            serde_json::from_slice::<Value>(&replacement.bytes).expect("decode"),
            expected
        );
    }

    #[test]
    fn replacement_rejects_a_payload_with_foreign_attempt_authority() {
        let error = replacement_dispatch_payload(&record(&serde_json::json!({"attempt": 2})))
            .err()
            .expect("foreign attempt must fail");
        assert_eq!(error.kind(), crate::StorageErrorKind::Adapter);
    }
}
