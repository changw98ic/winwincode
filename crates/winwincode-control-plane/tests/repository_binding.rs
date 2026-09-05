// SPDX-License-Identifier: Apache-2.0

//! `RepositoryBindingService` and `RepositoryAccessGrantService` vertical
//! tests (plan 7.6, 7.7, 13.4, 13.5; contract 7).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    RepositoryAccessGrantService, RepositoryAvailability, RepositoryBindingService,
    RepositoryBindingServiceError, RepositoryBindingServiceErrorKind, RepositoryDirtyState,
};
use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, RepositoryAccessGrantIssuance, RepositoryBindingProjection,
    RepositoryGrantPermissions, RepositoryGrantState, RepositoryScanOutcome, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-repository-binding-service-{name}-{}-{suffix}-{nanos}",
        std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn node_id(seed: u64) -> String {
    format!("cnd_{seed:026}")
}

fn binding_id(seed: u64) -> String {
    format!("rbd_{seed:026}")
}

fn grant_id(seed: u64) -> String {
    format!("rag_{seed:026}")
}

fn client_grant_id(seed: u64) -> String {
    format!("cag_{seed:026}")
}

fn user_id(seed: u64) -> String {
    format!("usr_{seed:026}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";

fn projection(seed: u64, client: &str, fingerprint: &str) -> RepositoryBindingProjection {
    RepositoryBindingProjection::try_new(
        binding_id(seed),
        client,
        format!("Repo {seed}"),
        Some("main".to_owned()),
        Some(format!("{seed:040x}")),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        fingerprint,
    )
    .expect("projection")
}

fn issuance(seed: u64, binding: &str, user: &str) -> RepositoryAccessGrantIssuance {
    RepositoryAccessGrantIssuance::try_new(grant_id(seed), binding, user, user).expect("issuance")
}

/// Builds one validated scan outcome for the projection-machine tests.
fn outcome(
    availability: RepositoryAvailability,
    dirty_state: RepositoryDirtyState,
) -> RepositoryScanOutcome {
    RepositoryScanOutcome::try_new(availability, dirty_state).expect("outcome")
}

fn expect_kind(error: &RepositoryBindingServiceError, kind: RepositoryBindingServiceErrorKind) {
    assert_eq!(error.kind(), kind);
}

/// Seeds one registered, `online` client node and returns its id.
fn seed_client(storage: &mut SqliteStorage, seed: u64) -> String {
    let registration = ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "1.0.0",
        None,
        None,
        2,
    )
    .expect("registration");
    {
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &instant(T0))
            .expect("register");
        registry
            .update_presence(node_id(seed).as_str(), ClientPresenceState::Online, 1)
            .expect("presence online");
    }
    node_id(seed)
}

/// Creates one active `use` client access grant for `holder`.
fn seed_client_grant(storage: &mut SqliteStorage, seed: u64, client: &str, holder: &str) {
    let issuance = AccessGrantIssuance::try_new(
        client_grant_id(seed),
        client,
        holder,
        holder,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("client grant issuance");
    storage
        .client_connect_ledger()
        .expect("connect ledger")
        .create_grant(
            &issuance,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("client grant");
}

#[test]
fn service_upsert_is_idempotent_by_binding_id_and_cas_guarded() {
    let mut storage = SqliteStorage::open(temporary_directory("upsert")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let mut service = RepositoryBindingService::new(&mut storage);

    let first = service
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("first upsert");
    assert!(first.enrolled);
    assert_eq!(first.record.revision, 1);
    assert_eq!(first.record.client_node_id, client);

    // The identical re-report is an accepted replay: the revision stays 1.
    let replay = service
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("replay upsert");
    assert!(!replay.enrolled);
    assert_eq!(replay.record.revision, 1);

    // A changed projection advances the revision once under CAS.
    let changed = RepositoryBindingProjection::try_new(
        binding_id(10),
        &client,
        "Renamed Repo",
        Some("develop".to_owned()),
        Some("ffffffffffffffffffffffffffffffffffffffff".to_owned()),
        RepositoryDirtyState::Dirty,
        RepositoryAvailability::Dirty,
        "fingerprint-a",
    )
    .expect("changed projection");
    let refreshed = service
        .upsert(
            &changed,
            Some(&instant(T2)),
            first.record.revision,
            &instant(T2),
        )
        .expect("refresh upsert");
    assert_eq!(refreshed.record.revision, 2);
    assert_eq!(refreshed.record.availability, RepositoryAvailability::Dirty);

    // A stale expectedRevision fails closed.
    let error = service
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T3)),
            1,
            &instant(T3),
        )
        .expect_err("stale CAS must fail");
    expect_kind(&error, RepositoryBindingServiceErrorKind::RevisionConflict);

    // Removal is idempotent: the first call reports removal, the replay
    // reports nothing left to remove.
    assert!(service.remove(binding_id(10).as_str()).expect("remove"));
    assert!(
        !service
            .remove(binding_id(10).as_str())
            .expect("remove replay")
    );
    assert!(
        service
            .snapshot(binding_id(10).as_str())
            .expect("snapshot")
            .is_none()
    );
}

#[test]
fn service_updates_availability_across_all_seven_states() {
    let mut storage = SqliteStorage::open(temporary_directory("seven-states")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let mut service = RepositoryBindingService::new(&mut storage);
    let enrolled = service
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("upsert");
    let binding = binding_id(10);
    let mut revision = enrolled.record.revision;

    // The projection machine is non-transactional: any state may move to any
    // other state after re-verification, and there are no terminal states.
    let chain = [
        (
            outcome(
                RepositoryAvailability::Unavailable,
                RepositoryDirtyState::Clean,
            ),
            T2,
        ),
        (
            outcome(
                RepositoryAvailability::PermissionDenied,
                RepositoryDirtyState::Clean,
            ),
            T3,
        ),
        (
            outcome(
                RepositoryAvailability::InvalidGit,
                RepositoryDirtyState::Clean,
            ),
            T1,
        ),
        (
            outcome(RepositoryAvailability::Moved, RepositoryDirtyState::Clean),
            T2,
        ),
        (
            outcome(
                RepositoryAvailability::ScanFailed,
                RepositoryDirtyState::Clean,
            ),
            T3,
        ),
        (
            outcome(RepositoryAvailability::Dirty, RepositoryDirtyState::Dirty),
            T1,
        ),
        (
            outcome(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Clean,
            ),
            T2,
        ),
    ];
    for (reported, scanned_at) in chain {
        let updated = service
            .update_availability(binding.as_str(), &reported, &instant(scanned_at), revision)
            .expect("availability update");
        assert_eq!(updated.availability, reported.availability());
        revision += 1;
        assert_eq!(updated.revision, revision);
    }

    // An identical report is an accepted idempotent replay.
    let replay = service
        .update_availability(
            binding.as_str(),
            &outcome(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Clean,
            ),
            &instant(T2),
            revision,
        )
        .expect("availability replay");
    assert_eq!(replay.revision, revision);

    // An unknown binding fails closed.
    let error = service
        .update_availability(
            binding_id(99).as_str(),
            &outcome(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Clean,
            ),
            &instant(T2),
            revision,
        )
        .expect_err("unknown binding must fail");
    expect_kind(
        &error,
        RepositoryBindingServiceErrorKind::UnknownRepositoryBinding,
    );
}

#[test]
fn service_visibility_requires_both_client_and_repository_grants() {
    let mut storage = SqliteStorage::open(temporary_directory("visibility")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let outsider = user_id(2);
    let member = user_id(3);
    seed_client_grant(&mut storage, 30, &client, member.as_str());
    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        bindings
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding one");
        bindings
            .upsert(
                &projection(11, &client, "fingerprint-b"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding two");
    }
    // Grant phase through the grant service.
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        // A repository grant alone never makes a binding visible.
        grants
            .create_grant(
                &issuance(40, binding_id(10).as_str(), outsider.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("outsider repo grant");
        grants
            .create_grant(
                &issuance(41, binding_id(11).as_str(), member.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("member repo grant");
    }

    // No client grant: invisible even with a repository grant.
    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        assert!(
            bindings
                .visible_bindings(outsider.as_str(), &client)
                .expect("outsider visibility")
                .is_empty()
        );
    }
    // Client grant plus one repository grant: exactly that binding shows up.
    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        let visible = bindings
            .visible_bindings(member.as_str(), &client)
            .expect("member visibility");
        assert_eq!(
            visible
                .iter()
                .map(|record| record.repository_binding_id.as_str())
                .collect::<Vec<_>>(),
            vec![binding_id(11)]
        );
    }

    // Second repository grant widens the visible set.
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        grants
            .create_grant(
                &issuance(42, binding_id(10).as_str(), member.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("second member repo grant");
        assert_eq!(
            grants
                .active_grants_for_user(member.as_str())
                .expect("active grants")
                .len(),
            2
        );
    }

    let mut bindings = RepositoryBindingService::new(&mut storage);
    let visible = bindings
        .visible_bindings(member.as_str(), &client)
        .expect("member visibility");
    assert_eq!(visible.len(), 2);
}

#[test]
fn service_repo_grant_revoke_is_immediate_and_duplicates_conflict() {
    let mut storage = SqliteStorage::open(temporary_directory("revoke")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let member = user_id(3);
    seed_client_grant(&mut storage, 30, &client, member.as_str());
    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        bindings
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding one");
        bindings
            .upsert(
                &projection(11, &client, "fingerprint-b"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding two");
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        grants
            .create_grant(
                &issuance(41, binding_id(11).as_str(), member.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("member repo grant on binding two");
        grants
            .create_grant(
                &issuance(42, binding_id(10).as_str(), member.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("member repo grant on binding one");
    }

    // Revocation ends visibility immediately; the replay is accepted.
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        let revoked = grants
            .revoke_grant(grant_id(42).as_str(), 1)
            .expect("revoke");
        assert_eq!(revoked.state, RepositoryGrantState::Revoked);
        let replay = grants
            .revoke_grant(grant_id(42).as_str(), revoked.revision)
            .expect("revoke replay");
        assert_eq!(replay.state, RepositoryGrantState::Revoked);
        assert_eq!(
            grants
                .active_grants_for_user(member.as_str())
                .expect("active grants")
                .len(),
            1
        );
        // A duplicate active grant for the same user and binding conflicts.
        let duplicate = grants
            .create_grant(
                &issuance(43, binding_id(11).as_str(), member.as_str()),
                RepositoryGrantPermissions::UseManage,
                &instant(T2),
            )
            .expect_err("duplicate active grant must fail");
        expect_kind(
            &duplicate,
            RepositoryBindingServiceErrorKind::AccessGrantConflict,
        );
    }

    let mut bindings = RepositoryBindingService::new(&mut storage);
    let visible = bindings
        .visible_bindings(member.as_str(), &client)
        .expect("member visibility");
    assert_eq!(
        visible
            .iter()
            .map(|record| record.repository_binding_id.as_str())
            .collect::<Vec<_>>(),
        vec![binding_id(11)],
        "only the still-active repository grant keeps its binding visible"
    );
}

#[test]
fn service_multi_users_hold_disjoint_sets_and_removal_cascades() {
    let mut storage = SqliteStorage::open(temporary_directory("multi-user")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let alice = user_id(2);
    let bob = user_id(3);
    seed_client_grant(&mut storage, 30, &client, alice.as_str());
    seed_client_grant(&mut storage, 31, &client, bob.as_str());
    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        bindings
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding one");
        bindings
            .upsert(
                &projection(11, &client, "fingerprint-b"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding two");
    }
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        grants
            .create_grant(
                &issuance(40, binding_id(10).as_str(), alice.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("alice grant");
        grants
            .create_grant(
                &issuance(41, binding_id(11).as_str(), bob.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("bob grant");
    }

    {
        let mut bindings = RepositoryBindingService::new(&mut storage);
        let alice_visible = bindings
            .visible_bindings(alice.as_str(), &client)
            .expect("alice visibility");
        assert_eq!(
            alice_visible
                .iter()
                .map(|record| record.repository_binding_id.as_str())
                .collect::<Vec<_>>(),
            vec![binding_id(10)]
        );
        let bob_visible = bindings
            .visible_bindings(bob.as_str(), &client)
            .expect("bob visibility");
        assert_eq!(
            bob_visible
                .iter()
                .map(|record| record.repository_binding_id.as_str())
                .collect::<Vec<_>>(),
            vec![binding_id(11)]
        );

        // Removing binding one cascades its grants away: Alice's visible set
        // and her active grants both empty out, while Bob is untouched.
        assert!(bindings.remove(binding_id(10).as_str()).expect("remove"));
        assert!(
            bindings
                .visible_bindings(alice.as_str(), &client)
                .expect("alice visibility")
                .is_empty()
        );
    }
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        assert!(
            grants
                .active_grants_for_user(alice.as_str())
                .expect("alice grants")
                .is_empty()
        );
        assert_eq!(
            grants
                .active_grants_for_user(bob.as_str())
                .expect("bob grants")
                .len(),
            1
        );
    }
    let mut bindings = RepositoryBindingService::new(&mut storage);
    let bob_visible = bindings
        .visible_bindings(bob.as_str(), &client)
        .expect("bob visibility");
    assert_eq!(
        bob_visible
            .iter()
            .map(|record| record.repository_binding_id.as_str())
            .collect::<Vec<_>>(),
        vec![binding_id(11)],
        "bob's visibility is untouched by the removal"
    );
}
