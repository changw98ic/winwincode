use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sha2::Digest;
use sha2::Sha256;
use winwincode_domain::Instant;
use winwincode_server::WorkerSessionCredentialErrorKind;
use winwincode_server::WorkerSessionCredentialPolicy;
use winwincode_server::WorkerSessionCredentialService;
use winwincode_server::issue_credential_material;
use winwincode_storage::CredentialAuditAction;
use winwincode_storage::SqliteStorage;
use winwincode_storage::WorkerSessionCredentialRecord;
use winwincode_storage::WorkerSessionCredentialState;

const T0: &str = "2026-09-04T12:00:00.000Z";

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-worker-session-credential-{name}-{}-{suffix}-{nanos}",
        std::process::id()
    ))
}

fn open_storage(directory: &PathBuf) -> SqliteStorage {
    SqliteStorage::open(directory).expect("product-state storage")
}

fn now_instant() -> Instant {
    Instant(T0.to_owned())
}

fn instant_after(millis: i64) -> Instant {
    let total_seconds = 12 * 3600 + millis.div_euclid(1000);
    let rest = millis.rem_euclid(1000);
    Instant(format!(
        "2026-09-04T{:02}:{:02}:{:02}.{rest:03}Z",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60
    ))
}

fn crockford(seed: u64) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut identity = String::with_capacity(26);
    let mut value = seed;
    for _ in 0..26 {
        let digit = usize::try_from(value % 32).expect("digit fits");
        identity.push(ALPHABET[digit] as char);
        value /= 32;
    }
    identity
}

fn session_id(seed: u64) -> String {
    format!("wsn_{}", crockford(seed))
}

fn worker_id(seed: u64) -> String {
    format!("wrk_{}", crockford(seed))
}

fn worker_instance_id(seed: u64) -> String {
    format!("wki_{}", crockford(seed))
}

fn launch_grant_id(seed: u64) -> String {
    format!("wlg_{}", crockford(seed))
}

fn actor_user_id(seed: u64) -> String {
    format!("usr_{}", crockford(seed))
}

fn short_policy() -> WorkerSessionCredentialPolicy {
    WorkerSessionCredentialPolicy {
        ttl: Duration::from_secs(1),
    }
}

fn with_service<R>(
    storage: &mut SqliteStorage,
    policy: WorkerSessionCredentialPolicy,
    run: impl FnOnce(&mut WorkerSessionCredentialService<'_>) -> R,
) -> R {
    let mut service = WorkerSessionCredentialService::with_policy(storage, policy).expect("policy");
    run(&mut service)
}

fn default_service<R>(
    storage: &mut SqliteStorage,
    run: impl FnOnce(&mut WorkerSessionCredentialService<'_>) -> R,
) -> R {
    with_service(storage, WorkerSessionCredentialPolicy::default(), run)
}

fn issue_for_session(
    service: &mut WorkerSessionCredentialService<'_>,
    seed: u64,
    now: &Instant,
) -> (WorkerSessionCredentialRecord, String) {
    let material = issue_credential_material().expect("entropy");
    let record = service
        .issue_for_launch(
            &session_id(seed),
            &worker_id(seed),
            &worker_instance_id(seed),
            &launch_grant_id(seed),
            material.credential_digest(),
            now,
        )
        .expect("credential issuance");
    (record, material.material().to_owned())
}

#[test]
fn issue_records_only_the_digest_and_conflicts_on_a_second_active() {
    let directory = temporary_directory("digest-only");
    let mut storage = open_storage(&directory);

    let (record, material) = default_service(&mut storage, |service| {
        issue_for_session(service, 11, &now_instant())
    });

    assert_eq!(material.len(), 64);
    assert!(
        material
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    let mut raw = Vec::with_capacity(32);
    for pair in material.as_bytes().chunks_exact(2) {
        let nibble = |byte: u8| {
            u8::try_from((byte as char).to_digit(16).expect("hex digit")).expect("nibble")
        };
        raw.push(nibble(pair[0]) << 4 | nibble(pair[1]));
    }
    assert_eq!(raw.len(), 32, "the material decodes to 32 raw bytes");
    let expected = format!("sha256:{:x}", Sha256::digest(&raw));
    assert_eq!(record.credential_digest, expected);
    assert_ne!(
        record.credential_digest, material,
        "the material itself is never stored"
    );
    assert_eq!(record.state, WorkerSessionCredentialState::Active);
    assert_eq!(
        record.expires_at.0, "2026-09-04T12:30:00.000Z",
        "the default policy gives the credential a short 30 minute life"
    );
    assert_eq!(record.worker_session_id, session_id(11));
    assert_eq!(record.worker_id, worker_id(11));
    assert_eq!(record.worker_instance_id, worker_instance_id(11));
    assert_eq!(record.worker_launch_grant_id, launch_grant_id(11));

    default_service(&mut storage, |service| {
        let conflict = service
            .issue_for_launch(
                &session_id(11),
                &worker_id(11),
                &worker_instance_id(11),
                &launch_grant_id(11),
                &record.credential_digest,
                &now_instant(),
            )
            .expect_err("a second active credential is a conflict");
        assert_eq!(
            conflict.kind(),
            WorkerSessionCredentialErrorKind::CredentialConflict
        );

        let status = service
            .status_for_session(&session_id(11))
            .expect("status query")
            .expect("the credential survives the issue flow");
        assert_eq!(status.credential_digest, expected);
    });
}

#[test]
fn verification_is_uniform_for_every_failure_and_open_only_for_live_material() {
    let directory = temporary_directory("uniform-verification");
    let mut storage = open_storage(&directory);
    default_service(&mut storage, |service| {
        let (record, material) = issue_for_session(service, 21, &now_instant());

        let verified = service
            .verify_credential(material.as_bytes(), &now_instant())
            .expect("live material authenticates");
        assert_eq!(
            verified.worker_session_credential_id,
            record.worker_session_credential_id
        );

        let failures = vec![
            service
                .verify_credential(b"counterfeit-proof-bytes", &now_instant())
                .expect_err("unknown material"),
            service
                .verify_credential(b"", &now_instant())
                .expect_err("empty proof"),
            service
                .verify_credential(material.as_bytes(), &instant_after(30 * 60 * 1000))
                .expect_err("expired at its deadline"),
        ];
        for failure in &failures {
            assert_eq!(
                failure.kind(),
                WorkerSessionCredentialErrorKind::AuthenticationRejected,
                "every authentication failure is one uniform category"
            );
        }
        let messages: Vec<String> = failures.iter().map(ToString::to_string).collect();
        assert!(
            messages.windows(2).all(|pair| pair[0] == pair[1]),
            "rejection diagnostics never vary: {messages:?}"
        );

        service
            .verify_credential(material.as_bytes(), &instant_after(30 * 60 * 1000 - 1))
            .expect("still live one millisecond before the deadline");
    });
}

#[test]
fn revocation_is_immediate_and_indistinguishable_from_unknown() {
    let directory = temporary_directory("immediate-revocation");
    let mut storage = open_storage(&directory);
    default_service(&mut storage, |service| {
        let (record, material) = issue_for_session(service, 31, &now_instant());
        service
            .verify_credential(material.as_bytes(), &now_instant())
            .expect("live before revocation");

        let actor = actor_user_id(7);
        let revoked = service
            .revoke_for_session(
                &session_id(31),
                &actor,
                Some("stop requested"),
                &now_instant(),
            )
            .expect("revocation");
        assert_eq!(
            revoked.worker_session_credential_id,
            record.worker_session_credential_id
        );
        assert_eq!(revoked.state, WorkerSessionCredentialState::Revoked);
        assert_eq!(
            revoked.ended_at.as_ref().map(|instant| instant.0.as_str()),
            Some(T0)
        );

        let rejected = service
            .verify_credential(material.as_bytes(), &now_instant())
            .expect_err("revoked material is dead");
        let unknown = service
            .verify_credential(b"never-issued-material", &now_instant())
            .expect_err("unknown material");
        assert_eq!(
            rejected, unknown,
            "revocation is indistinguishable from unknown"
        );
        assert!(
            service
                .status_for_session(&session_id(31))
                .expect("status query")
                .is_none(),
            "no live credential remains for the session"
        );

        let trail = service
            .audit_trail_for_credential(&record.worker_session_credential_id)
            .expect("audit trail");
        let actions: Vec<CredentialAuditAction> = trail.iter().map(|entry| entry.action).collect();
        assert_eq!(
            actions,
            vec![
                CredentialAuditAction::Issued,
                CredentialAuditAction::Revoked
            ]
        );
        assert_eq!(trail[1].actor_user_id, actor);
        assert_eq!(trail[1].reason.as_deref(), Some("stop requested"));

        let again = service
            .revoke_for_session(&session_id(31), &actor, None, &now_instant())
            .expect_err("nothing left to revoke");
        assert_eq!(
            again.kind(),
            WorkerSessionCredentialErrorKind::UnknownCredential
        );
    });
}

#[test]
fn expiry_sweep_and_verification_agree_on_the_deadline() {
    let directory = temporary_directory("expiry-sweep");
    let mut storage = open_storage(&directory);
    let (due, due_material) = with_service(&mut storage, short_policy(), |service| {
        issue_for_session(service, 41, &now_instant())
    });
    let (kept, kept_material) = default_service(&mut storage, |service| {
        issue_for_session(service, 42, &now_instant())
    });
    assert_eq!(
        due.expires_at.0, "2026-09-04T12:00:01.000Z",
        "the one second policy lands one second out"
    );

    with_service(&mut storage, short_policy(), |service| {
        let swept = service.expire_before(&now_instant()).expect("expiry sweep");
        assert!(
            swept.is_empty(),
            "no credential is due at the issuance instant"
        );

        let expired = service
            .verify_credential(due_material.as_bytes(), &instant_after(2_000))
            .expect_err("deadline passed");
        assert_eq!(
            expired.kind(),
            WorkerSessionCredentialErrorKind::AuthenticationRejected
        );

        let swept = service
            .expire_before(&instant_after(2_000))
            .expect("expiry sweep");
        assert_eq!(swept, vec![due.worker_session_credential_id.clone()]);

        let again = service
            .expire_before(&instant_after(2_000))
            .expect("expiry sweep");
        assert!(
            again.is_empty(),
            "the sweep never retires a credential twice"
        );
        let trail = service
            .audit_trail_for_credential(&due.worker_session_credential_id)
            .expect("audit trail");
        let actions: Vec<CredentialAuditAction> = trail.iter().map(|entry| entry.action).collect();
        assert_eq!(
            actions,
            vec![
                CredentialAuditAction::Issued,
                CredentialAuditAction::Expired
            ]
        );
        assert_eq!(trail[1].reason.as_deref(), Some("expiry deadline passed"));
    });

    default_service(&mut storage, |service| {
        let still_active = service
            .verify_credential(kept_material.as_bytes(), &instant_after(2_000))
            .expect("long-lived credential survives the sweep");
        assert_eq!(
            still_active.worker_session_credential_id,
            kept.worker_session_credential_id
        );
    });
}

#[test]
fn rotation_replaces_material_atomically_and_inherits_identities() {
    let directory = temporary_directory("rotation");
    let mut storage = open_storage(&directory);
    default_service(&mut storage, |service| {
        let (record, material) = issue_for_session(service, 51, &now_instant());

        let receipt = service
            .rotate_session_credential(&session_id(51), Some("scheduled rotation"), &now_instant())
            .expect("rotation");

        assert_eq!(
            receipt.retired_id, record.worker_session_credential_id,
            "the rotation retires exactly the issued credential"
        );
        assert_ne!(
            receipt.issued.worker_session_credential_id,
            record.worker_session_credential_id
        );
        assert_ne!(receipt.material.material(), material);
        assert_eq!(receipt.issued.worker_session_id, record.worker_session_id);
        assert_eq!(receipt.issued.worker_id, record.worker_id);
        assert_eq!(receipt.issued.worker_instance_id, record.worker_instance_id);
        assert_eq!(
            receipt.issued.worker_launch_grant_id,
            record.worker_launch_grant_id
        );
        assert_eq!(receipt.issued.state, WorkerSessionCredentialState::Active);

        let rotated_instant = instant_after(1);
        let fresh = service
            .verify_credential(receipt.material.material().as_bytes(), &rotated_instant)
            .expect("replacement material authenticates");
        assert_eq!(
            fresh.worker_session_credential_id,
            receipt.issued.worker_session_credential_id
        );
        let dead = service
            .verify_credential(material.as_bytes(), &rotated_instant)
            .expect_err("rotated material is dead");
        assert_eq!(
            dead.kind(),
            WorkerSessionCredentialErrorKind::AuthenticationRejected
        );

        let status = service
            .status_for_session(&session_id(51))
            .expect("status query")
            .expect("the replacement is live");
        assert_eq!(
            status.worker_session_credential_id,
            receipt.issued.worker_session_credential_id
        );
        let retired = service
            .snapshot_credential(&record.worker_session_credential_id)
            .expect("snapshot")
            .expect("the retired row is durable");
        assert_eq!(retired.state, WorkerSessionCredentialState::Rotated);
        assert_eq!(
            retired.ended_at.as_ref().map(|instant| instant.0.as_str()),
            Some(T0)
        );
        let trail = service
            .audit_trail_for_credential(&record.worker_session_credential_id)
            .expect("retired audit trail");
        let actions: Vec<CredentialAuditAction> = trail.iter().map(|entry| entry.action).collect();
        assert_eq!(
            actions,
            vec![
                CredentialAuditAction::Issued,
                CredentialAuditAction::Rotated
            ]
        );

        let missing = service
            .rotate_session_credential(&session_id(99), None, &now_instant())
            .expect_err("no active credential to rotate");
        assert_eq!(
            missing.kind(),
            WorkerSessionCredentialErrorKind::UnknownCredential
        );
    });
}

#[test]
fn status_query_reports_the_live_credential_by_worker_session() {
    let directory = temporary_directory("status-query");
    let mut storage = open_storage(&directory);
    default_service(&mut storage, |service| {
        assert!(
            service
                .status_for_session(&session_id(61))
                .expect("status query")
                .is_none(),
            "an unknown session has no credential"
        );

        let (record, _material) = issue_for_session(service, 61, &now_instant());
        let status = service
            .status_for_session(&session_id(61))
            .expect("status query")
            .expect("the issued credential is live");
        assert_eq!(
            status.worker_session_credential_id,
            record.worker_session_credential_id
        );
        assert_eq!(status.state, WorkerSessionCredentialState::Active);

        let snapshot = service
            .snapshot_credential(&record.worker_session_credential_id)
            .expect("snapshot")
            .expect("the credential is durable");
        assert_eq!(snapshot.credential_digest, record.credential_digest);
        assert!(
            service
                .snapshot_credential(&format!("wcred_{}", crockford(1)))
                .expect("snapshot")
                .is_none(),
            "an unknown credential id has no row"
        );

        service
            .revoke_for_session(&session_id(61), &actor_user_id(7), None, &now_instant())
            .expect("revocation");
        assert!(
            service
                .status_for_session(&session_id(61))
                .expect("status query")
                .is_none()
        );
    });
}

#[test]
fn bound_verification_refuses_claimed_foreign_identities() {
    let directory = temporary_directory("bound-verification");
    let mut storage = open_storage(&directory);
    default_service(&mut storage, |service| {
        let (_record, material) = issue_for_session(service, 71, &now_instant());

        let bound = service
            .verify_bound_credential(
                material.as_bytes(),
                &session_id(71),
                &worker_id(71),
                &worker_instance_id(71),
                &now_instant(),
            )
            .expect("the bound identities authenticate");
        assert_eq!(bound.worker_session_id, session_id(71));

        let forgeries = vec![
            service
                .verify_bound_credential(
                    material.as_bytes(),
                    &session_id(72),
                    &worker_id(71),
                    &worker_instance_id(71),
                    &now_instant(),
                )
                .expect_err("another session"),
            service
                .verify_bound_credential(
                    material.as_bytes(),
                    &session_id(71),
                    &worker_id(72),
                    &worker_instance_id(71),
                    &now_instant(),
                )
                .expect_err("another worker"),
            service
                .verify_bound_credential(
                    material.as_bytes(),
                    &session_id(71),
                    &worker_id(71),
                    &worker_instance_id(72),
                    &now_instant(),
                )
                .expect_err("another instance"),
            service
                .verify_bound_credential(
                    material.as_bytes(),
                    "wsn_not_canonical",
                    &worker_id(71),
                    &worker_instance_id(71),
                    &now_instant(),
                )
                .expect_err("non-canonical claim"),
        ];
        for forgery in &forgeries {
            assert_eq!(
                forgery.kind(),
                WorkerSessionCredentialErrorKind::AuthenticationRejected,
                "identity mismatches never become a distinct category"
            );
        }
        let unknown = service
            .verify_credential(b"never-issued-material", &now_instant())
            .expect_err("unknown material");
        assert_eq!(forgeries[0].kind(), unknown.kind());
    });
}
