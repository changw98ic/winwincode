// SPDX-License-Identifier: Apache-2.0

//! Account credentials and immutable connections in the Device-private store.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    DeviceProviderConfig, DeviceProviderOpenCodeAccount, DeviceProviderOpenCodeAccountIssuer,
    DeviceProviderOpenCodeAccountState, DeviceProviderOpenCodeConnection,
    DeviceProviderOpenCodeSessionBinding, DeviceProviderOutcome,
};
use winwincode_domain::{OpenCodeAccountId, ProductSessionId, is_canonical_prefixed_id};

use crate::opencode_auth::{
    OPENCODE_CLIENT_ID, OPENCODE_ISSUER, OpenCodeAuthError, OpenCodeOAuth, OpenCodeOrganization,
    OpenCodeTokenGrant, OpenCodeUser,
};
use crate::opencode_route::{OpenCodeGoRoute, valid_text};

#[cfg(test)]
#[path = "opencode_store_tests.rs"]
mod tests;
use crate::{DeviceProviderError, DeviceProviderStore, ResolvedSecret};

/// Bounded failures used by the model lane and account controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeCredentialError {
    Unavailable,
    ReauthorizationRequired,
    ConnectionChanged,
    Cancelled,
}

impl From<DeviceProviderError> for OpenCodeCredentialError {
    fn from(_: DeviceProviderError) -> Self {
        Self::Unavailable
    }
}
impl From<rusqlite::Error> for OpenCodeCredentialError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Unavailable
    }
}
impl From<serde_json::Error> for OpenCodeCredentialError {
    fn from(_: serde_json::Error) -> Self {
        Self::Unavailable
    }
}

struct AccountSecrets {
    access: ResolvedSecret,
    refresh: ResolvedSecret,
    expires_at_ms: i64,
    version: i64,
    state: String,
}

impl DeviceProviderStore {
    pub(crate) fn guard_opencode_projection(
        &self,
        snapshot: &winwincode_api::generated::DeviceProviderSnapshot,
    ) -> Result<(), DeviceProviderError> {
        let mut gate = crate::CredentialLeakGate::new();
        if !crate::valid_opencode_projection(snapshot) {
            return Err(DeviceProviderError);
        }
        let mut query = self.connection.prepare("SELECT access_token FROM opencode_accounts UNION ALL SELECT refresh_token FROM opencode_accounts UNION ALL SELECT device_code FROM opencode_logins")?;
        for bytes in query.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
            let bytes = bytes?;
            if bytes.is_empty() {
                continue;
            }
            let secret = ResolvedSecret::from_bytes(bytes).map_err(|_| DeviceProviderError)?;
            gate.track_secret(&secret);
        }
        gate.inspect_serializable(crate::CredentialOutputBoundary::WebSocket, snapshot)
            .map_err(|_| DeviceProviderError)
    }
    pub(crate) fn edit_opencode_connection(
        &self,
        config: &DeviceProviderConfig,
    ) -> Result<DeviceProviderOutcome, DeviceProviderError> {
        let Ok(original) = self.provider_config(&config.provider_id) else {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        };
        if config.endpoint != original.endpoint
            || config.protocol != original.protocol
            || config.model_ids != original.model_ids
        {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        }
        self.connection.execute(
            "UPDATE providers SET config=?1 WHERE provider_id=?2",
            params![serde_json::to_string(config)?, config.provider_id],
        )?;
        self.connection.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        Ok(DeviceProviderOutcome::Saved)
    }
    /// Saves a verified upstream identity. Reauthorization updates the same account.
    ///
    /// # Errors
    /// Rejects invalid identities, unsafe tokens, full storage, and lock failures.
    pub fn save_opencode_account(
        &self,
        user: &OpenCodeUser,
        grant: OpenCodeTokenGrant,
        can_start: impl Fn() -> bool,
    ) -> Result<OpenCodeAccountId, OpenCodeCredentialError> {
        if !valid_text(&user.id, 200) || !valid_text(&user.email, 320) {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        let account = account_id(&user.id);
        let _lock = self.opencode_lock(&account.0, &can_start)?;
        let expires_at_ms = token_expiry(&grant)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM opencode_accounts WHERE account_ref<>?1",
            [&account.0],
            |row| row.get(0),
        )?;
        if count >= 100 || !can_start() {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        transaction.execute(
            "INSERT INTO opencode_accounts (account_ref,issuer,subject,client_id,email,access_token,refresh_token,expires_at_ms,credential_version,state)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1,'authorized')
             ON CONFLICT(issuer,subject) DO UPDATE SET email=excluded.email,access_token=excluded.access_token,
             refresh_token=excluded.refresh_token,expires_at_ms=excluded.expires_at_ms,
             credential_version=credential_version+1,state='authorized'",
            params![account.0,OPENCODE_ISSUER,user.id,OPENCODE_CLIENT_ID,user.email,
                grant.access_token.expose(),grant.refresh_token.expose(),expires_at_ms],
        )?;
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        drop(grant);
        Ok(account)
    }

    pub(crate) fn opencode_accounts(
        &self,
    ) -> Result<Vec<DeviceProviderOpenCodeAccount>, DeviceProviderError> {
        let mut query = self.connection.prepare(
            "SELECT account_ref,issuer,subject,email,state,credential_version,usage FROM opencode_accounts ORDER BY account_ref",
        )?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, issuer, subject, email, state, version, usage) = row?;
            if !is_canonical_prefixed_id(&id, "oca_")
                || issuer != OPENCODE_ISSUER
                || !valid_text(&subject, 200)
                || !valid_text(&email, 320)
                || version < 1
            {
                return Err(DeviceProviderError);
            }
            Ok(DeviceProviderOpenCodeAccount {
                account_ref: OpenCodeAccountId(id),
                issuer: DeviceProviderOpenCodeAccountIssuer::HttpsOpencodeAiConsole,
                subject,
                email,
                state: state_projection(&state)?,
                credential_version: version,
                usage: usage
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
            })
        })
        .collect()
    }

    pub(crate) fn opencode_connection(
        &self,
        provider_id: &str,
    ) -> Result<Option<DeviceProviderOpenCodeConnection>, DeviceProviderError> {
        let row: Option<(String,String,String)> = self.connection.query_row(
            "SELECT account_ref,organization_id,organization_name FROM opencode_connections WHERE provider_id=?1",
            [provider_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        row.map(|(account, organization_id, organization_name)| {
            if !is_canonical_prefixed_id(&account, "oca_")
                || !valid_text(&organization_id, 200)
                || !valid_text(&organization_name, 200)
            {
                return Err(DeviceProviderError);
            }
            Ok(DeviceProviderOpenCodeConnection {
                account_ref: OpenCodeAccountId(account),
                organization_id,
                organization_name,
            })
        })
        .transpose()
    }

    /// Creates a new connection. Its account and organization can never be edited in place.
    ///
    /// # Errors
    /// Rejects existing provider IDs, unavailable accounts and unverified Go config.
    pub fn connect_opencode(
        &self,
        account: &OpenCodeAccountId,
        organization: &OpenCodeOrganization,
        configuration: &serde_json::Value,
        provider_id: String,
        display_name: String,
    ) -> Result<(), DeviceProviderError> {
        self.connect_opencode_model(
            account,
            organization,
            configuration,
            provider_id,
            display_name,
            None,
        )
    }

    /// Creates an immutable connection using the selected authenticated Go model.
    ///
    /// # Errors
    /// Rejects an unknown model, foreign endpoint, existing connection or unavailable account.
    pub fn connect_opencode_model(
        &self,
        account: &OpenCodeAccountId,
        organization: &OpenCodeOrganization,
        configuration: &serde_json::Value,
        provider_id: String,
        display_name: String,
        model_id: Option<&str>,
    ) -> Result<(), DeviceProviderError> {
        let route = OpenCodeGoRoute::from_configuration_for_model(
            configuration,
            &organization.id,
            model_id,
        )?;
        let config = route.config(provider_id, display_name)?;
        if !valid_text(&organization.name, 200) {
            return Err(DeviceProviderError);
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let authorized: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM opencode_accounts WHERE account_ref=?1 AND state='authorized')", [&account.0], |row| row.get(0),
        )?;
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM providers", [], |row| row.get(0))?;
        if !authorized || count >= 100 {
            return Err(DeviceProviderError);
        }
        transaction.execute(
            "INSERT INTO providers VALUES (?1,?2,X'')",
            params![config.provider_id, serde_json::to_string(&config)?],
        )?;
        transaction.execute(
            "INSERT INTO opencode_connections VALUES (?1,?2,?3,?4)",
            params![
                config.provider_id,
                account.0,
                organization.id,
                organization.name
            ],
        )?;
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Resolves fresh credentials under an account-wide OS lock.
    ///
    /// # Errors
    /// An uncertain refresh requires reauthorization. It is never retried automatically.
    pub fn opencode_access(
        &self,
        account: &OpenCodeAccountId,
        can_start: impl Fn() -> bool,
    ) -> Result<ResolvedSecret, OpenCodeCredentialError> {
        self.opencode_access_with(account, &can_start, |refresh| {
            OpenCodeOAuth::new().refresh(refresh)
        })
    }

    fn opencode_access_with(
        &self,
        account: &OpenCodeAccountId,
        can_start: &impl Fn() -> bool,
        refresh: impl FnOnce(&ResolvedSecret) -> Result<OpenCodeTokenGrant, OpenCodeAuthError>,
    ) -> Result<ResolvedSecret, OpenCodeCredentialError> {
        let _lock = self.opencode_lock(&account.0, can_start)?;
        let row = self.read_account_secrets(account)?;
        if row.state == "refresh_in_flight" {
            self.invalidate_account(account)?;
            return Err(OpenCodeCredentialError::ReauthorizationRequired);
        }
        if row.state != "authorized" {
            return Err(OpenCodeCredentialError::ReauthorizationRequired);
        }
        if row.expires_at_ms > now_ms()?.saturating_add(60_000) {
            return Ok(row.access);
        }
        if !can_start() {
            return Err(OpenCodeCredentialError::Cancelled);
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE opencode_accounts SET state='refresh_in_flight' WHERE account_ref=?1 AND credential_version=?2 AND state='authorized'",
            params![account.0,row.version],
        )?;
        if changed != 1 {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        // The FULL synchronous autocommit above is durable before HTTP. No SQLite transaction spans HTTP.
        let Ok(grant) = refresh(&row.refresh) else {
            self.invalidate_account(account)?;
            return Err(OpenCodeCredentialError::ReauthorizationRequired);
        };
        let expires_at_ms = match token_expiry(&grant) {
            Ok(value) => value,
            Err(error) => {
                self.invalidate_account(account)?;
                return Err(error);
            }
        };
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE opencode_accounts SET access_token=?1,refresh_token=?2,expires_at_ms=?3,
             credential_version=credential_version+1,state='authorized' WHERE account_ref=?4 AND credential_version=?5 AND state='refresh_in_flight'",
            params![grant.access_token.expose(),grant.refresh_token.expose(),expires_at_ms,account.0,row.version],
        )?;
        if changed != 1 {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        if !can_start() {
            return Err(OpenCodeCredentialError::Cancelled);
        }
        Ok(grant.access_token)
    }

    fn read_account_secrets(
        &self,
        account: &OpenCodeAccountId,
    ) -> Result<AccountSecrets, OpenCodeCredentialError> {
        let (access,refresh,expires_at_ms,version,state,issuer,client): (Vec<u8>,Vec<u8>,i64,i64,String,String,String) =
            self.connection.query_row("SELECT access_token,refresh_token,expires_at_ms,credential_version,state,issuer,client_id FROM opencode_accounts WHERE account_ref=?1",
                [&account.0], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)))?;
        if issuer != OPENCODE_ISSUER || client != OPENCODE_CLIENT_ID {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        if !matches!(state.as_str(), "authorized" | "refresh_in_flight") {
            return Err(OpenCodeCredentialError::ReauthorizationRequired);
        }
        Ok(AccountSecrets {
            access: ResolvedSecret::from_bytes(access)
                .map_err(|_| OpenCodeCredentialError::Unavailable)?,
            refresh: ResolvedSecret::from_bytes(refresh)
                .map_err(|_| OpenCodeCredentialError::Unavailable)?,
            expires_at_ms,
            version,
            state,
        })
    }

    fn invalidate_account(
        &self,
        account: &OpenCodeAccountId,
    ) -> Result<(), OpenCodeCredentialError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute("UPDATE opencode_accounts SET state='reauthorization_required',access_token=X'',refresh_token=X'' WHERE account_ref=?1",[&account.0])?;
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn reject_opencode_access(
        &self,
        account: &OpenCodeAccountId,
        rejected: &ResolvedSecret,
    ) -> Result<(), OpenCodeCredentialError> {
        let _lock = self.opencode_lock(&account.0, &|| true)?;
        if let Ok(current) = self.read_account_secrets(account) {
            // A late 401 for an old token must not revoke a newer authorization.
            if current.access.expose() == rejected.expose() {
                self.invalidate_account(account)?;
            }
        }
        Ok(())
    }

    /// Removes credentials while retaining account identity and its connections.
    ///
    /// # Errors
    /// Uses the same account lock as authorization and refresh.
    pub fn logout_opencode(
        &self,
        account: &OpenCodeAccountId,
    ) -> Result<(), OpenCodeCredentialError> {
        let _lock = self.opencode_lock(&account.0, &|| true)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute("UPDATE opencode_accounts SET state='logged_out',access_token=X'',refresh_token=X'',credential_version=credential_version+1 WHERE account_ref=?1",[&account.0])?;
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn bind_opencode_session(
        &self,
        product_session: &ProductSessionId,
        provider_id: &str,
    ) -> Result<Option<DeviceProviderOpenCodeSessionBinding>, OpenCodeCredentialError> {
        let connection = self.opencode_connection(provider_id)?;
        let prior: Option<String> = self.connection.query_row(
            "SELECT binding FROM opencode_session_bindings WHERE product_session_id=?1 AND provider_id=?2",
            params![product_session.0,provider_id], |row|row.get(0),
        ).optional()?;
        let Some(connection) = connection else {
            return if prior.is_some() {
                Err(OpenCodeCredentialError::ConnectionChanged)
            } else {
                Ok(None)
            };
        };
        if !is_canonical_prefixed_id(&product_session.0, "psn_") {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        let binding = DeviceProviderOpenCodeSessionBinding {
            product_session_id: product_session.clone(),
            provider_id: provider_id.to_owned(),
            account_ref: connection.account_ref,
            organization_id: connection.organization_id,
            conversation_id: format!(
                "wwc-{}",
                &format!(
                    "{:x}",
                    Sha256::digest(format!("{}\n{provider_id}", product_session.0).as_bytes())
                )[..32]
            ),
        };
        self.connection.execute(
            "INSERT OR IGNORE INTO opencode_session_bindings VALUES (?1,?2,?3)",
            params![
                product_session.0,
                provider_id,
                serde_json::to_string(&binding)?
            ],
        )?;
        let stored: String = self.connection.query_row("SELECT binding FROM opencode_session_bindings WHERE product_session_id=?1 AND provider_id=?2",
            params![product_session.0,provider_id],|row|row.get(0))?;
        let stored: DeviceProviderOpenCodeSessionBinding = serde_json::from_str(&stored)?;
        if stored != binding {
            return Err(OpenCodeCredentialError::ConnectionChanged);
        }
        Ok(Some(stored))
    }

    pub(crate) fn opencode_model_headers(
        &self,
        binding: &DeviceProviderOpenCodeSessionBinding,
    ) -> Result<BTreeMap<String, String>, DeviceProviderError> {
        let config: String = self.connection.query_row(
            "SELECT config FROM providers WHERE provider_id=?1",
            [&binding.provider_id],
            |row| row.get(0),
        )?;
        let config: DeviceProviderConfig = serde_json::from_str(&config)?;
        let connection = self
            .opencode_connection(&binding.provider_id)?
            .ok_or(DeviceProviderError)?;
        if connection.account_ref != binding.account_ref
            || connection.organization_id != binding.organization_id
            || !config.enabled
            || !winwincode_api::opencode::valid_opencode_provider_route(&config)
        {
            return Err(DeviceProviderError);
        }
        let authorized: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM opencode_accounts WHERE account_ref=?1 AND state='authorized')", [&binding.account_ref.0], |row| row.get(0))?;
        if !authorized {
            return Err(DeviceProviderError);
        }
        Ok(BTreeMap::from([
            ("x-opencode-org-id".into(), binding.organization_id.clone()),
            ("x-opencode-session".into(), binding.conversation_id.clone()),
            (
                "User-Agent".into(),
                concat!("winwincode/", env!("CARGO_PKG_VERSION")).into(),
            ),
        ]))
    }

    pub(crate) fn opencode_lock(
        &self,
        id: &str,
        can_start: &impl Fn() -> bool,
    ) -> Result<File, OpenCodeCredentialError> {
        if !is_canonical_prefixed_id(id, "oca_") && !is_canonical_prefixed_id(id, "ocl_") {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        let directory = Path::new(
            self.connection
                .path()
                .ok_or(OpenCodeCredentialError::Unavailable)?,
        )
        .parent()
        .ok_or(OpenCodeCredentialError::Unavailable)?
        .join("oauth-locks");
        private_directory(&directory)?;
        let path = directory.join(format!("{id}.lock"));
        if let Ok(meta) = fs::symlink_metadata(&path)
            && (!meta.is_file() || meta.permissions().mode() & 0o077 != 0)
        {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|_| OpenCodeCredentialError::Unavailable)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| OpenCodeCredentialError::Unavailable)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(OpenCodeCredentialError::Unavailable);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if !can_start() {
                return Err(OpenCodeCredentialError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(OpenCodeCredentialError::Unavailable);
            }
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(25)),
                Err(TryLockError::Error(_)) => return Err(OpenCodeCredentialError::Unavailable),
            }
        }
    }
}

fn private_directory(path: &Path) -> Result<(), OpenCodeCredentialError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(OpenCodeCredentialError::Unavailable),
    }
    let meta = fs::symlink_metadata(path).map_err(|_| OpenCodeCredentialError::Unavailable)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err(OpenCodeCredentialError::Unavailable);
    }
    Ok(())
}

pub(crate) fn now_ms() -> Result<i64, OpenCodeCredentialError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .ok_or(OpenCodeCredentialError::Unavailable)
}

fn account_id(subject: &str) -> OpenCodeAccountId {
    OpenCodeAccountId(format!(
        "oca_{}",
        &format!(
            "{:X}",
            Sha256::digest(format!("{OPENCODE_ISSUER}\n{subject}").as_bytes())
        )[..26]
    ))
}

fn token_expiry(grant: &OpenCodeTokenGrant) -> Result<i64, OpenCodeCredentialError> {
    for token in [&grant.access_token, &grant.refresh_token] {
        if token.expose().is_empty()
            || token.expose().len() > 8192
            || std::str::from_utf8(token.expose())
                .ok()
                .is_none_or(|text| text.chars().any(char::is_control))
        {
            return Err(OpenCodeCredentialError::Unavailable);
        }
    }
    let lifetime = i64::try_from(grant.expires_in.as_millis())
        .map_err(|_| OpenCodeCredentialError::Unavailable)?;
    if lifetime <= 0 || lifetime > 31_536_000_000 {
        return Err(OpenCodeCredentialError::Unavailable);
    }
    now_ms()?
        .checked_add(lifetime)
        .ok_or(OpenCodeCredentialError::Unavailable)
}

fn state_projection(
    state: &str,
) -> Result<DeviceProviderOpenCodeAccountState, DeviceProviderError> {
    match state {
        "authorized" => Ok(DeviceProviderOpenCodeAccountState::Authorized),
        "refresh_in_flight" => Ok(DeviceProviderOpenCodeAccountState::RefreshInFlight),
        "reauthorization_required" => {
            Ok(DeviceProviderOpenCodeAccountState::ReauthorizationRequired)
        }
        "logged_out" => Ok(DeviceProviderOpenCodeAccountState::LoggedOut),
        _ => Err(DeviceProviderError),
    }
}
