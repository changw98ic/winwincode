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

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, EnterpriseApiTokenIssuePayload, EnterpriseApiTokenIssuePayloadKind,
    EnterpriseApiTokenRevokePayload, EnterpriseApiTokenRevokePayloadAction,
    EnterpriseApiTokenRevokePayloadKind, EnterpriseExternalIdentityLinkPayload,
    EnterpriseExternalIdentityLinkPayloadAction, EnterpriseExternalIdentityLinkPayloadKind,
    EnterpriseIdentityListParameters, EnterpriseIdentityListQuery,
    EnterpriseIdentityListQueryQuery, EnterpriseIdentityUpdateCommand,
    EnterpriseIdentityUpdateCommandCommand, EnterpriseIdentityUpdatePayload,
    EnterpriseServiceAccountRevokePayload, EnterpriseServiceAccountRevokePayloadAction,
    EnterpriseServiceAccountRevokePayloadKind, EnterpriseServiceAccountUpsertPayload,
    EnterpriseServiceAccountUpsertPayloadAction, EnterpriseServiceAccountUpsertPayloadKind,
    OrganizationScope, OrganizationScopeKind, PageRequest, Scope,
};
use winwincode_control_plane::{
    EnterpriseIdentityClock, EnterpriseIdentityClockError, EnterpriseIdentityErrorKind,
    EnterpriseIdentityService, generate_api_token,
};
use winwincode_domain::{
    ApiTokenId, ExternalIdentityId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId,
    Revision, SchemaVersion, ServiceAccountId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

const NOW_MILLIS: u64 = 1_700_000_000_000;
const EXPIRES_AT: &str = "2030-01-01T00:00:00.000Z";

#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl EnterpriseIdentityClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseIdentityClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[test]
fn token_lifecycle_is_secret_free_immediate_and_restart_safe() {
    let fixture = Fixture::new("lifecycle");
    let service = fixture.service();
    let account = service_account_command(&fixture, request(1), 0, repository_scope(&fixture));
    let account_response = service.update(&account).expect("create Service Account");
    assert_eq!(account_response.current_revision, Revision(1));

    let (token_id, raw, digest) = token(1, 7);
    let issue = token_issue_command(
        &fixture,
        request(2),
        0,
        "issue",
        token_id.clone(),
        digest.clone(),
    );
    let issue_response = service.update(&issue).expect("issue API Token");
    let public_json = serde_json::to_string(&issue_response).expect("encode response");
    assert!(!public_json.contains(&raw));
    assert!(!public_json.contains(&digest.0));
    assert!(!public_json.contains("tokenSha256"));

    let principal = service
        .authenticate_bearer(&raw)
        .expect("authenticate Token");
    assert_eq!(principal.organization_id, fixture.organization_id);
    assert_eq!(
        principal.authorized_scopes,
        vec![repository_scope(&fixture)]
    );

    let mut wrong = raw.clone();
    let replacement = if wrong.ends_with('A') { "B" } else { "A" };
    wrong.replace_range(wrong.len() - 1.., replacement);
    assert_eq!(
        service
            .authenticate_bearer(&wrong)
            .expect_err("wrong Token")
            .kind(),
        EnterpriseIdentityErrorKind::Authentication
    );

    let restarted = fixture.service();
    assert_eq!(
        restarted
            .authenticate_bearer(&raw)
            .expect("restart Token")
            .api_token_id,
        token_id
    );

    let revoke = token_revoke_command(&fixture, request(3), 1, token_id);
    restarted.update(&revoke).expect("revoke Token");
    assert_eq!(
        service
            .authenticate_bearer(&raw)
            .expect_err("revocation is immediate")
            .kind(),
        EnterpriseIdentityErrorKind::Authentication
    );

    let inspection = SqliteStorage::open(&fixture.root).expect("open audit inspection");
    let identity = winwincode_control_plane::command_receipt_identity(
        &issue.actor,
        &Scope::OrganizationScope(issue.scope.clone()),
        issue.request_id.clone(),
    )
    .expect("receipt identity");
    let pending = inspection
        .load_pending_audit_event(&identity)
        .expect("load pending audit")
        .expect("pending audit exists");
    let audit = String::from_utf8(pending.payload().to_vec()).expect("audit UTF-8");
    assert!(!audit.contains(&raw));
    assert!(!audit.contains(&digest.0));
    Box::new(inspection).close().expect("close inspection");
}

#[test]
fn concurrent_rotation_has_one_winner_and_exact_replay() {
    let fixture = Fixture::new("rotation");
    let service = fixture.service();
    service
        .update(&service_account_command(
            &fixture,
            request(10),
            0,
            repository_scope(&fixture),
        ))
        .expect("create Service Account");
    let (token_id, original_raw, original_digest) = token(2, 11);
    service
        .update(&token_issue_command(
            &fixture,
            request(11),
            0,
            "issue",
            token_id.clone(),
            original_digest,
        ))
        .expect("issue original Token");

    let barrier = Arc::new(Barrier::new(3));
    let attempts = [(12, 21), (13, 22)]
        .into_iter()
        .map(|(request_number, secret)| {
            let service = fixture.service();
            let fixture = fixture.clone();
            let barrier = Arc::clone(&barrier);
            let token_id = token_id.clone();
            thread::spawn(move || {
                let (_, raw, verifier) = token(2, secret);
                let command = token_issue_command(
                    &fixture,
                    request(request_number),
                    1,
                    "rotate",
                    token_id,
                    verifier,
                );
                barrier.wait();
                (raw, command.clone(), service.update(&command))
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = attempts
        .into_iter()
        .map(|attempt| attempt.join().expect("rotation thread"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(_, _, result)| result.is_ok())
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .find_map(|(_, _, result)| result.as_ref().err())
            .expect("loser")
            .kind(),
        EnterpriseIdentityErrorKind::RevisionConflict
    );
    assert_eq!(
        service
            .authenticate_bearer(&original_raw)
            .expect_err("rotated Token is rejected")
            .kind(),
        EnterpriseIdentityErrorKind::Authentication
    );
    let (winner_raw, winner_command, winner_response) = results
        .into_iter()
        .find(|(_, _, result)| result.is_ok())
        .expect("winner");
    service
        .authenticate_bearer(&winner_raw)
        .expect("winning Token authenticates");
    assert_eq!(
        service.update(&winner_command).expect("exact replay"),
        winner_response.expect("winner response")
    );
    let mut changed = winner_command;
    if let EnterpriseIdentityUpdatePayload::EnterpriseApiTokenIssuePayload(payload) =
        &mut changed.payload
    {
        payload.expires_at = Instant("2031-01-01T00:00:00.000Z".to_owned());
    }
    assert_eq!(
        service
            .update(&changed)
            .expect_err("changed request replay")
            .kind(),
        EnterpriseIdentityErrorKind::RequestConflict
    );
}

#[test]
fn tenant_scope_external_identity_pagination_and_account_revoke_are_closed() {
    let fixture = Fixture::new("scope-list");
    let service = fixture.service();
    let foreign_scope = Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization(9),
    });
    assert_eq!(
        service
            .update(&service_account_command(
                &fixture,
                request(20),
                0,
                foreign_scope
            ))
            .expect_err("foreign scope")
            .kind(),
        EnterpriseIdentityErrorKind::ScopeDenied
    );
    service
        .update(&service_account_command(
            &fixture,
            request(21),
            0,
            repository_scope(&fixture),
        ))
        .expect("create Service Account");
    service
        .update(&external_identity_command(&fixture, request(22)))
        .expect("link External Identity");
    let (token_id, raw, verifier) = token(3, 31);
    service
        .update(&token_issue_command(
            &fixture,
            request(23),
            0,
            "issue",
            token_id,
            verifier,
        ))
        .expect("issue Token");

    let first = service
        .list(&identity_list_query(&fixture, request(24), 1, None))
        .expect("first page");
    assert_eq!(first.result.items.len(), 1);
    assert!(first.page.has_more);
    let second = service
        .list(&identity_list_query(
            &fixture,
            request(25),
            1,
            first.page.next_cursor,
        ))
        .expect("second page");
    assert_eq!(second.result.items.len(), 1);

    service
        .update(&service_account_revoke_command(&fixture, request(26), 1))
        .expect("revoke Service Account");
    assert_eq!(
        service
            .authenticate_bearer(&raw)
            .expect_err("account revocation is immediate")
            .kind(),
        EnterpriseIdentityErrorKind::Authentication
    );
    assert_eq!(
        service
            .list(&identity_list_query(
                &fixture,
                request(27),
                1,
                second.page.next_cursor,
            ))
            .expect_err("stale cursor")
            .kind(),
        EnterpriseIdentityErrorKind::InvalidRequest
    );
}

#[test]
fn locally_generated_secret_is_available_once() {
    let mut generated =
        generate_api_token(ApiTokenId(format!("tok_{}", suffix(8)))).expect("generate API Token");
    assert!(generated.token_sha256().0.starts_with("sha256:"));
    assert!(generated.take_raw().is_some());
    assert!(generated.take_raw().is_none());
}

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    clock: Arc<AtomicU64>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    repository_id: RepositoryId,
    user_id: UserId,
    service_account_id: ServiceAccountId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "winwincode-enterprise-identity-{label}-{}-{}",
            std::process::id(),
            NOW_MILLIS
        ));
        fs::remove_dir_all(&root).ok();
        Self {
            root,
            clock: Arc::new(AtomicU64::new(NOW_MILLIS)),
            organization_id: organization(1),
            workspace_id: WorkspaceId(format!("wsp_{}", suffix(2))),
            project_id: ProjectId(format!("prj_{}", suffix(3))),
            repository_id: RepositoryId(format!("rep_{}", suffix(4))),
            user_id: UserId(format!("usr_{}", suffix(5))),
            service_account_id: ServiceAccountId(format!("svc_{}", suffix(6))),
        }
    }

    fn service(&self) -> EnterpriseIdentityService {
        EnterpriseIdentityService::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("open identity storage")),
            Box::new(SharedClock(Arc::clone(&self.clock))),
        )
    }
}

fn actor(fixture: &Fixture) -> Actor {
    Actor::UserActor(UserActor {
        id: fixture.user_id.clone(),
        kind: UserActorKind::User,
    })
}

fn organization(number: u8) -> OrganizationId {
    OrganizationId(format!("org_{}", suffix(number)))
}

fn repository_scope(fixture: &Fixture) -> Scope {
    Scope::RepositoryScope(RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: fixture.organization_id.clone(),
        workspace_id: fixture.workspace_id.clone(),
        project_id: fixture.project_id.clone(),
        repository_id: fixture.repository_id.clone(),
    })
}

fn service_account_command(
    fixture: &Fixture,
    request_id: RequestId,
    expected_revision: i64,
    authorized_scope: Scope,
) -> EnterpriseIdentityUpdateCommand {
    update_command(
        fixture,
        request_id,
        expected_revision,
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountUpsertPayload(
            EnterpriseServiceAccountUpsertPayload {
                action: EnterpriseServiceAccountUpsertPayloadAction::Upsert,
                authorized_scopes: vec![authorized_scope],
                display_name: "Build Service".to_owned(),
                kind: EnterpriseServiceAccountUpsertPayloadKind::ServiceAccount,
                service_account_id: fixture.service_account_id.clone(),
            },
        ),
    )
}

fn service_account_revoke_command(
    fixture: &Fixture,
    request_id: RequestId,
    expected_revision: i64,
) -> EnterpriseIdentityUpdateCommand {
    update_command(
        fixture,
        request_id,
        expected_revision,
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountRevokePayload(
            EnterpriseServiceAccountRevokePayload {
                action: EnterpriseServiceAccountRevokePayloadAction::Revoke,
                kind: EnterpriseServiceAccountRevokePayloadKind::ServiceAccount,
                service_account_id: fixture.service_account_id.clone(),
            },
        ),
    )
}

fn external_identity_command(
    fixture: &Fixture,
    request_id: RequestId,
) -> EnterpriseIdentityUpdateCommand {
    update_command(
        fixture,
        request_id,
        0,
        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityLinkPayload(
            EnterpriseExternalIdentityLinkPayload {
                action: EnterpriseExternalIdentityLinkPayloadAction::Link,
                authorized_scopes: vec![repository_scope(fixture)],
                external_identity_id: ExternalIdentityId(format!("xid_{}", suffix(7))),
                issuer_sha256: digest(b"issuer"),
                kind: EnterpriseExternalIdentityLinkPayloadKind::ExternalIdentity,
                provider: "oidc".to_owned(),
                subject_sha256: digest(b"subject"),
                user_id: fixture.user_id.clone(),
            },
        ),
    )
}

fn token_issue_command(
    fixture: &Fixture,
    request_id: RequestId,
    expected_revision: i64,
    action: &str,
    token_id: ApiTokenId,
    token_sha256: Sha256Digest,
) -> EnterpriseIdentityUpdateCommand {
    update_command(
        fixture,
        request_id,
        expected_revision,
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenIssuePayload(
            EnterpriseApiTokenIssuePayload {
                action: action.to_owned(),
                api_token_id: token_id,
                expires_at: Instant(EXPIRES_AT.to_owned()),
                kind: EnterpriseApiTokenIssuePayloadKind::ApiToken,
                service_account_id: fixture.service_account_id.clone(),
                token_sha256,
            },
        ),
    )
}

fn token_revoke_command(
    fixture: &Fixture,
    request_id: RequestId,
    expected_revision: i64,
    token_id: ApiTokenId,
) -> EnterpriseIdentityUpdateCommand {
    update_command(
        fixture,
        request_id,
        expected_revision,
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenRevokePayload(
            EnterpriseApiTokenRevokePayload {
                action: EnterpriseApiTokenRevokePayloadAction::Revoke,
                api_token_id: token_id,
                kind: EnterpriseApiTokenRevokePayloadKind::ApiToken,
            },
        ),
    )
}

fn update_command(
    fixture: &Fixture,
    request_id: RequestId,
    expected_revision: i64,
    payload: EnterpriseIdentityUpdatePayload,
) -> EnterpriseIdentityUpdateCommand {
    EnterpriseIdentityUpdateCommand {
        actor: actor(fixture),
        command: EnterpriseIdentityUpdateCommandCommand::EnterpriseIdentityUpdate,
        expected_revision: Revision(expected_revision),
        payload,
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.organization_id.clone(),
        },
    }
}

fn identity_list_query(
    fixture: &Fixture,
    request_id: RequestId,
    limit: i64,
    cursor: Option<winwincode_domain::OpaqueCursor>,
) -> EnterpriseIdentityListQuery {
    EnterpriseIdentityListQuery {
        actor: actor(fixture),
        page: PageRequest { cursor, limit },
        parameters: EnterpriseIdentityListParameters {
            kinds: Vec::new(),
            states: Vec::new(),
        },
        query: EnterpriseIdentityListQueryQuery::EnterpriseIdentityList,
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.organization_id.clone(),
        },
    }
}

fn token(number: u8, secret: u8) -> (ApiTokenId, String, Sha256Digest) {
    let id = ApiTokenId(format!("tok_{}", suffix(number)));
    let raw = format!(
        "wwc_api_{}.{}",
        suffix(number),
        URL_SAFE_NO_PAD.encode([secret; 32])
    );
    let verifier = digest(raw.as_bytes());
    (id, raw, verifier)
}

fn digest(value: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(value)))
}

fn request(number: u8) -> RequestId {
    RequestId(format!("req_{}", suffix(number)))
}

fn suffix(number: u8) -> String {
    format!("{number:026}")
}
