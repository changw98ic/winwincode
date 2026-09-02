// SPDX-License-Identifier: Apache-2.0

use winwincode_api::generated::{ModelRoute, RepositoryScope, RepositoryScopeKind};
use winwincode_control_plane::{
    ModelFrameWriteStatus, ModelRequestAdmission, ModelRequestAdmissionStatus, ModelRequestPool,
    ModelRequestPoolConfig, ModelRequestPoolErrorCode, ModelRequestState,
    ModelRequestTerminalOutcome, ModelStreamFrame, ModelStreamReadControl, ProviderGatewayIdentity,
};
use winwincode_domain::{
    CredentialReferenceId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, WorkspaceId,
};

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn identity(organization: u64, project: u64) -> ProviderGatewayIdentity {
    identity_with_repository(organization, project, 1)
}

fn identity_with_repository(
    organization: u64,
    project: u64,
    repository: u64,
) -> ProviderGatewayIdentity {
    ProviderGatewayIdentity::product_session(
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(id("org", organization)),
            workspace_id: WorkspaceId(id("wsp", 1)),
            project_id: ProjectId(id("prj", project)),
            repository_id: RepositoryId(id("rep", repository)),
        },
        ProductSessionId(id("psn", project)),
    )
}

fn admission_for_identity(
    identity: &ProviderGatewayIdentity,
    provider: &str,
    model: &str,
    credential: u64,
    exchange: u64,
) -> ModelRequestAdmission {
    ModelRequestAdmission::from_gateway_route(
        identity,
        &ModelRoute {
            credential_reference_id: CredentialReferenceId(id("crd", credential)),
            model_id: model.into(),
            provider_id: provider.into(),
        },
        ModelExchangeId(id("mdl", exchange)),
        RequestId(id("req", exchange)),
    )
    .expect("Gateway pool admission")
}

fn admission(
    organization: u64,
    project: u64,
    provider: &str,
    model: &str,
    credential: u64,
    exchange: u64,
) -> ModelRequestAdmission {
    let identity = identity(organization, project);
    admission_for_identity(&identity, provider, model, credential, exchange)
}

#[test]
fn route_limit_is_project_scoped_and_same_project_repositories_share_the_limit() {
    let mut bounded = config();
    bounded.max_routes = 1;
    let mut pool = ModelRequestPool::new(bounded).expect("project-bounded pool");
    let project_a_repository_one = admission_for_identity(
        &identity_with_repository(1, 1, 1),
        "provider-a",
        "model-a",
        1,
        90,
    );
    let project_a_repository_two = admission_for_identity(
        &identity_with_repository(1, 1, 2),
        "provider-b",
        "model-b",
        2,
        91,
    );
    let project_b = admission_for_identity(
        &identity_with_repository(1, 2, 1),
        "provider-c",
        "model-c",
        3,
        92,
    );

    pool.submit(&project_a_repository_one)
        .expect("first Project A route");
    assert_eq!(
        pool.submit(&project_a_repository_two)
            .expect_err("second Project A route exceeds the shared limit")
            .code(),
        ModelRequestPoolErrorCode::RouteLimit
    );
    pool.submit(&project_b)
        .expect("Project B owns an independent route limit");
}

fn config() -> ModelRequestPoolConfig {
    ModelRequestPoolConfig {
        max_routes: 8,
        max_active_per_route: 1,
        max_waiting_per_route: 2,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 2,
        resume_buffered_frames_per_stream: 1,
        max_buffered_bytes_per_stream: 6,
        resume_buffered_bytes_per_stream: 3,
    }
}

#[test]
fn saturated_route_does_not_block_another_route_and_fifo_grants_are_fair() {
    let mut pool = ModelRequestPool::new(config()).expect("pool");
    let a1 = admission(1, 1, "provider-a", "model-a", 1, 1);
    let a2 = admission(1, 1, "provider-a", "model-a", 1, 2);
    let a3 = admission(1, 1, "provider-a", "model-a", 1, 3);
    let a4 = admission(1, 1, "provider-a", "model-a", 1, 4);
    let b1 = admission(1, 1, "provider-b", "model-b", 2, 5);
    let other_model = admission(1, 1, "provider-a", "model-b", 1, 6);
    let other_credential = admission(1, 1, "provider-a", "model-a", 2, 7);
    let other_organization = admission(2, 1, "provider-a", "model-a", 1, 8);
    let other_project = admission(1, 2, "provider-a", "model-a", 1, 9);

    assert_eq!(
        pool.submit(&a1).expect("route A active").status,
        ModelRequestAdmissionStatus::Started
    );
    assert_eq!(
        pool.submit(&a2).expect("route A queued one").queue_position,
        Some(1)
    );
    assert_eq!(
        pool.submit(&a3).expect("route A queued two").queue_position,
        Some(2)
    );
    assert_eq!(
        pool.submit(&a4)
            .expect_err("route A queue is bounded")
            .code(),
        ModelRequestPoolErrorCode::QueueFull
    );
    assert_eq!(
        pool.submit(&b1).expect("independent route starts").status,
        ModelRequestAdmissionStatus::Started
    );
    for independent in [
        &other_model,
        &other_credential,
        &other_organization,
        &other_project,
    ] {
        assert_eq!(
            pool.submit(independent).expect("partition starts").status,
            ModelRequestAdmissionStatus::Started
        );
    }

    let first_terminal = pool
        .terminate(
            &a1.model_exchange_id,
            ModelRequestTerminalOutcome::Succeeded,
        )
        .expect("first route A terminal");
    assert_eq!(
        first_terminal.granted_exchange_id,
        Some(a2.model_exchange_id.clone())
    );
    assert_eq!(
        pool.reconnect(&a2.model_exchange_id).expect("A2").state,
        ModelRequestState::Active
    );
    assert_eq!(
        pool.reconnect(&a3.model_exchange_id)
            .expect("A3")
            .queue_position,
        Some(1)
    );
    assert_eq!(
        pool.reconnect(&b1.model_exchange_id).expect("B1").state,
        ModelRequestState::Active
    );

    let second_terminal = pool
        .terminate(
            &a2.model_exchange_id,
            ModelRequestTerminalOutcome::Succeeded,
        )
        .expect("second route A terminal");
    assert_eq!(
        second_terminal.granted_exchange_id,
        Some(a3.model_exchange_id.clone())
    );
    let route_a = pool.route_snapshot(&a1.route).expect("route A snapshot");
    assert_eq!(route_a.active, 1);
    assert_eq!(route_a.waiting, 0);
    assert_eq!(route_a.retained_terminal, 2);
}

#[test]
fn slow_client_backpressures_at_fixed_memory_and_final_stream_releases_slot() {
    let mut pool = ModelRequestPool::new(config()).expect("pool");
    let first = admission(1, 1, "provider-a", "model-a", 1, 10);
    let waiting = admission(1, 1, "provider-a", "model-a", 1, 11);
    pool.submit(&first).expect("active");
    pool.submit(&waiting).expect("waiting");

    let first_frame = pool
        .push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::data(1, b"abc".to_vec()),
        )
        .expect("first frame");
    assert_eq!(first_frame.status, ModelFrameWriteStatus::Accepted);
    let second_frame = pool
        .push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::data(2, b"def".to_vec()),
        )
        .expect("second frame");
    assert_eq!(second_frame.buffered_frames, 2);
    assert_eq!(second_frame.buffered_bytes, 6);
    assert_eq!(second_frame.read_control, ModelStreamReadControl::Paused);
    let backpressured = pool
        .push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::data(3, b"x".to_vec()),
        )
        .expect("backpressure is a stable result");
    assert_eq!(backpressured.status, ModelFrameWriteStatus::Backpressured);
    assert_eq!(backpressured.highest_sequence, 2);
    assert_eq!(backpressured.buffered_bytes, 6);

    let acknowledged = pool
        .acknowledge(&first.model_exchange_id, 1)
        .expect("ack first frame");
    assert_eq!(acknowledged.buffered_frames, 1);
    assert_eq!(acknowledged.buffered_bytes, 3);
    assert_eq!(acknowledged.read_control, ModelStreamReadControl::Read);
    assert_eq!(
        pool.push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::data(3, b"x".to_vec()),
        )
        .expect("retry after ack")
        .status,
        ModelFrameWriteStatus::Accepted
    );
    let one_frame = pool
        .read_buffered(&first.model_exchange_id, 0, 1, 6)
        .expect("bounded read");
    assert_eq!(one_frame.len(), 1);
    assert_eq!(one_frame[0].sequence(), 2);

    assert_eq!(
        pool.push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::terminal(4, b"z".to_vec(), ModelRequestTerminalOutcome::Succeeded,),
        )
        .expect("final remains backpressured")
        .status,
        ModelFrameWriteStatus::Backpressured
    );
    pool.acknowledge(&first.model_exchange_id, 2)
        .expect("free another frame");
    let terminal = pool
        .push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::terminal(4, b"z".to_vec(), ModelRequestTerminalOutcome::Succeeded),
        )
        .expect("terminal frame");
    assert_eq!(terminal.status, ModelFrameWriteStatus::Accepted);
    assert_eq!(terminal.state, ModelRequestState::Succeeded);
    assert_eq!(terminal.read_control, ModelStreamReadControl::Closed);
    assert_eq!(
        terminal.granted_exchange_id,
        Some(waiting.model_exchange_id.clone())
    );
    assert_eq!(
        pool.reconnect(&waiting.model_exchange_id)
            .expect("waiting granted")
            .state,
        ModelRequestState::Active
    );
}

#[test]
fn provider_event_batches_are_atomic_retryable_and_hard_bounded() {
    let mut pool = ModelRequestPool::new(config()).expect("pool");
    let request = admission(1, 1, "provider-a", "model-a", 1, 15);
    pool.submit(&request).expect("active");
    pool.push_frame(
        &request.model_exchange_id,
        &ModelStreamFrame::data(1, b"abc".to_vec()),
    )
    .expect("first frame");

    let terminal_batch = [
        ModelStreamFrame::data(2, b"xy".to_vec()),
        ModelStreamFrame::terminal(3, b"z".to_vec(), ModelRequestTerminalOutcome::Succeeded),
    ];
    let backpressured = pool
        .push_frames(&request.model_exchange_id, &terminal_batch)
        .expect("batch backpressure");
    assert_eq!(backpressured.status, ModelFrameWriteStatus::Backpressured);
    assert_eq!(backpressured.highest_sequence, 1);
    assert_eq!(backpressured.buffered_frames, 1);
    assert_eq!(backpressured.read_control, ModelStreamReadControl::Paused);

    let resumed = pool
        .acknowledge(&request.model_exchange_id, 1)
        .expect("free capacity");
    assert_eq!(resumed.read_control, ModelStreamReadControl::Read);
    let accepted = pool
        .push_frames(&request.model_exchange_id, &terminal_batch)
        .expect("retry exact batch");
    assert_eq!(accepted.status, ModelFrameWriteStatus::Accepted);
    assert_eq!(accepted.highest_sequence, 3);
    assert_eq!(accepted.read_control, ModelStreamReadControl::Closed);
    assert_eq!(
        pool.push_frames(&request.model_exchange_id, &terminal_batch)
            .expect("terminal batch exact replay")
            .status,
        ModelFrameWriteStatus::Duplicate
    );

    let another = admission(1, 2, "provider-a", "model-a", 1, 16);
    pool.submit(&another).expect("other route active");
    assert_eq!(
        pool.push_frame(
            &another.model_exchange_id,
            &ModelStreamFrame::data(1, vec![0; 7]),
        )
        .expect_err("single frame cannot exceed its hard byte bound")
        .code(),
        ModelRequestPoolErrorCode::InvalidInput
    );
    assert_eq!(
        pool.reconnect(&another.model_exchange_id)
            .expect("other route remains readable")
            .read_control,
        ModelStreamReadControl::Read
    );
}

#[test]
fn cancellation_releases_once_and_reconnect_or_replay_never_reterminates() {
    let mut pool = ModelRequestPool::new(config()).expect("pool");
    let first = admission(1, 1, "provider-a", "model-a", 1, 20);
    let second = admission(1, 1, "provider-a", "model-a", 1, 21);
    let third = admission(1, 1, "provider-a", "model-a", 1, 22);
    pool.submit(&first).expect("first active");
    pool.submit(&second).expect("second queued");
    pool.submit(&third).expect("third queued");

    let cancellation = pool
        .cancel(&first.model_exchange_id)
        .expect("cancel active");
    assert!(!cancellation.replayed);
    assert_eq!(
        cancellation.granted_exchange_id,
        Some(second.model_exchange_id.clone())
    );
    let reconnect = pool
        .reconnect(&first.model_exchange_id)
        .expect("terminal reconnect");
    assert_eq!(reconnect.state, ModelRequestState::Cancelled);
    assert_eq!(
        reconnect.terminal_outcome,
        Some(ModelRequestTerminalOutcome::Cancelled)
    );

    let replay = pool
        .cancel(&first.model_exchange_id)
        .expect("cancel replay");
    assert!(replay.replayed);
    assert!(replay.granted_exchange_id.is_none());
    assert_eq!(
        pool.submit(&first).expect("submit replay").status,
        ModelRequestAdmissionStatus::Duplicate
    );
    assert_eq!(
        pool.terminate(
            &first.model_exchange_id,
            ModelRequestTerminalOutcome::Failed,
        )
        .expect_err("terminal outcome cannot change")
        .code(),
        ModelRequestPoolErrorCode::TerminalConflict
    );

    let queued_cancel = pool
        .cancel(&third.model_exchange_id)
        .expect("cancel queued");
    assert!(queued_cancel.granted_exchange_id.is_none());
    assert_eq!(
        pool.reconnect(&second.model_exchange_id)
            .expect("second")
            .state,
        ModelRequestState::Active
    );
    assert_eq!(pool.route_snapshot(&first.route).expect("route").waiting, 0);
}

#[test]
fn complete_authority_restores_fifo_ack_cursor_frame_digests_and_pause_state() {
    let first = admission(1, 1, "provider-a", "model-a", 1, 30);
    let waiting = admission(1, 1, "provider-a", "model-a", 1, 31);
    let mut original = ModelRequestPool::new(config()).expect("original pool");
    original.submit(&first).expect("first active");
    original.submit(&waiting).expect("waiting queued");
    for (sequence, payload) in [(1, b"aa".as_slice()), (2, b"bb".as_slice())] {
        original
            .push_frame(
                &first.model_exchange_id,
                &ModelStreamFrame::data(sequence, payload.to_vec()),
            )
            .expect("buffer frame");
    }
    original
        .acknowledge(&first.model_exchange_id, 1)
        .expect("advance durable ack cursor");
    let paused = original
        .push_frame(
            &first.model_exchange_id,
            &ModelStreamFrame::data(3, b"cc".to_vec()),
        )
        .expect("pause at hard watermark");
    assert_eq!(paused.read_control, ModelStreamReadControl::Paused);

    let authority = original.export_authority().expect("canonical authority");
    let mut restored = ModelRequestPool::new(config()).expect("restored pool");
    restored
        .restore_authority(&authority)
        .expect("restore complete authority");
    let snapshot = restored
        .reconnect(&first.model_exchange_id)
        .expect("restored active");
    assert_eq!(snapshot.next_sequence, 4);
    assert_eq!(snapshot.acknowledged_sequence, 1);
    assert_eq!(snapshot.buffered_frames, 2);
    assert_eq!(snapshot.read_control, ModelStreamReadControl::Paused);
    assert_eq!(
        restored
            .reconnect(&waiting.model_exchange_id)
            .expect("restored waiter")
            .queue_position,
        Some(1)
    );
    assert_eq!(
        restored
            .push_frame(
                &first.model_exchange_id,
                &ModelStreamFrame::data(2, b"zz".to_vec()),
            )
            .expect_err("changed retained sequence conflicts")
            .code(),
        ModelRequestPoolErrorCode::FrameConflict
    );
    assert_eq!(
        restored
            .push_frame(
                &first.model_exchange_id,
                &ModelStreamFrame::data(2, b"bb".to_vec()),
            )
            .expect("exact retained frame replays")
            .status,
        ModelFrameWriteStatus::Duplicate
    );
    assert_eq!(
        restored
            .terminate(
                &first.model_exchange_id,
                ModelRequestTerminalOutcome::Failed,
            )
            .expect("release restored active slot")
            .granted_exchange_id,
        Some(waiting.model_exchange_id.clone())
    );

    let mut noncanonical = authority.clone();
    noncanonical.insert(0, b' ');
    let mut rejected = ModelRequestPool::new(config()).expect("rejected pool");
    assert_eq!(
        rejected
            .restore_authority(&noncanonical)
            .expect_err("noncanonical authority is rejected")
            .code(),
        ModelRequestPoolErrorCode::InvalidState
    );
}
