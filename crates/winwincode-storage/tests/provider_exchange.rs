use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ControlPlaneEventId, ExecutionMessageId, Instant, ModelExchangeId, OrganizationId, ProjectId,
    RequestId, Sha256Digest, SystemActorId, WorkspaceId,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ProjectionEventStream, ProviderExchangeBegin,
    ProviderExchangeFailure, ProviderExchangeOpened, ProviderExchangeState,
    ProviderExchangeStoreErrorCode, ProviderExchangeTerminal, ProviderExchangeTerminalStage,
    PublicEventActor, PublicEventScope, PublicEventSource, SqliteStorage, StateCommit,
    public_receipt_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-exchange-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2031-03-15T09:00:{second:02}.000Z"))
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn readiness_commit(seed: u64, marker: &str, authority: &[u8]) -> StateCommit {
    let actor = PublicEventActor::System {
        id: SystemActorId(id("sys", 1)),
    };
    let scope = PublicEventScope::Project {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
    };
    let request_id = RequestId(id("req", 10_000 + seed));
    let identity =
        public_receipt_identity(&actor, &scope, request_id).expect("canonical readiness receipt");
    let event = NewOutboxEvent::public_projection(
        ControlPlaneEventId(format!("evt_pool_readiness_{marker}")),
        "model-route-availability.invalidated.v1",
        format!(r#"{{"marker":"{marker}"}}"#).into_bytes(),
        ProjectionEventStream::Scope,
        scope,
        at(4),
        PublicEventSource::ControlPlane {
            actor,
            component: "model-route-availability".to_owned(),
        },
    )
    .expect("canonical readiness event");
    let mut command = Vec::from(marker.as_bytes());
    command.extend_from_slice(authority);
    StateCommit::new(
        identity,
        digest(&command),
        format!("model-request-pool-readiness:{marker}"),
        0,
        br#"{"schema":"pool-readiness-v1"}"#.to_vec(),
        vec![event],
    )
}

fn begin(seed: u64, body: &[u8]) -> ProviderExchangeBegin {
    ProviderExchangeBegin {
        model_exchange_id: ModelExchangeId(id("mdl", seed)),
        request_id: RequestId(id("req", seed)),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        open_digest: digest(body),
        provider_id: format!("provider-{seed}"),
        adapter_request_id: format!("adapter-request-{seed}"),
        started_at: at(1),
    }
}

fn opened(marker: &str) -> ProviderExchangeOpened {
    ProviderExchangeOpened::new(
        digest(format!("authority-{marker}").as_bytes()),
        format!(r#"{{"kind":"frozen-route","marker":"{marker}"}}"#).into_bytes(),
        format!(r#"{{"adapterRequestId":"upstream-{marker}"}}"#).into_bytes(),
        format!(r#"{{"requestFingerprint":"retry-{marker}"}}"#).into_bytes(),
        at(2),
    )
    .expect("valid opened metadata")
}

fn terminal(marker: &str) -> ProviderExchangeTerminal {
    ProviderExchangeTerminal::new(
        digest(format!("terminal-{marker}").as_bytes()),
        format!(r#"{{"outcome":"completed","marker":"{marker}"}}"#).into_bytes(),
        at(3),
    )
    .expect("valid terminal metadata")
}

#[test]
fn restart_replays_exact_open_and_terminal_without_accepting_changed_facts() {
    let root = temporary_directory("restart");
    let request = begin(1, b"original-model-request");
    let open = opened("one");
    let first = {
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        let opening = exchanges
            .begin_open(&request)
            .expect("begin before Provider");
        assert_eq!(opening.state, ProviderExchangeState::Opening);
        assert!(!opening.idempotent_replay);
        exchanges
            .commit_opened(&request.model_exchange_id, &request.open_digest, &open)
            .expect("commit accepted Provider exchange")
    };
    assert_eq!(first.state, ProviderExchangeState::Opened);
    assert_eq!(first.route_authority_json(), Some(opened_json()));

    {
        let mut storage = SqliteStorage::open(&root).expect("restart storage");
        let mut exchanges = storage
            .provider_exchange_store()
            .expect("restart exchange store");
        let replay = exchanges
            .begin_open(&request)
            .expect("exact restart replay");
        assert_eq!(replay.state, ProviderExchangeState::Opened);
        assert!(replay.idempotent_replay);

        let changed = begin(1, b"changed-model-request");
        assert_eq!(
            exchanges
                .begin_open(&changed)
                .expect_err("changed body conflicts")
                .code(),
            ProviderExchangeStoreErrorCode::Conflict
        );
        let mut changed_adapter_identity = request.clone();
        changed_adapter_identity.adapter_request_id = "adapter-request-changed".to_owned();
        assert_eq!(
            exchanges
                .begin_open(&changed_adapter_identity)
                .expect_err("changed precommitted adapter identity conflicts")
                .code(),
            ProviderExchangeStoreErrorCode::Conflict
        );
        let mut changed_provider = request.clone();
        changed_provider.provider_id = "provider-changed".to_owned();
        assert_eq!(
            exchanges
                .begin_open(&changed_provider)
                .expect_err("changed precommitted Provider conflicts")
                .code(),
            ProviderExchangeStoreErrorCode::Conflict
        );
        let terminal_record = terminal("one");
        let settled = exchanges
            .commit_terminal(&request.model_exchange_id, &terminal_record)
            .expect("commit terminal");
        assert_eq!(settled.state, ProviderExchangeState::Terminal);
        let replay = exchanges
            .commit_terminal(&request.model_exchange_id, &terminal_record)
            .expect("terminal replay");
        assert!(replay.idempotent_replay);
        assert_eq!(
            exchanges
                .commit_terminal(&request.model_exchange_id, &terminal("changed"))
                .expect_err("changed terminal conflicts")
                .code(),
            ProviderExchangeStoreErrorCode::Conflict
        );
    }
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn failed_open_is_a_stable_tombstone_and_never_transitions_to_opened() {
    let root = temporary_directory("failed");
    let request = begin(2, b"rejected-model-request");
    {
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        exchanges.begin_open(&request).expect("begin open");
        let failure = ProviderExchangeFailure {
            failure_kind: "credential_unavailable".to_owned(),
            failed_at: at(2),
        };
        exchanges
            .commit_failed(&request.model_exchange_id, &request.open_digest, &failure)
            .expect("commit stable failure");
        let replay = exchanges.begin_open(&request).expect("replay failed open");
        assert_eq!(replay.state, ProviderExchangeState::Failed);
        assert_eq!(
            replay.failure_kind.as_deref(),
            Some("credential_unavailable")
        );
        assert!(replay.idempotent_replay);
        assert_eq!(
            exchanges
                .commit_opened(
                    &request.model_exchange_id,
                    &request.open_digest,
                    &opened("late"),
                )
                .expect_err("failed exchange cannot reopen")
                .code(),
            ProviderExchangeStoreErrorCode::InvalidState
        );
    }
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn raw_model_request_never_reaches_database_debug_or_durable_metadata() {
    const RAW_REQUEST: &[u8] = b"FORBIDDEN_MODEL_PAYLOAD_FIXTURE_7db928";
    let root = temporary_directory("payload-boundary");
    let request = begin(3, RAW_REQUEST);
    let open = opened("safe");
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let snapshot = {
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        exchanges.begin_open(&request).expect("begin open");
        exchanges
            .commit_opened(&request.model_exchange_id, &request.open_digest, &open)
            .expect("commit opened")
    };
    assert!(!format!("{open:?}{snapshot:?}").contains("FORBIDDEN_MODEL_PAYLOAD"));
    drop(storage);
    assert_database_files_exclude(&root, RAW_REQUEST);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn noncanonical_digest_and_partial_state_are_rejected() {
    let root = temporary_directory("corruption");
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut uppercase = begin(4, b"uppercase-digest");
    uppercase.open_digest = Sha256Digest(format!("sha256:{}", "A".repeat(64)));
    assert_eq!(
        storage
            .provider_exchange_store()
            .expect("exchange store")
            .begin_open(&uppercase)
            .expect_err("uppercase digest is not canonical")
            .code(),
        ProviderExchangeStoreErrorCode::InvalidInput
    );
    {
        let _store = storage.provider_exchange_store().expect("prepare schema");
    }
    let corrupt = begin(5, b"corrupt-partial-state");
    let connection = Connection::open(storage.database_path()).expect("open raw connection");
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .expect("enable corruption fixture");
    connection
        .execute(
            "INSERT INTO internal_provider_exchanges
                (model_exchange_id, request_id, message_id, open_digest,
                 provider_id, adapter_request_id, state,
                 route_authority_fingerprint, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'opening', ?7, ?8, ?8)",
            params![
                corrupt.model_exchange_id.0,
                corrupt.request_id.0,
                corrupt.message_id.0,
                corrupt.open_digest.0,
                corrupt.provider_id,
                corrupt.adapter_request_id,
                digest(b"partial-authority").0,
                corrupt.started_at.0,
            ],
        )
        .expect("inject partial state");
    drop(connection);
    assert_eq!(
        storage
            .provider_exchange_store()
            .expect("exchange store")
            .load(&corrupt.model_exchange_id)
            .expect_err("partial state is corrupt")
            .code(),
        ProviderExchangeStoreErrorCode::Storage
    );
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn one_pool_authority_commits_with_open_failure_and_terminal_transitions() {
    let root = temporary_directory("single-pool-authority");
    let active = begin(6, b"active-request");
    let failed = begin(7, b"failed-request");
    let active_authority = br#"{"generation":1,"state":"active"}"#;
    let failed_authority = br#"{"generation":2,"state":"failed"}"#;
    let terminal_authority = br#"{"generation":3,"state":"terminal"}"#;
    let forgotten_authority = br#"{"generation":4,"state":"forgotten"}"#;
    let final_ack_receipt = br#"{"acknowledgedSequence":4,"schema":"final-ack-v1"}"#;
    let active_readiness = readiness_commit(1, "active", active_authority);
    let failed_readiness = readiness_commit(2, "failed", failed_authority);
    let terminal_readiness = readiness_commit(3, "terminal", terminal_authority);
    let forgotten_readiness = readiness_commit(4, "forgotten", forgotten_authority);
    {
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        exchanges
            .begin_open_with_pool_authority(&active, active_authority, &active_readiness)
            .expect("atomic active authority and opening tombstone");
        assert_eq!(
            exchanges
                .load_pool_authority()
                .expect("load active authority")
                .expect("active authority")
                .state_json(),
            active_authority
        );
        exchanges
            .commit_opened(
                &active.model_exchange_id,
                &active.open_digest,
                &opened("atomic"),
            )
            .expect("commit opened without a second pool snapshot");
        exchanges.begin_open(&failed).expect("begin failed request");
        exchanges
            .commit_failed_with_pool_authority(
                &failed.model_exchange_id,
                &failed.open_digest,
                &ProviderExchangeFailure {
                    failure_kind: "provider_rejected".to_owned(),
                    failed_at: at(3),
                },
                failed_authority,
                &failed_readiness,
            )
            .expect("atomic failed state and authority");
        exchanges
            .commit_terminal_with_pool_authority(
                &active.model_exchange_id,
                &terminal("atomic"),
                terminal_authority,
                &terminal_readiness,
            )
            .expect("atomic terminal state and authority");
        let final_ack_digest = digest(b"exact-final-ack-envelope");
        let acknowledgement = exchanges
            .commit_final_ack_with_pool_authority(
                &active.model_exchange_id,
                &final_ack_digest,
                4,
                final_ack_receipt,
                forgotten_authority,
                &at(4),
                &forgotten_readiness,
            )
            .expect("atomic final ack tombstone and forgotten pool authority");
        assert!(!acknowledgement.idempotent_replay);
        let replay = exchanges
            .commit_final_ack_with_pool_authority(
                &active.model_exchange_id,
                &final_ack_digest,
                4,
                final_ack_receipt,
                forgotten_authority,
                &at(4),
                &forgotten_readiness,
            )
            .expect("exact final ack replay");
        assert!(replay.idempotent_replay);
        assert_eq!(
            exchanges
                .commit_final_ack_with_pool_authority(
                    &active.model_exchange_id,
                    &digest(b"changed-final-ack-envelope"),
                    4,
                    final_ack_receipt,
                    forgotten_authority,
                    &at(4),
                    &forgotten_readiness,
                )
                .expect_err("changed final ack conflicts")
                .code(),
            ProviderExchangeStoreErrorCode::Conflict
        );
    }
    assert_restarted_single_authority(
        &root,
        &active,
        &failed,
        forgotten_authority,
        final_ack_receipt,
    );
    let storage = SqliteStorage::open(&root).expect("reopen public outbox");
    assert_eq!(
        storage
            .pending_events()
            .expect("pending readiness events")
            .into_iter()
            .filter(|event| event.topic == "model-route-availability.invalidated.v1")
            .count(),
        4
    );
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_ack_write_failure_keeps_terminal_pool_authority_for_exact_retry() {
    let root = temporary_directory("final-ack-rollback");
    let request = begin(8, b"final-ack-rollback");
    let active_authority = br#"{"generation":1,"state":"active"}"#;
    let terminal_authority = br#"{"generation":2,"state":"terminal"}"#;
    let forgotten_authority = br#"{"generation":3,"state":"forgotten"}"#;
    let receipt = br#"{"acknowledgedSequence":1,"schema":"final-ack-v1"}"#;
    let ack_digest = digest(b"final-ack-rollback-envelope");
    let active_readiness = readiness_commit(5, "rollback-active", active_authority);
    let terminal_readiness = readiness_commit(6, "rollback-terminal", terminal_authority);
    let forgotten_readiness = readiness_commit(7, "rollback-forgotten", forgotten_authority);
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    {
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        exchanges
            .begin_open_with_pool_authority(&request, active_authority, &active_readiness)
            .expect("begin active");
        exchanges
            .commit_opened(
                &request.model_exchange_id,
                &request.open_digest,
                &opened("rollback"),
            )
            .expect("commit open");
        exchanges
            .commit_terminal_with_pool_authority(
                &request.model_exchange_id,
                &terminal("rollback"),
                terminal_authority,
                &terminal_readiness,
            )
            .expect("commit terminal");
    }
    let connection = Connection::open(storage.database_path()).expect("open trigger connection");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_final_ack BEFORE INSERT
             ON internal_provider_exchange_final_acks
             BEGIN SELECT RAISE(ABORT, 'injected final ack failure'); END;",
        )
        .expect("install failure trigger");
    assert_eq!(
        storage
            .provider_exchange_store()
            .expect("exchange store")
            .commit_final_ack_with_pool_authority(
                &request.model_exchange_id,
                &ack_digest,
                1,
                receipt,
                forgotten_authority,
                &at(4),
                &forgotten_readiness,
            )
            .expect_err("injected final ack failure")
            .code(),
        ProviderExchangeStoreErrorCode::Storage
    );
    assert_eq!(
        storage
            .pending_events()
            .expect("rolled-back readiness outbox")
            .into_iter()
            .filter(|event| event.topic == "model-route-availability.invalidated.v1")
            .count(),
        2
    );
    connection
        .execute_batch("DROP TRIGGER fail_final_ack;")
        .expect("remove failure trigger");
    {
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        assert!(
            exchanges
                .load_final_ack(&request.model_exchange_id)
                .expect("load absent ack")
                .is_none()
        );
        assert_eq!(
            exchanges
                .load_pool_authority()
                .expect("load terminal authority")
                .expect("terminal authority")
                .state_json(),
            terminal_authority
        );
        exchanges
            .commit_final_ack_with_pool_authority(
                &request.model_exchange_id,
                &ack_digest,
                1,
                receipt,
                forgotten_authority,
                &at(4),
                &forgotten_readiness,
            )
            .expect("exact retry commits once");
    }
    assert_eq!(
        storage
            .pending_events()
            .expect("retried readiness outbox")
            .into_iter()
            .filter(|event| event.topic == "model-route-availability.invalidated.v1")
            .count(),
        3
    );
    drop(connection);
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn terminal_saga_receipts_survive_every_ordered_restart_checkpoint() {
    let root = temporary_directory("terminal-progress");
    let request = begin(9, b"terminal-progress");
    let command_digest = digest(b"cancel-command");
    let admission = br#"{"outcome":"cancelled","revision":2}"#;
    let receipt = br#"{"outcome":"cancelled","settledAt":"2031-03-15T09:00:04Z"}"#;
    {
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        let mut exchanges = storage.provider_exchange_store().expect("exchange store");
        exchanges.begin_open(&request).expect("begin open");
        exchanges
            .commit_opened(
                &request.model_exchange_id,
                &request.open_digest,
                &opened("progress"),
            )
            .expect("commit open");
        for stage in [
            ProviderExchangeTerminalStage::Prepared,
            ProviderExchangeTerminalStage::CancelStarted,
            ProviderExchangeTerminalStage::Cancelled,
            ProviderExchangeTerminalStage::ReleaseStarted,
            ProviderExchangeTerminalStage::Released,
            ProviderExchangeTerminalStage::AdmissionStarted,
        ] {
            exchanges
                .record_terminal_progress(
                    &request.model_exchange_id,
                    &command_digest,
                    stage,
                    None,
                    None,
                    &at(3),
                )
                .expect("advance external-effect checkpoint");
        }
        for stage in [
            ProviderExchangeTerminalStage::AdmissionSettled,
            ProviderExchangeTerminalStage::SettlementStarted,
        ] {
            exchanges
                .record_terminal_progress(
                    &request.model_exchange_id,
                    &command_digest,
                    stage,
                    Some(admission),
                    None,
                    &at(4),
                )
                .expect("advance exact downstream checkpoint");
        }
        exchanges
            .record_terminal_progress(
                &request.model_exchange_id,
                &command_digest,
                ProviderExchangeTerminalStage::SettlementSettled,
                Some(admission),
                Some(receipt),
                &at(4),
            )
            .expect("persist final settlement receipt");
    }
    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    let mut exchanges = storage.provider_exchange_store().expect("exchange store");
    let progress = exchanges
        .load_terminal_progress(&request.model_exchange_id)
        .expect("load progress")
        .expect("terminal progress");
    assert_eq!(
        progress.stage,
        ProviderExchangeTerminalStage::SettlementSettled
    );
    assert_eq!(
        progress.admission_receipt_json(),
        Some(admission.as_slice())
    );
    assert_eq!(progress.terminal_receipt_json(), Some(receipt.as_slice()));
    assert_eq!(
        exchanges
            .record_terminal_progress(
                &request.model_exchange_id,
                &digest(b"changed-command"),
                ProviderExchangeTerminalStage::SettlementSettled,
                Some(admission),
                Some(receipt),
                &at(4),
            )
            .expect_err("changed command conflicts")
            .code(),
        ProviderExchangeStoreErrorCode::Conflict
    );
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

fn assert_restarted_single_authority(
    root: &Path,
    active: &ProviderExchangeBegin,
    failed: &ProviderExchangeBegin,
    forgotten_authority: &[u8],
    final_ack_receipt: &[u8],
) {
    let mut storage = SqliteStorage::open(root).expect("restart storage");
    let exchanges = storage.provider_exchange_store().expect("exchange store");
    assert_eq!(
        exchanges
            .load_pool_authority()
            .expect("load terminal authority")
            .expect("terminal authority")
            .state_json(),
        forgotten_authority
    );
    for (request, expected) in [
        (active, ProviderExchangeState::Terminal),
        (failed, ProviderExchangeState::Failed),
    ] {
        assert_eq!(
            exchanges
                .load(&request.model_exchange_id)
                .expect("load exchange")
                .expect("exchange")
                .state,
            expected
        );
    }
    let final_ack = exchanges
        .load_final_ack(&active.model_exchange_id)
        .expect("load final ack")
        .expect("final ack tombstone");
    assert_eq!(final_ack.ack_sequence, 4);
    assert_eq!(final_ack.receipt_json(), final_ack_receipt);
}

fn opened_json() -> &'static [u8] {
    br#"{"kind":"frozen-route","marker":"one"}"#
}

fn assert_database_files_exclude(root: &Path, needle: &[u8]) {
    for entry in fs::read_dir(root).expect("read fixture directory") {
        let path = entry.expect("directory entry").path();
        if path.is_file() {
            let bytes = fs::read(&path).expect("read database file");
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "raw model request reached {}",
                path.display()
            );
        }
    }
}
