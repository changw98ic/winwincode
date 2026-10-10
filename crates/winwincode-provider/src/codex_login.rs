// SPDX-License-Identifier: Apache-2.0

//! Read-only binding to the selected device's Codex file login. Codex owns refresh.

use crate::{DeviceProviderError, ResolvedSecret};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read as _,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CodexLoginBinding {
    codex_home: PathBuf,
    account_id: String,
}

#[derive(Deserialize)]
struct AuthFile {
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    api_key: Option<String>,
    tokens: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    account_id: String,
}

impl CodexLoginBinding {
    pub(crate) fn current() -> Result<Self, DeviceProviderError> {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .ok_or(DeviceProviderError)?;
        Self::at(&home)
    }

    fn at(home: &Path) -> Result<Self, DeviceProviderError> {
        let codex_home = fs::canonicalize(home)?;
        let tokens = read_tokens(&codex_home)?;
        Ok(Self {
            codex_home,
            account_id: tokens.account_id,
        })
    }

    pub(crate) fn resolve(&self) -> Result<(ResolvedSecret, String), DeviceProviderError> {
        let tokens = read_tokens(&self.codex_home)?;
        if tokens.account_id != self.account_id {
            return Err(DeviceProviderError);
        }
        let secret = ResolvedSecret::from_bytes(tokens.access_token.into_bytes())
            .map_err(|_| DeviceProviderError)?;
        Ok((secret, self.account_id.clone()))
    }
}

fn read_tokens(home: &Path) -> Result<Tokens, DeviceProviderError> {
    let path = home.join("auth.json");
    let before = fs::symlink_metadata(&path)?;
    let file = fs::File::open(&path)?;
    let after = file.metadata()?;
    if !before.is_file()
        || !after.is_file()
        || before.ino() != after.ino()
        || before.dev() != after.dev()
        || after.mode() & 0o077 != 0
        || after.len() > 65_536
    {
        return Err(DeviceProviderError);
    }
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        bytes.fill(0);
        return Err(DeviceProviderError);
    }
    let parsed = serde_json::from_slice::<AuthFile>(&bytes);
    bytes.fill(0);
    let auth = parsed?;
    if auth.api_key.is_some()
        || auth
            .auth_mode
            .as_deref()
            .is_some_and(|mode| mode != "chatgpt")
    {
        return Err(DeviceProviderError);
    }
    let tokens = auth.tokens.ok_or(DeviceProviderError)?;
    validate_tokens(&tokens)?;
    Ok(tokens)
}

fn validate_tokens(tokens: &Tokens) -> Result<(), DeviceProviderError> {
    if tokens.account_id.is_empty()
        || tokens.account_id.len() > 200
        || tokens.account_id.chars().any(char::is_control)
        || tokens.access_token.len() > 16_384
    {
        return Err(DeviceProviderError);
    }
    let mut parts = tokens.access_token.split('.');
    let _header = parts.next().ok_or(DeviceProviderError)?;
    let claims = parts.next().ok_or(DeviceProviderError)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(DeviceProviderError);
    }
    let mut bytes = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| DeviceProviderError)?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
    bytes.fill(0);
    let claims = parsed?;
    let expires = claims
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .ok_or(DeviceProviderError)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DeviceProviderError)?
        .as_secs();
    if expires <= now.saturating_add(60)
        || claims
            .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
            .and_then(serde_json::Value::as_str)
            != Some(tokens.account_id.as_str())
    {
        return Err(DeviceProviderError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn auth(account: &str, expires: u64) -> Vec<u8> {
        let claims = serde_json::json!({"exp":expires,"https://api.openai.com/auth":{"chatgpt_account_id":account}});
        serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt","tokens":{"account_id":account,"access_token":format!("e30.{}.test", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()))}})).unwrap()
    }

    #[test]
    fn binding_rereads_tokens_but_rejects_account_switch_expiry_and_public_files() {
        let home = std::env::temp_dir().join(format!("wwc-codex-login-{}", std::process::id()));
        fs::create_dir_all(&home).unwrap();
        let path = home.join("auth.json");
        fs::write(&path, auth("account-one", u64::MAX)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let binding = CodexLoginBinding::at(&home).unwrap();
        let original = binding.resolve().unwrap().0;
        fs::write(&path, auth("account-one", u64::MAX - 1)).unwrap();
        assert_ne!(binding.resolve().unwrap().0.expose(), original.expose());
        fs::write(&path, auth("account-two", u64::MAX)).unwrap();
        assert!(binding.resolve().is_err());
        fs::write(&path, auth("account-one", 1)).unwrap();
        assert!(binding.resolve().is_err());
        fs::write(&path, auth("account-one", u64::MAX)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(binding.resolve().is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(binding.resolve().is_err());
        fs::remove_dir_all(home).unwrap();
    }
}
