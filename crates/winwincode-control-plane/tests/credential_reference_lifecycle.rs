// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceDeleteCommand,
    CredentialReferenceDeleteCommandCommand, CredentialReferenceDeletePayload,
    CredentialReferenceGetParameters, CredentialReferenceGetQuery,
    CredentialReferenceGetQueryQuery, CredentialReferenceListParameters,
    CredentialReferenceListQuery, CredentialReferenceListQueryQuery,
    CredentialReferenceRevokeCommand, CredentialReferenceRevokeCommandCommand,
    CredentialReferenceRevokePayload, CredentialReferenceRotateCommand,
    CredentialReferenceRotateCommandCommand, CredentialReferenceRotatePayload, OrganizationScope,
    OrganizationScopeKind, PageRequest, Scope, UserActor, UserActorKind,
};
use winwincode_audit::{AuditActionKind, AuditEvent};
use winwincode_control_plane::{
    CredentialReferenceErrorKind, CredentialReferenceService, CredentialSecretResolutionError,
    ProductStateStorage, ResolvedSecret, SecretStoreError, SecretStorePort,
};
use winwincode_domain::{
    CredentialReferenceId, OpaqueCursor, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_storage::{
    AggregateJournalKey, PublicEventScope, SqliteStorage,
    receipt_scope_key as storage_receipt_scope_key,
};

const FIRST_LOCATOR: &str = "local-fixture://SENSITIVE_SECRET_CREATE";
const ROTATED_LOCATOR: &str = "local-fixture://SENSITIVE_SECRET_ROTATED";

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct CountingSecretStore {
    resolutions: AtomicU64,
}

impl SecretStorePort for CountingSecretStore {
    fn resolve(
        &self,
        _reference: &winwincode_control_plane::CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolutions.fetch_add(1, Ordering::Relaxed);
        ResolvedSecret::from_bytes(b"SENSITIVE_PROVIDER_SECRET".to_vec())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-credential-reference-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn create_command(seed: u64) -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(seed),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", seed)),
            display_name: "Provider credential".to_owned(),
            provider_id: "provider-main".to_owned(),
            vault_locator: FIRST_LOCATOR.to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
    }
}

fn rotate_command(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
) -> CredentialReferenceRotateCommand {
    CredentialReferenceRotateCommand {
        actor: create.actor.clone(),
        command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
        expected_revision: Revision(1),
        payload: CredentialReferenceRotatePayload {
            credential_reference_id: create.payload.credential_reference_id.clone(),
            vault_locator: ROTATED_LOCATOR.to_owned(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: create.scope.clone(),
    }
}

fn revoke_command(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
) -> CredentialReferenceRevokeCommand {
    CredentialReferenceRevokeCommand {
        actor: create.actor.clone(),
        command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(2),
        payload: CredentialReferenceRevokePayload {
            credential_reference_id: create.payload.credential_reference_id.clone(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: create.scope.clone(),
    }
}

fn delete_command(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
) -> CredentialReferenceDeleteCommand {
    CredentialReferenceDeleteCommand {
        actor: create.actor.clone(),
        command: CredentialReferenceDeleteCommandCommand::CredentialReferenceDelete,
        expected_revision: Revision(3),
        payload: CredentialReferenceDeletePayload {
            credential_reference_id: create.payload.credential_reference_id.clone(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: create.scope.clone(),
    }
}

fn get_query(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
    query_scope: Scope,
) -> CredentialReferenceGetQuery {
    CredentialReferenceGetQuery {
        actor: create.actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CredentialReferenceGetParameters {
            credential_reference_id: create.payload.credential_reference_id.clone(),
        },
        query: CredentialReferenceGetQueryQuery::CredentialReferenceGet,
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: query_scope,
    }
}

fn create_in_scope(
    reference_seed: u64,
    request_seed: u64,
    command_scope: Scope,
    provider_id: &str,
) -> CredentialReferenceCreateCommand {
    let mut command = create_command(reference_seed);
    command.scope = command_scope;
    command.request_id = RequestId(id("req", request_seed));
    provider_id.clone_into(&mut command.payload.provider_id);
    command
}

fn list_query(
    query_scope: Scope,
    request_seed: u64,
    provider_id: Option<&str>,
    limit: i64,
    cursor: Option<OpaqueCursor>,
) -> CredentialReferenceListQuery {
    CredentialReferenceListQuery {
        actor: actor(1),
        page: PageRequest { cursor, limit },
        parameters: CredentialReferenceListParameters {
            provider_id: provider_id.map(str::to_owned),
        },
        query: CredentialReferenceListQueryQuery::CredentialReferenceList,
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: query_scope,
    }
}

#[test]
// One ordered scenario proves that replay returns the original body even after
// later lifecycle changes and that revocation precedes SecretStore access.
#[allow(clippy::too_many_lines)]
fn lifecycle_is_scope_bound_replayable_and_revocation_stops_new_resolution() {
    let root = temporary_directory("lifecycle");
    let mut storage = SqliteStorage::open(&root).expect("open Credential reference storage");
    let create = create_command(1);

    let created = CredentialReferenceService::new(&mut storage)
        .create(&create, 1_800_000_000_000)
        .expect("create Credential reference");
    assert_eq!(created.previous_revision, Revision(0));
    assert_eq!(created.current_revision, Revision(1));
    assert_eq!(created.result.rotation_version, 1);
    assert_eq!(created.result.secret_state, "available");
    assert_eq!(created.result.last_rotated_at, None);
    assert_eq!(created.result.revoked_at, None);

    let get = get_query(&create, 2, create.scope.clone());
    let queried = CredentialReferenceService::new(&mut storage)
        .get(&get)
        .expect("query Credential reference");
    assert_eq!(queried.result, created.result);

    let foreign_scope = scope(99);
    let foreign_get = get_query(&create, 3, foreign_scope.clone());
    let foreign_error = CredentialReferenceService::new(&mut storage)
        .get(&foreign_get)
        .expect_err("foreign scope must not read Credential reference");
    assert_eq!(
        foreign_error.kind(),
        CredentialReferenceErrorKind::ScopeDenied
    );
    let foreign_resolution = CredentialReferenceService::new(&mut storage)
        .resolve(&foreign_scope, &create.payload.credential_reference_id)
        .expect_err("foreign scope must not resolve Credential reference");
    assert_eq!(
        foreign_resolution.kind(),
        CredentialReferenceErrorKind::ScopeDenied
    );

    let rotate = rotate_command(&create, 4);
    let rotated = CredentialReferenceService::new(&mut storage)
        .rotate(&rotate, 1_800_000_001_000)
        .expect("rotate Credential reference metadata");
    assert_eq!(rotated.previous_revision, Revision(1));
    assert_eq!(rotated.current_revision, Revision(2));
    assert_eq!(rotated.result.rotation_version, 2);
    assert_eq!(
        rotated
            .result
            .last_rotated_at
            .as_ref()
            .map(|instant| instant.0.as_str()),
        Some("2027-01-15T08:00:01.000Z")
    );

    let resolution = CredentialReferenceService::new(&mut storage)
        .resolve(&create.scope, &create.payload.credential_reference_id)
        .expect("resolve active Credential reference");
    assert_eq!(resolution.rotation_version(), 2);
    assert_eq!(resolution.provider_id(), "provider-main");
    let secret_store = CountingSecretStore::default();
    let resolved_secret = CredentialReferenceService::new(&mut storage)
        .resolve_secret(
            &secret_store,
            &create.scope,
            &create.payload.credential_reference_id,
        )
        .expect("resolve active secret through SecretStorePort");
    assert_eq!(resolved_secret.expose(), b"SENSITIVE_PROVIDER_SECRET");
    assert_eq!(format!("{resolved_secret:?}"), "ResolvedSecret([REDACTED])");
    drop(resolved_secret);
    assert_eq!(secret_store.resolutions.load(Ordering::Relaxed), 1);

    let create_replay = CredentialReferenceService::new(&mut storage)
        .create(&create, u64::MAX)
        .expect("replay original create before consulting the current clock");
    assert_eq!(create_replay, created);

    let mut changed_replay = create.clone();
    changed_replay.payload.display_name = "Changed replay".to_owned();
    let conflict = CredentialReferenceService::new(&mut storage)
        .create(&changed_replay, 1_900_000_000_000)
        .expect_err("changed requestId replay must conflict");
    assert_eq!(
        conflict.kind(),
        CredentialReferenceErrorKind::RequestConflict
    );

    let revoke = revoke_command(&create, 5);
    let revoked = CredentialReferenceService::new(&mut storage)
        .revoke(&revoke, 1_800_000_002_000)
        .expect("revoke Credential reference");
    assert_eq!(revoked.previous_revision, Revision(2));
    assert_eq!(revoked.current_revision, Revision(3));
    assert_eq!(revoked.result.secret_state, "revoked");
    assert_eq!(
        revoked
            .result
            .revoked_at
            .as_ref()
            .map(|instant| instant.0.as_str()),
        Some("2027-01-15T08:00:02.000Z")
    );

    let revoked_resolution = CredentialReferenceService::new(&mut storage)
        .resolve(&create.scope, &create.payload.credential_reference_id)
        .expect_err("revocation must stop new resolution immediately");
    assert_eq!(
        revoked_resolution.kind(),
        CredentialReferenceErrorKind::Revoked
    );
    let revoked_secret = CredentialReferenceService::new(&mut storage)
        .resolve_secret(
            &secret_store,
            &create.scope,
            &create.payload.credential_reference_id,
        )
        .expect_err("revocation must stop before SecretStorePort access");
    assert!(matches!(
        revoked_secret,
        CredentialSecretResolutionError::Reference(ref error)
            if error.kind() == CredentialReferenceErrorKind::Revoked
    ));
    assert_eq!(secret_store.resolutions.load(Ordering::Relaxed), 1);

    let mut rotate_after_revoke = rotate_command(&create, 6);
    rotate_after_revoke.expected_revision = Revision(3);
    rotate_after_revoke.payload.vault_locator = "local-fixture://DO_NOT_ECHO".to_owned();
    let rotate_error = CredentialReferenceService::new(&mut storage)
        .rotate(&rotate_after_revoke, 1_800_000_003_000)
        .expect_err("revoked Credential reference cannot rotate");
    assert_eq!(rotate_error.kind(), CredentialReferenceErrorKind::Revoked);
    assert!(!rotate_error.to_string().contains("DO_NOT_ECHO"));

    let delete = delete_command(&create, 7);
    let deleted = CredentialReferenceService::new(&mut storage)
        .delete(&delete, 1_800_000_004_000)
        .expect("delete Credential reference metadata");
    assert_eq!(deleted.previous_revision, Revision(3));
    assert_eq!(deleted.current_revision, Revision(4));
    assert_eq!(deleted.result.resource_kind, "credential_reference");

    let delete_replay = CredentialReferenceService::new(&mut storage)
        .delete(&delete, 1_900_000_004_000)
        .expect("replay exact delete");
    assert_eq!(delete_replay, deleted);

    let deleted_get = get_query(&create, 8, create.scope.clone());
    let deleted_error = CredentialReferenceService::new(&mut storage)
        .get(&deleted_get)
        .expect_err("deleted Credential reference is absent from queries");
    assert_eq!(deleted_error.kind(), CredentialReferenceErrorKind::NotFound);

    let mut foreign_recreate = create.clone();
    foreign_recreate.scope = foreign_scope;
    foreign_recreate.request_id = RequestId(id("req", 9));
    let recreate_error = CredentialReferenceService::new(&mut storage)
        .create(&foreign_recreate, 1_800_000_005_000)
        .expect_err("deleted identity remains bound to its original scope");
    assert_eq!(
        recreate_error.kind(),
        CredentialReferenceErrorKind::ScopeDenied
    );

    let audit_events = storage
        .pending_audit_events()
        .expect("load Credential reference audit events");
    assert_eq!(audit_events.len(), 4);
    for pending in &audit_events {
        let event: AuditEvent =
            serde_json::from_slice(pending.payload()).expect("decode typed Credential audit event");
        assert_eq!(event.action().kind(), AuditActionKind::Credential);
        assert_eq!(
            event.action().credential_reference_id(),
            Some(&create.payload.credential_reference_id)
        );
    }
    let outbox = storage
        .pending_events()
        .expect("load Credential reference lifecycle events");
    assert_eq!(outbox.len(), 4);
    let stored = storage
        .load_state(&format!(
            "credential-reference:{}",
            create.payload.credential_reference_id.0
        ))
        .expect("load Credential reference state")
        .expect("Credential reference state exists");

    let public_and_durable_bytes = [
        serde_json::to_vec(&created).expect("encode create response"),
        serde_json::to_vec(&rotated).expect("encode rotate response"),
        serde_json::to_vec(&revoked).expect("encode revoke response"),
        serde_json::to_vec(&deleted).expect("encode delete response"),
        stored.payload,
    ]
    .into_iter()
    .chain(outbox.into_iter().map(|event| event.payload))
    .chain(
        audit_events
            .into_iter()
            .map(|event| event.payload().to_vec()),
    )
    .flatten()
    .collect::<Vec<_>>();
    let text = String::from_utf8(public_and_durable_bytes).expect("JSON is UTF-8");
    for forbidden in [
        FIRST_LOCATOR,
        ROTATED_LOCATOR,
        "vaultLocator",
        "secretMaterial",
        "providerCredential",
    ] {
        assert!(
            !text.contains(forbidden),
            "durable/public output leaked forbidden Credential input field"
        );
    }

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove Credential reference fixture");
}

#[test]
fn generated_projection_rejects_secret_or_locator_fields() {
    let create = create_command(20);
    let root = temporary_directory("projection-shape");
    let mut storage = SqliteStorage::open(&root).expect("open Credential reference storage");
    let created = CredentialReferenceService::new(&mut storage)
        .create(&create, 1_800_000_000_000)
        .expect("create Credential reference");
    let mut value = serde_json::to_value(created.result).expect("projection JSON");
    value["vaultLocator"] = serde_json::json!(FIRST_LOCATOR);
    assert!(
        serde_json::from_value::<winwincode_api::generated::CredentialReferenceProjection>(value)
            .is_err()
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove Credential reference fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn scope_catalog_lists_stably_across_replay_restart_filters_and_lifecycle_changes() {
    let root = temporary_directory("scope-catalog");
    let catalog_scope = scope(40);
    let first = create_in_scope(100, 400, catalog_scope.clone(), "provider-main");
    let second = create_in_scope(101, 401, catalog_scope.clone(), "provider-other");
    let third = create_in_scope(102, 402, catalog_scope.clone(), "provider-main");
    let mut storage = SqliteStorage::open(&root).expect("open Credential catalog storage");
    for (offset, command) in [&third, &first, &second].into_iter().enumerate() {
        CredentialReferenceService::new(&mut storage)
            .create(
                command,
                1_800_100_000_000 + u64::try_from(offset).expect("offset") * 1_000,
            )
            .expect("create catalog entry");
    }

    let first_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 410, None, 2, None))
        .expect("list first Credential page");
    assert_eq!(
        first_page
            .result
            .items
            .iter()
            .map(|projection| projection.id.clone())
            .collect::<Vec<_>>(),
        vec![
            first.payload.credential_reference_id.clone(),
            second.payload.credential_reference_id.clone()
        ]
    );
    assert!(first_page.page.has_more);
    let stable_cursor = first_page
        .page
        .next_cursor
        .clone()
        .expect("first page cursor");

    CredentialReferenceService::new(&mut storage)
        .create(&first, u64::MAX)
        .expect("exact replay must not advance catalog revision");
    let replay_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            411,
            None,
            2,
            Some(stable_cursor.clone()),
        ))
        .expect("cursor remains valid after exact command replay");
    assert_eq!(replay_page.result.items.len(), 1);
    assert_eq!(
        replay_page.result.items[0].id,
        third.payload.credential_reference_id
    );
    assert!(!replay_page.page.has_more);

    Box::new(storage).close().expect("close before restart");
    let mut storage = SqliteStorage::open(&root).expect("reopen Credential catalog storage");
    let restarted_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            412,
            None,
            2,
            Some(stable_cursor),
        ))
        .expect("cursor remains stable across restart");
    assert_eq!(restarted_page.result.items, replay_page.result.items);

    let provider_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            413,
            Some("provider-main"),
            1,
            None,
        ))
        .expect("filter Credential catalog by provider");
    assert_eq!(provider_page.result.items.len(), 1);
    assert_eq!(
        provider_page.result.items[0].id,
        first.payload.credential_reference_id
    );
    let provider_cursor = provider_page.page.next_cursor.expect("provider cursor");

    let mut revoke = revoke_command(&first, 414);
    revoke.expected_revision = Revision(1);
    CredentialReferenceService::new(&mut storage)
        .revoke(&revoke, 1_800_100_010_000)
        .expect("revoke catalog entry");
    let stale_error = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            415,
            Some("provider-main"),
            1,
            Some(provider_cursor),
        ))
        .expect_err("scope mutation expires prior catalog cursor");
    assert_eq!(
        stale_error.kind(),
        CredentialReferenceErrorKind::CursorInvalid
    );
    let revoked_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            416,
            Some("provider-main"),
            2,
            None,
        ))
        .expect("revoked references remain visible metadata");
    assert_eq!(revoked_page.result.items.len(), 2);
    assert_eq!(revoked_page.result.items[0].secret_state, "revoked");
    assert_eq!(revoked_page.result.items[1].secret_state, "available");

    let mut delete = delete_command(&second, 417);
    delete.expected_revision = Revision(1);
    CredentialReferenceService::new(&mut storage)
        .delete(&delete, 1_800_100_011_000)
        .expect("delete catalog entry");
    let after_delete = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 418, None, 200, None))
        .expect("list excludes deleted reference");
    assert_eq!(after_delete.result.items.len(), 2);
    assert!(
        after_delete
            .result
            .items
            .iter()
            .all(|projection| projection.id != second.payload.credential_reference_id)
    );

    let foreign_page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(scope(41), 419, None, 200, None))
        .expect("foreign empty scope has an isolated catalog");
    assert!(foreign_page.result.items.is_empty());
    assert!(!foreign_page.page.has_more);

    let Scope::OrganizationScope(organization_scope) = &catalog_scope else {
        panic!("fixture uses organization scope");
    };
    let scope_key = storage_receipt_scope_key(&PublicEventScope::Organization {
        organization_id: organization_scope.organization_id.clone(),
    })
    .expect("encode catalog scope");
    let catalog_key = AggregateJournalKey::new(
        "credential-reference-catalog",
        format!("{:x}", Sha256::digest(scope_key.as_bytes())),
    )
    .expect("build catalog key");
    let journal = storage
        .load_journal(&catalog_key)
        .expect("load Credential catalog")
        .expect("Credential catalog exists");
    let catalog_bytes = journal
        .records
        .iter()
        .flat_map(|record| record.payload.iter().copied())
        .chain(journal.manifest)
        .collect::<Vec<_>>();
    let catalog_text = String::from_utf8(catalog_bytes).expect("catalog JSON is UTF-8");
    for forbidden in [
        FIRST_LOCATOR,
        ROTATED_LOCATOR,
        "vaultLocator",
        "secret",
        "displayName",
        "providerId",
        "provider-main",
    ] {
        assert!(!catalog_text.contains(forbidden));
    }

    let cursor_json = String::from_utf8(
        base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            first_page.page.next_cursor.expect("cursor").0,
        )
        .expect("decode opaque cursor"),
    )
    .expect("cursor JSON is UTF-8");
    for forbidden in [
        FIRST_LOCATOR,
        ROTATED_LOCATOR,
        "vaultLocator",
        "secret",
        "provider-main",
    ] {
        assert!(!cursor_json.contains(forbidden));
    }

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove Credential catalog fixture");
}

#[test]
fn list_rejects_foreign_filter_and_malformed_or_mismatched_cursors() {
    let root = temporary_directory("catalog-cursor-negative");
    let catalog_scope = scope(50);
    let create = create_in_scope(110, 500, catalog_scope.clone(), "provider-main");
    let mut storage = SqliteStorage::open(&root).expect("open Credential catalog storage");
    CredentialReferenceService::new(&mut storage)
        .create(&create, 1_800_200_000_000)
        .expect("create Credential reference");

    let invalid_limit = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 501, None, 0, None))
        .expect_err("zero limit is invalid");
    assert_eq!(
        invalid_limit.kind(),
        CredentialReferenceErrorKind::InvalidRequest
    );
    let invalid_provider = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 502, Some(""), 1, None))
        .expect_err("empty provider filter is invalid");
    assert_eq!(
        invalid_provider.kind(),
        CredentialReferenceErrorKind::InvalidRequest
    );
    let malformed = CredentialReferenceService::new(&mut storage)
        .list(&list_query(
            catalog_scope.clone(),
            503,
            None,
            1,
            Some(OpaqueCursor("not-base64".to_owned())),
        ))
        .expect_err("malformed cursor is invalid");
    assert_eq!(
        malformed.kind(),
        CredentialReferenceErrorKind::CursorInvalid
    );

    let page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 504, None, 1, None))
        .expect("list only catalog entry");
    assert!(!page.page.has_more);
    let other = create_in_scope(111, 505, catalog_scope.clone(), "provider-main");
    CredentialReferenceService::new(&mut storage)
        .create(&other, 1_800_200_001_000)
        .expect("create second reference");
    let page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope.clone(), 506, None, 1, None))
        .expect("list first of two entries");
    let cursor = page.page.next_cursor.expect("cursor");
    for (request_seed, query_scope, provider_id) in [
        (507, scope(51), None),
        (508, catalog_scope, Some("provider-main")),
    ] {
        let error = CredentialReferenceService::new(&mut storage)
            .list(&list_query(
                query_scope,
                request_seed,
                provider_id,
                1,
                Some(cursor.clone()),
            ))
            .expect_err("cursor cannot cross scope or provider filter");
        assert_eq!(error.kind(), CredentialReferenceErrorKind::CursorInvalid);
    }

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove Credential cursor fixture");
}

#[test]
fn concurrent_exact_create_returns_the_one_durable_original_response() {
    const CALLERS: usize = 4;
    let root = temporary_directory("concurrent-create");
    let command = Arc::new(create_command(30));
    let barrier = Arc::new(Barrier::new(CALLERS));
    // Open and migrate each connection in order. The concurrency exercised by
    // this test starts at the service call, not at SQLite schema setup.
    let storages = (0..CALLERS)
        .map(|_| SqliteStorage::open(&root).expect("open concurrent Credential storage"))
        .collect::<Vec<_>>();
    let handles = storages
        .into_iter()
        .enumerate()
        .map(|(index, mut storage)| {
            let command = Arc::clone(&command);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let response = CredentialReferenceService::new(&mut storage)
                    .create(
                        &command,
                        1_800_000_000_000 + u64::try_from(index).expect("index") * 1_000,
                    )
                    .expect("concurrent exact create");
                Box::new(storage).close().expect("close concurrent storage");
                response
            })
        })
        .collect::<Vec<_>>();
    let responses = handles
        .into_iter()
        .map(|handle| handle.join().expect("join concurrent create"))
        .collect::<Vec<_>>();
    assert!(responses.windows(2).all(|pair| pair[0] == pair[1]));

    let storage = SqliteStorage::open(&root).expect("reopen concurrent Credential storage");
    assert_eq!(storage.pending_events().expect("pending events").len(), 1);
    assert_eq!(
        storage
            .pending_audit_events()
            .expect("pending audit events")
            .len(),
        1
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove Credential reference fixture");
}

#[test]
fn concurrent_distinct_creates_share_one_complete_scope_catalog() {
    const CALLERS: usize = 4;
    let root = temporary_directory("concurrent-catalog");
    let catalog_scope = scope(60);
    let barrier = Arc::new(Barrier::new(CALLERS));
    let storages = (0..CALLERS)
        .map(|_| SqliteStorage::open(&root).expect("open concurrent catalog storage"))
        .collect::<Vec<_>>();
    let handles = storages
        .into_iter()
        .enumerate()
        .map(|(index, mut storage)| {
            let barrier = Arc::clone(&barrier);
            let command = create_in_scope(
                120 + u64::try_from(index).expect("index"),
                600 + u64::try_from(index).expect("index"),
                catalog_scope.clone(),
                "provider-main",
            );
            thread::spawn(move || {
                barrier.wait();
                CredentialReferenceService::new(&mut storage)
                    .create(
                        &command,
                        1_800_300_000_000 + u64::try_from(index).expect("index") * 1_000,
                    )
                    .expect("concurrent distinct create");
                Box::new(storage).close().expect("close concurrent storage");
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().expect("join concurrent catalog create");
    }

    let mut storage = SqliteStorage::open(&root).expect("reopen concurrent catalog storage");
    let page = CredentialReferenceService::new(&mut storage)
        .list(&list_query(catalog_scope, 610, None, 200, None))
        .expect("list complete concurrent catalog");
    assert_eq!(page.result.items.len(), CALLERS);
    assert!(
        page.result
            .items
            .windows(2)
            .all(|pair| pair[0].id.0 < pair[1].id.0)
    );
    assert_eq!(
        storage.pending_events().expect("pending events").len(),
        CALLERS
    );
    assert_eq!(
        storage
            .pending_audit_events()
            .expect("pending audit events")
            .len(),
        CALLERS
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove concurrent catalog fixture");
}
