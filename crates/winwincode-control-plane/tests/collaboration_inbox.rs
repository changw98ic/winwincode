// SPDX-License-Identifier: Apache-2.0

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    Actor, RepositoryScope, RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CollaborationAnnotationAction, CollaborationAnnotationCommand, CollaborationAnnotationId,
    CollaborationAnnotationState, CollaborationAnnotationTarget, CollaborationCandidateIdentity,
    CollaborationClaimAction, CollaborationClaimCommand, CollaborationInboxAudience,
    CollaborationInboxAuthorityError, CollaborationInboxAuthorityPort,
    CollaborationInboxAuthoritySnapshot, CollaborationInboxClock, CollaborationInboxClockError,
    CollaborationInboxCommandContext, CollaborationInboxErrorKind, CollaborationInboxFilter,
    CollaborationInboxItemId, CollaborationInboxItemKind, CollaborationInboxItemState,
    CollaborationInboxListRequest, CollaborationInboxReceipt, CollaborationInboxService,
    CollaborationInboxSourceError, CollaborationInboxSourceItem, CollaborationInboxSourcePort,
    CollaborationInboxSourceSnapshot, CollaborationResponsibilityEntitlement,
    FormalCollaborationCommandRoute, ResponsibilityAssignment, ResponsibilityAssignmentId,
    ResponsibilityAssignmentState, ResponsibilityReviewKind, ResponsibilityRole,
    ResponsibilityTarget,
};
use winwincode_domain::{
    ApprovalId, AttentionItemId, DeliveryId, EnterpriseTeamId, OpaqueCursor, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::SqliteStorage;
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    StateCommit, StateRevisionGuard,
};

const NOW: u64 = 1_700_000_000_000;
const SOURCE_GUARD_STREAM: &str = "collaboration-inbox-source-fixture";
const AUTHORITY_GUARD_STREAM: &str = "collaboration-inbox-authority-fixture";

#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl CollaborationInboxClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationInboxClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Clone)]
struct SharedSource(Arc<Mutex<CollaborationInboxSourceSnapshot>>);

impl CollaborationInboxSourcePort for SharedSource {
    fn snapshot(
        &mut self,
        _scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxSourceError> {
        Ok(self.0.lock().expect("source").clone())
    }
}

struct RacingSource {
    source: SharedSource,
    root: PathBuf,
    fired: bool,
}

impl CollaborationInboxSourcePort for RacingSource {
    fn snapshot(
        &mut self,
        _scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxSourceError> {
        let snapshot = self.source.0.lock().expect("source").clone();
        if !self.fired {
            advance_guard_state(&self.root, SOURCE_GUARD_STREAM, 1, 90);
            self.fired = true;
        }
        Ok(snapshot)
    }
}

#[derive(Clone)]
struct SharedAuthority(Arc<Mutex<CollaborationInboxAuthoritySnapshot>>);

impl CollaborationInboxAuthorityPort for SharedAuthority {
    fn authorize(
        &mut self,
        _actor: &Actor,
        _authenticated_scopes: &[Scope],
        _scope: &RepositoryScope,
        _audience: &CollaborationInboxAudience,
    ) -> Result<CollaborationInboxAuthoritySnapshot, CollaborationInboxAuthorityError> {
        Ok(self.0.lock().expect("authority").clone())
    }
}

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    scope: RepositoryScope,
    viewer: UserId,
    team: EnterpriseTeamId,
    clock: Arc<AtomicU64>,
    source: SharedSource,
    authority: SharedAuthority,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = temporary_directory(label);
        let scope = repository_scope(1);
        let viewer = user(1);
        let team = EnterpriseTeamId("team_01J00000000000000000000000".to_owned());
        let source_guard = seed_guard_state(&root, SOURCE_GUARD_STREAM, 80);
        let authority_guard = seed_guard_state(&root, AUTHORITY_GUARD_STREAM, 81);
        let items = vec![
            approval_item(1, NOW + 300, CollaborationInboxItemState::Pending),
            delivery_attention_item(1, NOW + 100),
            approval_item(2, NOW + 50, CollaborationInboxItemState::Approved),
        ];
        let item_state_guards = items
            .iter()
            .map(|item| (item.id.clone(), vec![source_guard.clone()]))
            .collect();
        let source = CollaborationInboxSourceSnapshot {
            scope: scope.clone(),
            revision: 1,
            snapshot_sha256: items_digest(&items),
            item_state_guards,
            items,
        };
        let authority = CollaborationInboxAuthoritySnapshot {
            scope: scope.clone(),
            viewer_user_id: viewer.clone(),
            visible_team_ids: vec![team.clone()],
            assignments: vec![
                entitlement(
                    &scope,
                    &viewer,
                    ResponsibilityTarget::ProductSession {
                        product_session_id: product_session(1),
                    },
                    ResponsibilityRole::Approver,
                    vec![team.clone()],
                    1,
                ),
                entitlement(
                    &scope,
                    &viewer,
                    ResponsibilityTarget::ProductSession {
                        product_session_id: product_session(2),
                    },
                    ResponsibilityRole::Approver,
                    Vec::new(),
                    2,
                ),
                entitlement(
                    &scope,
                    &viewer,
                    ResponsibilityTarget::Delivery {
                        delivery_id: delivery(1),
                    },
                    ResponsibilityRole::Assignee,
                    vec![team.clone()],
                    3,
                ),
            ],
            authority_revision: 1,
            authority_sha256: digest('a'),
            state_guards: vec![authority_guard],
        };
        Self {
            root,
            scope,
            viewer,
            team,
            clock: Arc::new(AtomicU64::new(NOW)),
            source: SharedSource(Arc::new(Mutex::new(source))),
            authority: SharedAuthority(Arc::new(Mutex::new(authority))),
        }
    }

    fn service(&self) -> CollaborationInboxService {
        CollaborationInboxService::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("Inbox storage")),
            Box::new(self.source.clone()),
            Box::new(self.authority.clone()),
            Box::new(SharedClock(Arc::clone(&self.clock))),
        )
    }

    fn personal_list(
        &self,
        limit: usize,
        cursor: Option<OpaqueCursor>,
    ) -> CollaborationInboxListRequest {
        CollaborationInboxListRequest {
            actor: actor(&self.viewer),
            authenticated_scopes: vec![Scope::RepositoryScope(self.scope.clone())],
            scope: self.scope.clone(),
            audience: CollaborationInboxAudience::Personal(self.viewer.clone()),
            filter: CollaborationInboxFilter::default(),
            limit,
            cursor,
        }
    }

    fn context(&self, sequence: u64, expected_revision: u64) -> CollaborationInboxCommandContext {
        CollaborationInboxCommandContext {
            actor: actor(&self.viewer),
            authenticated_scopes: vec![Scope::RepositoryScope(self.scope.clone())],
            scope: self.scope.clone(),
            audience: CollaborationInboxAudience::Personal(self.viewer.clone()),
            request_id: request(sequence),
            expected_revision,
        }
    }

    fn replace_items(&self, items: Vec<CollaborationInboxSourceItem>) {
        let mut source = self.source.0.lock().expect("source");
        source.revision += 1;
        source.snapshot_sha256 = items_digest(&items);
        let guard = source
            .item_state_guards
            .values()
            .next()
            .and_then(|guards| guards.first())
            .cloned()
            .expect("source fixture guard");
        source.item_state_guards = items
            .iter()
            .map(|item| (item.id.clone(), vec![guard.clone()]))
            .collect();
        source.items = items;
    }
}

#[test]
fn stable_personal_and_team_pages_sort_filter_expire_and_reject_changed_cuts() {
    let fixture = Fixture::new("stable-pages");
    let mut service = fixture.service();
    let first = service
        .list(&fixture.personal_list(1, None))
        .expect("first page");
    assert_eq!(first.items.len(), 1);
    assert_eq!(
        first.items[0].source.id,
        CollaborationInboxItemId::Approval(approval(1))
    );
    assert_eq!(
        first.items[0].effective_state,
        CollaborationInboxItemState::Pending
    );
    assert!(first.has_more);

    fixture.clock.store(NOW + 200, Ordering::SeqCst);
    let second = service
        .list(&fixture.personal_list(1, first.next_cursor.clone()))
        .expect("cursor freezes time");
    assert_eq!(second.snapshot_at_millis, NOW);
    assert_eq!(
        second.items[0].source.id,
        CollaborationInboxItemId::DeliveryAttention(attention(1))
    );
    assert_eq!(
        second.items[0].effective_state,
        CollaborationInboxItemState::Pending
    );

    let refreshed = service
        .list(&fixture.personal_list(10, None))
        .expect("refreshed page");
    assert_eq!(refreshed.snapshot_at_millis, NOW + 200);
    assert_eq!(
        refreshed.items[0].effective_state,
        CollaborationInboxItemState::Pending
    );
    assert_eq!(
        refreshed.items[1].effective_state,
        CollaborationInboxItemState::Expired
    );

    let mut team = fixture.personal_list(10, None);
    team.audience = CollaborationInboxAudience::Team(fixture.team.clone());
    let team_page = service.list(&team).expect("Team page");
    assert_eq!(team_page.items.len(), 2);
    assert!(
        team_page
            .items
            .iter()
            .all(|item| item.source.id != CollaborationInboxItemId::Approval(approval(2)))
    );

    let mut pending = fixture.personal_list(10, None);
    pending.filter.states = vec![CollaborationInboxItemState::Pending];
    assert_eq!(
        service.list(&pending).expect("pending filter").items.len(),
        1
    );

    let old_cursor = first.next_cursor.expect("old cursor");
    fixture.replace_items(vec![approval_item(
        1,
        NOW + 300,
        CollaborationInboxItemState::Rejected,
    )]);
    assert_eq!(
        service
            .list(&fixture.personal_list(1, Some(old_cursor)))
            .expect_err("changed source cut")
            .kind(),
        CollaborationInboxErrorKind::CursorExpired
    );
}

#[test]
fn claim_is_replay_safe_restart_safe_and_fails_closed_after_assignment_revocation() {
    let fixture = Fixture::new("claim-restart");
    let command = CollaborationClaimCommand {
        context: fixture.context(10, 0),
        item_id: CollaborationInboxItemId::Approval(approval(1)),
        action: CollaborationClaimAction::Claim,
    };
    let mut service = fixture.service();
    let applied = service.apply_claim(&command).expect("claim");
    let CollaborationInboxReceipt::Claim {
        claim: Some(claim),
        catalog_revision,
        replayed,
    } = applied
    else {
        panic!("claim receipt");
    };
    assert_eq!(catalog_revision, 1);
    assert!(!replayed);
    assert_eq!(claim.claimant_user_id, fixture.viewer);
    assert_eq!(
        service.apply_claim(&command).expect("same-process replay"),
        CollaborationInboxReceipt::Claim {
            claim: Some(claim.clone()),
            catalog_revision: 1,
            replayed: true,
        }
    );

    drop(service);
    let mut restarted = fixture.service();
    assert!(matches!(
        restarted.apply_claim(&command).expect("restart replay"),
        CollaborationInboxReceipt::Claim { replayed: true, .. }
    ));
    assert_eq!(
        restarted
            .list(&fixture.personal_list(10, None))
            .expect("restart page")
            .items[0]
            .claim,
        Some(claim)
    );

    let changed = CollaborationClaimCommand {
        action: CollaborationClaimAction::Release,
        ..command.clone()
    };
    assert_eq!(
        restarted
            .apply_claim(&changed)
            .expect_err("changed request body")
            .kind(),
        CollaborationInboxErrorKind::RequestConflict
    );

    fixture
        .authority
        .0
        .lock()
        .expect("authority")
        .assignments
        .clear();
    let new_claim = CollaborationClaimCommand {
        context: fixture.context(11, 1),
        item_id: CollaborationInboxItemId::DeliveryAttention(attention(1)),
        action: CollaborationClaimAction::Claim,
    };
    assert_eq!(
        restarted
            .apply_claim(&new_claim)
            .expect_err("revoked assignment")
            .kind(),
        CollaborationInboxErrorKind::Unauthorized
    );
}

#[test]
fn exact_candidate_annotations_cover_node_file_hunk_and_stale_candidate_is_zero_write() {
    let fixture = Fixture::new("annotations");
    let original_candidate = candidate('c', 1);
    fixture.replace_items(vec![review_approval_item(original_candidate.clone())]);
    let mut service = fixture.service();
    let annotation = annotation_command(
        &fixture,
        20,
        0,
        "annotation_node",
        original_candidate.clone(),
        CollaborationAnnotationTarget::Node {
            node_id: "solution-node-7".to_owned(),
        },
    );
    let applied = service
        .apply_annotation(&annotation)
        .expect("node annotation");
    assert!(matches!(
        applied,
        CollaborationInboxReceipt::Annotation {
            replayed: false,
            ..
        }
    ));
    assert!(matches!(
        service
            .apply_annotation(&annotation)
            .expect("annotation replay"),
        CollaborationInboxReceipt::Annotation { replayed: true, .. }
    ));

    add_file_and_hunk_annotations(&fixture, &mut service, &original_candidate);

    let mut changed_candidate = review_approval_item(candidate('f', 2));
    changed_candidate.source_revision = 2;
    changed_candidate.source_sha256 = digest('f');
    fixture.replace_items(vec![changed_candidate]);
    let stale = annotation_command(
        &fixture,
        23,
        3,
        "annotation_stale",
        original_candidate,
        CollaborationAnnotationTarget::Node {
            node_id: "old-node".to_owned(),
        },
    );
    assert_eq!(
        service
            .apply_annotation(&stale)
            .expect_err("old candidate")
            .kind(),
        CollaborationInboxErrorKind::CandidateChanged
    );

    let current = candidate('f', 2);
    let valid = annotation_command(
        &fixture,
        24,
        3,
        "annotation_current",
        current,
        CollaborationAnnotationTarget::Node {
            node_id: "new-node".to_owned(),
        },
    );
    let accepted = service
        .apply_annotation(&valid)
        .expect("failed stale write left revision unchanged");
    assert!(matches!(
        accepted,
        CollaborationInboxReceipt::Annotation {
            catalog_revision: 4,
            ..
        }
    ));

    drop(service);
    let mut restarted = fixture.service();
    let page = restarted
        .list(&fixture.personal_list(10, None))
        .expect("restart annotations");
    assert_eq!(page.items[0].annotations.len(), 1);
    assert_eq!(page.items[0].annotations[0].id.0, "annotation_current");

    let revoke = CollaborationAnnotationCommand {
        context: fixture.context(25, 4),
        item_id: CollaborationInboxItemId::Approval(approval(1)),
        annotation_id: CollaborationAnnotationId("annotation_current".to_owned()),
        action: CollaborationAnnotationAction::Revoke,
    };
    let receipt = restarted
        .apply_annotation(&revoke)
        .expect("revoke annotation");
    let CollaborationInboxReceipt::Annotation { annotation, .. } = receipt else {
        panic!("annotation receipt");
    };
    assert_eq!(annotation.state, CollaborationAnnotationState::Revoked);
}

#[test]
fn cross_tenant_team_and_concurrent_claims_fail_closed_without_duplicate_business_decisions() {
    let fixture = Fixture::new("cross-tenant-concurrency");
    let mut service = fixture.service();
    let mut foreign = fixture.personal_list(10, None);
    foreign.scope = repository_scope(2);
    assert_eq!(
        service.list(&foreign).expect_err("foreign scope").kind(),
        CollaborationInboxErrorKind::Unauthorized
    );
    let mut hidden_team = fixture.personal_list(10, None);
    hidden_team.audience = CollaborationInboxAudience::Team(EnterpriseTeamId(
        "team_01J99999999999999999999999".to_owned(),
    ));
    assert_eq!(
        service.list(&hidden_team).expect_err("hidden Team").kind(),
        CollaborationInboxErrorKind::Unauthorized
    );

    let left = fixture.clone();
    let right = fixture.clone();
    let left_thread = std::thread::spawn(move || {
        left.service().apply_claim(&CollaborationClaimCommand {
            context: left.context(30, 0),
            item_id: CollaborationInboxItemId::Approval(approval(1)),
            action: CollaborationClaimAction::Claim,
        })
    });
    let right_thread = std::thread::spawn(move || {
        right.service().apply_claim(&CollaborationClaimCommand {
            context: right.context(31, 0),
            item_id: CollaborationInboxItemId::Approval(approval(1)),
            action: CollaborationClaimAction::Claim,
        })
    });
    let results = [
        left_thread.join().expect("left thread"),
        right_thread.join().expect("right thread"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one conflict")
            .kind(),
        CollaborationInboxErrorKind::RevisionConflict
    );

    let page = service
        .list(&fixture.personal_list(10, None))
        .expect("refresh only reads");
    assert!(page.items[0].claim.is_some());
    assert!(matches!(
        page.items[0].source.command_route,
        FormalCollaborationCommandRoute::ApprovalDecide { .. }
    ));
}

#[test]
fn source_change_between_snapshot_and_commit_is_atomically_rejected() {
    let fixture = Fixture::new("source-guard-race");
    let mut service = CollaborationInboxService::with_clock(
        Box::new(SqliteStorage::open(&fixture.root).expect("Inbox storage")),
        Box::new(RacingSource {
            source: fixture.source.clone(),
            root: fixture.root.clone(),
            fired: false,
        }),
        Box::new(fixture.authority.clone()),
        Box::new(SharedClock(Arc::clone(&fixture.clock))),
    );
    let error = service
        .apply_claim(&CollaborationClaimCommand {
            context: fixture.context(90, 0),
            item_id: CollaborationInboxItemId::Approval(approval(1)),
            action: CollaborationClaimAction::Claim,
        })
        .expect_err("changed source guard rejects the Inbox write");
    assert_eq!(error.kind(), CollaborationInboxErrorKind::SourceUnavailable);
    let page = fixture
        .service()
        .list(&fixture.personal_list(10, None))
        .expect("failed guarded commit left no claim");
    assert!(page.items[0].claim.is_none());
}

fn add_file_and_hunk_annotations(
    fixture: &Fixture,
    service: &mut CollaborationInboxService,
    candidate: &CollaborationCandidateIdentity,
) {
    let file = annotation_command(
        fixture,
        21,
        1,
        "annotation_file",
        candidate.clone(),
        CollaborationAnnotationTarget::File {
            path: "src/lib.rs".to_owned(),
            blob_sha256: digest('d'),
        },
    );
    service.apply_annotation(&file).expect("file annotation");
    let hunk = annotation_command(
        fixture,
        22,
        2,
        "annotation_hunk",
        candidate.clone(),
        CollaborationAnnotationTarget::Hunk {
            path: "src/lib.rs".to_owned(),
            base_blob_sha256: digest('d'),
            start_line: 10,
            end_line: 20,
            hunk_sha256: digest('e'),
        },
    );
    service.apply_annotation(&hunk).expect("hunk annotation");
}

fn seed_guard_state(root: &PathBuf, stream_id: &str, sequence: u64) -> StateRevisionGuard {
    advance_guard_state(root, stream_id, 0, sequence);
    StateRevisionGuard::new(stream_id, 1).expect("fixture state guard")
}

fn advance_guard_state(root: &PathBuf, stream_id: &str, revision: u64, sequence: u64) {
    let mut storage = SqliteStorage::open(root).expect("guard storage");
    storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                ReceiptActorKey::from_encoded(b"collaboration-fixture-actor".to_vec())
                    .expect("fixture actor key"),
                ReceiptScopeKey::from_encoded(b"collaboration-fixture-scope".to_vec())
                    .expect("fixture scope key"),
                request(sequence),
            )
            .expect("fixture receipt identity"),
            digest('f'),
            stream_id,
            revision,
            format!("fixture-state-{}", revision + 1).into_bytes(),
            vec![NewOutboxEvent::internal(
                format!("evt_collaboration_guard_{sequence:020}"),
                "collaboration-inbox.guard.fixture.v1",
                b"{}".to_vec(),
            )],
        ))
        .expect("advance guard state");
    Box::new(storage).close().expect("close guard storage");
}

fn annotation_command(
    fixture: &Fixture,
    request_sequence: u64,
    expected_revision: u64,
    id: &str,
    candidate: CollaborationCandidateIdentity,
    target: CollaborationAnnotationTarget,
) -> CollaborationAnnotationCommand {
    CollaborationAnnotationCommand {
        context: fixture.context(request_sequence, expected_revision),
        item_id: CollaborationInboxItemId::Approval(approval(1)),
        annotation_id: CollaborationAnnotationId(id.to_owned()),
        action: CollaborationAnnotationAction::Upsert {
            candidate,
            target,
            body_sha256: digest('9'),
        },
    }
}

fn entitlement(
    scope: &RepositoryScope,
    principal: &UserId,
    target: ResponsibilityTarget,
    role: ResponsibilityRole,
    team_ids: Vec<EnterpriseTeamId>,
    sequence: u64,
) -> CollaborationResponsibilityEntitlement {
    CollaborationResponsibilityEntitlement {
        assignment: ResponsibilityAssignment {
            id: ResponsibilityAssignmentId(format!("assignment-{sequence}")),
            scope: scope.clone(),
            target,
            role,
            principal_user_id: principal.clone(),
            state: ResponsibilityAssignmentState::Active,
            revision: 2,
            assigned_by: actor(&user(9)),
            assigned_at_millis: NOW - 100,
            accepted_at_millis: Some(NOW - 50),
            expires_at_millis: None,
            ended_at_millis: None,
            target_revision: 1,
            target_sha256: digest('1'),
            rbac_revision: 1,
            rbac_sha256: digest('2'),
        },
        team_ids,
    }
}

fn approval_item(
    sequence: u64,
    expires_at: u64,
    state: CollaborationInboxItemState,
) -> CollaborationInboxSourceItem {
    let product_session_id = product_session(sequence);
    CollaborationInboxSourceItem {
        id: CollaborationInboxItemId::Approval(approval(sequence)),
        kind: CollaborationInboxItemKind::Approval,
        target: ResponsibilityTarget::ProductSession {
            product_session_id: product_session_id.clone(),
        },
        responsibility_role: ResponsibilityRole::Approver,
        source_revision: 1,
        source_sha256: digest(
            char::from_digit(u32::try_from(sequence).unwrap_or(1), 16).unwrap_or('1'),
        ),
        title_sha256: digest('a'),
        opened_at_millis: NOW - 10 + sequence,
        expires_at_millis: Some(expires_at),
        state,
        candidate: None,
        command_route: FormalCollaborationCommandRoute::ApprovalDecide {
            approval_id: approval(sequence),
            product_session_id,
        },
    }
}

fn review_approval_item(candidate: CollaborationCandidateIdentity) -> CollaborationInboxSourceItem {
    let mut item = approval_item(1, NOW + 300, CollaborationInboxItemState::Pending);
    item.candidate = Some(candidate);
    item
}

fn delivery_attention_item(sequence: u64, expires_at: u64) -> CollaborationInboxSourceItem {
    CollaborationInboxSourceItem {
        id: CollaborationInboxItemId::DeliveryAttention(attention(sequence)),
        kind: CollaborationInboxItemKind::DeliveryAttention,
        target: ResponsibilityTarget::Delivery {
            delivery_id: delivery(sequence),
        },
        responsibility_role: ResponsibilityRole::Assignee,
        source_revision: 1,
        source_sha256: digest('b'),
        title_sha256: digest('c'),
        opened_at_millis: NOW + sequence,
        expires_at_millis: Some(expires_at),
        state: CollaborationInboxItemState::Pending,
        candidate: None,
        command_route: FormalCollaborationCommandRoute::DeliveryResolveAttention {
            attention_item_id: attention(sequence),
            delivery_id: delivery(sequence),
        },
    }
}

fn candidate(hex: char, revision: u64) -> CollaborationCandidateIdentity {
    CollaborationCandidateIdentity {
        candidate_ref: format!("candidate-{revision}"),
        candidate_digest: digest(hex),
        candidate_revision: revision,
    }
}

fn items_digest(items: &[CollaborationInboxSourceItem]) -> Sha256Digest {
    let mut canonical = items.to_vec();
    canonical.sort_by(|left, right| left.id.cmp(&right.id));
    let bytes = serde_json::to_vec(&canonical).expect("source JSON");
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn digest(hex: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", hex.to_string().repeat(64)))
}

fn repository_scope(sequence: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical("org", sequence)),
        workspace_id: WorkspaceId(canonical("wsp", sequence)),
        project_id: ProjectId(canonical("prj", sequence)),
        repository_id: RepositoryId(canonical("rep", sequence)),
    }
}

fn actor(user_id: &UserId) -> Actor {
    Actor::UserActor(UserActor {
        id: user_id.clone(),
        kind: UserActorKind::User,
    })
}

fn user(sequence: u64) -> UserId {
    UserId(canonical("usr", sequence))
}

fn product_session(sequence: u64) -> ProductSessionId {
    ProductSessionId(format!("ps_01J{sequence:023}"))
}

fn delivery(sequence: u64) -> DeliveryId {
    DeliveryId(format!("del_01J{sequence:023}"))
}

fn approval(sequence: u64) -> ApprovalId {
    ApprovalId(format!("approval_01J{sequence:023}"))
}

fn attention(sequence: u64) -> AttentionItemId {
    AttentionItemId(format!("attention_01J{sequence:023}"))
}

fn request(sequence: u64) -> RequestId {
    RequestId(canonical("req", sequence))
}

fn canonical(prefix: &str, sequence: u64) -> String {
    format!("{prefix}_01J{sequence:023}")
}

fn temporary_directory(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "winwincode-collaboration-inbox-{label}-{}-{}",
        std::process::id(),
        NOW
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temporary directory");
    root
}

#[allow(dead_code)]
fn _review_target(delivery_id: DeliveryId) -> ResponsibilityTarget {
    ResponsibilityTarget::Review {
        delivery_id,
        review: ResponsibilityReviewKind::Solution,
    }
}
