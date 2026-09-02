// SPDX-License-Identifier: Apache-2.0

//! Control Plane-issued receipts for Worker action execution.
//!
//! The Worker derives Policy facts from the same normalized tool request it is
//! about to execute. The Control Plane signs those facts after evaluating the
//! canonical enterprise Policy ledger. A receipt is claimed durably before the
//! tool executor is called, so an exact replay never repeats the side effect.

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
        ActionEnforcementRequestMessageKind, ActionPolicyKind,
    },
};

const POLICY_SUBJECT_NAMESPACE: &[u8] = b"winwincode.enterprise-policy-enforcement-subject.v1";
const POLICY_CONDITION_NAMESPACE: &[u8] = b"winwincode.enterprise-policy-enforcement-condition.v1";
const RECEIPT_SIGNATURE_NAMESPACE: &[u8] = b"winwincode.action-enforcement-receipt-signature.v1";
const RECEIPT_USE_SCHEMA_VERSION: &str = "winwincode.action-enforcement-use.v1";

/// Secret signing key shared only by the issuing Control Plane and its Worker.
#[derive(Clone, Eq, PartialEq)]
pub struct ActionEnforcementSigningKey([u8; 32]);

impl ActionEnforcementSigningKey {
    /// Constructs a signing key from 256 bits of caller-owned secret material.
    ///
    /// # Errors
    ///
    /// Rejects the all-zero value rather than installing a public sentinel key.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, ActionEnforcementError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(ActionEnforcementError::InvalidKey);
        }
        Ok(Self(bytes))
    }

    /// Consumes the Worker-owned copy into a receipt verifier.
    #[must_use]
    pub const fn into_verifier(self) -> ActionEnforcementVerifier {
        ActionEnforcementVerifier(self)
    }
}

/// Control Plane receipt signer.
#[derive(Clone)]
pub struct ActionEnforcementIssuer(ActionEnforcementSigningKey);

impl ActionEnforcementIssuer {
    /// Installs the caller-owned action receipt signing key.
    #[must_use]
    pub const fn new(key: ActionEnforcementSigningKey) -> Self {
        Self(key)
    }

    /// Signs the complete immutable receipt in place.
    ///
    /// # Errors
    ///
    /// Returns an encoding error when the generated receipt cannot be
    /// represented as canonical JSON.
    pub fn sign(
        &self,
        receipt: &mut ActionEnforcementReceiptMessage,
    ) -> Result<(), ActionEnforcementError> {
        receipt.receipt_signature = receipt_signature(&self.0, receipt)?;
        Ok(())
    }

    /// Returns the matching Worker verifier without exposing raw key bytes.
    #[must_use]
    pub fn verifier(&self) -> ActionEnforcementVerifier {
        ActionEnforcementVerifier(self.0.clone())
    }

    /// Verifies that a durable receipt still has its exact issued signature.
    ///
    /// # Errors
    ///
    /// Rejects altered or non-canonical receipt bytes.
    pub fn verify_signature(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<(), ActionEnforcementError> {
        let expected = receipt_signature(&self.0, receipt)?;
        if constant_time_eq(
            expected.0.as_bytes(),
            receipt.receipt_signature.0.as_bytes(),
        ) {
            Ok(())
        } else {
            Err(ActionEnforcementError::InvalidSignature)
        }
    }
}

/// Worker-side verifier for Control Plane action receipts.
#[derive(Clone)]
pub struct ActionEnforcementVerifier(ActionEnforcementSigningKey);

impl ActionEnforcementVerifier {
    /// Verifies the signature and every action, invocation, lease, session, and
    /// Policy fact bound into a permit receipt.
    ///
    /// # Errors
    ///
    /// Rejects a denied, malformed, forged, stale, cross-action, or
    /// cross-authority receipt before the action can be claimed or executed.
    pub fn verify(
        &self,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionNormalization, ActionEnforcementError> {
        if receipt.decision != ActionEnforcementDecision::Permit {
            return Err(ActionEnforcementError::ReceiptMismatch);
        }
        self.verify_outcome(action, receipt)
    }

    /// Verifies the signature and immutable action facts for either a permit
    /// or reject outcome. Callers must branch on the verified decision before
    /// entering a tool handler.
    ///
    /// # Errors
    ///
    /// Rejects malformed, forged, stale, cross-action, or cross-authority receipts.
    pub fn verify_outcome(
        &self,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionNormalization, ActionEnforcementError> {
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
            || receipt.evaluated_at.0 < receipt.lease.issued_at.0
            || receipt.evaluated_at.0 >= receipt.lease.expires_at.0
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
        let expected = receipt_signature(&self.0, receipt)?;
        if !constant_time_eq(
            expected.0.as_bytes(),
            receipt.receipt_signature.0.as_bytes(),
        ) {
            return Err(ActionEnforcementError::InvalidSignature);
        }
        Ok(normalization)
    }
}

/// Stable action facts sent to the Control Plane and bound into its receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionEnforcementFacts {
    pub policy_kind: ActionPolicyKind,
    pub resource: String,
    pub subject_sha256: Sha256Digest,
    pub matched_condition_sha256: Vec<Sha256Digest>,
}

/// Builds the sole generated enforcement request from a pending normalized action.
///
/// # Errors
///
/// Rejects invalid or intent-mismatched actions before any Worker message is emitted.
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

/// Derives secret-free Policy facts from the canonical normalization.
///
/// # Errors
///
/// Returns an encoding error only if the typed normalization cannot be encoded.
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

/// Computes the shared enterprise Policy condition digest.
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

/// Outcome of the mandatory durable receipt claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionReceiptClaim {
    Fresh,
    AlreadyConsumed,
}

/// Durable store used by the Worker immediately before a side effect.
pub trait ActionReceiptUseStore {
    /// Claims a signed receipt exactly once.
    ///
    /// # Errors
    ///
    /// Returns `Conflict` when an invocation identity was already claimed by a
    /// different receipt, and `Storage` for unavailable durable state.
    fn claim(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionReceiptClaim, ActionReceiptUseError>;
}

/// File-backed Worker receipt store. Each invocation is durably created once.
pub struct FileActionReceiptUseStore {
    directory: PathBuf,
}

impl FileActionReceiptUseStore {
    /// Opens the action receipt directory owned by one Worker data root.
    ///
    /// # Errors
    ///
    /// Returns `Storage` if the directory cannot be created or synchronized.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ActionReceiptUseError> {
        let directory = root.as_ref().join("action-enforcement-receipts");
        fs::create_dir_all(&directory).map_err(|_| ActionReceiptUseError::Storage)?;
        restrict_directory(&directory)?;
        sync_directory(&directory)?;
        Ok(Self { directory })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReceiptUse {
    schema_version: String,
    request_id: RequestId,
    receipt_signature: Sha256Digest,
}

impl ActionReceiptUseStore for FileActionReceiptUseStore {
    fn claim(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionReceiptClaim, ActionReceiptUseError> {
        if !canonical_id(&receipt.request_id.0, "req_") || !digest(&receipt.receipt_signature.0) {
            return Err(ActionReceiptUseError::Conflict);
        }
        let key = format!("{:x}.json", Sha256::digest(receipt.request_id.0.as_bytes()));
        let path = self.directory.join(key);
        let stored = StoredReceiptUse {
            schema_version: RECEIPT_USE_SCHEMA_VERSION.to_owned(),
            request_id: receipt.request_id.clone(),
            receipt_signature: receipt.receipt_signature.clone(),
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
                if decoded.receipt_signature != receipt.receipt_signature {
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

/// Stable Worker receipt claim failure.
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

/// Stable receipt preparation or verification failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionEnforcementError {
    InvalidKey,
    InvalidAction,
    ReceiptMismatch,
    InvalidSignature,
    Encoding,
}

impl fmt::Display for ActionEnforcementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidKey => "action enforcement signing key is invalid",
            Self::InvalidAction => "action enforcement input is invalid",
            Self::ReceiptMismatch => "action enforcement receipt does not match the action",
            Self::InvalidSignature => "action enforcement receipt signature is invalid",
            Self::Encoding => "action enforcement receipt encoding failed",
        })
    }
}

impl std::error::Error for ActionEnforcementError {}

fn receipt_signature(
    key: &ActionEnforcementSigningKey,
    receipt: &ActionEnforcementReceiptMessage,
) -> Result<Sha256Digest, ActionEnforcementError> {
    let mut unsigned = receipt.clone();
    unsigned.receipt_signature = Sha256Digest("sha256:".to_owned() + &"0".repeat(64));
    let bytes = serde_json::to_vec(&unsigned).map_err(|_| ActionEnforcementError::Encoding)?;
    Ok(Sha256Digest(format!(
        "sha256:{}",
        hex(&hmac_sha256(&key.0, &bytes))
    )))
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for (index, byte) in block.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let inner = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(RECEIPT_SIGNATURE_NAMESPACE)
        .chain_update(value)
        .finalize();
    Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize()
        .into()
}

fn namespaced_digest(namespace: &[u8], value: &[u8]) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::new()
            .chain_update(namespace)
            .chain_update(value)
            .finalize()
    ))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
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
