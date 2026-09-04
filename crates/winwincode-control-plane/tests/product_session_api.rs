// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, ChatSubmitCommand, ChatSubmitCommandCommand, ChatSubmitPayload, ModelRoute, PageRequest,
    SessionCloseCommand, SessionCloseCommandCommand, SessionClosePayload, SessionCreateCommand,
    SessionCreateCommandCommand, SessionCreatePayload, SessionGetParameters, SessionGetQuery,
    SessionGetQueryQuery, SessionListParameters, SessionListQuery, SessionListQueryQuery,
    SessionMessagesListParameters, SessionMessagesListQuery, SessionMessagesListQueryQuery,
};
use winwincode_control_plane::{
    ProductSessionApiClock, ProductSessionApiService, ProductSessionExecutionConfig,
};
use winwincode_domain::{
    CredentialReferenceId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::SqliteStorage;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-product-session-api-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct AdvancingClock(u64);

impl ProductSessionApiClock for AdvancingClock {
    fn now(&mut self) -> Instant {
        let second = self.0;
        self.0 += 1;
        Instant(format!("2027-02-16T08:00:{second:02}.000Z"))
    }
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn execution_config() -> ProductSessionExecutionConfig {
    ProductSessionExecutionConfig::try_new(
        scope(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config")
}

fn page() -> PageRequest {
    PageRequest {
        cursor: None,
        limit: 20,
    }
}

fn create_command(session: u64, request: u64, title: &str) -> SessionCreateCommand {
    SessionCreateCommand {
        actor: actor(),
        command: SessionCreateCommandCommand::SessionCreate,
        expected_revision: Revision(0),
        payload: SessionCreatePayload {
            model_route: ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                model_id: "fixture-model".to_owned(),
                provider_id: "fixture-provider".to_owned(),
            },
            product_session_id: ProductSessionId(id("psn", session)),
            project_id: ProjectId(id("prj", 1)),
            repository_id: RepositoryId(id("rep", 1)),
            title: title.to_owned(),
        },
        request_id: RequestId(id("req", request)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(),
    }
}

fn assert_single_create_publication(directory: &TestDirectory) {
    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("outbox inspection");
    let replay_event_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE request_id = ?1",
            [id("req", 1)],
            |row| row.get(0),
        )
        .expect("stable create outbox count");
    assert_eq!(replay_event_count, 2);
}

#[test]
fn generated_product_session_commands_and_queries_share_one_durable_service() {
    let directory = TestDirectory::new();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let mut clock = AdvancingClock(1);
    let execution = execution_config();
    let mut api = ProductSessionApiService::new(&mut storage, &mut clock, &execution);

    let create = create_command(1, 1, "Generated API");
    let created = api.create(create.clone()).expect("session.create");
    assert_eq!(created.current_revision, Revision(1));
    assert_eq!(created.result.id, ProductSessionId(id("psn", 1)));
    let replayed = api.create(create).expect("stable create replay");
    assert_eq!(replayed, created);

    let submitted = api
        .submit_chat(ChatSubmitCommand {
            actor: actor(),
            command: ChatSubmitCommandCommand::ChatSubmit,
            expected_revision: Revision(1),
            payload: ChatSubmitPayload {
                message: "Persist this public message".to_owned(),
                product_session_id: ProductSessionId(id("psn", 1)),
            },
            request_id: RequestId(id("req", 2)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("chat.submit");
    assert_eq!(submitted.current_revision, Revision(2));

    let listed = api
        .list(SessionListQuery {
            actor: actor(),
            page: page(),
            parameters: SessionListParameters {
                states: vec!["running".to_owned()],
            },
            query: SessionListQueryQuery::SessionList,
            request_id: RequestId(id("req", 3)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("session.list");
    assert_eq!(listed.result.items.len(), 1);
    assert!(!listed.page.has_more);

    let fetched = api
        .get(SessionGetQuery {
            actor: actor(),
            page: page(),
            parameters: SessionGetParameters {
                product_session_id: ProductSessionId(id("psn", 1)),
            },
            query: SessionGetQueryQuery::SessionGet,
            request_id: RequestId(id("req", 4)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("session.get");
    assert_eq!(fetched.result.revision, Revision(2));

    let messages = api
        .messages(SessionMessagesListQuery {
            actor: actor(),
            page: page(),
            parameters: SessionMessagesListParameters {
                product_session_id: ProductSessionId(id("psn", 1)),
            },
            query: SessionMessagesListQueryQuery::SessionMessagesList,
            request_id: RequestId(id("req", 5)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("session.messages.list");
    assert_eq!(messages.result.items.len(), 1);
    assert_eq!(
        messages.result.items[0].content,
        "Persist this public message"
    );

    let second = api
        .create(create_command(2, 6, "Closable"))
        .expect("second session");
    let closed = api
        .close(SessionCloseCommand {
            actor: actor(),
            command: SessionCloseCommandCommand::SessionClose,
            expected_revision: second.current_revision,
            payload: SessionClosePayload {
                product_session_id: ProductSessionId(id("psn", 2)),
            },
            request_id: RequestId(id("req", 7)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("session.close");
    assert_eq!(closed.result.state, "closed");

    drop(api);
    drop(storage);
    assert_single_create_publication(&directory);
}
