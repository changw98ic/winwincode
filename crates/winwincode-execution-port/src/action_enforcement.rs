// SPDX-License-Identifier: Apache-2.0

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionMessageId, Instant, RequestId, SchemaVersion, Sha256Digest};

use crate::{
    action_gateway::WorkerActionRequest,
    action_normalizer::{ActionNormalization, ActionSource, ObservedFact, normalize_action},
    generated::{
        ActionEnforcementDecision, ActionEnforcementReceiptMessage,
        ActionEnforcementReceiptMessageKind, ActionEnforcementRequestMessage,
        ActionEnforcementRequestMessageKind, ActionPolicyKind, ExecutionLeaseStamp,
    },
};

const POLICY_SUBJECT_NAMESPACE: &[u8] = b"winwincode.action-policy-enforcement-subject.v1";
const POLICY_CONDITION_NAMESPACE: &[u8] = b"winwincode.action-policy-enforcement-condition.v1";
const RECEIPT_USE_SCHEMA_VERSION: &str = "winwincode.action-enforcement-use.v1";

pub fn validate_action_receipt(
    action: &WorkerActionRequest,
    receipt: &ActionEnforcementReceiptMessage,
) -> Result<ActionNormalization, ActionEnforcementError> {
    if receipt.decision != ActionEnforcementDecision::Permit {
        return Err(ActionEnforcementError::ReceiptMismatch);
    }
    validate_action_receipt_outcome(action, receipt)
}

pub fn validate_action_receipt_outcome(
    action: &WorkerActionRequest,
    receipt: &ActionEnforcementReceiptMessage,
) -> Result<ActionNormalization, ActionEnforcementError> {
    validate_action_receipt_outcome_with_current_lease(action, receipt, &action.authority.lease)
}

pub fn validate_action_receipt_outcome_with_current_lease(
    action: &WorkerActionRequest,
    receipt: &ActionEnforcementReceiptMessage,
    current_lease: &ExecutionLeaseStamp,
) -> Result<ActionNormalization, ActionEnforcementError> {
    if !crate::execution_identity::retained_lease_matches_current(
        &action.authority.lease,
        current_lease,
    ) {
        return Err(ActionEnforcementError::ReceiptMismatch);
    }
    let normalization = normalize_action(&action.intent, &action.request)
        .map_err(|_| ActionEnforcementError::InvalidAction)?;
    if !normalization.comparison.matches {
        return Err(ActionEnforcementError::InvalidAction);
    }
    let facts = action_enforcement_facts(&normalization)?;
    if receipt.kind != ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt
        || receipt.schema_version != SchemaVersion::WinwincodeV1
        || receipt.request_id != action.invocation_request_id
        || receipt.job_id != action.authority.lease.job_id
        || receipt.lease != action.authority.lease
        || receipt.worker_session_id != action.authority.worker_session_id
        || receipt.session_identity != action.authority.session_identity
        || receipt.policy_kind != facts.policy_kind
        || receipt.resource != facts.resource
        || receipt.subject_sha256 != facts.subject_sha256
        || receipt.matched_condition_sha256 != facts.matched_condition_sha256
        || receipt.sent_at != receipt.evaluated_at
        || !crate::execution_identity::canonical_instant(&receipt.evaluated_at)
        || receipt.evaluated_at.0 < current_lease.issued_at.0
        || receipt.evaluated_at.0 >= current_lease.expires_at.0
        || !canonical_id(&receipt.message_id.0, "xmsg_")
        || !canonical_id(&receipt.request_id.0, "req_")
        || receipt.actor.kind != winwincode_domain::UserActorKind::User
        || receipt.scope.kind != winwincode_domain::RepositoryScopeKind::Repository
        || !canonical_id(&receipt.actor.id.0, "usr_")
        || !canonical_repository_scope(receipt)
        || !valid_policy_reference(receipt)
        || !digest(&receipt.evaluation_sha256.0)
    {
        return Err(ActionEnforcementError::ReceiptMismatch);
    }
    Ok(normalization)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionEnforcementFacts {
    pub policy_kind: ActionPolicyKind,
    pub resource: String,
    pub subject_sha256: Sha256Digest,
    pub matched_condition_sha256: Vec<Sha256Digest>,
}

pub fn prepare_action_enforcement_request(
    message_id: ExecutionMessageId,
    sent_at: Instant,
    action: &WorkerActionRequest,
) -> Result<ActionEnforcementRequestMessage, ActionEnforcementError> {
    let normalization = normalize_action(&action.intent, &action.request)
        .map_err(|_| ActionEnforcementError::InvalidAction)?;
    if !normalization.comparison.matches {
        return Err(ActionEnforcementError::InvalidAction);
    }
    let facts = action_enforcement_facts(&normalization)?;
    Ok(ActionEnforcementRequestMessage {
        job_id: action.authority.lease.job_id.clone(),
        kind: ActionEnforcementRequestMessageKind::ActionEnforcementRequest,
        lease: action.authority.lease.clone(),
        matched_condition_sha256: facts.matched_condition_sha256,
        message_id,
        policy_kind: facts.policy_kind,
        request_id: action.invocation_request_id.clone(),
        resource: facts.resource,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at,
        session_identity: action.authority.session_identity.clone(),
        subject_sha256: facts.subject_sha256,
        worker_session_id: action.authority.worker_session_id.clone(),
    })
}

pub fn action_enforcement_facts(
    normalization: &ActionNormalization,
) -> Result<ActionEnforcementFacts, ActionEnforcementError> {
    let encoded =
        serde_json::to_vec(normalization).map_err(|_| ActionEnforcementError::Encoding)?;
    let policy_kind = match normalization.observed.source {
        ActionSource::File | ActionSource::Git => ActionPolicyKind::Repository,
        ActionSource::Network => ActionPolicyKind::Network,
        ActionSource::Shell | ActionSource::Mcp => ActionPolicyKind::Tool,
    };
    let source = enum_name(&normalization.observed.source)?;
    let operation = enum_name(&normalization.observed.operation)?;
    let mut conditions = vec![
        policy_condition_sha256(&format!("source:{source}")),
        policy_condition_sha256(&format!("operation:{operation}")),
    ];
    conditions.extend(
        normalization
            .observed
            .facts
            .iter()
            .copied()
            .map(condition_for_observed_fact)
            .collect::<Result<Vec<_>, _>>()?,
    );
    conditions.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    conditions.dedup();
    let resource_identity = serde_json::to_vec(&(
        &normalization.observed.source,
        &normalization.observed.targets,
    ))
    .map_err(|_| ActionEnforcementError::Encoding)?;
    let resource = format!(
        "action:{source}:{}",
        namespaced_digest(b"winwincode.action-resource.v2", &resource_identity).0
    );
    Ok(ActionEnforcementFacts {
        policy_kind,
        resource,
        subject_sha256: namespaced_digest(POLICY_SUBJECT_NAMESPACE, &encoded),
        matched_condition_sha256: conditions,
    })
}

#[must_use]
pub fn policy_condition_sha256(condition: &str) -> Sha256Digest {
    namespaced_digest(POLICY_CONDITION_NAMESPACE, condition.as_bytes())
}

fn condition_for_observed_fact(fact: ObservedFact) -> Result<Sha256Digest, ActionEnforcementError> {
    Ok(policy_condition_sha256(&format!(
        "fact:{}",
        enum_name(&fact)?
    )))
}

fn enum_name(value: &impl Serialize) -> Result<String, ActionEnforcementError> {
    let encoded = serde_json::to_string(value).map_err(|_| ActionEnforcementError::Encoding)?;
    encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(str::to_owned)
        .ok_or(ActionEnforcementError::Encoding)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionReceiptClaim {
    Fresh,
    AlreadyConsumed,
}

pub trait ActionReceiptUseStore {
    fn claim(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionReceiptClaim, ActionReceiptUseError>;
}

pub struct FileActionReceiptUseStore {
    directory: PathBuf,
}

impl FileActionReceiptUseStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ActionReceiptUseError> {
        let directory = root.as_ref().join("action-enforcement-receipts");
        fs::create_dir_all(&directory).map_err(|_| ActionReceiptUseError::Storage)?;
        restrict_directory(&directory)?;
        sync_directory(&directory)?;
        Ok(Self { directory })
    }

    pub fn has_consumed(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<bool, ActionReceiptUseError> {
        if !canonical_id(&receipt.request_id.0, "req_") {
            return Err(ActionReceiptUseError::Conflict);
        }
        let key = format!("{:x}.json", Sha256::digest(receipt.request_id.0.as_bytes()));
        let bytes = match fs::read(self.directory.join(key)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(ActionReceiptUseError::Storage),
        };
        let decoded: StoredReceiptUse =
            serde_json::from_slice(&bytes).map_err(|_| ActionReceiptUseError::Storage)?;
        let canonical = serde_json::to_vec(&decoded).map_err(|_| ActionReceiptUseError::Storage)?;
        if canonical != bytes
            || decoded.schema_version != RECEIPT_USE_SCHEMA_VERSION
            || decoded.request_id != receipt.request_id
            || decoded.receipt != *receipt
        {
            return Err(ActionReceiptUseError::Conflict);
        }
        Ok(true)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReceiptUse {
    schema_version: String,
    request_id: RequestId,
    receipt: ActionEnforcementReceiptMessage,
}

impl ActionReceiptUseStore for FileActionReceiptUseStore {
    fn claim(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionReceiptClaim, ActionReceiptUseError> {
        if !canonical_id(&receipt.request_id.0, "req_") {
            return Err(ActionReceiptUseError::Conflict);
        }
        let key = format!("{:x}.json", Sha256::digest(receipt.request_id.0.as_bytes()));
        let path = self.directory.join(key);
        let stored = StoredReceiptUse {
            schema_version: RECEIPT_USE_SCHEMA_VERSION.to_owned(),
            request_id: receipt.request_id.clone(),
            receipt: receipt.clone(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(|_| ActionReceiptUseError::Storage)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|_| ActionReceiptUseError::Storage)?;
                sync_directory(&self.directory)?;
                Ok(ActionReceiptClaim::Fresh)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                restrict_file(&path)?;
                let mut file = OpenOptions::new()
                    .read(true)
                    .open(path)
                    .map_err(|_| ActionReceiptUseError::Storage)?;
                let mut existing = Vec::new();
                file.read_to_end(&mut existing)
                    .map_err(|_| ActionReceiptUseError::Storage)?;
                let decoded: StoredReceiptUse = serde_json::from_slice(&existing)
                    .map_err(|_| ActionReceiptUseError::Storage)?;
                let canonical =
                    serde_json::to_vec(&decoded).map_err(|_| ActionReceiptUseError::Storage)?;
                if canonical != existing
                    || decoded.schema_version != RECEIPT_USE_SCHEMA_VERSION
                    || decoded.request_id != receipt.request_id
                {
                    return Err(ActionReceiptUseError::Conflict);
                }
                if decoded.receipt != *receipt {
                    return Err(ActionReceiptUseError::Conflict);
                }
                Ok(ActionReceiptClaim::AlreadyConsumed)
            }
            Err(_) => Err(ActionReceiptUseError::Storage),
        }
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), ActionReceiptUseError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| ActionReceiptUseError::Storage)
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), ActionReceiptUseError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), ActionReceiptUseError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| ActionReceiptUseError::Storage)
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), ActionReceiptUseError> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionReceiptUseError {
    Conflict,
    Storage,
}

impl fmt::Display for ActionReceiptUseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conflict => "action receipt conflicts with a prior invocation",
            Self::Storage => "action receipt durable state is unavailable",
        })
    }
}

impl std::error::Error for ActionReceiptUseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionEnforcementError {
    InvalidAction,
    ReceiptMismatch,
    Encoding,
}

impl fmt::Display for ActionEnforcementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidAction => "action enforcement input is invalid",
            Self::ReceiptMismatch => "action enforcement receipt does not match the action",
            Self::Encoding => "action enforcement receipt encoding failed",
        })
    }
}

impl std::error::Error for ActionEnforcementError {}

fn namespaced_digest(namespace: &[u8], value: &[u8]) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::new()
            .chain_update(namespace)
            .chain_update(value)
            .finalize()
    ))
}

fn valid_policy_reference(receipt: &ActionEnforcementReceiptMessage) -> bool {
    match (&receipt.policy_mode, &receipt.policy_version) {
        (None, None) => true,
        (Some(_), Some(reference)) => {
            canonical_id(&reference.policy_id, "pol_")
                && (1..=9_007_199_254_740_991).contains(&reference.version)
                && digest(&reference.version_digest.0)
                && digest(&reference.effective_definition_sha256.0)
        }
        _ => false,
    }
}

fn canonical_repository_scope(receipt: &ActionEnforcementReceiptMessage) -> bool {
    canonical_id(&receipt.scope.organization_id.0, "org_")
        && canonical_id(&receipt.scope.workspace_id.0, "wsp_")
        && canonical_id(&receipt.scope.project_id.0, "prj_")
        && canonical_id(&receipt.scope.repository_id.0, "rep_")
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

fn sync_directory(path: &Path) -> Result<(), ActionReceiptUseError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ActionReceiptUseError::Storage)
}
