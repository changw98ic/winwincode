use super::RefreshCredentialLock;
use anyhow::Result;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn acquisition_times_out_without_stealing() -> Result<()> {
    let codex_home = tempdir()?;
    let store_key = "test-store-key";
    let held_lock = RefreshCredentialLock::acquire_in(
        codex_home.path(),
        store_key,
        Duration::from_millis(/*millis*/ 100),
    )
    .await?;

    let error = RefreshCredentialLock::acquire_in(
        codex_home.path(),
        store_key,
        Duration::from_millis(/*millis*/ 50),
    )
    .await
    .err()
    .expect("contending lock acquisition should time out");
    assert!(
        error
            .to_string()
            .contains("timed out after 50ms waiting for OAuth refresh lock"),
        "unexpected error: {error:#}"
    );

    drop(held_lock);
    let _reacquired = RefreshCredentialLock::acquire_in(
        codex_home.path(),
        store_key,
        Duration::from_millis(/*millis*/ 100),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn unknown_refresh_is_fenced_after_restart_until_credential_rotation() -> Result<()> {
    let home = tempdir()?;
    let old = "a".repeat(64);
    let rotated = "b".repeat(64);
    let mut lock =
        RefreshCredentialLock::acquire_in(home.path(), "unknown", Duration::from_secs(1)).await?;
    lock.check_refresh_outcome(&old)?;
    lock.begin_refresh(&old)?;
    drop(lock);
    let mut reopened =
        RefreshCredentialLock::acquire_in(home.path(), "unknown", Duration::from_secs(1)).await?;
    assert!(reopened.check_refresh_outcome(&old).is_err());
    reopened.check_refresh_outcome(&rotated)?;
    reopened.begin_refresh(&rotated)?;
    reopened.clear_refresh_outcome()?;
    drop(reopened);
    let mut completed =
        RefreshCredentialLock::acquire_in(home.path(), "unknown", Duration::from_secs(1)).await?;
    completed.check_refresh_outcome(&rotated)?;
    Ok(())
}
