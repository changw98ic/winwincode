// SPDX-License-Identifier: Apache-2.0

//! Short-lived Worker session credential lifecycle service (plan 17.2,
//! WORKER-200.2).
//!
//! One 32-byte credential replaces the shared static Worker bearer: it is
//! bound to exactly one `WorkerSession`, one `WorkerInstance`, and the one
//! `WorkerLaunchGrant` it was issued beside. The lifecycle is
//!
//! - **issue** — fresh random material crosses the launch `201` response and
//!   the device chain exactly once; only its `sha256:` digest is durable
//!   (`worker_session_credentials` ledger), mirroring the launch grant
//!   ledger's digest-only rule;
//! - **verify** — the worker presents the launch response's lowercase-hex
//!   material; the boundary decodes it back to its 32 bytes and hashes them
//!   with the same `sha256:` shape as `FileRemoteWorkerAuthenticator`,
//!   matching the result against the durable digest; an `active`, unexpired
//!   credential authenticates, everything else is one uniform rejection
//!   that never distinguishes unknown, expired, revoked, or rotated
//!   credentials;
//! - **rotate** — the session's `active` credential is retired and its
//!   replacement inserted atomically, so the old material dies at the same
//!   instant the new material starts;
//! - **revoke** — the stop flow kills the session's `active` credential;
//!   the very next exchange fails;
//! - **expire** — verification already refuses an expired credential, the
//!   sweep retires the durable rows over the same rule;
//! - **status** — launch/stop/retry flows query the session's live
//!   credential by `workerSessionId`.
//!
//! Every transition lands in the durable credential audit trail.

use std::fmt;
use std::time::Duration;

use sha2::Digest;
use sha2::Sha256;
use winwincode_domain::Instant;
use winwincode_storage::CredentialAuditEntry;
use winwincode_storage::CredentialIssuance;
use winwincode_storage::CredentialRotation;
use winwincode_storage::CredentialRotationOutcome;
use winwincode_storage::SqliteStorage;
use winwincode_storage::WorkerSessionCredentialRecord;
use winwincode_storage::WorkerSessionCredentialState;
use winwincode_storage::WorkerSessionCredentialStoreError;
use winwincode_storage::WorkerSessionCredentialStoreErrorKind;

/// Default time-to-live of one issued credential. Short by design: a worker
/// session keeps authenticating only while it rotates before the deadline.
pub const DEFAULT_CREDENTIAL_TTL: Duration = Duration::from_mins(30);

/// Issuance and rotation TTL policy of the credential service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialPolicy {
    /// Time-to-live of one issued or rotated credential.
    pub ttl: Duration,
}

impl Default for WorkerSessionCredentialPolicy {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_CREDENTIAL_TTL,
        }
    }
}

/// Freshly minted credential material: the lowercase-hex material that
/// crosses the launch response exactly once, plus the `sha256:` digest that
/// is the only form ever persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialMaterial {
    material: String,
    digest: String,
}

impl CredentialMaterial {
    /// The one-time credential material (lowercase hex of 32 bytes).
    #[must_use]
    pub fn material(&self) -> &str {
        &self.material
    }

    /// The persisted `sha256:` digest of the material.
    #[must_use]
    pub fn credential_digest(&self) -> &str {
        &self.digest
    }
}

/// Stable credential lifecycle failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialErrorKind {
    /// A lifecycle command input violated the frozen schema bounds.
    InvalidInput,
    /// Uniform authentication failure. Unknown, malformed, expired,
    /// revoked, and rotated credentials are indistinguishable here, so no
    /// response ever leaks a credential's existence.
    AuthenticationRejected,
    /// The worker session already carries an `active` credential.
    CredentialConflict,
    /// No credential matches the requested identity.
    UnknownCredential,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost an impossible race.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free credential lifecycle error.
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

/// The one uniform authentication failure. The message is constant so even
/// diagnostics cannot distinguish why a proof failed.
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

/// Mints one 32-byte credential and returns its lowercase-hex material plus
/// the persisted `sha256:` digest (the same digest shape
/// `FileRemoteWorkerAuthenticator` verifies against). Only the digest ever
/// enters durable state (plan 17.2).
///
/// # Errors
///
/// Fails only when the platform entropy source is unavailable; nothing was
/// decided and nothing was persisted.
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

/// Computes the persisted `sha256:` digest of one credential proof — the
/// exact shape `FileRemoteWorkerAuthenticator` stores and compares.
fn credential_digest(proof: &[u8]) -> String {
    winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(proof))).0
}

/// Decodes the canonical presented proof — the 64 lowercase hex characters
/// of one 32-byte credential — back to its raw bytes. Any other shape is
/// not a credential and folds into the uniform rejection.
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

/// The worker session credential lifecycle service over one storage
/// connection. Like the launch grant service, every operation opens its own
/// ledger on the caller's connection so concurrent flows never share state
/// in memory.
pub struct WorkerSessionCredentialService<'storage> {
    storage: &'storage mut SqliteStorage,
    policy: WorkerSessionCredentialPolicy,
}

impl<'storage> WorkerSessionCredentialService<'storage> {
    /// Builds one service with the default short-lived TTL policy.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self {
            storage,
            policy: WorkerSessionCredentialPolicy::default(),
        }
    }

    /// Builds one service with an explicit TTL policy.
    ///
    /// # Errors
    ///
    /// Rejects a zero TTL.
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

    /// Records the durable credential of one launch (plan 17.2): the digest
    /// computed beside the grant issuance becomes the `active` credential of
    /// the launched worker session, bound to its worker identities and the
    /// one launch grant it authorizes. The raw material is never an input —
    /// it crossed the launch response once and lives nowhere on the server.
    ///
    /// # Errors
    ///
    /// Rejects a session that already carries an `active` credential or
    /// storage failure.
    pub fn issue_for_launch(
        &mut self,
        worker_session_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        worker_launch_grant_id: &str,
        credential_digest: &str,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
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
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .issue(&issuance, now)?)
    }

    /// Rotates one worker session's credential (plan 17.2): the `active`
    /// credential is retired and its replacement becomes `active` in one
    /// transaction. The fresh replacement material crosses this receipt
    /// exactly once; the retired material stops authenticating at the
    /// rotation instant.
    ///
    /// # Errors
    ///
    /// Rejects a session without an `active` credential or storage failure.
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

    /// Revokes the session's `active` credential immediately (the stop flow
    /// and revocation-driven retries share this entry point): the material
    /// stops authenticating at once, whether the worker session is still
    /// launching or already running.
    ///
    /// # Errors
    ///
    /// Rejects a session without an `active` credential or storage failure.
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

    /// Expires every `active` credential whose deadline is at or before
    /// `cutoff` and returns the retired credential ids.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_before(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .expire(cutoff)?)
    }

    /// Returns the session's live credential, if any — the shared status
    /// query for the launch, stop, and retry flows.
    ///
    /// # Errors
    ///
    /// Rejects storage failure.
    pub fn status_for_session(
        &mut self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .active_for_session(worker_session_id)?)
    }

    /// Returns one durable credential projection by credential id.
    ///
    /// # Errors
    ///
    /// Rejects storage failure.
    pub fn snapshot_credential(
        &mut self,
        worker_session_credential_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .snapshot(worker_session_credential_id)?)
    }

    /// Verifies one presented credential proof (plan 17.2): the proof is
    /// hashed with the same `sha256:` shape `FileRemoteWorkerAuthenticator`
    /// uses and matched against the durable digest. Only an `active`,
    /// unexpired credential authenticates. Every authentication failure is
    /// the one uniform `AuthenticationRejected` category, so no caller can
    /// learn whether a credential exists, expired, or was revoked.
    ///
    /// # Errors
    ///
    /// Returns `AuthenticationRejected` for every failed authentication;
    /// infrastructure failures surface as their own stable categories.
    pub fn verify_credential(
        &mut self,
        proof: &[u8],
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let record = self.lookup_verified(proof, now)?;
        Ok(record)
    }

    /// Verifies one presented proof against the claimed worker identities:
    /// the proof must authenticate to the very worker session, worker, and
    /// worker instance it claims. Identity mismatches fold into the same
    /// uniform rejection.
    ///
    /// # Errors
    ///
    /// Returns `AuthenticationRejected` for every failed authentication;
    /// infrastructure failures surface as their own stable categories.
    pub fn verify_bound_credential(
        &mut self,
        proof: &[u8],
        worker_session_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        let record = self.lookup_verified(proof, now)?;
        let identities_match = canonical_id(worker_session_id, 3)
            && canonical_id(worker_id, 4)
            && canonical_id(worker_instance_id, 6)
            && record.worker_session_id == worker_session_id
            && record.worker_id == worker_id
            && record.worker_instance_id == worker_instance_id;
        if identities_match {
            Ok(record)
        } else {
            Err(rejected())
        }
    }

    /// Returns every durable credential audit entry of one credential,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical credential identity or storage failure.
    pub fn audit_trail_for_credential(
        &mut self,
        worker_session_credential_id: &str,
    ) -> Result<Vec<CredentialAuditEntry>, WorkerSessionCredentialError> {
        Ok(self
            .storage
            .worker_session_credential_ledger()?
            .audit_trail(worker_session_credential_id)?)
    }

    /// The durable half of verification: digest lookup, live state, and an
    /// open deadline. Every failure branch returns the same uniform
    /// rejection.
    fn lookup_verified(
        &mut self,
        proof: &[u8],
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialError> {
        // The proof a worker presents is the lowercase-hex material of the
        // launch response; it must decode back to the exact 32 bytes whose
        // digest was stored at issuance.
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
        Ok(record)
    }
}

/// One rotation's receipt: the retired credential id, the replacement
/// record, and the replacement material that crosses exactly once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotationReceipt {
    /// The credential that was retired by this rotation.
    pub retired_id: String,
    /// The replacement, `active` from the rotation instant on.
    pub issued: WorkerSessionCredentialRecord,
    /// The replacement's one-time material.
    pub material: CredentialMaterial,
}

/// The credential deadline one TTL away from `now`, in the canonical
/// instant shape the durable ledger stores.
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

/// Signed millisecond amount of one duration, clamped to the `i64` range.
fn signed_millis(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Shape check for the canonical 24-character `domain.Instant`.
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

/// Prefix-length shape check for canonical identities (cheap, rejection
/// only — it never produces a diagnostic).
fn canonical_id(value: &str, prefix_len: usize) -> bool {
    value.len() == prefix_len + 26
        && value
            .as_bytes()
            .iter()
            .skip(prefix_len)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// Generates one canonical `wcred_` + 26 character Crockford identifier.
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
        // The same proof bytes must hash to the same `sha256:` digest the
        // `FileRemoteWorkerAuthenticator` stores and compares.
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
            assert!(canonical_id(&id, "wcred_".len()));
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
        // The hex text itself is not the credential: hashing it must not
        // produce the stored digest.
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
