// SPDX-License-Identifier: Apache-2.0

//! Device-owned authorization controls with durable command and grant state.

use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    DeviceConfigurationEnvelope, DeviceProviderOpenCodeCommand, DeviceProviderOpenCodeLogin,
    DeviceProviderOpenCodeLoginState as LoginState, DeviceProviderOpenCodeOperation as Operation,
    DeviceProviderOpenCodeOrganization, DeviceProviderOpenCodeUsage,
    DeviceProviderOpenCodeUsageWindow, DeviceProviderOutcome, DeviceProviderReceipt,
};
use winwincode_domain::{OpenCodeAccountId, OpenCodeLoginId, is_canonical_prefixed_id};

use crate::opencode_auth::{
    OpenCodeAuthError, OpenCodeOAuth, OpenCodeOrganization, OpenCodePollResult,
};
use crate::opencode_route::valid_text;
use crate::opencode_store::now_ms;
use crate::{DeviceProviderError, DeviceProviderStore, ResolvedSecret};

const CONTEXT: &str = "winwincode.device-provider.v1";

#[cfg(test)]
#[path = "opencode_control_tests.rs"]
mod tests;

impl DeviceProviderStore {
    pub(crate) fn opencode_command(
        &self,
        envelope: &DeviceConfigurationEnvelope,
    ) -> Option<DeviceProviderOpenCodeCommand> {
        self.decrypt_configuration(envelope, CONTEXT).ok()
    }

    pub(crate) fn apply_opencode_command(
        &self,
        envelope: &DeviceConfigurationEnvelope,
        command: DeviceProviderOpenCodeCommand,
    ) -> Result<DeviceProviderReceipt, DeviceProviderError> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(envelope)?));
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = transaction
            .query_row(
                "SELECT digest,receipt FROM receipts WHERE request_id=?1",
                [&envelope.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored, receipt)) = previous {
            if stored != digest {
                return Err(DeviceProviderError);
            }
            return Ok(serde_json::from_str(&receipt)?);
        }
        let mut receipt = DeviceProviderReceipt {
            request_id: envelope.request_id.clone(),
            revision: self.revision()?,
            outcome: if envelope.expected_revision != self.revision()? {
                DeviceProviderOutcome::RevisionConflict
            } else if !valid_command(&command) {
                DeviceProviderOutcome::InvalidRequest
            } else {
                DeviceProviderOutcome::Interrupted
            },
        };
        transaction.execute(
            "INSERT INTO receipts VALUES(?1,?2,?3)",
            params![
                envelope.request_id,
                digest,
                serde_json::to_string(&receipt)?
            ],
        )?;
        transaction.commit()?;
        if receipt.outcome != DeviceProviderOutcome::Interrupted {
            return Ok(receipt);
        }
        // A recovered interrupted command returns its receipt. It does not repeat its HTTP effect.
        receipt.outcome = match self.execute_opencode_command(&envelope.request_id, command) {
            Ok(()) => DeviceProviderOutcome::Saved,
            Err(_) => DeviceProviderOutcome::ProviderUnavailable,
        };
        receipt.revision = self.revision()?;
        self.connection.execute(
            "UPDATE receipts SET receipt=?1 WHERE request_id=?2",
            params![serde_json::to_string(&receipt)?, envelope.request_id],
        )?;
        Ok(receipt)
    }

    fn execute_opencode_command(
        &self,
        request_id: &str,
        command: DeviceProviderOpenCodeCommand,
    ) -> Result<(), DeviceProviderError> {
        let oauth = OpenCodeOAuth::new();
        match command.operation {
            Operation::BeginOpencodeLogin => self.begin_opencode_login(request_id, &oauth),
            Operation::PollOpencodeLogin => {
                self.poll_opencode_login(&command.login_id.ok_or(DeviceProviderError)?, &oauth)
            }
            Operation::CancelOpencodeLogin => {
                let id = command.login_id.ok_or(DeviceProviderError)?;
                let _lock = self
                    .opencode_lock(&id.0, &|| true)
                    .map_err(|_| DeviceProviderError)?;
                let (mut login, _, _, _) = self.read_login(&id)?;
                login.state = LoginState::Cancelled;
                login.user_code = None;
                login.verification_uri = None;
                self.write_login(&login, &[], 0, false)
            }
            Operation::ConnectOpencode => {
                let id = command.login_id.ok_or(DeviceProviderError)?;
                let _lock = self
                    .opencode_lock(&id.0, &|| true)
                    .map_err(|_| DeviceProviderError)?;
                let (mut login, _, _, _) = self.read_login(&id)?;
                if !matches!(login.state, LoginState::Authorized | LoginState::Completed) {
                    return Err(DeviceProviderError);
                }
                let org_id = command.organization_id.ok_or(DeviceProviderError)?;
                let org = login
                    .organizations
                    .iter()
                    .find(|org| org.id == org_id)
                    .ok_or(DeviceProviderError)?;
                let account = login.account_ref.as_ref().ok_or(DeviceProviderError)?;
                let access = self
                    .opencode_access(account, || true)
                    .map_err(|_| DeviceProviderError)?;
                let config = oauth
                    .configuration(&access, &org.id)
                    .map_err(|_| DeviceProviderError)?;
                self.connect_opencode_model(
                    account,
                    &OpenCodeOrganization {
                        id: org.id.clone(),
                        name: org.name.clone(),
                    },
                    &config,
                    command.provider_id.ok_or(DeviceProviderError)?,
                    command.display_name.ok_or(DeviceProviderError)?,
                    command.model_id.as_deref(),
                )?;
                login.state = LoginState::Completed;
                self.write_login(&login, &[], 0, false)
            }
            Operation::LogoutOpencode => self
                .logout_opencode(&command.account_ref.ok_or(DeviceProviderError)?)
                .map_err(|_| DeviceProviderError),
            Operation::OpencodeUsage => {
                let account = command.account_ref.ok_or(DeviceProviderError)?;
                let org = command.organization_id.ok_or(DeviceProviderError)?;
                self.update_opencode_usage(&oauth, &account, &org)
            }
            Operation::SetDefaultProvider => {
                let provider = command.provider_id.ok_or(DeviceProviderError)?;
                if !self.provider_config(&provider)?.enabled {
                    return Err(DeviceProviderError);
                }
                self.connection.execute("INSERT INTO provider_defaults VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET provider_id=excluded.provider_id",[provider])?;
                self.connection.execute(
                    "UPDATE identity SET revision=revision+1 WHERE singleton=1",
                    [],
                )?;
                Ok(())
            }
        }
    }

    fn begin_opencode_login(
        &self,
        request_id: &str,
        oauth: &OpenCodeOAuth,
    ) -> Result<(), DeviceProviderError> {
        let id = OpenCodeLoginId(format!(
            "ocl_{}",
            &format!("{:X}", Sha256::digest(request_id.as_bytes()))[..26]
        ));
        let _lock = self
            .opencode_lock(&id.0, &|| true)
            .map_err(|_| DeviceProviderError)?;
        // A reserved lock serializes capacity checks across concurrent grant creation.
        let _capacity_lock = self
            .opencode_lock("ocl_00000000000000000000000000", &|| true)
            .map_err(|_| DeviceProviderError)?;
        let count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM opencode_logins", [], |row| row.get(0))?;
        if count >= 100 {
            return Err(DeviceProviderError);
        }
        let (uri, code, private, expires, interval) = oauth
            .begin()
            .map_err(|_| DeviceProviderError)?
            .into_private_parts();
        let now = now_ms().map_err(|_| DeviceProviderError)?;
        let interval = i64::try_from(interval.as_millis()).map_err(|_| DeviceProviderError)?;
        let login = DeviceProviderOpenCodeLogin {
            login_id: id,
            state: LoginState::Pending,
            verification_uri: Some(uri),
            user_code: Some(code),
            expires_at_ms: now
                .checked_add(i64::try_from(expires.as_millis()).map_err(|_| DeviceProviderError)?)
                .ok_or(DeviceProviderError)?,
            poll_after_ms: now + interval,
            account_ref: None,
            organizations: Vec::new(),
        };
        self.write_login(&login, private.expose(), interval, false)
    }

    fn poll_opencode_login(
        &self,
        id: &OpenCodeLoginId,
        oauth: &OpenCodeOAuth,
    ) -> Result<(), DeviceProviderError> {
        self.poll_opencode_login_with(
            id,
            |private| oauth.poll_code(private),
            |access| Ok((oauth.user(access)?, oauth.organizations(access)?)),
        )
    }

    fn poll_opencode_login_with(
        &self,
        id: &OpenCodeLoginId,
        poll: impl FnOnce(&ResolvedSecret) -> Result<OpenCodePollResult, OpenCodeAuthError>,
        identity: impl FnOnce(
            &ResolvedSecret,
        ) -> Result<
            (
                crate::opencode_auth::OpenCodeUser,
                Vec<OpenCodeOrganization>,
            ),
            OpenCodeAuthError,
        >,
    ) -> Result<(), DeviceProviderError> {
        let _lock = self
            .opencode_lock(&id.0, &|| true)
            .map_err(|_| DeviceProviderError)?;
        let (mut login, private, mut interval, in_flight) = self.read_login(id)?;
        if login.state != LoginState::Pending {
            return Ok(());
        }
        let now = now_ms().map_err(|_| DeviceProviderError)?;
        if now >= login.expires_at_ms {
            login.state = LoginState::Expired;
            return self.end_login(login);
        }
        if in_flight {
            login.state = LoginState::Failed;
            return self.end_login(login);
        }
        if now < login.poll_after_ms {
            return Ok(());
        }
        let private = private.ok_or(DeviceProviderError)?;
        self.write_login(&login, private.expose(), interval, true)?;
        let outcome = poll(&private);
        if now_ms().map_err(|_| DeviceProviderError)? >= login.expires_at_ms {
            login.state = LoginState::Expired;
            return self.end_login(login);
        }
        match outcome {
            Ok(OpenCodePollResult::Pending | OpenCodePollResult::SlowDown) => {
                if matches!(outcome, Ok(OpenCodePollResult::SlowDown)) {
                    interval = interval.saturating_add(5000);
                }
                login.poll_after_ms = now_ms()
                    .map_err(|_| DeviceProviderError)?
                    .saturating_add(interval);
                self.write_login(&login, private.expose(), interval, false)
            }
            Ok(OpenCodePollResult::Authorized(token)) => {
                let Ok((user, organizations)) = identity(&token.access_token) else {
                    login.state = LoginState::Failed;
                    return self.end_login(login);
                };
                let account = self
                    .save_opencode_account(&user, token, || true)
                    .map_err(|_| DeviceProviderError)?;
                login.account_ref = Some(account);
                login.organizations = organizations
                    .into_iter()
                    .map(|org| DeviceProviderOpenCodeOrganization {
                        id: org.id,
                        name: org.name,
                    })
                    .collect();
                login.state = LoginState::Authorized;
                self.end_login(login)
            }
            Err(error) => {
                login.state = match error {
                    OpenCodeAuthError::Denied => LoginState::Denied,
                    OpenCodeAuthError::Expired => LoginState::Expired,
                    OpenCodeAuthError::Cancelled => LoginState::Cancelled,
                    _ => LoginState::Failed,
                };
                self.end_login(login)
            }
        }
    }

    fn end_login(&self, mut login: DeviceProviderOpenCodeLogin) -> Result<(), DeviceProviderError> {
        login.user_code = None;
        login.verification_uri = None;
        self.write_login(&login, &[], 0, false)
    }

    fn read_login(
        &self,
        id: &OpenCodeLoginId,
    ) -> Result<
        (
            DeviceProviderOpenCodeLogin,
            Option<ResolvedSecret>,
            i64,
            bool,
        ),
        DeviceProviderError,
    > {
        if !is_canonical_prefixed_id(&id.0, "ocl_") {
            return Err(DeviceProviderError);
        }
        let (projection,private,interval,in_flight):(String,Vec<u8>,i64,bool)=self.connection.query_row(
            "SELECT projection,device_code,interval_ms,in_flight FROM opencode_logins WHERE login_id=?1",[&id.0],
            |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        Ok((
            serde_json::from_str(&projection)?,
            if private.is_empty() {
                None
            } else {
                Some(ResolvedSecret::from_bytes(private).map_err(|_| DeviceProviderError)?)
            },
            interval,
            in_flight,
        ))
    }

    fn write_login(
        &self,
        login: &DeviceProviderOpenCodeLogin,
        private: &[u8],
        interval: i64,
        in_flight: bool,
    ) -> Result<(), DeviceProviderError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute("INSERT INTO opencode_logins VALUES(?1,?2,?3,?4,?5) ON CONFLICT(login_id) DO UPDATE SET projection=excluded.projection,device_code=excluded.device_code,interval_ms=excluded.interval_ms,in_flight=excluded.in_flight",
            params![login.login_id.0,serde_json::to_string(login)?,private,interval,in_flight])?;
        transaction.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn opencode_logins(
        &self,
    ) -> Result<Vec<DeviceProviderOpenCodeLogin>, DeviceProviderError> {
        let mut query = self
            .connection
            .prepare("SELECT projection FROM opencode_logins ORDER BY login_id")?;
        query
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    fn update_opencode_usage(
        &self,
        oauth: &OpenCodeOAuth,
        account: &OpenCodeAccountId,
        org: &str,
    ) -> Result<(), DeviceProviderError> {
        let bound:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM opencode_connections WHERE account_ref=?1 AND organization_id=?2)",params![account.0,org],|row|row.get(0))?;
        if !bound {
            return Err(DeviceProviderError);
        }
        let access = self
            .opencode_access(account, || true)
            .map_err(|_| DeviceProviderError)?;
        let body = oauth.usage(&access, org).map_err(|_| DeviceProviderError)?;
        let window =
            |name: &str| -> Result<DeviceProviderOpenCodeUsageWindow, DeviceProviderError> {
                let value = body
                    .get("usage")
                    .and_then(|usage| usage.get(name))
                    .ok_or(DeviceProviderError)?;
                let status = value
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(DeviceProviderError)?;
                let resets_at = value
                    .get("resetsAt")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(DeviceProviderError)?;
                let percent = value
                    .get("percent")
                    .and_then(serde_json::Value::as_f64)
                    .ok_or(DeviceProviderError)?;
                if !valid_text(status, 128)
                    || !valid_text(resets_at, 64)
                    || !percent.is_finite()
                    || !(0.0..=10000.0).contains(&percent)
                {
                    return Err(DeviceProviderError);
                }
                Ok(DeviceProviderOpenCodeUsageWindow {
                    status: status.into(),
                    resets_at: resets_at.into(),
                    percent,
                })
            };
        let usage = DeviceProviderOpenCodeUsage {
            rolling: window("rolling")?,
            weekly: window("weekly")?,
            monthly: window("monthly")?,
            organization_id: org.into(),
            updated_at_ms: now_ms().map_err(|_| DeviceProviderError)?,
        };
        self.connection.execute(
            "UPDATE opencode_accounts SET usage=?1 WHERE account_ref=?2",
            params![serde_json::to_string(&usage)?, account.0],
        )?;
        self.connection.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        Ok(())
    }
}

fn valid_command(command: &DeviceProviderOpenCodeCommand) -> bool {
    if command.model_id.as_ref().is_some_and(|model| {
        command.operation != Operation::ConnectOpencode || !valid_text(model, 128)
    }) {
        return false;
    }
    let login = command
        .login_id
        .as_ref()
        .is_some_and(|id| is_canonical_prefixed_id(&id.0, "ocl_"));
    let account = command
        .account_ref
        .as_ref()
        .is_some_and(|id| is_canonical_prefixed_id(&id.0, "oca_"));
    let provider = command
        .provider_id
        .as_ref()
        .is_some_and(|id| valid_text(id, 128));
    let org = command
        .organization_id
        .as_ref()
        .is_some_and(|id| valid_text(id, 200));
    let name = command
        .display_name
        .as_ref()
        .is_some_and(|name| valid_text(name, 200));
    let shape = (
        command.login_id.is_some(),
        command.account_ref.is_some(),
        command.organization_id.is_some(),
        command.provider_id.is_some(),
        command.display_name.is_some(),
    );
    match command.operation {
        Operation::BeginOpencodeLogin => shape == (false, false, false, false, false),
        Operation::PollOpencodeLogin | Operation::CancelOpencodeLogin => {
            login && shape == (true, false, false, false, false)
        }
        Operation::ConnectOpencode => {
            login && org && provider && name && shape == (true, false, true, true, true)
        }
        Operation::LogoutOpencode => account && shape == (false, true, false, false, false),
        Operation::OpencodeUsage => account && org && shape == (false, true, true, false, false),
        Operation::SetDefaultProvider => provider && shape == (false, false, false, true, false),
    }
}
