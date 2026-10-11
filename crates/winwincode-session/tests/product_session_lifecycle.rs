// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{Instant, ProductSessionId, ProjectId, RepositoryId};
use winwincode_session::{
    ProductSession, ProductSessionCreate, ProductSessionError, ProductSessionState,
};

const SESSION_ID: &str = "psn_00000000000000000000000001";
const PROJECT_ID: &str = "prj_00000000000000000000000001";
const REPOSITORY_ID: &str = "rep_00000000000000000000000001";

fn create() -> ProductSession {
    ProductSession::create(ProductSessionCreate {
        product_session_id: ProductSessionId(SESSION_ID.to_owned()),
        project_id: ProjectId(PROJECT_ID.to_owned()),
        repository_id: RepositoryId(REPOSITORY_ID.to_owned()),
        title: "A product session".to_owned(),
        now: Instant("2026-01-01T00:00:00.000Z".to_owned()),
    })
    .expect("valid ProductSession")
}

#[test]
fn create_starts_an_idle_product_session_at_revision_one() {
    let session = create();

    assert_eq!(session.id().0, SESSION_ID);
    assert_eq!(session.project_id().0, PROJECT_ID);
    assert_eq!(session.repository_id().0, REPOSITORY_ID);
    assert_eq!(session.title(), "A product session");
    assert_eq!(session.state(), ProductSessionState::Idle);
    assert_eq!(session.revision(), 1);
    assert_eq!(
        session.updated_at(),
        &Instant("2026-01-01T00:00:00.000Z".to_owned())
    );
}

#[test]
fn lifecycle_transitions_keep_one_monotonic_revision_per_change() {
    let mut session = create();

    session
        .begin_turn(Instant("2026-01-01T00:00:01.000Z".to_owned()))
        .expect("idle session starts a turn");
    assert_eq!(session.state(), ProductSessionState::Running);
    assert_eq!(session.revision(), 2);

    session
        .wait_for_input(Instant("2026-01-01T00:00:02.000Z".to_owned()))
        .expect("running session waits for input");
    assert_eq!(session.state(), ProductSessionState::WaitingForInput);
    assert_eq!(session.revision(), 3);

    session
        .resume(Instant("2026-01-01T00:00:03.000Z".to_owned()))
        .expect("input response resumes the turn");
    assert_eq!(session.state(), ProductSessionState::Running);
    assert_eq!(session.revision(), 4);

    session
        .complete_turn(Instant("2026-01-01T00:00:04.000Z".to_owned()))
        .expect("running turn completes to idle");
    assert_eq!(session.state(), ProductSessionState::Idle);
    assert_eq!(session.revision(), 5);

    session
        .cancel(
            "user requested cancellation",
            Instant("2026-01-01T00:00:05.000Z".to_owned()),
        )
        .expect("idle session can be cancelled");
    assert_eq!(session.state(), ProductSessionState::Cancelled);
    assert_eq!(session.revision(), 6);

    session
        .close(Instant("2026-01-01T00:00:06.000Z".to_owned()))
        .expect("cancelled session can be closed");
    assert_eq!(session.state(), ProductSessionState::Closed);
    assert_eq!(session.revision(), 7);
}

#[test]
fn approval_wait_and_failure_are_distinct_terminal_paths() {
    let mut session = create();
    session
        .begin_turn(Instant("2026-01-01T00:00:01.000Z".to_owned()))
        .expect("start turn");
    session
        .wait_for_approval(Instant("2026-01-01T00:00:02.000Z".to_owned()))
        .expect("request approval");
    assert_eq!(session.state(), ProductSessionState::WaitingForApproval);

    session
        .resume(Instant("2026-01-01T00:00:03.000Z".to_owned()))
        .expect("approval decision resumes turn");
    session
        .fail(
            "model failed",
            Instant("2026-01-01T00:00:04.000Z".to_owned()),
        )
        .expect("turn can fail");
    assert_eq!(session.state(), ProductSessionState::Failed);
    assert_eq!(session.revision(), 5);
}

#[test]
fn illegal_transition_leaves_the_snapshot_and_revision_unchanged() {
    let mut session = create();
    let before = session.clone();

    let error = session
        .complete_turn(Instant("2026-01-01T00:00:01.000Z".to_owned()))
        .expect_err("idle session cannot complete a running turn");

    assert_eq!(
        error,
        ProductSessionError::InvalidTransition {
            from: ProductSessionState::Idle,
            operation: "complete_turn",
        }
    );
    assert_eq!(session, before);
}

#[test]
fn closed_sessions_are_immutable_and_close_is_idempotent() {
    let mut session = create();
    session
        .close(Instant("2026-01-01T00:00:01.000Z".to_owned()))
        .expect("idle session closes");
    let revision = session.revision();
    session
        .close(Instant("2026-01-01T00:00:02.000Z".to_owned()))
        .expect("replayed close is idempotent");
    assert_eq!(session.revision(), revision);

    let error = session
        .begin_turn(Instant("2026-01-01T00:00:03.000Z".to_owned()))
        .expect_err("closed session cannot start a turn");
    assert_eq!(
        error,
        ProductSessionError::InvalidTransition {
            from: ProductSessionState::Closed,
            operation: "begin_turn",
        }
    );
}

#[test]
fn create_rejects_noncanonical_identity_and_empty_title() {
    let mut input = ProductSessionCreate {
        product_session_id: ProductSessionId("session-1".to_owned()),
        project_id: ProjectId(PROJECT_ID.to_owned()),
        repository_id: RepositoryId(REPOSITORY_ID.to_owned()),
        title: "A product session".to_owned(),
        now: Instant("2026-01-01T00:00:00.000Z".to_owned()),
    };
    assert_eq!(
        ProductSession::create(input.clone()).expect_err("invalid session id"),
        ProductSessionError::InvalidIdentity("productSessionId")
    );

    input.product_session_id = ProductSessionId(SESSION_ID.to_owned());
    input.title.clear();
    assert_eq!(
        ProductSession::create(input).expect_err("empty title"),
        ProductSessionError::InvalidTitle
    );
}
