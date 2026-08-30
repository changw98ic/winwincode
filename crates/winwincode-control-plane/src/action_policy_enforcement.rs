// SPDX-License-Identifier: Apache-2.0

//! Durable Control Plane issuance of Worker action enforcement receipts.
//!
//! Worker input contributes only the normalized action facts and stable
//! invocation identity. Tenant, actor, Job, lease, and session authority are
//! reloaded from the canonical durable execution records before Policy is
//! evaluated or a receipt is signed.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    RepositoryScope, RepositoryScopeKind, SchemaVersion, Sha256Digest, UserActor, UserActorKind,
    UserId,
};
use winwincode_execution_port::{
    action_enforcement::ActionEnforcementIssuer,
    generated::{
        ActionEnforcementDecision, ActionEnforcementReceiptMessage,
        ActionEnforcementReceiptMessageKind, ActionEnforcementRequestMessage,
        ActionEnforcementRequestMessageKind, ActionPolicyKind, ActionPolicyMode,
        ActionPolicyVersionReference,
    },
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyScope,
    NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventScope, ReceiptIdentity,
    SqliteStorage, StateCommit, StorageError, public_actor_from_receipt_key, receipt_actor_key,
    receipt_scope_key, repository_scope_from_receipt_key,
};

use crate::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementError,
    EnterprisePolicyEnforcementRequest, enforce_enterprise_policy,
    execution_port_service::{lease_stamp, load_runtime_replay_authority},
};

const ACTION_RECEIPT_STREAM_PREFIX: &str = "action-enforcement-receipt:";
const ACTION_RECEIPT_TOPIC: &str = "action.enforcement-receipt.issued";
const EVALUATION_TIME_REQUEST_NAMESPACE: &[u8] =
    b"winwincode.action-enforcement-evaluation-time-request.v1";
const EVALUATION_TIME_STREAM_PREFIX: &str = "action-enforcement-evaluation-time:";
const EVALUATION_TIME_SCHEMA_VERSION: &str = "winwincode.action-enforcement-evaluation-time.v1";
const EVALUATION_TIME_TOPIC: &str = "action.enforcement-evaluation-time.frozen";
const RESPONSE_MESSAGE_NAMESPACE: &[u8] = b"winwincode.action-enforcement-response-message.v1";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenActionEvaluationTime {
    schema_version: String,
    request_sha256: Sha256Digest,
    evaluated_at: winwincode_domain::Instant,
}

/// Stable action receipt issuance failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionPolicyEnforcementErrorKind {
    InvalidRequest,
    AuthorityRejected,
    Policy,
    Storage,
    CorruptReceipt,
}

/// Secret-free action receipt issuance error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionPolicyEnforcementError {
    kind: ActionPolicyEnforcementErrorKind,
}

impl ActionPolicyEnforcementError {
    const fn new(kind: ActionPolicyEnforcementErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ActionPolicyEnforcementErrorKind {
        self.kind
    }
}

impl fmt::Display for ActionPolicyEnforcementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("action Policy enforcement receipt issuance failed")
    }
}

impl std::error::Error for ActionPolicyEnforcementError {}

impl From<StorageError> for ActionPolicyEnforcementError {
    fn from(_error: StorageError) -> Self {
        Self::new(ActionPolicyEnforcementErrorKind::Storage)
    }
}

impl From<EnterprisePolicyEnforcementError> for ActionPolicyEnforcementError {
    fn from(_error: EnterprisePolicyEnforcementError) -> Self {
        Self::new(ActionPolicyEnforcementErrorKind::Policy)
    }
}

struct ResolvedActionAuthority {
    actor: UserId,
    scope: RepositoryScope,
    receipt_identity: ReceiptIdentity,
}

/// Issues or exactly replays one immutable action receipt.
///
/// # Errors
///
/// Rejects malformed Worker input, a stale/foreign lease or session, a
/// non-User durable actor, changed request reuse, unavailable Policy/storage,
/// or altered durable receipt bytes.
pub fn issue_action_enforcement_receipt(
    storage: &mut SqliteStorage,
    issuer: &ActionEnforcementIssuer,
    evaluated_at: &winwincode_domain::Instant,
    request: &ActionEnforcementRequestMessage,
) -> Result<ActionEnforcementReceiptMessage, ActionPolicyEnforcementError> {
    validate_request_shape(request)?;
    let authority = resolve_action_authority(storage, evaluated_at, request)?;
    let request_bytes = serde_json::to_vec(request).map_err(|_| invalid_request())?;
    let command_digest = sha256(&request_bytes);
    let frozen_evaluated_at = freeze_evaluation_time(
        storage,
        &authority.receipt_identity,
        &command_digest,
        evaluated_at,
    )?;
    let stream_id = receipt_stream_id(&request.request_id.0);
    if storage
        .load_receipt(&authority.receipt_identity, &command_digest)?
        .is_some()
    {
        return load_receipt(storage, issuer, &stream_id);
    }

    let policy = evaluate_action_policy(storage, request, &authority, frozen_evaluated_at)?;
    let receipt = build_action_receipt(issuer, request, &authority, &policy)?;
    persist_action_receipt(
        storage,
        issuer,
        &authority.receipt_identity,
        &command_digest,
        &stream_id,
        receipt,
    )
}

fn resolve_action_authority(
    storage: &mut SqliteStorage,
    evaluated_at: &winwincode_domain::Instant,
    request: &ActionEnforcementRequestMessage,
) -> Result<ResolvedActionAuthority, ActionPolicyEnforcementError> {
    let (durable, job) =
        crate::delivery_transaction::load_durable_execution_job(storage, &request.job_id)
            .map_err(|_| authority_rejected())?;
    let authority = load_runtime_replay_authority(storage, &job, evaluated_at)
        .map_err(|_| authority_rejected())?;
    if request.lease != lease_stamp(&authority.lease)
        || request.worker_session_id != authority.worker_session_id
        || request.session_identity != authority.session_identity
        || request.sent_at.0 < authority.lease.issued_at.0
        || request.sent_at.0 >= authority.lease.expires_at.0
        || evaluated_at.0 < authority.lease.issued_at.0
        || evaluated_at.0 >= authority.lease.expires_at.0
    {
        return Err(authority_rejected());
    }

    let actor = match public_actor_from_receipt_key(durable.receipt_identity().actor_key())
        .map_err(|_| authority_rejected())?
    {
        PublicEventActor::User { id } => id,
        PublicEventActor::ServiceAccount { .. } | PublicEventActor::System { .. } => {
            return Err(authority_rejected());
        }
    };
    let scope = match repository_scope_from_receipt_key(durable.receipt_identity().scope_key())
        .map_err(|_| authority_rejected())?
    {
        PublicEventScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        },
        PublicEventScope::Organization { .. }
        | PublicEventScope::Workspace { .. }
        | PublicEventScope::Project { .. } => return Err(authority_rejected()),
    };
    if job.workspace.repository_id != scope.repository_id {
        return Err(authority_rejected());
    }

    let receipt_identity = ReceiptIdentity::new(
        receipt_actor_key(&PublicEventActor::User { id: actor.clone() })?,
        receipt_scope_key(&PublicEventScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        })?,
        request.request_id.clone(),
    )?;
    Ok(ResolvedActionAuthority {
        actor,
        scope,
        receipt_identity,
    })
}

fn evaluate_action_policy(
    storage: &mut SqliteStorage,
    request: &ActionEnforcementRequestMessage,
    authority: &ResolvedActionAuthority,
    evaluated_at: winwincode_domain::Instant,
) -> Result<EnterprisePolicyEnforcement, ActionPolicyEnforcementError> {
    let policy_scope = EnterprisePolicyScope::Repository {
        organization_id: authority.scope.organization_id.clone(),
        workspace_id: authority.scope.workspace_id.clone(),
        project_id: authority.scope.project_id.clone(),
        repository_id: authority.scope.repository_id.clone(),
    };
    enforce_enterprise_policy(
        storage,
        &EnterprisePolicyEnforcementRequest {
            actor: EnterprisePolicyActor::User {
                id: authority.actor.clone(),
            },
            base_request_id: request.request_id.clone(),
            scope: policy_scope,
            policy_kind: storage_policy_kind(&request.policy_kind),
            resource: request.resource.clone(),
            subject_sha256: request.subject_sha256.clone(),
            matched_condition_sha256: request.matched_condition_sha256.clone(),
            evaluated_at,
            exception_id: None,
        },
    )
    .map_err(Into::into)
}

fn build_action_receipt(
    issuer: &ActionEnforcementIssuer,
    request: &ActionEnforcementRequestMessage,
    authority: &ResolvedActionAuthority,
    policy: &EnterprisePolicyEnforcement,
) -> Result<ActionEnforcementReceiptMessage, ActionPolicyEnforcementError> {
    let decision = &policy.receipt().audit.decision;
    let mut receipt = ActionEnforcementReceiptMessage {
        actor: UserActor {
            id: authority.actor.clone(),
            kind: UserActorKind::User,
        },
        decision: if policy.is_permitted() {
            ActionEnforcementDecision::Permit
        } else {
            ActionEnforcementDecision::Reject
        },
        evaluated_at: decision.evaluated_at.clone(),
        evaluation_sha256: decision.decision_sha256.clone(),
        job_id: request.job_id.clone(),
        kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
        lease: request.lease.clone(),
        matched_condition_sha256: request.matched_condition_sha256.clone(),
        message_id: response_message_id(&request.request_id.0),
        policy_kind: request.policy_kind.clone(),
        policy_mode: decision.policy_mode.map(action_policy_mode),
        policy_version: decision.policy_version.as_ref().map(|version| {
            ActionPolicyVersionReference {
                effective_definition_sha256: version.effective_definition_sha256.clone(),
                policy_id: version.policy_id.0.clone(),
                version: i64::try_from(version.version).unwrap_or(i64::MAX),
                version_digest: version.version_digest.clone(),
            }
        }),
        receipt_signature: sha256(b"unsigned"),
        request_id: request.request_id.clone(),
        resource: request.resource.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: authority.scope.clone(),
        sent_at: decision.evaluated_at.clone(),
        session_identity: request.session_identity.clone(),
        subject_sha256: request.subject_sha256.clone(),
        worker_session_id: request.worker_session_id.clone(),
    };
    issuer.sign(&mut receipt).map_err(|_| corrupt_receipt())?;
    Ok(receipt)
}

fn persist_action_receipt(
    storage: &mut SqliteStorage,
    issuer: &ActionEnforcementIssuer,
    receipt_identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    stream_id: &str,
    receipt: ActionEnforcementReceiptMessage,
) -> Result<ActionEnforcementReceiptMessage, ActionPolicyEnforcementError> {
    let receipt_bytes = serde_json::to_vec(&receipt).map_err(|_| corrupt_receipt())?;
    match storage.commit(&StateCommit::new(
        receipt_identity.clone(),
        command_digest.clone(),
        stream_id.to_owned(),
        0,
        receipt_bytes.clone(),
        vec![NewOutboxEvent::internal(
            stream_id.to_owned(),
            ACTION_RECEIPT_TOPIC,
            receipt_bytes,
        )],
    )) {
        Ok(_) => Ok(receipt),
        Err(error) => {
            if storage
                .load_receipt(receipt_identity, command_digest)?
                .is_some()
            {
                load_receipt(storage, issuer, stream_id)
            } else {
                Err(error.into())
            }
        }
    }
}

fn freeze_evaluation_time(
    storage: &mut SqliteStorage,
    action_identity: &ReceiptIdentity,
    request_sha256: &Sha256Digest,
    evaluated_at: &winwincode_domain::Instant,
) -> Result<winwincode_domain::Instant, ActionPolicyEnforcementError> {
    let request_id = derived_request_id(
        EVALUATION_TIME_REQUEST_NAMESPACE,
        action_identity.request_id().0.as_bytes(),
    );
    let identity = ReceiptIdentity::new(
        action_identity.actor_key().clone(),
        action_identity.scope_key().clone(),
        request_id,
    )?;
    let stream_id = format!(
        "{EVALUATION_TIME_STREAM_PREFIX}{:x}",
        Sha256::digest(action_identity.request_id().0.as_bytes())
    );
    if storage.load_receipt(&identity, request_sha256)?.is_some() {
        return load_frozen_evaluation_time(storage, &stream_id, request_sha256);
    }
    let frozen = FrozenActionEvaluationTime {
        schema_version: EVALUATION_TIME_SCHEMA_VERSION.to_owned(),
        request_sha256: request_sha256.clone(),
        evaluated_at: evaluated_at.clone(),
    };
    let payload = serde_json::to_vec(&frozen).map_err(|_| corrupt_receipt())?;
    match storage.commit(&StateCommit::new(
        identity.clone(),
        request_sha256.clone(),
        stream_id.clone(),
        0,
        payload.clone(),
        vec![NewOutboxEvent::internal(
            stream_id.clone(),
            EVALUATION_TIME_TOPIC,
            payload,
        )],
    )) {
        Ok(_) => Ok(frozen.evaluated_at),
        Err(error) => {
            if storage.load_receipt(&identity, request_sha256)?.is_some() {
                load_frozen_evaluation_time(storage, &stream_id, request_sha256)
            } else {
                Err(error.into())
            }
        }
    }
}

fn load_frozen_evaluation_time(
    storage: &dyn ProductStateStorage,
    stream_id: &str,
    request_sha256: &Sha256Digest,
) -> Result<winwincode_domain::Instant, ActionPolicyEnforcementError> {
    let stored = storage.load_state(stream_id)?.ok_or_else(corrupt_receipt)?;
    if stored.stream_id != stream_id || stored.revision != 1 {
        return Err(corrupt_receipt());
    }
    let frozen: FrozenActionEvaluationTime =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt_receipt())?;
    let canonical = serde_json::to_vec(&frozen).map_err(|_| corrupt_receipt())?;
    if canonical != stored.payload
        || frozen.schema_version != EVALUATION_TIME_SCHEMA_VERSION
        || frozen.request_sha256 != *request_sha256
    {
        return Err(corrupt_receipt());
    }
    Ok(frozen.evaluated_at)
}

fn load_receipt(
    storage: &dyn ProductStateStorage,
    issuer: &ActionEnforcementIssuer,
    stream_id: &str,
) -> Result<ActionEnforcementReceiptMessage, ActionPolicyEnforcementError> {
    let stored = storage.load_state(stream_id)?.ok_or_else(corrupt_receipt)?;
    if stored.stream_id != stream_id || stored.revision != 1 {
        return Err(corrupt_receipt());
    }
    let receipt: ActionEnforcementReceiptMessage =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt_receipt())?;
    if serde_json::to_vec(&receipt).map_err(|_| corrupt_receipt())? != stored.payload {
        return Err(corrupt_receipt());
    }
    issuer
        .verify_signature(&receipt)
        .map_err(|_| corrupt_receipt())?;
    Ok(receipt)
}

fn validate_request_shape(
    request: &ActionEnforcementRequestMessage,
) -> Result<(), ActionPolicyEnforcementError> {
    let mut conditions = request.matched_condition_sha256.clone();
    conditions.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    conditions.dedup();
    if request.kind != ActionEnforcementRequestMessageKind::ActionEnforcementRequest
        || request.schema_version != SchemaVersion::WinwincodeV1
        || request.resource.is_empty()
        || request.resource.len() > 512
        || conditions != request.matched_condition_sha256
        || conditions.len() > 64
        || !canonical_id(&request.message_id.0, "xmsg_")
        || !canonical_id(&request.request_id.0, "req_")
        || !digest(&request.subject_sha256.0)
        || request
            .matched_condition_sha256
            .iter()
            .any(|condition| !digest(&condition.0))
    {
        return Err(invalid_request());
    }
    Ok(())
}

fn storage_policy_kind(kind: &ActionPolicyKind) -> EnterprisePolicyKind {
    match kind {
        ActionPolicyKind::Repository => EnterprisePolicyKind::Repository,
        ActionPolicyKind::Tool => EnterprisePolicyKind::Tool,
        ActionPolicyKind::Network => EnterprisePolicyKind::Network,
    }
}

fn action_policy_mode(mode: EnterprisePolicyMode) -> ActionPolicyMode {
    match mode {
        EnterprisePolicyMode::Enforce => ActionPolicyMode::Enforce,
        EnterprisePolicyMode::Audit => ActionPolicyMode::Audit,
    }
}

fn receipt_stream_id(request_id: &str) -> String {
    format!(
        "{ACTION_RECEIPT_STREAM_PREFIX}{:x}",
        Sha256::digest(request_id.as_bytes())
    )
}

fn derived_request_id(namespace: &[u8], identity: &[u8]) -> winwincode_domain::RequestId {
    let digest = Sha256::new()
        .chain_update(namespace)
        .chain_update(identity)
        .finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    winwincode_domain::RequestId(format!("req_{suffix}"))
}

fn response_message_id(request_id: &str) -> winwincode_domain::ExecutionMessageId {
    let digest = Sha256::new()
        .chain_update(RESPONSE_MESSAGE_NAMESPACE)
        .chain_update(request_id.as_bytes())
        .finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    winwincode_domain::ExecutionMessageId(format!("xmsg_{suffix}"))
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

fn digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

const fn invalid_request() -> ActionPolicyEnforcementError {
    ActionPolicyEnforcementError::new(ActionPolicyEnforcementErrorKind::InvalidRequest)
}

const fn authority_rejected() -> ActionPolicyEnforcementError {
    ActionPolicyEnforcementError::new(ActionPolicyEnforcementErrorKind::AuthorityRejected)
}

const fn corrupt_receipt() -> ActionPolicyEnforcementError {
    ActionPolicyEnforcementError::new(ActionPolicyEnforcementErrorKind::CorruptReceipt)
}
