// SPDX-License-Identifier: Apache-2.0

//! Small OC-01 probe for the native encrypted configuration and Device model path.
//! This is a transport probe, not a Worker or full product acceptance run.

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::de::DeserializeOwned;
use winwincode_api::generated::{DeviceConfigurationEnvelope, DeviceProviderOutcome};
use winwincode_execution_port::generated::ModelOpenMessage;
use winwincode_provider::{DeviceModelAdmission, DeviceProviderStore};
use winwincode_provider::{
    ResolvedSecret,
    opencode_auth::{OPENCODE_CLIENT_ID, OPENCODE_ISSUER, OpenCodeOAuth, OpenCodeTokenGrant},
};

fn main() {
    if run().is_err() {
        eprintln!("native OpenCode probe failed; inspect the private Device receipt");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [
            "import-auth",
            directory,
            authorization,
            provider,
            organization,
            model @ ..,
        ] if model.len() <= 1 => {
            import_authorization(
                directory,
                authorization,
                provider,
                organization,
                model.first().copied(),
            )?;
        }
        ["snapshot", directory] => {
            let store = DeviceProviderStore::open(Path::new(directory)).map_err(|_| ())?;
            let snapshot = store
                .snapshot("cnd_00000000000000000000000001")
                .map_err(|_| ())?;
            println!("{}", serde_json::to_string(&snapshot).map_err(|_| ())?);
        }
        ["apply", directory, input] => {
            let mut store = DeviceProviderStore::open(Path::new(directory)).map_err(|_| ())?;
            let envelope: DeviceConfigurationEnvelope = read_private_json(input)?;
            let receipt = store
                .apply(&envelope.client_node_id, &envelope)
                .map_err(|_| ())?;
            if receipt.outcome != DeviceProviderOutcome::Saved {
                return Err(());
            }
            println!("{}", serde_json::to_string(&receipt).map_err(|_| ())?);
        }
        [
            mode @ ("execute" | "execute-benchmark" | "replay"),
            directory,
            input,
            output,
        ] => {
            execute_exchange(mode, directory, input, output)?;
        }
        _ => return Err(()),
    }
    Ok(())
}

fn execute_exchange(mode: &str, directory: &str, input: &str, output: &str) -> Result<(), ()> {
    let store = DeviceProviderStore::open(Path::new(directory)).map_err(|_| ())?;
    let limit = u64::try_from(winwincode_execution_port::transport::MAX_REMOTE_FRAME_BYTES)
        .map_err(|_| ())?;
    let open: ModelOpenMessage = read_private_json_bounded(input, limit)?;
    let payload = STANDARD.decode(&open.request.data_base64).map_err(|_| ())?;
    let payload: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| ())?;
    let provider = payload
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    let snapshot = store
        .snapshot("cnd_00000000000000000000000001")
        .map_err(|_| ())?;
    let config = snapshot
        .providers
        .into_iter()
        .find(|projection| projection.config.provider_id == provider)
        .ok_or(())?
        .config;
    if mode != "execute-benchmark"
        && !winwincode_api::opencode::valid_opencode_provider_route(&config)
    {
        return Err(());
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .map_err(|_| ())?;
    let _permit = if mode == "replay" {
        if !store.model_start_recorded(&open).map_err(|_| ())? {
            return Err(());
        }
        None
    } else {
        match store.try_model_permit(&open).map_err(|_| ())? {
            DeviceModelAdmission::Ready(permit) => Some(permit),
            DeviceModelAdmission::Deferred => return Err(()),
        }
    };
    let chunks = store
        .execute_model_authorized(&open, || mode != "replay")
        .map_err(|_| ())?;
    output
        .write_all(&serde_json::to_vec(&chunks).map_err(|_| ())?)
        .map_err(|_| ())?;
    output.sync_all().map_err(|_| ())?;
    if chunks.is_empty() || chunks.iter().any(|chunk| chunk.error.is_some()) {
        return Err(());
    }
    println!("native Device exchange complete: {} chunks", chunks.len());
    Ok(())
}

// Test provisioning from an already completed native grant. This is not a UI token import.
fn import_authorization(
    directory: &str,
    authorization: &str,
    provider: &str,
    organization: &str,
    selected_model: Option<&str>,
) -> Result<(), ()> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let mut value: serde_json::Value = read_private_json(
        &Path::new(authorization)
            .join("tokens.json")
            .to_string_lossy(),
    )?;
    if value["issuer"].as_str() != Some(OPENCODE_ISSUER)
        || value["client_id"].as_str() != Some(OPENCODE_CLIENT_ID)
    {
        return Err(());
    }
    let remaining = value["expires_at"].as_f64().ok_or(())?
        - SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ())?
            .as_secs_f64();
    if !remaining.is_finite() || remaining <= 60.0 {
        return Err(());
    }
    let secret = |value: serde_json::Value| match value {
        serde_json::Value::String(token) => {
            ResolvedSecret::from_bytes(token.into_bytes()).map_err(|_| ())
        }
        _ => Err(()),
    };
    let grant = OpenCodeTokenGrant {
        access_token: secret(value["access_token"].take())?,
        refresh_token: secret(value["refresh_token"].take())?,
        expires_in: Duration::try_from_secs_f64(remaining).map_err(|_| ())?,
    };
    let oauth = OpenCodeOAuth::new();
    let user = oauth.user(&grant.access_token).map_err(|_| ())?;
    let organizations = oauth.organizations(&grant.access_token).map_err(|_| ())?;
    let org = organizations
        .iter()
        .find(|org| org.id == organization)
        .ok_or(())?;
    let configuration = oauth
        .configuration(&grant.access_token, &org.id)
        .map_err(|_| ())?;
    let store = DeviceProviderStore::open(Path::new(directory)).map_err(|_| ())?;
    let account = store
        .save_opencode_account(&user, grant, || true)
        .map_err(|_| ())?;
    store
        .connect_opencode_model(
            &account,
            org,
            &configuration,
            provider.to_owned(),
            format!("OpenCode Go · {}", user.email),
            selected_model,
        )
        .map_err(|_| ())?;
    println!(
        "{}",
        serde_json::json!({"accountRef":account,"subject":user.id,"email":user.email,"organizationId":org.id,"providerId":provider})
    );
    Ok(())
}

fn read_private_json<T: DeserializeOwned>(path: &str) -> Result<T, ()> {
    read_private_json_bounded(path, 65_536)
}

fn read_private_json_bounded<T: DeserializeOwned>(path: &str, limit: u64) -> Result<T, ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| ())?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if u64::try_from(bytes.len()).map_err(|_| ())? > limit {
        return Err(());
    }
    let result = serde_json::from_slice(&bytes).map_err(|_| ());
    bytes.fill(0);
    result
}
