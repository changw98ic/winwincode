// SPDX-License-Identifier: Apache-2.0

//! Durable `RepositoryBinding` registry and `RepositoryAccessGrant` ledger
//! contract tests (plan 7.6, 7.7, 13.4, 13.5; contract 7).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, RepositoryAccessGrantIssuance, RepositoryAvailability,
    RepositoryBindingProjection, RepositoryBindingStoreErrorKind, RepositoryDirtyState,
    RepositoryGrantPermissions, RepositoryGrantState, RepositoryScanOutcome, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-repository-binding-{name}-{}-{suffix}",
        std::process::id()
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
        Some("1111111111111111111111111111111111111111".to_owned()),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        fingerprint,
    )
    .expect("projection")
}

/// Seeds one registered, `online` client node and returns its id.
fn seed_client(storage: &mut SqliteStorage, seed: u64) -> String {
    let registration = ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        None,
        2,
    )
    .expect("registration");
    let mut registry = storage.client_node_registry().expect("registry");
    registry
        .register(&registration, 0, &instant(T0))
        .expect("register");
    registry
        .update_presence(node_id(seed).as_str(), ClientPresenceState::Online, 1)
        .expect("presence online");
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

#[test]
fn upserts_are_idempotent_by_binding_id_and_cas_guarded() {
    let mut storage = SqliteStorage::open(temporary_directory("upsert")).expect("storage");
    let client = seed_client(&mut storage, 1);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");

        // First report creates the binding at revision 1.
        let first = ledger
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("first upsert");
        assert!(first.enrolled);
        assert_eq!(first.record.revision, 1);
        assert_eq!(first.record.repository_kind, "git");
        assert_eq!(first.record.availability, RepositoryAvailability::Available);
        assert_eq!(first.record.dirty_state, RepositoryDirtyState::Clean);
        assert_eq!(
            first.record.head_commit.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(first.record.last_scanned_at, Some(instant(T1)));

        // A byte-identical re-report is an accepted idempotent replay that
        // leaves the revision untouched.
        let replay = ledger
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("replay upsert");
        assert!(!replay.enrolled);
        assert_eq!(replay.record.revision, 1);

        // A changed projection advances the revision exactly once under CAS.
        let changed = RepositoryBindingProjection::try_new(
            binding_id(10),
            &client,
            "Renamed Repo",
            Some("develop".to_owned()),
            Some("2222222222222222222222222222222222222222".to_owned()),
            RepositoryDirtyState::Dirty,
            RepositoryAvailability::Dirty,
            "fingerprint-a",
        )
        .expect("changed projection");
        let refreshed = ledger
            .upsert(
                &changed,
                Some(&instant(T2)),
                first.record.revision,
                &instant(T2),
            )
            .expect("refresh upsert");
        assert!(!refreshed.enrolled);
        assert_eq!(refreshed.record.revision, 2);
        assert_eq!(refreshed.record.display_name, "Renamed Repo");
        assert_eq!(refreshed.record.availability, RepositoryAvailability::Dirty);

        // A stale expectedRevision fails closed.
        let error = ledger
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T3)),
                1,
                &instant(T3),
            )
            .expect_err("stale CAS must fail");
        assert_eq!(
            error.kind(),
            RepositoryBindingStoreErrorKind::RevisionConflict
        );
    }
}

#[test]
fn fingerprint_is_unique_per_client_and_freed_by_removal() {
    let mut storage = SqliteStorage::open(temporary_directory("fingerprint")).expect("storage");
    let client = seed_client(&mut storage, 1);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        ledger
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("first upsert");

        // A different binding id claiming the same fingerprint on the same
        // client fails closed.
        let conflict = ledger
            .upsert(
                &projection(11, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect_err("duplicate fingerprint must fail");
        assert_eq!(
            conflict.kind(),
            RepositoryBindingStoreErrorKind::FingerprintConflict
        );
    }
    // The same fingerprint on a different client is fine.
    let other = seed_client(&mut storage, 2);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let cross_client = ledger
            .upsert(
                &projection(12, &other, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("same fingerprint on another client");
        assert!(cross_client.enrolled);

        // Removing the first binding frees the fingerprint for a new binding
        // id on the original client.
        let removed = ledger.remove(binding_id(10).as_str()).expect("remove");
        assert!(removed);
        let re_registered = ledger
            .upsert(
                &projection(13, &client, "fingerprint-a"),
                Some(&instant(T2)),
                0,
                &instant(T2),
            )
            .expect("re-registration after removal");
        assert!(re_registered.enrolled);
        assert_ne!(
            re_registered.record.repository_binding_id,
            binding_id(10),
            "a fresh binding id replaces the removed binding"
        );
        assert_eq!(
            ledger.snapshot(binding_id(10).as_str()).expect("snapshot"),
            None
        );
    }
}

#[test]
fn seven_availability_states_move_freely_after_reverification() {
    let mut storage = SqliteStorage::open(temporary_directory("seven-states")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let enrolled = ledger
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
            outcome(RepositoryAvailability::Moved, RepositoryDirtyState::Clean),
            T2,
        ),
        (
            outcome(
                RepositoryAvailability::Available,
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
                RepositoryAvailability::Unavailable,
                RepositoryDirtyState::Dirty,
            ),
            T2,
        ),
        (
            outcome(
                RepositoryAvailability::ScanFailed,
                RepositoryDirtyState::Dirty,
            ),
            T3,
        ),
        (
            outcome(
                RepositoryAvailability::PermissionDenied,
                RepositoryDirtyState::Dirty,
            ),
            T1,
        ),
        (
            outcome(
                RepositoryAvailability::InvalidGit,
                RepositoryDirtyState::Dirty,
            ),
            T2,
        ),
        (
            outcome(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Clean,
            ),
            T3,
        ),
    ];
    for (reported, scanned_at) in chain {
        let updated = ledger
            .update_availability(binding.as_str(), &reported, &instant(scanned_at), revision)
            .expect("availability update");
        assert_eq!(updated.availability, reported.availability());
        assert_eq!(updated.dirty_state, reported.dirty_state());
        assert_eq!(updated.last_scanned_at, Some(instant(scanned_at)));
        revision += 1;
        assert_eq!(updated.revision, revision);
    }

    // An identical report is an accepted idempotent replay.
    let replay = ledger
        .update_availability(
            binding.as_str(),
            &outcome(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Clean,
            ),
            &instant(T3),
            revision,
        )
        .expect("availability replay");
    assert_eq!(replay.revision, revision);
}

#[test]
fn only_available_and_dirty_allow_launch_and_stale_cas_fails() {
    let mut storage = SqliteStorage::open(temporary_directory("launch-gate")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let enrolled = ledger
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("upsert");
    let binding = binding_id(10);

    // Only `available` and `dirty` carry the launch gate (contract 7).
    assert!(RepositoryAvailability::Available.allows_launch());
    assert!(RepositoryAvailability::Dirty.allows_launch());
    assert!(!RepositoryAvailability::Unavailable.allows_launch());
    assert!(!RepositoryAvailability::Moved.allows_launch());
    assert!(!RepositoryAvailability::InvalidGit.allows_launch());
    assert!(!RepositoryAvailability::PermissionDenied.allows_launch());
    assert!(!RepositoryAvailability::ScanFailed.allows_launch());

    // A stale expectedRevision fails closed.
    let error = ledger
        .update_availability(
            binding.as_str(),
            &outcome(RepositoryAvailability::Moved, RepositoryDirtyState::Clean),
            &instant(T3),
            enrolled.record.revision + 1,
        )
        .expect_err("stale CAS must fail");
    assert_eq!(
        error.kind(),
        RepositoryBindingStoreErrorKind::RevisionConflict
    );
}

#[test]
fn grants_are_unique_active_per_user_and_revoke_is_immediate() {
    let mut storage = SqliteStorage::open(temporary_directory("grants")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let binding = binding_id(10);
    let holder = user_id(2);
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    ledger
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("upsert");

    let grant = ledger
        .create_grant(
            &issuance(20, binding.as_str(), holder.as_str()),
            RepositoryGrantPermissions::Use,
            &instant(T1),
        )
        .expect("grant");
    assert_eq!(grant.state, RepositoryGrantState::Active);
    assert_eq!(grant.permissions, RepositoryGrantPermissions::Use);
    assert_eq!(
        ledger
            .active_grants_for_binding(binding.as_str())
            .expect("grants")
            .len(),
        1
    );

    // A second active grant for the same user and binding conflicts.
    let duplicate = ledger
        .create_grant(
            &issuance(21, binding.as_str(), holder.as_str()),
            RepositoryGrantPermissions::UseManage,
            &instant(T1),
        )
        .expect_err("duplicate active grant must fail");
    assert_eq!(
        duplicate.kind(),
        RepositoryBindingStoreErrorKind::AccessGrantConflict
    );

    // Revocation is immediate; a replay is an accepted no-op.
    let revoked = ledger
        .revoke_grant(grant.repository_access_grant_id.as_str(), grant.revision)
        .expect("revoke");
    assert_eq!(revoked.state, RepositoryGrantState::Revoked);
    assert!(
        ledger
            .active_grants_for_binding(binding.as_str())
            .expect("grants")
            .is_empty()
    );
    let replay = ledger
        .revoke_grant(grant.repository_access_grant_id.as_str(), revoked.revision)
        .expect("revoke replay");
    assert_eq!(replay.state, RepositoryGrantState::Revoked);

    // A stale expectedRevision fails closed on an active grant.
    let fresh = ledger
        .create_grant(
            &issuance(22, binding.as_str(), holder.as_str()),
            RepositoryGrantPermissions::Use,
            &instant(T2),
        )
        .expect("re-grant after revoke");
    let error = ledger
        .revoke_grant(fresh.repository_access_grant_id.as_str(), 0)
        .expect_err("stale CAS must fail");
    assert_eq!(
        error.kind(),
        RepositoryBindingStoreErrorKind::RevisionConflict
    );

    // Removing the binding cascades its grants away.
    ledger.remove(binding.as_str()).expect("remove");
    assert!(
        ledger
            .grant_snapshot(fresh.repository_access_grant_id.as_str())
            .expect("grant snapshot")
            .is_none()
    );
}

#[test]
fn visibility_requires_both_client_and_repository_grants() {
    let mut storage = SqliteStorage::open(temporary_directory("visibility")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let outsider = user_id(2);
    let member = user_id(3);
    seed_client_grant(&mut storage, 30, &client, member.as_str());
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    ledger
        .upsert(
            &projection(10, &client, "fingerprint-a"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("binding one");
    ledger
        .upsert(
            &projection(11, &client, "fingerprint-b"),
            Some(&instant(T1)),
            0,
            &instant(T1),
        )
        .expect("binding two");

    // No client grant at all: invisible even with a repository grant.
    ledger
        .create_grant(
            &issuance(40, binding_id(10).as_str(), outsider.as_str()),
            RepositoryGrantPermissions::Use,
            &instant(T1),
        )
        .expect("repo grant without client grant");
    assert!(
        ledger
            .visible_bindings(outsider.as_str(), &client)
            .expect("visibility")
            .is_empty()
    );

    // A client grant plus a repository grant on only one binding makes
    // exactly that binding visible.
    ledger
        .create_grant(
            &issuance(41, binding_id(11).as_str(), member.as_str()),
            RepositoryGrantPermissions::Use,
            &instant(T1),
        )
        .expect("member repo grant on binding two");
    let visible = ledger
        .visible_bindings(member.as_str(), &client)
        .expect("visibility");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].repository_binding_id, binding_id(11));

    // The second repository grant widens the visible set to two.
    ledger
        .create_grant(
            &issuance(42, binding_id(10).as_str(), member.as_str()),
            RepositoryGrantPermissions::Use,
            &instant(T1),
        )
        .expect("member repo grant on binding one");
    let visible = ledger
        .visible_bindings(member.as_str(), &client)
        .expect("visibility");
    assert_eq!(visible.len(), 2);

    // Revoking the repository grant hides the binding immediately.
    let revoked = ledger
        .revoke_grant(grant_id(42).as_str(), 1)
        .expect("revoke");
    assert_eq!(revoked.state, RepositoryGrantState::Revoked);
    let visible = ledger
        .visible_bindings(member.as_str(), &client)
        .expect("visibility");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].repository_binding_id, binding_id(11));
}

#[test]
fn multi_users_hold_disjoint_visible_sets() {
    let mut storage = SqliteStorage::open(temporary_directory("multi-user")).expect("storage");
    let client = seed_client(&mut storage, 1);
    let alice = user_id(2);
    let bob = user_id(3);
    seed_client_grant(&mut storage, 30, &client, alice.as_str());
    seed_client_grant(&mut storage, 31, &client, bob.as_str());
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        ledger
            .upsert(
                &projection(10, &client, "fingerprint-a"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding one");
        ledger
            .upsert(
                &projection(11, &client, "fingerprint-b"),
                Some(&instant(T1)),
                0,
                &instant(T1),
            )
            .expect("binding two");
        // Alice sees only binding one; Bob sees only binding two.
        ledger
            .create_grant(
                &issuance(40, binding_id(10).as_str(), alice.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("alice grant");
        ledger
            .create_grant(
                &issuance(41, binding_id(11).as_str(), bob.as_str()),
                RepositoryGrantPermissions::Use,
                &instant(T1),
            )
            .expect("bob grant");

        let alice_visible = ledger
            .visible_bindings(alice.as_str(), &client)
            .expect("alice visibility");
        assert_eq!(
            alice_visible
                .iter()
                .map(|record| record.repository_binding_id.as_str())
                .collect::<Vec<_>>(),
            vec![binding_id(10)]
        );
        let bob_visible = ledger
            .visible_bindings(bob.as_str(), &client)
            .expect("bob visibility");
        assert_eq!(
            bob_visible
                .iter()
                .map(|record| record.repository_binding_id.as_str())
                .collect::<Vec<_>>(),
            vec![binding_id(11)]
        );
        assert_eq!(
            ledger
                .active_grants_for_user(alice.as_str())
                .expect("active grants")
                .len(),
            1
        );
    }
    // Revoking Alice's client grant hides every binding from her even while
    // her repository grant row stays active.
    storage
        .client_connect_ledger()
        .expect("connect ledger")
        .revoke_grant(client_grant_id(30).as_str(), 1)
        .expect("client grant revoke");
    {
        let ledger = storage.repository_binding_ledger().expect("ledger");
        assert!(
            ledger
                .visible_bindings(alice.as_str(), &client)
                .expect("alice visibility")
                .is_empty()
        );
        let bob_visible = ledger
            .visible_bindings(bob.as_str(), &client)
            .expect("bob visibility");
        assert_eq!(
            bob_visible.len(),
            1,
            "bob's visibility is untouched by alice's revocation"
        );
    }
}
