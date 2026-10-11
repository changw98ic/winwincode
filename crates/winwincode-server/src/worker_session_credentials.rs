// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::time::Duration;

use sha2::Digest;
use sha2::Sha256;
use winwincode_domain::Instant;
use winwincode_domain::is_canonical_prefixed_id;
use winwincode_storage::CredentialAuditEntry;
use winwincode_storage::CredentialIssuance;
use winwincode_storage::CredentialRotation;
use winwincode_storage::CredentialRotationOutcome;
use winwincode_storage::SqliteStorage;
use winwincode_storage::WorkerSessionCredentialRecord;
use winwincode_storage::WorkerSessionCredentialState;
use winwincode_storage::WorkerSessionCredentialStoreError;
use winwincode_storage::WorkerSessionCredentialStoreErrorKind;

pub const DEFAULT_CREDENTIAL_TTL: Duration = Duration::from_mins(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialPolicy {
    pub ttl: Duration,
}

impl Default for WorkerSessionCredentialPolicy {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_CREDENTIAL_TTL,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialMaterial {
    material: String,
    digest: String,
}

impl CredentialMaterial {
    #[must_use]
    pub fn material(&self) -> &str {
        &self.material
    }

    #[must_use]
    pub fn credential_digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialErrorKind {
    InvalidInput,
    AuthenticationRejected,
    CredentialConflict,
    UnknownCredential,
    IllegalStateTransition,
    RevisionConflict,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialError {
    kind: WorkerSessionCredentialErrorKind,
    message: String,
}

impl WorkerSessionCredentialError {
    #[must_use]
    pub const fn kind(&self) -> WorkerSessionCredentialErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerSessionCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerSessionCredentialError {}

impl From<WorkerSessionCredentialStoreError> for WorkerSessionCredentialError {
    fn from(source: WorkerSessionCredentialStoreError) -> Self {
        Self {
            kind: match source.kind() {
                WorkerSessionCredentialStoreErrorKind::InvalidInput => {
                    WorkerSessionCredentialErrorKind::InvalidInput
                }
                WorkerSessionCredentialStoreErrorKind::UnknownCredential => {
                    WorkerSessionCredentialErrorKind::UnknownCredential
                }
                WorkerSessionCredentialStoreErrorKind::CredentialConflict => {
                    WorkerSessionCredentialErrorKind::CredentialConflict
                }
                WorkerSessionCredentialStoreErrorKind::IllegalStateTransition => {
                    WorkerSessionCredentialErrorKind::IllegalStateTransition
                }
                WorkerSessionCredentialStoreErrorKind::RevisionConflict => {
                    WorkerSessionCredentialErrorKind::RevisionConflict
                }
                WorkerSessionCredentialStoreErrorKind::CorruptState => {
                    WorkerSessionCredentialErrorKind::CorruptState
                }
                WorkerSessionCredentialStoreErrorKind::Storage => {
                    WorkerSessionCredentialErrorKind::Storage
                }
            },
            message: source.to_string(),
        }
    }
}

fn rejected() -> WorkerSessionCredentialError {
    WorkerSessionCredentialError {
        kind: WorkerSessionCredentialErrorKind::AuthenticationRejected,
        message: "worker session credential authentication failed".to_owned(),
    }
}

fn error(
    kind: WorkerSessionCredentialErrorKind,
    message: impl Into<String>,
) -> WorkerSessionCredentialError {
    WorkerSessionCredentialError {
        kind,
        message: message.into(),
    }
}

pub fn issue_credential_material() -> Result<CredentialMaterial, WorkerSessionCredentialError> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|_| {
        error(
            WorkerSessionCredentialErrorKind::Storage,
            "credential entropy is unavailable",
        )
    })?;
    Ok(CredentialMaterial {
        material: hex_encode(&secret),
        digest: credential_digest(&secret),
    })
}

fn credential_digest(proof: &[u8]) -> String {
    winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(proof))).0
}

fn decode_material(proof: &[u8]) -> Option<[u8; 32]> {
    if proof.len() != 64 {
        return None;
    }
    let mut raw = [0_u8; 32];
    for (index, pair) in proof.chunks_exact(2).enumerate() {
        let high = decode_nibble(pair[0])?;
        let low = decode_nibble(pair[1])?;
        raw[index] = high << 4 | low;
    }
    Some(raw)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub struct WorkerSessionCredentialService<'storage> {
    storage: &'storage mut SqliteStorage,
    policy: WorkerSessionCredentialPolicy,
}

impl<'storage> WorkerSessionCredentialService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self {
            storage,
            policy: WorkerSessionCredentialPolicy::default(),
        }
    }

    pub fn renew_after_accepted_heartbeat(
        &mut self,
        credential_digest: &str,
        heartbeat: &winwincode_execution_port::generated::WorkerHeartbeatMessage,
        now: &Instant,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialError> {
        let Some(record) = self
            .storage
            .worker_session_credential_ledger()?
            .find_by_digest(credential_digest)?
        else {
            return Ok(None);
        };
        if record.state != WorkerSessionCredentialState::Active
            || record.expires_at.0 <= now.0
            || heartbeat.active_leases.is_empty()
            || record.worker_id != heartbeat.worker_id.0
            || record.worker_instance_id != heartbeat.worker_instance_id.0
        {
            return Ok(None);
        }
        let threshold = credential_deadline(&(self.policy.ttl / 2), now)?;
        if record.expires_at.0 > threshold.0 {
            return Ok(None);
        }
        let expires_at = credential_deadline(&self.policy.ttl, now)?;
        for lease in &heartbeat.active_leases {
            if let Some(renewed) = self
                .storage
                .worker_session_credential_ledger()?
                .renew_after_accepted_heartbeat(&record, heartbeat, lease, &expires_at, now)?
            {
                return Ok(Some(renewed));
            }
        }
        Ok(None)
    }

    pub fn with_policy(
        storage: &'storage mut SqliteStorage,
        policy: WorkerSessionCredentialPolicy,
    ) -> Result<Self, WorkerSessionCredentialError> {
        if policy.ttl.is_zero() {
            return Err(error(
                WorkerSessionCredentialErrorKind::InvalidInput,
                "worker session credential ttl must be positive",
            ));
        }
        Ok(Self { storage, policy })
    }

    pub fn issue_for_launch(
        &mut self,
        worker_session_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        worker_launch_grant_id: &str,
        credential_digest: &str,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let issuance = self.prepare_launch_issuance(
            worker_session_id,
            worker_id,
            worker_instance_id,
            worker_launch_grant_id,
            credential_digest,
            now,
        )?;
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .issue(&issuance, now)?)
    }

    pub(crate) fn prepare_launch_issuance(
        &self,
        worker_session_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        worker_launch_grant_id: &str,
        credential_digest: &str,
        now: &Instant,
    ) -> Result<CredentialIssuance, WorkerSessionCredentialError> {
        let expires_at = credential_deadline(&self.policy.ttl, now)?;
        let issuance = CredentialIssuance::try_new(
            generate_credential_id()?,
            worker_session_id,
            worker_id,
            worker_instance_id,
            worker_launch_grant_id,
            credential_digest,
            expires_at,
        )?;
        Ok(issuance)
    }

    pub fn rotate_session_credential(
        &mut self,
        worker_session_id: &str,
        reason: Option<&str>,
        now: &Instant,
    ) -> Result<CredentialRotationReceipt, WorkerSessionCredentialError> {
        let material = issue_credential_material()?;
        let expires_at = credential_deadline(&self.policy.ttl, now)?;
        let rotation = CredentialRotation::try_new(
            worker_session_id,
            generate_credential_id()?,
            material.credential_digest(),
            expires_at,
            reason,
        )?;
        let outcome: CredentialRotationOutcome = self
            .storage
            .worker_session_credential_ledger()?
            .rotate(&rotation, now)?;
        Ok(CredentialRotationReceipt {
            retired_id: outcome.retired.worker_session_credential_id.clone(),
            issued: outcome.issued,
            material,
        })
    }

    pub fn revoke_for_session(
        &mut self,
        worker_session_id: &str,
        actor_user_id: &str,
        reason: Option<&str>,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .revoke_for_session(worker_session_id, actor_user_id, reason, now)?)
    }

    pub fn expire_before(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .expire(cutoff)?)
    }

    pub fn status_for_session(
        &mut self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .active_for_session(worker_session_id)?)
    }

    pub fn snapshot_credential(
        &mut self,
        worker_session_credential_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .snapshot(worker_session_credential_id)?)
    }

    pub fn verify_credential(
        &mut self,
        proof: &[u8],
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let record = self.lookup_verified(proof, now)?;
        Ok(record)
    }

    pub fn verify_bound_credential(
        &mut self,
        proof: &[u8],
        worker_session_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let record = self.lookup_verified(proof, now)?;
        let identities_match = is_canonical_prefixed_id(worker_session_id, "wsn_")
            && is_canonical_prefixed_id(worker_id, "wrk_")
            && is_canonical_prefixed_id(worker_instance_id, "wki_")
            && record.worker_session_id == worker_session_id
            && record.worker_id == worker_id
            && record.worker_instance_id == worker_instance_id;
        if identities_match {
            Ok(record)
        } else {
            Err(rejected())
        }
    }

    pub fn audit_trail_for_credential(
        &mut self,
        worker_session_credential_id: &str,
    ) -> Result<Vec<CredentialAuditEntry>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .audit_trail(worker_session_credential_id)?)
    }

    fn lookup_verified(
        &mut self,
        proof: &[u8],
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let Some(raw) = decode_material(proof) else {
            return Err(rejected());
        };
        let ledger = self.storage.worker_session_credential_ledger()?;
        let Some(record) = ledger.find_by_digest(&credential_digest(&raw))? else {
            return Err(rejected());
        };
        if record.state != WorkerSessionCredentialState::Active {
            return Err(rejected());
        }
        let canonical_now = canonical_instant(&now.0);
        if !canonical_now || now.0.as_str() >= record.expires_at.0.as_str() {
            return Err(rejected());
        }
        if self
            .storage
            .worker_launch_grant_ledger()
            .map_err(|_| rejected())?
            .has_trusted_exit(&record.worker_launch_grant_id)
            .map_err(|_| rejected())?
        {
            return Err(rejected());
        }
        Ok(record)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotationReceipt {
    pub retired_id: String,
    pub issued: WorkerSessionCredentialRecord,
    pub material: CredentialMaterial,
}

fn credential_deadline(
    ttl: &Duration,
    now: &Instant,
) -> Result<Instant, WorkerSessionCredentialError> {
    crate::client_occupancy::offset_instant(now, signed_millis(ttl)).ok_or_else(|| {
        error(
            WorkerSessionCredentialErrorKind::InvalidInput,
            "credential ttl does not resolve to a canonical expiry instant",
        )
    })
}

fn signed_millis(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn canonical_instant(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            byte.is_ascii_digit() || matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23)
        })
}

fn generate_credential_id() -> Result<String, WorkerSessionCredentialError> {
    const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| {
        error(
            WorkerSessionCredentialErrorKind::Storage,
            "credential entropy is unavailable",
        )
    })?;
    let mut identity = String::with_capacity(6 + 26);
    identity.push_str("wcred_");
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
    }
    Ok(identity)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_material_is_32_bytes_and_digest_bound() {
        let material = issue_credential_material().expect("entropy");
        assert_eq!(material.material().len(), 64, "lowercase hex of 32 bytes");
        assert!(
            material
                .material()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(material.credential_digest().starts_with("sha256:"));
        assert_eq!(material.credential_digest().len(), 71);
        let again = issue_credential_material().expect("entropy");
        assert_ne!(
            material.material(),
            again.material(),
            "every credential is fresh randomness"
        );
        assert_ne!(material.credential_digest(), again.credential_digest());
    }

    #[test]
    fn digest_shape_matches_the_remote_worker_authenticator() {
        let proof = b"fixture-remote-token";
        let digest = credential_digest(proof);
        let expected =
            winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(proof)));
        assert_eq!(digest, expected.0);
        assert_eq!(digest.len(), 71);
    }

    #[test]
    fn credential_ids_are_canonical_crockford() {
        for _ in 0..8 {
            let id = generate_credential_id().expect("entropy");
            assert_eq!(id.len(), "wcred_".len() + 26);
            assert!(id.starts_with("wcred_"));
            assert!(is_canonical_prefixed_id(&id, "wcred_"));
        }
    }

    #[test]
    fn policy_rejects_zero_ttl_only_through_the_constructor() {
        assert_ne!(WorkerSessionCredentialPolicy::default().ttl, Duration::ZERO);
    }

    #[test]
    fn only_the_canonical_hex_material_decodes_to_raw_bytes() {
        let material = issue_credential_material().expect("entropy");
        let decoded = decode_material(material.material().as_bytes()).expect("canonical material");
        assert_eq!(credential_digest(&decoded), material.credential_digest());
        assert_ne!(
            credential_digest(material.material().as_bytes()),
            material.credential_digest()
        );
        assert_eq!(decode_material(b""), None, "empty proof");
        assert_eq!(decode_material(b"0"), None, "truncated proof");
        assert_eq!(decode_material(&[b'a'; 63]), None, "short proof");
        assert_eq!(decode_material(&[b'a'; 65]), None, "long proof");
        assert_eq!(
            decode_material(&[b'A'; 64]),
            None,
            "uppercase is not canonical"
        );
        assert_eq!(decode_material(&[b'g'; 64]), None, "out of alphabet");
    }

    #[test]
    fn uniform_rejection_message_is_constant() {
        assert_eq!(
            rejected().to_string(),
            "worker session credential authentication failed"
        );
        assert_eq!(
            rejected(),
            WorkerSessionCredentialError {
                kind: WorkerSessionCredentialErrorKind::AuthenticationRejected,
                message: "worker session credential authentication failed".to_owned(),
            }
        );
    }
}
