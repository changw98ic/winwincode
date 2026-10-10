// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn additive_migration_preserves_existing_thread_metadata_and_old_readers() {
    use std::borrow::Cow;
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home).await.unwrap();
    let sqlite = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let current = &crate::migrations::STATE_MIGRATOR;
    let old = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            current
                .migrations
                .iter()
                .filter(|m| m.version < 51)
                .cloned()
                .collect(),
        ),
        ignore_missing: true,
        locking: current.locking,
        no_tx: current.no_tx,
        table_name: current.table_name.clone(),
        create_schemas: current.create_schemas.clone(),
    };
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    old.run(&pool).await.unwrap();
    let thread = ThreadId::new();
    sqlx::query("INSERT INTO threads(id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES (?, 'rollout', 1, 1, 'cli', 'test', '/workspace', 'existing', 'read-only', 'never')")
        .bind(thread.to_string()).execute(&pool).await.unwrap();
    pool.close().await;
    let runtime = StateRuntime::init(sqlite.clone(), "test".into())
        .await
        .unwrap();
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    old.run(&pool).await.unwrap();
    let row = sqlx::query("SELECT title FROM threads WHERE id = ?")
        .bind(thread.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("title"), "existing");
    let request = ToolRequestIdentity {
        thread_id: thread.to_string(),
        logical_id: "call".into(),
        turn_id: "turn".into(),
        scope_id: "scope".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "test".into(),
        source: "model".into(),
        binding: "digest".into(),
    };
    assert!(matches!(
        runtime.observe_tool_request(&request).await.unwrap(),
        ToolRequestObservation::New(_)
    ));
    assert_eq!(
        runtime
            .list_tool_fact_events(&thread.to_string(), 0, 200)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DELETE FROM threads WHERE id = ?")
        .bind(thread.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        runtime
            .list_tool_fact_events(&thread.to_string(), 0, 200)
            .await
            .unwrap()
            .is_empty()
    );
}
