// SPDX-License-Identifier: Apache-2.0

//! Connect-code and access-grant application service vertical tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    AccessGrantService, ClientConnectServiceError, ClientConnectServiceErrorKind,
    ConnectCodeService,
};
use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, AccessGrantState, AttemptDimension, ConnectCodeConsume,
    ConnectCodePublication, ConnectCodeRevocation, ConnectCodeState, GrantPermissions, GrantSource,
    GrantTrustMode, SqliteStorage, connect_attempt_window_anchor,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-client-connect-service-{name}-{}-{suffix}",
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

fn open(path: PathBuf) -> SqliteStorage {
    SqliteStorage::open(path).expect("storage")
}

fn seed_client_node(storage: &mut SqliteStorage, seed: u64) -> String {
    let registration = winwincode_storage::ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "1.0.0",
        None,
        Some(instance_id(seed)),
        2,
    )
    .expect("registration");
    let mut registry = storage.client_node_registry().expect("registry");
    registry
        .register(&registration, 0, &instant(T0))
        .expect("register");
    node_id(seed)
}

fn publication(seed: u64, client_node_id: &str, expires_at: &str) -> ConnectCodePublication {
    ConnectCodePublication::try_new(
        code_id(seed),
        digest(seed),
        client_node_id,
        instance_id(seed),
        seed,
        instant(expires_at),
        3,
    )
    .expect("publication")
}

#[allow(clippy::too_many_arguments)]
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

fn consume(code: u64) -> ConnectCodeConsume {
    ConnectCodeConsume::try_new(code_id(code), digest(code), code).expect("consume")
}

fn expect_kind(error: &ClientConnectServiceError, kind: ClientConnectServiceErrorKind) {
    assert_eq!(error.kind(), kind);
}

#[test]
fn service_consumes_codes_exactly_once_and_derives_user_permissions() {
    let mut storage = open(temporary_directory("consume-vertical"));
    let client = seed_client_node(&mut storage, 1);
    let alice = user_id(2);
    let bob = user_id(3);

    let first = {
        let mut codes = ConnectCodeService::new(&mut storage);
        codes
            .publish(&publication(10, &client, T2), &instant(T0))
            .expect("publish");
        let snapshot = codes
            .code_snapshot(&code_id(10))
            .expect("snapshot")
            .expect("code");
        assert_eq!(snapshot.state, ConnectCodeState::Active);
        assert_eq!(snapshot.generation, 10);
        assert_eq!(codes.code_snapshot(&code_id(99)).expect("snapshot"), None);

        let first = codes
            .consume_and_grant(
                &consume(10),
                &issuance(20, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                &instant(T0),
            )
            .expect("first consume");
        assert!(first.first_user);
        assert_eq!(first.grant.permissions, GrantPermissions::USE_MANAGE_SHARE);
        assert_eq!(first.code.state, ConnectCodeState::Consumed);

        // A replayed consume never wins and never creates a second grant.
        let replay = codes
            .consume_and_grant(
                &consume(10),
                &issuance(21, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                &instant(T0),
            )
            .expect_err("replay consume");
        expect_kind(&replay, ClientConnectServiceErrorKind::CodeNotActive);
        first
    };

    // A later user only receives `use`.
    {
        let mut codes = ConnectCodeService::new(&mut storage);
        codes
            .publish(&publication(11, &client, T2), &instant(T0))
            .expect("publish");
        let second = codes
            .consume_and_grant(
                &consume(11),
                &issuance(
                    22,
                    &client,
                    &bob,
                    &alice,
                    GrantTrustMode::Temporary,
                    Some(T1),
                ),
                &instant(T0),
            )
            .expect("second consume");
        assert!(!second.first_user);
        assert_eq!(second.grant.permissions, GrantPermissions::USE);
        assert_eq!(second.grant.expires_at, Some(instant(T1)));
    }

    // Grants are durably visible per user and client.
    {
        let mut grants = AccessGrantService::new(&mut storage);
        let active_alice = grants
            .active_grant(&client, &alice)
            .expect("active grant")
            .expect("grant");
        assert_eq!(active_alice.client_access_grant_id, grant_id(20));
        assert_eq!(
            grants
                .active_grants_for_user(&alice)
                .expect("active grants")
                .len(),
            1
        );
        assert!(
            grants
                .active_grant(&client, &bob)
                .expect("active grant")
                .is_some()
        );
        let stored = grants
            .grant_snapshot(&first.grant.client_access_grant_id)
            .expect("snapshot")
            .expect("grant");
        assert_eq!(stored.state, AccessGrantState::Active);
        assert_eq!(stored.grant_source, GrantSource::ConnectCode);
    }
}

#[test]
fn service_revocation_is_immediate_and_terminal_states_hold() {
    let mut storage = open(temporary_directory("revoke-vertical"));
    let client = seed_client_node(&mut storage, 1);
    let alice = user_id(2);

    let grant_id_owned = {
        let mut grants = AccessGrantService::new(&mut storage);
        let created = grants
            .create_grant(
                &issuance(30, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                GrantSource::Administrator,
                GrantPermissions::try_new(true, true, false).expect("permissions"),
                &instant(T0),
            )
            .expect("create");
        assert_eq!(created.permissions.as_str(), "use+manage");
        assert!(
            grants
                .active_grant(&client, &alice)
                .expect("active grant")
                .is_some()
        );
        expect_kind(
            &grants
                .revoke_grant(&created.client_access_grant_id, 99)
                .expect_err("stale revision"),
            ClientConnectServiceErrorKind::RevisionConflict,
        );
        created.client_access_grant_id
    };

    {
        let mut grants = AccessGrantService::new(&mut storage);
        let revoked = grants.revoke_grant(&grant_id_owned, 1).expect("revoke");
        assert_eq!(revoked.state, AccessGrantState::Revoked);
        // Revocation is immediately visible to the query surface.
        assert!(
            grants
                .active_grant(&client, &alice)
                .expect("active grant")
                .is_none()
        );
        assert!(
            grants
                .active_grants_for_user(&alice)
                .expect("active grants")
                .is_empty()
        );
        // Revoking a revoked grant is an idempotent replay.
        let replay = grants.revoke_grant(&grant_id_owned, 2).expect("replay");
        assert_eq!(replay.state, AccessGrantState::Revoked);
        // The connect-code source can never bypass the consume path.
        expect_kind(
            &grants
                .create_grant(
                    &issuance(31, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                    GrantSource::ConnectCode,
                    GrantPermissions::USE,
                    &instant(T0),
                )
                .expect_err("connect code source"),
            ClientConnectServiceErrorKind::InvalidInput,
        );
    }

    // Codes revoke and refresh through the service with frozen transitions.
    {
        let mut codes = ConnectCodeService::new(&mut storage);
        codes
            .publish(&publication(40, &client, T3), &instant(T0))
            .expect("publish");
        let refreshed = codes
            .refresh_code(
                &ConnectCodeRevocation::try_new(code_id(40), 1).expect("revocation"),
                &publication(41, &client, T3),
                &instant(T1),
            )
            .expect("refresh");
        assert_eq!(refreshed.connect_code_id, code_id(41));
        assert_eq!(refreshed.state, ConnectCodeState::Active);
        let old = codes
            .code_snapshot(&code_id(40))
            .expect("snapshot")
            .expect("code");
        assert_eq!(old.state, ConnectCodeState::Revoked);

        let revoked = codes
            .revoke_code(&ConnectCodeRevocation::try_new(code_id(41), 1).expect("revocation"))
            .expect("revoke");
        assert_eq!(revoked.state, ConnectCodeState::Revoked);
        // Revoking a refresh-revoked code is an idempotent replay.
        let replay = codes
            .revoke_code(&ConnectCodeRevocation::try_new(code_id(41), 2).expect("revocation"))
            .expect("replay revoke");
        assert_eq!(replay.revision, 2);
    }
}

#[test]
fn service_rejects_revoking_a_consumed_code() {
    let mut storage = open(temporary_directory("consumed-vertical"));
    let client = seed_client_node(&mut storage, 1);
    let alice = user_id(2);

    let mut codes = ConnectCodeService::new(&mut storage);
    codes
        .publish(&publication(42, &client, T3), &instant(T0))
        .expect("publish");
    codes
        .consume_and_grant(
            &consume(42),
            &issuance(32, &client, &alice, &alice, GrantTrustMode::Trusted, None),
            &instant(T0),
        )
        .expect("consume");
    expect_kind(
        &codes
            .revoke_code(&ConnectCodeRevocation::try_new(code_id(42), 2).expect("revocation"))
            .expect_err("revoke consumed code"),
        ClientConnectServiceErrorKind::IllegalStateTransition,
    );
}

#[test]
fn service_expires_temporary_grants_and_codes_by_time_judgement() {
    let mut storage = open(temporary_directory("expiry-vertical"));
    let client = seed_client_node(&mut storage, 1);
    let alice = user_id(2);
    let bob = user_id(3);

    {
        let mut grants = AccessGrantService::new(&mut storage);
        grants
            .create_grant(
                &issuance(
                    50,
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
            .expect("create temporary grant");
        grants
            .create_grant(
                &issuance(51, &client, &bob, &alice, GrantTrustMode::Trusted, None),
                GrantSource::Administrator,
                GrantPermissions::USE,
                &instant(T0),
            )
            .expect("create trusted grant");
        assert!(
            grants
                .expire_grants_due(&instant(T0))
                .expect("early sweep")
                .is_empty()
        );
    }

    {
        let mut grants = AccessGrantService::new(&mut storage);
        let expired = grants.expire_grants_due(&instant(T1)).expect("sweep");
        assert_eq!(expired, vec![grant_id(50)]);
        assert!(
            grants
                .active_grant(&client, &alice)
                .expect("active grant")
                .is_none()
        );
        // The trusted grant without an expiry never expires.
        assert!(
            grants
                .active_grant(&client, &bob)
                .expect("active grant")
                .is_some()
        );
    }

    {
        let mut codes = ConnectCodeService::new(&mut storage);
        codes
            .publish(&publication(60, &client, T1), &instant(T0))
            .expect("publish");
        assert!(
            codes
                .expire_codes_due(&instant(T0))
                .expect("early sweep")
                .is_empty()
        );
        let expired = codes.expire_codes_due(&instant(T1)).expect("sweep");
        assert_eq!(expired, vec![code_id(60)]);
        // An expired code can no longer be consumed even before the sweep.
        codes
            .publish(&publication(61, &client, T1), &instant(T0))
            .expect("publish");
        let late = codes
            .consume_and_grant(
                &consume(61),
                &issuance(70, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                &instant(T1),
            )
            .expect_err("consume after expiry");
        expect_kind(&late, ClientConnectServiceErrorKind::ConnectCodeExpired);
    }
}

#[test]
fn service_throttles_connect_attempts_with_fixed_windows() {
    let mut storage = open(temporary_directory("throttle-vertical"));
    let anchor =
        connect_attempt_window_anchor(&instant("2026-01-01T00:00:37.000Z"), 60).expect("anchor");
    assert_eq!(anchor, instant(T0));
    let next_window = instant(T1);

    {
        let mut codes = ConnectCodeService::new(&mut storage);
        for expected in 1..=3 {
            let state = codes
                .record_connect_failure(AttemptDimension::Ip, "203.0.113.9", &anchor)
                .expect("record failure");
            assert_eq!(state.failed_attempts, expected);
        }
        assert!(
            codes
                .connect_attempts_blocked(AttemptDimension::Ip, "203.0.113.9", &anchor, 3)
                .expect("blocked")
        );
        assert!(
            !codes
                .connect_attempts_blocked(AttemptDimension::Ip, "203.0.113.9", &anchor, 4)
                .expect("not blocked")
        );
        // Other dimensions and subjects count independently.
        assert!(
            !codes
                .connect_attempts_blocked(AttemptDimension::User, &user_id(9), &anchor, 1)
                .expect("user not blocked")
        );
        assert_eq!(
            codes
                .connect_failure_count(AttemptDimension::Ip, "203.0.113.9", &next_window)
                .expect("count"),
            0
        );
        let reset = codes
            .record_connect_failure(AttemptDimension::Ip, "203.0.113.9", &next_window)
            .expect("record in next window");
        assert_eq!(reset.failed_attempts, 1);
        // The user dimension only accepts canonical user ids.
        expect_kind(
            &codes
                .record_connect_failure(AttemptDimension::User, "someone", &anchor)
                .expect_err("non-canonical user key"),
            ClientConnectServiceErrorKind::InvalidInput,
        );
    }
}

#[test]
fn service_burns_attempts_and_reports_exhaustion_before_consume() {
    let mut storage = open(temporary_directory("exhaustion-vertical"));
    let client = seed_client_node(&mut storage, 1);
    let alice = user_id(2);

    let short_lived = ConnectCodePublication::try_new(
        code_id(80),
        digest(80),
        &client,
        instance_id(80),
        80,
        instant(T2),
        1,
    )
    .expect("publication");

    {
        let mut codes = ConnectCodeService::new(&mut storage);
        codes.publish(&short_lived, &instant(T0)).expect("publish");
        let burnt = codes
            .record_failed_attempt(&code_id(80), 1)
            .expect("failed attempt");
        assert_eq!(burnt.remaining_attempts, 0);
        expect_kind(
            &codes
                .record_failed_attempt(&code_id(80), 2)
                .expect_err("exhausted budget"),
            ClientConnectServiceErrorKind::AttemptsExhausted,
        );
        expect_kind(
            &codes
                .consume_and_grant(
                    &consume(80),
                    &issuance(90, &client, &alice, &alice, GrantTrustMode::Trusted, None),
                    &instant(T0),
                )
                .expect_err("consume exhausted code"),
            ClientConnectServiceErrorKind::AttemptsExhausted,
        );
        // Malformed publications are rejected before any durable write.
        let malformed = ConnectCodePublication::try_new(
            code_id(81),
            "sha256:short",
            &client,
            instance_id(81),
            1,
            instant(T1),
            1,
        )
        .expect_err("malformed digest");
        assert_eq!(
            malformed.kind(),
            winwincode_storage::ClientConnectStoreErrorKind::InvalidInput
        );
    }

    // The exhausted consume rolled back: the client still has no grant.
    {
        let mut grants = AccessGrantService::new(&mut storage);
        assert!(
            grants
                .active_grant(&client, &alice)
                .expect("active grant")
                .is_none()
        );
    }
}
