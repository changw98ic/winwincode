// SPDX-License-Identifier: Apache-2.0

//! Durable connect-code, access-grant, and connect-attempt contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, AccessGrantState, AttemptDimension, ClientConnectStoreErrorKind,
    ConnectCodeConsume, ConnectCodePublication, ConnectCodeRevocation, ConnectCodeState,
    GrantPermissions, GrantSource, GrantTrustMode, SqliteStorage, connect_attempt_window_anchor,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-client-connect-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn node_id(seed: u64) -> String {
    format!("cnd_{seed:026}")
}

fn instance_id(seed: u64) -> String {
    format!("cix_{seed:026}")
}

fn code_id(seed: u64) -> String {
    format!("cct_{seed:026}")
}

fn grant_id(seed: u64) -> String {
    format!("cag_{seed:026}")
}

fn user_id(seed: u64) -> String {
    format!("usr_{seed:026}")
}

fn digest(seed: u64) -> String {
    format!("sha256:{seed:064x}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";
const T4: &str = "2026-01-01T00:04:00.000Z";

fn open(path: PathBuf) -> SqliteStorage {
    SqliteStorage::open(path).expect("storage")
}

/// Seeds one enrolled-enough client node so connect rows can reference it.
fn seed_client_node(storage: &mut SqliteStorage, seed: u64) -> String {
    let registration = winwincode_storage::ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        Some(instance_id(seed)),
        2,
    )
    .expect("registration");
    storage
        .client_node_registry()
        .expect("registry")
        .register(&registration, 0, &instant(T0))
        .expect("register");
    node_id(seed)
}

fn publication(
    seed: u64,
    client_node_id: &str,
    expires_at: &str,
    attempts: u32,
) -> ConnectCodePublication {
    ConnectCodePublication::try_new(
        code_id(seed),
        digest(seed),
        client_node_id,
        instance_id(seed),
        seed,
        instant(expires_at),
        attempts,
    )
    .expect("publication")
}

fn issuance(
    seed: u64,
    client_node_id: &str,
    user: &str,
    granted_by: &str,
    trust: GrantTrustMode,
    expires_at: Option<&str>,
) -> AccessGrantIssuance {
    AccessGrantIssuance::try_new(
        grant_id(seed),
        client_node_id,
        user,
        granted_by,
        trust,
        expires_at.map(instant),
    )
    .expect("issuance")
}

fn consume(code: u64, presented_digest: &str, ack_generation: u64) -> ConnectCodeConsume {
    ConnectCodeConsume::try_new(code_id(code), presented_digest, ack_generation).expect("consume")
}

fn expect_kind<T>(
    result: Result<T, winwincode_storage::ClientConnectStoreError>,
    kind: ClientConnectStoreErrorKind,
) where
    T: std::fmt::Debug,
{
    let error = result.expect_err("operation must fail");
    assert_eq!(error.kind(), kind);
}

#[test]
fn publishes_connect_code_digest_and_enforces_publication_rules() {
    let mut storage = open(temporary_directory("publish"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");

    let published = ledger
        .publish(&publication(10, &client, T1, 3), &instant(T0))
        .expect("publish");
    assert_eq!(published.connect_code_id, code_id(10));
    assert_eq!(published.code_digest, digest(10));
    assert_eq!(published.client_node_id, client);
    assert_eq!(published.issued_by_instance_id, instance_id(10));
    assert_eq!(published.generation, 10);
    assert_eq!(published.expires_at, instant(T1));
    assert_eq!(published.remaining_attempts, 3);
    assert_eq!(published.state, ConnectCodeState::Active);
    assert_eq!(published.created_at, instant(T0));
    assert_eq!(published.revision, 1);

    let by_id = ledger
        .code_snapshot(&code_id(10))
        .expect("snapshot")
        .expect("code");
    assert_eq!(by_id, published);
    let by_digest = ledger
        .code_snapshot_by_digest(&digest(10))
        .expect("snapshot by digest")
        .expect("code");
    assert_eq!(by_digest, published);

    // The same digest cannot be registered twice.
    let duplicate_digest = ConnectCodePublication::try_new(
        code_id(11),
        digest(10),
        &client,
        instance_id(11),
        11,
        instant(T1),
        3,
    )
    .expect("publication");
    expect_kind(
        ledger.publish(&duplicate_digest, &instant(T0)),
        ClientConnectStoreErrorKind::ConnectCodeDigestConflict,
    );

    // The same code id cannot be reused.
    let duplicate_id = ConnectCodePublication::try_new(
        code_id(10),
        digest(11),
        &client,
        instance_id(11),
        11,
        instant(T1),
        3,
    )
    .expect("publication");
    expect_kind(
        ledger.publish(&duplicate_id, &instant(T0)),
        ClientConnectStoreErrorKind::ConnectCodeIdConflict,
    );

    // An unknown client node is rejected.
    let orphan = ConnectCodePublication::try_new(
        code_id(12),
        digest(12),
        node_id(99),
        instance_id(12),
        12,
        instant(T1),
        3,
    )
    .expect("publication");
    expect_kind(
        ledger.publish(&orphan, &instant(T0)),
        ClientConnectStoreErrorKind::UnknownClientNode,
    );

    // An already-expired code cannot be published.
    expect_kind(
        ledger.publish(&publication(13, &client, T0, 3), &instant(T1)),
        ClientConnectStoreErrorKind::InvalidInput,
    );
}

#[test]
fn rejects_malformed_connect_code_publications() {
    let mut storage = open(temporary_directory("malformed"));
    let client = seed_client_node(&mut storage, 1);

    // Malformed facts never reach durable storage.
    expect_kind(
        ConnectCodePublication::try_new(
            code_id(14),
            "sha256:not-a-digest",
            &client,
            instance_id(14),
            14,
            instant(T1),
            3,
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ConnectCodePublication::try_new(
            code_id(14),
            digest(14),
            &client,
            instance_id(14),
            0,
            instant(T1),
            3,
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ConnectCodePublication::try_new(
            code_id(14),
            digest(14),
            &client,
            instance_id(14),
            14,
            instant(T1),
            0,
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );
}

#[test]
fn refresh_and_revoke_follow_frozen_transitions() {
    let mut storage = open(temporary_directory("revoke-refresh"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");

    ledger
        .publish(&publication(20, &client, T3, 3), &instant(T0))
        .expect("publish");
    expect_kind(
        ledger.revoke_code(&ConnectCodeRevocation::try_new(code_id(20), 99).expect("revocation")),
        ClientConnectStoreErrorKind::RevisionConflict,
    );

    let revoked = ledger
        .revoke_code(&ConnectCodeRevocation::try_new(code_id(20), 1).expect("revocation"))
        .expect("revoke");
    assert_eq!(revoked.state, ConnectCodeState::Revoked);
    assert_eq!(revoked.revision, 2);

    // Revoking an already-revoked code is an idempotent replay.
    let replay = ledger
        .revoke_code(&ConnectCodeRevocation::try_new(code_id(20), 2).expect("revocation"))
        .expect("revoke replay");
    assert_eq!(replay.revision, 2);

    // Refreshing a revoked code is not a legal transition.
    let replacement = publication(21, &client, T3, 3);
    expect_kind(
        ledger.refresh_code(
            &ConnectCodeRevocation::try_new(code_id(20), 2).expect("revocation"),
            &replacement,
            &instant(T1),
        ),
        ClientConnectStoreErrorKind::IllegalStateTransition,
    );

    // Refreshing an active code revokes it and publishes the replacement in
    // one transaction.
    let second = ledger
        .publish(&publication(22, &client, T3, 3), &instant(T0))
        .expect("publish");
    let refreshed = ledger
        .refresh_code(
            &ConnectCodeRevocation::try_new(code_id(22), second.revision).expect("revocation"),
            &replacement,
            &instant(T1),
        )
        .expect("refresh");
    assert_eq!(refreshed.connect_code_id, code_id(21));
    assert_eq!(refreshed.state, ConnectCodeState::Active);
    assert_eq!(refreshed.revision, 1);
    let old = ledger
        .code_snapshot(&code_id(22))
        .expect("snapshot")
        .expect("old code");
    assert_eq!(old.state, ConnectCodeState::Revoked);
    // Refreshing a refreshed (revoked) code under its current revision is
    // not a legal transition either.
    expect_kind(
        ledger.refresh_code(
            &ConnectCodeRevocation::try_new(code_id(22), 2).expect("revocation"),
            &replacement,
            &instant(T1),
        ),
        ClientConnectStoreErrorKind::IllegalStateTransition,
    );

    expect_kind(
        ledger.revoke_code(&ConnectCodeRevocation::try_new(code_id(99), 1).expect("revocation")),
        ClientConnectStoreErrorKind::UnknownConnectCode,
    );
}

#[test]
fn consume_and_grant_is_atomic_and_first_user_receives_full_permissions() {
    let mut storage = open(temporary_directory("consume"));
    let client = seed_client_node(&mut storage, 1);
    let other_client = seed_client_node(&mut storage, 3);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);

    ledger
        .publish(&publication(30, &client, T1, 3), &instant(T0))
        .expect("publish");

    // A wrong presented digest is indistinguishable from an unknown code.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(30, &digest(31), 30),
            &issuance(40, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::UnknownConnectCode,
    );
    // The challenge ACK must name the published generation.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(30, &digest(30), 29),
            &issuance(40, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::GenerationMismatch,
    );
    // Consuming at or after the expiry is rejected.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(30, &digest(30), 30),
            &issuance(40, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T1),
        ),
        ClientConnectStoreErrorKind::ConnectCodeExpired,
    );
    // The grant must reference the same client as the consumed code.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(30, &digest(30), 30),
            &issuance(
                40,
                &other_client,
                &alice,
                &alice,
                GrantTrustMode::Trusted,
                None,
            ),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );

    let receipt = ledger
        .consume_and_create_grant(
            &consume(30, &digest(30), 30),
            &issuance(40, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        )
        .expect("consume and grant");
    assert!(receipt.first_user);
    assert_eq!(receipt.code.state, ConnectCodeState::Consumed);
    assert_eq!(receipt.code.revision, 2);
    assert_eq!(
        receipt.grant.permissions,
        GrantPermissions::USE_MANAGE_SHARE
    );
    assert_eq!(receipt.grant.grant_source, GrantSource::ConnectCode);
    assert_eq!(receipt.grant.trust_mode, GrantTrustMode::Trusted);
    assert_eq!(receipt.grant.state, AccessGrantState::Active);
    assert_eq!(receipt.grant.expires_at, None);

    // Replaying the consume never creates a second grant.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(30, &digest(30), 30),
            &issuance(41, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::CodeNotActive,
    );
    assert!(
        ledger
            .grant_snapshot(&grant_id(41))
            .expect("snapshot")
            .is_none()
    );
    assert_eq!(
        ledger
            .code_snapshot_by_digest(&digest(30))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Consumed
    );
}

#[test]
fn later_users_receive_use_only_and_one_active_grant_is_unique() {
    let mut storage = open(temporary_directory("later-users"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);
    let bob = user_id(3);

    ledger
        .publish(&publication(50, &client, T3, 3), &instant(T0))
        .expect("publish");
    let first = ledger
        .consume_and_create_grant(
            &consume(50, &digest(50), 50),
            &issuance(60, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        )
        .expect("first consume");
    assert!(first.first_user);

    ledger
        .publish(&publication(51, &client, T3, 3), &instant(T0))
        .expect("publish");
    let second = ledger
        .consume_and_create_grant(
            &consume(51, &digest(51), 51),
            &issuance(61, &client, &bob, &bob, GrantTrustMode::Temporary, Some(T2)),
            &instant(T0),
        )
        .expect("second consume");
    assert!(!second.first_user);
    assert_eq!(second.grant.permissions, GrantPermissions::USE);
    assert_eq!(second.grant.expires_at, Some(instant(T2)));

    // While Bob's grant is active, a new active grant for Bob conflicts.
    expect_kind(
        ledger.create_grant(
            &issuance(62, &client, &bob, &alice, GrantTrustMode::Trusted, None),
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::AccessGrantConflict,
    );

    // A failed consume rolls back atomically: the code stays active.
    ledger
        .publish(&publication(52, &client, T3, 3), &instant(T0))
        .expect("publish");
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(52, &digest(52), 52),
            &issuance(63, &client, &bob, &bob, GrantTrustMode::Trusted, None),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::AccessGrantConflict,
    );
    assert_eq!(
        ledger
            .code_snapshot(&code_id(52))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Active
    );

    // Even after revocation, a reconnecting user is a later user.
    ledger
        .revoke_grant(&first.grant.client_access_grant_id, 1)
        .expect("revoke first grant");
    ledger
        .publish(&publication(53, &client, T3, 3), &instant(T0))
        .expect("publish");
    let reconnected = ledger
        .consume_and_create_grant(
            &consume(53, &digest(53), 53),
            &issuance(64, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        )
        .expect("reconnect consume");
    assert!(!reconnected.first_user);
    assert_eq!(reconnected.grant.permissions, GrantPermissions::USE);
}

#[test]
fn failed_attempts_decrement_and_exhaust_the_code_budget() {
    let mut storage = open(temporary_directory("attempts"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);

    ledger
        .publish(&publication(70, &client, T3, 2), &instant(T0))
        .expect("publish");
    expect_kind(
        ledger.record_failed_attempt(&code_id(70), 99),
        ClientConnectStoreErrorKind::RevisionConflict,
    );

    let burnt = ledger
        .record_failed_attempt(&code_id(70), 1)
        .expect("failed attempt");
    assert_eq!(burnt.remaining_attempts, 1);
    assert_eq!(burnt.state, ConnectCodeState::Active);
    assert_eq!(burnt.revision, 2);

    let exhausted = ledger
        .record_failed_attempt(&code_id(70), 2)
        .expect("final failed attempt");
    assert_eq!(exhausted.remaining_attempts, 0);

    expect_kind(
        ledger.record_failed_attempt(&code_id(70), 3),
        ClientConnectStoreErrorKind::AttemptsExhausted,
    );
    // Consuming an exhausted code is rejected even inside the window.
    expect_kind(
        ledger.consume_and_create_grant(
            &consume(70, &digest(70), 70),
            &issuance(80, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::AttemptsExhausted,
    );
    expect_kind(
        ledger.record_failed_attempt(&code_id(71), 1),
        ClientConnectStoreErrorKind::UnknownConnectCode,
    );
}

#[test]
fn expire_codes_due_projects_only_due_active_codes() {
    let mut storage = open(temporary_directory("expire-codes"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);

    ledger
        .publish(&publication(90, &client, T1, 3), &instant(T0))
        .expect("publish");
    ledger
        .publish(&publication(91, &client, T3, 3), &instant(T0))
        .expect("publish");

    assert!(
        ledger
            .expire_codes_due(&instant(T0))
            .expect("early sweep")
            .is_empty()
    );

    let expired = ledger.expire_codes_due(&instant(T2)).expect("sweep");
    assert_eq!(expired, vec![code_id(90)]);
    assert_eq!(
        ledger
            .code_snapshot(&code_id(90))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Expired
    );
    assert_eq!(
        ledger
            .code_snapshot(&code_id(91))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Active
    );

    // The sweep is idempotent once every due code is terminal.
    assert!(
        ledger
            .expire_codes_due(&instant(T2))
            .expect("replay sweep")
            .is_empty()
    );

    // Consumed and revoked codes are never swept.
    ledger
        .publish(&publication(92, &client, T1, 3), &instant(T0))
        .expect("publish");
    let consumed = ledger
        .consume_and_create_grant(
            &consume(92, &digest(92), 92),
            &issuance(93, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        )
        .expect("consume");
    ledger
        .revoke_code(&ConnectCodeRevocation::try_new(code_id(91), 1).expect("revocation"))
        .expect("revoke");
    assert!(
        ledger
            .expire_codes_due(&instant(T4))
            .expect("sweep past everything")
            .is_empty()
    );
    assert_eq!(
        ledger
            .code_snapshot(&code_id(92))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Consumed
    );
    assert_eq!(
        ledger
            .code_snapshot(&code_id(91))
            .expect("snapshot")
            .expect("code")
            .state,
        ConnectCodeState::Revoked
    );
    assert_eq!(
        ledger
            .grant_snapshot(&consumed.grant.client_access_grant_id)
            .expect("grant snapshot")
            .expect("grant")
            .state,
        AccessGrantState::Active
    );
}

#[test]
fn standalone_grants_reject_connect_code_source_and_validate_expiry() {
    let mut storage = open(temporary_directory("standalone"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);
    let bob = user_id(3);

    // The connect-code source belongs to the atomic consume path only.
    expect_kind(
        ledger.create_grant(
            &issuance(100, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            GrantSource::ConnectCode,
            GrantPermissions::USE,
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );

    let administered = ledger
        .create_grant(
            &issuance(101, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            GrantSource::Administrator,
            GrantPermissions::try_new(true, true, false).expect("permissions"),
            &instant(T0),
        )
        .expect("create administrator grant");
    assert_eq!(administered.state, AccessGrantState::Active);
    assert_eq!(administered.grant_source, GrantSource::Administrator);
    assert_eq!(administered.permissions.as_str(), "use+manage");

    let confirmed = ledger
        .create_grant(
            &issuance(
                102,
                &client,
                &bob,
                &alice,
                GrantTrustMode::Temporary,
                Some(T1),
            ),
            GrantSource::LocalConfirmation,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("create local confirmation grant");
    assert_eq!(confirmed.trust_mode, GrantTrustMode::Temporary);
    assert_eq!(confirmed.expires_at, Some(instant(T1)));

    expect_kind(
        ledger.create_grant(
            &issuance(103, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::AccessGrantConflict,
    );
    expect_kind(
        ledger.create_grant(
            &issuance(
                104,
                &node_id(9),
                &bob,
                &alice,
                GrantTrustMode::Trusted,
                None,
            ),
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        ),
        ClientConnectStoreErrorKind::UnknownClientNode,
    );
    expect_kind(
        ledger.create_grant(
            &issuance(
                105,
                &client,
                &bob,
                &alice,
                GrantTrustMode::Temporary,
                Some(T0),
            ),
            GrantSource::LocalConfirmation,
            GrantPermissions::USE,
            &instant(T1),
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );
    expect_kind(
        GrantPermissions::try_new(false, true, true),
        ClientConnectStoreErrorKind::InvalidInput,
    );
    expect_kind(
        AccessGrantIssuance::try_new(
            grant_id(106),
            &client,
            &bob,
            &alice,
            GrantTrustMode::Temporary,
            None,
        ),
        ClientConnectStoreErrorKind::InvalidInput,
    );
}

#[test]
fn grant_revocation_is_immediate() {
    let mut storage = open(temporary_directory("grant-revoke"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);

    let trusted = ledger
        .create_grant(
            &issuance(110, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            GrantSource::Administrator,
            GrantPermissions::USE_MANAGE_SHARE,
            &instant(T0),
        )
        .expect("create");
    assert_eq!(
        ledger
            .active_grant(&client, &alice)
            .expect("active grant")
            .expect("grant")
            .client_access_grant_id,
        trusted.client_access_grant_id
    );
    assert_eq!(
        ledger
            .active_grants_for_user(&alice)
            .expect("active grants")
            .len(),
        1
    );

    expect_kind(
        ledger.revoke_grant(&trusted.client_access_grant_id, 99),
        ClientConnectStoreErrorKind::RevisionConflict,
    );
    let revoked = ledger
        .revoke_grant(&trusted.client_access_grant_id, 1)
        .expect("revoke");
    assert_eq!(revoked.state, AccessGrantState::Revoked);
    assert_eq!(revoked.revision, 2);
    // Revocation is immediately visible to the query surface.
    assert!(
        ledger
            .active_grant(&client, &alice)
            .expect("active grant")
            .is_none()
    );
    assert!(
        ledger
            .active_grants_for_user(&alice)
            .expect("active grants")
            .is_empty()
    );
    let replay = ledger
        .revoke_grant(&trusted.client_access_grant_id, 2)
        .expect("revoke replay");
    assert_eq!(replay.revision, 2);

    expect_kind(
        ledger.revoke_grant(&grant_id(999), 1),
        ClientConnectStoreErrorKind::UnknownAccessGrant,
    );
}

#[test]
fn temporary_grant_expiry_is_terminal_and_trusted_grants_persist() {
    let mut storage = open(temporary_directory("grant-expiry"));
    let client = seed_client_node(&mut storage, 1);
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);
    let bob = user_id(3);

    // A temporary grant expires by time judgement, and expired is terminal.
    let temporary = ledger
        .create_grant(
            &issuance(
                111,
                &client,
                &alice,
                &alice,
                GrantTrustMode::Temporary,
                Some(T1),
            ),
            GrantSource::LocalConfirmation,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("create temporary");
    assert!(
        ledger
            .expire_grants_due(&instant(T0))
            .expect("early sweep")
            .is_empty()
    );
    let expired = ledger.expire_grants_due(&instant(T1)).expect("sweep");
    assert_eq!(expired, vec![grant_id(111)]);
    assert!(
        ledger
            .active_grant(&client, &alice)
            .expect("active grant")
            .is_none()
    );
    expect_kind(
        ledger.revoke_grant(&temporary.client_access_grant_id, 2),
        ClientConnectStoreErrorKind::IllegalStateTransition,
    );
    assert!(
        ledger
            .expire_grants_due(&instant(T1))
            .expect("replay sweep")
            .is_empty()
    );

    // Trusted grants without an expiry never expire.
    let durable = ledger
        .create_grant(
            &issuance(112, &client, &bob, &alice, GrantTrustMode::Trusted, None),
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("create trusted");
    assert!(
        ledger
            .expire_grants_due(&instant(T4))
            .expect("sweep")
            .is_empty()
    );
    assert_eq!(
        ledger
            .active_grant(&client, &bob)
            .expect("active grant")
            .expect("grant")
            .client_access_grant_id,
        durable.client_access_grant_id
    );
}

#[test]
fn connect_attempt_windows_are_fixed_per_dimension_and_subject() {
    let mut storage = open(temporary_directory("attempts-window"));
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let alice = user_id(2);

    // The window anchor floors the instant onto the window grid.
    assert_eq!(
        connect_attempt_window_anchor(&instant("2026-01-01T00:00:37.000Z"), 60).expect("anchor"),
        instant("2026-01-01T00:00:00.000Z")
    );
    assert_eq!(
        connect_attempt_window_anchor(&instant("2026-06-15T12:34:56.000Z"), 3_600).expect("anchor"),
        instant("2026-06-15T12:00:00.000Z")
    );
    assert_eq!(
        connect_attempt_window_anchor(&instant("2026-12-31T23:59:59.000Z"), 86_400)
            .expect("anchor"),
        instant("2026-12-31T00:00:00.000Z")
    );
    assert_eq!(
        connect_attempt_window_anchor(&instant(T0), 0)
            .expect_err("zero window must be rejected")
            .kind(),
        ClientConnectStoreErrorKind::InvalidInput
    );

    let window = instant("2026-01-01T00:00:00.000Z");
    for expected in 1..=3 {
        let state = ledger
            .record_connect_failure(AttemptDimension::User, &alice, &window)
            .expect("record failure");
        assert_eq!(state.failed_attempts, expected);
        assert_eq!(state.window_started_at, window);
    }
    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::User, &alice, &window)
            .expect("count"),
        3
    );
    assert!(
        ledger
            .connect_attempts_blocked(AttemptDimension::User, &alice, &window, 3)
            .expect("blocked")
    );
    assert!(
        !ledger
            .connect_attempts_blocked(AttemptDimension::User, &alice, &window, 4)
            .expect("blocked")
    );

    // A different subject and a different dimension each count from zero.
    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::User, &user_id(3), &window)
            .expect("count"),
        0
    );
    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::Ip, &alice, &window)
            .expect("count"),
        0
    );

    // The next window reads as zero and resets the counter on first failure.
    let next_window = instant("2026-01-01T00:01:00.000Z");
    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::User, &alice, &next_window)
            .expect("count"),
        0
    );
    let reset = ledger
        .record_connect_failure(AttemptDimension::User, &alice, &next_window)
        .expect("record failure in next window");
    assert_eq!(reset.failed_attempts, 1);
    assert_eq!(reset.window_started_at, next_window);

    // The user dimension only accepts canonical user ids.
    expect_kind(
        ledger.record_connect_failure(AttemptDimension::User, "alice", &window),
        ClientConnectStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ledger.connect_attempts_blocked(AttemptDimension::User, &alice, &window, 0),
        ClientConnectStoreErrorKind::InvalidInput,
    );

    // A backwards clock resets the counter to the current window.
    let earlier_window = instant("2026-01-01T00:00:00.000Z");
    let backwards = ledger
        .record_connect_failure(AttemptDimension::User, &alice, &earlier_window)
        .expect("record failure after clock skew");
    assert_eq!(backwards.failed_attempts, 1);
    assert_eq!(backwards.window_started_at, earlier_window);
}

#[test]
fn connect_ip_and_client_dimensions_stay_independent() {
    let mut storage = open(temporary_directory("attempts-dimensions"));
    let mut ledger = storage.client_connect_ledger().expect("ledger");
    let client = node_id(1);
    let ip = "203.0.113.7";
    let window = instant("2026-01-01T00:00:00.000Z");

    ledger
        .record_connect_failure(AttemptDimension::Client, &client, &window)
        .expect("record client failure");
    ledger
        .record_connect_failure(AttemptDimension::Ip, ip, &window)
        .expect("record ip failure");
    ledger
        .record_connect_failure(AttemptDimension::Ip, ip, &window)
        .expect("record ip failure");

    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::Client, &client, &window)
            .expect("client count"),
        1
    );
    assert_eq!(
        ledger
            .connect_failure_count(AttemptDimension::Ip, ip, &window)
            .expect("ip count"),
        2
    );
    assert!(
        ledger
            .connect_attempts_blocked(AttemptDimension::Ip, ip, &window, 2)
            .expect("ip blocked")
    );
    assert!(
        !ledger
            .connect_attempts_blocked(AttemptDimension::Client, &client, &window, 2)
            .expect("client not blocked")
    );
}
