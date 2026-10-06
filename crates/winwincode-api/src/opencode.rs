// SPDX-License-Identifier: Apache-2.0

//! Bounded OAuth metadata at the Device → Server boundary.

use crate::generated::{
    DeviceProviderOpenCodeAccountState as AccountState,
    DeviceProviderOpenCodeLoginState as LoginState, DeviceProviderOpenCodeUsage,
    DeviceProviderProtocol, DeviceProviderSnapshot,
};
use std::collections::BTreeSet;
use winwincode_domain::is_canonical_prefixed_id;

/// Validates public OAuth metadata. Credentials are never part of this contract.
#[must_use]
pub fn valid_opencode_projection(snapshot: &DeviceProviderSnapshot) -> bool {
    let accounts = snapshot.open_code_accounts.as_deref().unwrap_or_default();
    let logins = snapshot.open_code_logins.as_deref().unwrap_or_default();
    if accounts.len() > 100 || logins.len() > 100 {
        return false;
    }
    let mut refs = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for account in accounts {
        if !is_canonical_prefixed_id(&account.account_ref.0, "oca_")
            || !refs.insert(&account.account_ref.0)
            || !identities.insert(&account.subject)
            || !valid_text(&account.subject, 200)
            || !valid_text(&account.email, 320)
            || !(1..=9_007_199_254_740_991).contains(&account.credential_version)
        {
            return false;
        }
        if account
            .usage
            .as_ref()
            .is_some_and(|usage| !valid_usage(usage))
        {
            return false;
        }
    }
    let mut login_refs = BTreeSet::new();
    for login in logins {
        if !is_canonical_prefixed_id(&login.login_id.0, "ocl_")
            || !login_refs.insert(&login.login_id.0)
            || !(0..=9_007_199_254_740_991).contains(&login.expires_at_ms)
            || !(0..=9_007_199_254_740_991).contains(&login.poll_after_ms)
            || login
                .account_ref
                .as_ref()
                .is_some_and(|account| !refs.contains(&account.0))
            || login.organizations.len() > 100
        {
            return false;
        }
        let mut orgs = BTreeSet::new();
        if login.organizations.iter().any(|org| {
            !valid_text(&org.id, 200) || !valid_text(&org.name, 200) || !orgs.insert(&org.id)
        }) {
            return false;
        }
        if login.state == LoginState::Pending {
            if login.account_ref.is_some()
                || !login.organizations.is_empty()
                || login
                    .user_code
                    .as_ref()
                    .is_none_or(|code| !valid_text(code, 128))
                || login
                    .verification_uri
                    .as_ref()
                    .is_none_or(|uri| !valid_authorization_uri(uri))
            {
                return false;
            }
        } else if login.user_code.is_some()
            || login.verification_uri.is_some()
            || (matches!(login.state, LoginState::Authorized | LoginState::Completed)
                && login.account_ref.is_none())
        {
            return false;
        }
    }
    for provider in &snapshot.providers {
        if let Some(connection) = &provider.open_code {
            let Some(account) = accounts
                .iter()
                .find(|account| account.account_ref == connection.account_ref)
            else {
                return false;
            };
            if !valid_opencode_provider_route(&provider.config)
                || !valid_text(&connection.organization_id, 200)
                || !valid_text(&connection.organization_name, 200)
                || provider.credential_configured
                    != matches!(
                        account.state,
                        AccountState::Authorized | AccountState::RefreshInFlight
                    )
            {
                return false;
            }
        }
    }
    snapshot.default_provider_id.as_ref().is_none_or(|default| {
        snapshot
            .providers
            .iter()
            .any(|provider| &provider.config.provider_id == default)
    })
}

fn valid_usage(usage: &DeviceProviderOpenCodeUsage) -> bool {
    valid_text(&usage.organization_id, 200)
        && (0..=9_007_199_254_740_991).contains(&usage.updated_at_ms)
        && [&usage.rolling, &usage.weekly, &usage.monthly]
            .iter()
            .all(|window| {
                valid_text(&window.status, 128)
                    && valid_text(&window.resets_at, 64)
                    && window.percent.is_finite()
                    && (0.0..=10000.0).contains(&window.percent)
            })
}

const GO_CHAT_ENDPOINT: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";
const GO_MESSAGES_ENDPOINT: &str = "https://opencode.ai/inference/go/anthropic/v1/messages";

/// Recognizes only the two supported official Go endpoint/protocol pairs.
#[must_use]
pub fn valid_opencode_provider_route(config: &crate::generated::DeviceProviderConfig) -> bool {
    matches!(
        (&config.protocol, config.endpoint.as_str()),
        (
            DeviceProviderProtocol::OpenaiChatCompletions,
            GO_CHAT_ENDPOINT
        ) | (
            DeviceProviderProtocol::AnthropicMessages,
            GO_MESSAGES_ENDPOINT
        )
    )
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= max
        && !value.chars().any(char::is_control)
}

fn valid_authorization_uri(value: &str) -> bool {
    valid_text(value, 2048)
        && !value.contains('#')
        && (value.starts_with("https://opencode.ai/console/")
            || value.starts_with("https://opencode.ai:443/console/"))
}
