// SPDX-License-Identifier: Apache-2.0

//! User-assisted live `OpenCode` authorization. Private files are never diagnostics.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use winwincode_provider::opencode_auth::{
    OPENCODE_CLIENT_ID, OPENCODE_ISSUER, OpenCodeOAuth, OpenCodePollResult,
};

fn main() {
    if run().is_err() {
        eprintln!("OpenCode authorization ended; private results are retained");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ()> {
    let directory = std::env::args().nth(1).ok_or(())?;
    let directory = Path::new(&directory);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|_| ())?;
    let metadata = fs::symlink_metadata(directory).map_err(|_| ())?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(());
    }
    let oauth = OpenCodeOAuth::new();
    let mut grant = oauth.begin().map_err(|_| ())?;
    println!(
        "{}",
        json!({"verificationUri":grant.verification_uri,"userCode":grant.user_code,"expiresInSeconds":grant.expires_in.as_secs()})
    );
    std::io::stdout().flush().map_err(|_| ())?;
    let cancelled = AtomicBool::new(false);
    let token = loop {
        thread::sleep(Duration::from_secs(1));
        match oauth.poll_once(&mut grant, &cancelled).map_err(|_| ())? {
            OpenCodePollResult::Pending | OpenCodePollResult::SlowDown => {}
            OpenCodePollResult::Authorized(token) => break token,
        }
    };
    let received = Instant::now();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_secs()
        .checked_add(token.expires_in.as_secs())
        .ok_or(())?;
    private_json(
        directory,
        "tokens.json",
        &json!({
            "access_token":std::str::from_utf8(token.access_token.expose()).map_err(|_|())?,
            "refresh_token":std::str::from_utf8(token.refresh_token.expose()).map_err(|_|())?,
            "expires_at":expires_at,"client_id":OPENCODE_CLIENT_ID,"issuer":OPENCODE_ISSUER,
        }),
    )?;
    let user = oauth.user(&token.access_token).map_err(|_| ())?;
    let organizations = oauth.organizations(&token.access_token).map_err(|_| ())?;
    let orgs = organizations
        .iter()
        .map(|org| json!({"id":org.id,"name":org.name}))
        .collect::<Vec<_>>();
    let account = json!({"user":{"id":user.id,"email":user.email},"orgs":orgs});
    private_json(directory, "account.json", &account)?;
    if organizations.len() == 1 {
        let config = oauth
            .configuration(&token.access_token, &organizations[0].id)
            .map_err(|_| ())?;
        private_json(directory, "config.json", &config)?;
    }
    println!(
        "{}",
        json!({"authorized":true,"account":account,"metadataSeconds":received.elapsed().as_secs()})
    );
    Ok(())
}

fn private_json(directory: &Path, name: &str, value: &Value) -> Result<(), ()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(name))
        .map_err(|_| ())?;
    let mut bytes = serde_json::to_vec(value).map_err(|_| ())?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| ());
    bytes.fill(0);
    result
}
