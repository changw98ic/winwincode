// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use sha2::Sha256;
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorProtocol, ConnectorRegistration, EnterpriseIntegrationId, InboundStatus,
    IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId, IntegrationOperationKey,
    IntegrationStorage, OutboundAttemptResult, OutboundRequest, RetryPolicy,
    SLACK_CONNECTOR_PROTOCOL, SlackAppId, SlackBotId, SlackBotPermissions, SlackBotToken,
    SlackChannelId, SlackConnectorConfig, SlackCredentialError, SlackCredentialPort,
    SlackEnterpriseConnector, SlackInstallationIdentity, SlackRateLimitGate, SlackSigningSecret,
    SlackTlsRoots, SlackWebhookHeaders, SlackWebhookRequestFactory, SlackWebhookVerifier,
    SlackWorkspaceId, SystemSlackClock,
};

const SLACK_BOT_TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
const SLACK_SIGNING_SECRET_ENV: &str = "SLACK_SIGNING_SECRET";
const SLACK_TEST_WORKSPACE_ID_ENV: &str = "SLACK_TEST_WORKSPACE_ID";
const SLACK_TEST_CHANNEL_ID_ENV: &str = "SLACK_TEST_CHANNEL_ID";
const REQUIRED_ENVIRONMENT: [&str; 4] = [
    SLACK_BOT_TOKEN_ENV,
    SLACK_SIGNING_SECRET_ENV,
    SLACK_TEST_WORKSPACE_ID_ENV,
    SLACK_TEST_CHANNEL_ID_ENV,
];
const REAL_SLACK_API_BASE_URL: &str = "https://slack.com/api/";
const CONTROL_PLANE_BASE_URL: &str = "https://example.com/winwincode";
const FIXTURE_WORKSPACE_ID: &str = "T12345678";
const FIXTURE_APP_ID: &str = "A12345678";
const FIXTURE_BOT_ID: &str = "B12345678";
const FIXTURE_USER_ID: &str = "U12345678";
const FIXTURE_CHANNEL_ID: &str = "C12345678";
const FIXTURE_BOT_TOKEN: &str = "xoxb-slack-live-gate-fixture";
const FIXTURE_SIGNING_SECRET: &str = "slack-live-gate-signing-secret-fixture";
const MAX_LIVE_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_LIVE_RETRY_SECONDS: u64 = 120;
const LIVE_REQUEST_COUNT_WITH_BOOTSTRAP: usize = 9;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct SecretBytes(Vec<u8>);

impl SecretBytes {
    fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn text(&self) -> Result<String, GateFailure> {
        String::from_utf8(self.0.clone()).map_err(|_| GateFailure::InvalidConfiguration)
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct SlackLiveGateInputs {
    bot_token: Arc<SecretBytes>,
    signing_secret: Arc<SecretBytes>,
    workspace_id: String,
    channel_id: String,
}

impl SlackLiveGateInputs {
    fn fixture() -> Self {
        Self {
            bot_token: Arc::new(SecretBytes::new(FIXTURE_BOT_TOKEN.as_bytes().to_vec())),
            signing_secret: Arc::new(SecretBytes::new(FIXTURE_SIGNING_SECRET.as_bytes().to_vec())),
            workspace_id: FIXTURE_WORKSPACE_ID.to_owned(),
            channel_id: FIXTURE_CHANNEL_ID.to_owned(),
        }
    }
}

enum SlackLiveGateConfiguration {
    Closed { missing: Vec<&'static str> },
    Ready(SlackLiveGateInputs),
}

impl SlackLiveGateConfiguration {
    fn inspect(mut lookup: impl FnMut(&'static str) -> Option<Vec<u8>>) -> Self {
        let bot_token = non_empty(lookup(SLACK_BOT_TOKEN_ENV));
        let signing_secret = non_empty(lookup(SLACK_SIGNING_SECRET_ENV));
        let workspace_id = non_empty(lookup(SLACK_TEST_WORKSPACE_ID_ENV));
        let channel_id = non_empty(lookup(SLACK_TEST_CHANNEL_ID_ENV));
        let values = [
            bot_token.as_ref(),
            signing_secret.as_ref(),
            workspace_id.as_ref(),
            channel_id.as_ref(),
        ];
        let missing = REQUIRED_ENVIRONMENT
            .into_iter()
            .zip(values)
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Self::Closed { missing };
        }
        let workspace_id = String::from_utf8(workspace_id.expect("checked live workspace input"));
        let channel_id = String::from_utf8(channel_id.expect("checked live channel input"));
        let (Ok(workspace_id), Ok(channel_id)) = (workspace_id, channel_id) else {
            return Self::Closed {
                missing: vec![SLACK_TEST_WORKSPACE_ID_ENV, SLACK_TEST_CHANNEL_ID_ENV],
            };
        };
        Self::Ready(SlackLiveGateInputs {
            bot_token: Arc::new(SecretBytes::new(
                bot_token.expect("checked live bot token input"),
            )),
            signing_secret: Arc::new(SecretBytes::new(
                signing_secret.expect("checked live signing secret input"),
            )),
            workspace_id,
            channel_id,
        })
    }

    fn from_process_environment() -> Self {
        Self::inspect(|name| std::env::var(name).ok().map(String::into_bytes))
    }
}

fn non_empty(value: Option<Vec<u8>>) -> Option<Vec<u8>> {
    value.filter(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateFailure {
    ApiRejected,
    CleanupFailed,
    CredentialRejected,
    DurableStateRejected,
    InvalidConfiguration,
    InvalidRemoteMessage,
    LeakDetected,
    LiveRetryExceeded,
    TransportUnavailable,
}

impl std::fmt::Display for GateFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::ApiRejected => "Slack Web API rejected the live-gate operation",
            Self::CleanupFailed => "Slack live-gate message cleanup failed",
            Self::CredentialRejected => "Slack live-gate credential boundary rejected input",
            Self::DurableStateRejected => "Slack live-gate durable state check failed",
            Self::InvalidConfiguration => "Slack live-gate configuration is invalid",
            Self::InvalidRemoteMessage => "Slack live-gate remote message is invalid",
            Self::LeakDetected => "Slack live-gate secret leakage scan failed",
            Self::LiveRetryExceeded => "Slack live-gate retry bound was exceeded",
            Self::TransportUnavailable => "Slack live-gate TLS transport failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for GateFailure {}

#[derive(Clone)]
enum LiveTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

struct SlackLiveApi {
    base_url: String,
    bot_token: Arc<SecretBytes>,
    agent: ureq::Agent,
}

impl SlackLiveApi {
    fn try_new(
        base_url: &str,
        bot_token: Arc<SecretBytes>,
        roots: &LiveTlsRoots,
    ) -> Result<Self, GateFailure> {
        let uri = base_url
            .parse::<ureq::http::Uri>()
            .map_err(|_| GateFailure::InvalidConfiguration)?;
        if uri.scheme_str() != Some("https")
            || uri.authority().is_none()
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
            || uri.query().is_some()
            || base_url.contains('#')
        {
            return Err(GateFailure::InvalidConfiguration);
        }
        let root_certs = match roots {
            LiveTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            LiveTlsRoots::Specific(values) if !values.is_empty() => values
                .iter()
                .map(|value| ureq::tls::Certificate::from_der(value).to_owned())
                .collect::<Vec<_>>()
                .into(),
            LiveTlsRoots::Specific(_) => return Err(GateFailure::InvalidConfiguration),
        };
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(root_certs)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            base_url: format!("{}/", base_url.trim_end_matches('/')),
            bot_token,
            agent,
        })
    }

    fn auth_test(&self) -> Result<Value, GateFailure> {
        self.post_json("auth.test", &json!({}))
    }

    fn post_message(&self, channel: &str, text: &str) -> Result<Value, GateFailure> {
        self.post_json(
            "chat.postMessage",
            &json!({
                "channel": channel,
                "metadata": {
                    "event_payload": {"marker": text},
                    "event_type": "winwincode_live_gate_bootstrap",
                },
                "text": text,
            }),
        )
    }

    fn delete_message(&self, channel: &str, timestamp: &str) -> Result<(), GateFailure> {
        let response =
            self.post_json("chat.delete", &json!({"channel": channel, "ts": timestamp}))?;
        if response.get("channel").and_then(Value::as_str) != Some(channel)
            || response.get("ts").and_then(Value::as_str) != Some(timestamp)
        {
            return Err(GateFailure::CleanupFailed);
        }
        Ok(())
    }

    fn history(&self, channel: &str) -> Result<Vec<Value>, GateFailure> {
        let response = self.get_json(&format!(
            "conversations.history?channel={}&include_all_metadata=true&limit=100",
            percent_encode(channel)
        ))?;
        response
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .ok_or(GateFailure::ApiRejected)
    }

    fn post_json(&self, method: &str, body: &Value) -> Result<Value, GateFailure> {
        self.request(method, Some(body))
    }

    fn get_json(&self, method: &str) -> Result<Value, GateFailure> {
        self.request(method, None)
    }

    fn request(&self, method: &str, body: Option<&Value>) -> Result<Value, GateFailure> {
        for attempt in 0..=1 {
            let authorization = format!("Bearer {}", self.bot_token.text()?);
            let url = format!("{}{}", self.base_url, method);
            let response = match body {
                Some(body) => self
                    .agent
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Authorization", &authorization)
                    .header("User-Agent", "WinWinCode-Slack-Live-Gate")
                    .send_json(body),
                None => self
                    .agent
                    .get(&url)
                    .header("Accept", "application/json")
                    .header("Authorization", &authorization)
                    .header("User-Agent", "WinWinCode-Slack-Live-Gate")
                    .call(),
            }
            .map_err(|_| GateFailure::TransportUnavailable)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let bytes = response
                .into_body()
                .with_config()
                .limit(MAX_LIVE_RESPONSE_BYTES)
                .read_to_vec()
                .map_err(|_| GateFailure::ApiRejected)?;
            if status == 429 && attempt == 0 {
                let seconds = retry_after
                    .filter(|seconds| *seconds > 0 && *seconds <= MAX_LIVE_RETRY_SECONDS)
                    .ok_or(GateFailure::LiveRetryExceeded)?;
                thread::sleep(Duration::from_secs(seconds));
                continue;
            }
            if status != 200 {
                return Err(GateFailure::ApiRejected);
            }
            let response: Value =
                serde_json::from_slice(&bytes).map_err(|_| GateFailure::ApiRejected)?;
            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(GateFailure::ApiRejected);
            }
            return Ok(response);
        }
        Err(GateFailure::LiveRetryExceeded)
    }
}

struct RemoteCleanup<'a> {
    api: &'a SlackLiveApi,
    channel: &'a str,
    pending_timestamps: Vec<String>,
}

impl<'a> RemoteCleanup<'a> {
    fn new(api: &'a SlackLiveApi, channel: &'a str) -> Self {
        Self {
            api,
            channel,
            pending_timestamps: Vec::new(),
        }
    }

    fn track(&mut self, timestamp: String) {
        self.pending_timestamps.push(timestamp);
    }

    fn delete(&mut self, timestamp: &str) -> Result<(), GateFailure> {
        self.api.delete_message(self.channel, timestamp)?;
        self.pending_timestamps
            .retain(|candidate| candidate != timestamp);
        Ok(())
    }
}

impl Drop for RemoteCleanup<'_> {
    fn drop(&mut self) {
        for timestamp in self.pending_timestamps.drain(..) {
            let _ = self.api.delete_message(self.channel, &timestamp);
        }
    }
}

struct BootstrapIdentity {
    installation: SlackInstallationIdentity,
    callback_user_id: String,
    used_probe_message: bool,
}

fn bootstrap_identity(
    inputs: &SlackLiveGateInputs,
    api: &SlackLiveApi,
    cleanup: &mut RemoteCleanup<'_>,
    marker: &str,
) -> Result<BootstrapIdentity, GateFailure> {
    let auth = api.auth_test()?;
    let workspace = required_string(&auth, &["team_id"])?;
    if workspace != inputs.workspace_id {
        return Err(GateFailure::CredentialRejected);
    }
    let bot = required_string(&auth, &["bot_id"])?;
    let callback_user_id = required_string(&auth, &["user_id"])?;
    let mut used_probe_message = false;
    let app = if let Some(app) = auth.get("app_id").and_then(Value::as_str) {
        app.to_owned()
    } else {
        used_probe_message = true;
        let probe = api.post_message(&inputs.channel_id, marker)?;
        let timestamp = required_string(&probe, &["ts"])?;
        cleanup.track(timestamp.clone());
        if required_string(&probe, &["channel"])? != inputs.channel_id {
            return Err(GateFailure::CredentialRejected);
        }
        let response_message = probe.get("message");
        let identity_message = if response_message
            .and_then(|message| message.get("app_id"))
            .and_then(Value::as_str)
            .is_some()
            && response_message
                .and_then(|message| message.get("bot_id"))
                .and_then(Value::as_str)
                .is_some()
        {
            response_message.ok_or(GateFailure::ApiRejected)?.clone()
        } else {
            let history = api.history(&inputs.channel_id)?;
            find_message(&history, marker)?.clone()
        };
        let message_bot = required_string(&identity_message, &["bot_id"])?;
        if message_bot != bot {
            return Err(GateFailure::CredentialRejected);
        }
        let app = required_string(&identity_message, &["app_id"])?;
        cleanup.delete(&timestamp)?;
        app
    };
    let workspace =
        SlackWorkspaceId::try_new(workspace).map_err(|_| GateFailure::InvalidConfiguration)?;
    let app = SlackAppId::try_new(app).map_err(|_| GateFailure::InvalidConfiguration)?;
    let bot = SlackBotId::try_new(bot).map_err(|_| GateFailure::InvalidConfiguration)?;
    Ok(BootstrapIdentity {
        installation: SlackInstallationIdentity::new(workspace, app, bot),
        callback_user_id,
        used_probe_message,
    })
}

fn required_string(value: &Value, path: &[&str]) -> Result<String, GateFailure> {
    let mut current = value;
    for component in path {
        current = current.get(*component).ok_or(GateFailure::ApiRejected)?;
    }
    current
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 4_096)
        .map(str::to_owned)
        .ok_or(GateFailure::ApiRejected)
}

struct LiveCredentialMaterial {
    bot_token: Arc<SecretBytes>,
    signing_secret: Arc<SecretBytes>,
    revoked: AtomicBool,
}

#[derive(Clone)]
struct LiveCredentialPort {
    material: Arc<LiveCredentialMaterial>,
    reference: CredentialReferenceId,
    installation: SlackInstallationIdentity,
    channel: SlackChannelId,
}

impl LiveCredentialPort {
    fn new(
        inputs: &SlackLiveGateInputs,
        reference: CredentialReferenceId,
        installation: SlackInstallationIdentity,
        channel: SlackChannelId,
    ) -> Self {
        Self {
            material: Arc::new(LiveCredentialMaterial {
                bot_token: Arc::clone(&inputs.bot_token),
                signing_secret: Arc::clone(&inputs.signing_secret),
                revoked: AtomicBool::new(false),
            }),
            reference,
            installation,
            channel,
        }
    }

    fn revoke(&self) {
        self.material.revoked.store(true, Ordering::SeqCst);
    }
}

impl SlackCredentialPort for LiveCredentialPort {
    fn resolve_signing_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError> {
        if reference != &self.reference || self.material.revoked.load(Ordering::SeqCst) {
            return Err(SlackCredentialError::revoked());
        }
        SlackSigningSecret::try_new(self.material.signing_secret.as_bytes())
            .map_err(|_| SlackCredentialError::unavailable())
    }

    fn resolve_bot_token(
        &mut self,
        reference: &CredentialReferenceId,
        installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError> {
        if reference != &self.reference
            || installation != &self.installation
            || self.material.revoked.load(Ordering::SeqCst)
        {
            return Err(SlackCredentialError::revoked());
        }
        SlackBotToken::try_new(
            self.material
                .bot_token
                .text()
                .map_err(|_| SlackCredentialError::unavailable())?,
            self.installation.clone(),
            self.channel.clone(),
            SlackBotPermissions::new(true, true),
        )
        .map_err(|_| SlackCredentialError::unavailable())
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(label: &str) -> Result<Self, GateFailure> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-slack-live-gate-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).map_err(|_| GateFailure::DurableStateRejected)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Eq, PartialEq)]
struct GateReport {
    audit_facts: usize,
    bootstrap_probe_cleaned: bool,
    callback_dispatches: usize,
    files_scanned: usize,
    production_message_cleaned: bool,
    revocation_closed: bool,
}

#[allow(clippy::too_many_lines)]
fn run_slack_live_gate(
    inputs: &SlackLiveGateInputs,
    api_base_url: &str,
    api_tls_roots: &LiveTlsRoots,
    connector_tls_roots: SlackTlsRoots,
) -> Result<GateReport, GateFailure> {
    let now = wall_clock_millis()?;
    let marker = format!("winwincode-slack-live-{}-{now}", std::process::id());
    let probe_marker = format!("{marker}-bootstrap");
    let api = SlackLiveApi::try_new(api_base_url, Arc::clone(&inputs.bot_token), api_tls_roots)?;
    let mut cleanup = RemoteCleanup::new(&api, &inputs.channel_id);
    let bootstrap = bootstrap_identity(inputs, &api, &mut cleanup, &probe_marker)?;
    let channel = SlackChannelId::try_new(inputs.channel_id.clone())
        .map_err(|_| GateFailure::InvalidConfiguration)?;
    let integration = integration_id();
    let credential_reference = credential_reference_id();
    let config = SlackConnectorConfig::try_new(
        integration.clone(),
        credential_reference.clone(),
        bootstrap.installation.clone(),
        channel.clone(),
        api_base_url,
        CONTROL_PLANE_BASE_URL,
        connector_tls_roots,
    )
    .map_err(|_| GateFailure::InvalidConfiguration)?;
    let directory = TemporaryDirectory::create("run")?;
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(directory.path())
            .map_err(|_| GateFailure::DurableStateRejected)?,
    );
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                integration.clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(SLACK_CONNECTOR_PROTOCOL)
                    .map_err(|_| GateFailure::InvalidConfiguration)?,
                credential_reference.clone(),
                now,
            )
            .map_err(|_| GateFailure::InvalidConfiguration)?,
        )
        .map_err(|_| GateFailure::DurableStateRejected)?;
    let credentials = LiveCredentialPort::new(
        inputs,
        credential_reference,
        bootstrap.installation.clone(),
        channel,
    );
    let mut diagnostic_credentials = credentials.clone();
    let redacted_credential_diagnostic = format!(
        "{:?}\n{:?}\n",
        diagnostic_credentials.resolve_signing_secret(&credential_reference_id()),
        diagnostic_credentials
            .resolve_bot_token(&credential_reference_id(), &bootstrap.installation)
    );
    assert_omits_secrets(
        redacted_credential_diagnostic.as_bytes(),
        inputs.bot_token.as_bytes(),
        inputs.signing_secret.as_bytes(),
    )?;
    let mut connector = SlackEnterpriseConnector::try_new(
        config.clone(),
        credentials.clone(),
        SlackRateLimitGate::open(directory.path())
            .map_err(|_| GateFailure::DurableStateRejected)?,
        SystemSlackClock,
    )
    .map_err(|_| GateFailure::InvalidConfiguration)?;
    let interaction_id = format!("{marker}-interaction");
    let expires_at = now
        .checked_add(10 * 60 * 1_000)
        .ok_or(GateFailure::InvalidConfiguration)?;
    let outbound = outbound_request(
        &marker,
        "slack.approval.notify",
        inputs,
        &interaction_id,
        7,
        expires_at,
        now,
    )?;
    framework
        .enqueue_outbound(&outbound)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    deliver_until_terminal(&mut framework, &integration, &mut connector)?;

    let history = api.history(&inputs.channel_id)?;
    let remote_message = find_message(&history, &marker)?.clone();
    let remote_timestamp = validate_remote_message(
        &remote_message,
        &bootstrap.installation,
        &inputs.channel_id,
        &marker,
        &interaction_id,
        outbound.operation_key().digest().0.as_str(),
        7,
        expires_at,
    )?;
    cleanup.track(remote_timestamp.clone());
    let remote_message_json =
        serde_json::to_vec(&remote_message).map_err(|_| GateFailure::InvalidRemoteMessage)?;
    assert_omits_secrets(
        &remote_message_json,
        inputs.bot_token.as_bytes(),
        inputs.signing_secret.as_bytes(),
    )?;
    let approve_value = approval_action_value(&remote_message)?;
    let callback_received_at = wall_clock_millis()?;
    let callback_body = interaction_form(
        &bootstrap,
        &inputs.channel_id,
        &remote_timestamp,
        &approve_value,
        callback_received_at,
    )?;
    let headers = signed_headers(
        inputs.signing_secret.as_bytes(),
        &callback_body,
        callback_received_at / 1_000,
    )?;
    let ingress = SlackWebhookRequestFactory::new(config.clone())
        .accept(
            tenant_scope(),
            &headers,
            callback_body,
            callback_received_at,
        )
        .map_err(|_| GateFailure::CredentialRejected)?;
    if ingress.acknowledgement().status_code() != 200
        || !ingress.acknowledgement().body().is_empty()
        || ingress.acknowledgement().send_by_millis()
            != callback_received_at
                .checked_add(3_000)
                .ok_or(GateFailure::InvalidConfiguration)?
    {
        return Err(GateFailure::CredentialRejected);
    }
    let callback = ingress
        .into_decision_request()
        .ok_or(GateFailure::CredentialRejected)?;
    let mut verifier = SlackWebhookVerifier::new(config, credentials.clone(), SystemSlackClock);
    let first = framework
        .receive_webhook(&callback, &mut verifier, &mut connector)
        .map_err(|_| GateFailure::CredentialRejected)?;
    if first.status() != InboundStatus::Accepted || first.idempotent_replay() {
        return Err(GateFailure::DurableStateRejected);
    }
    let replay = framework
        .receive_webhook(&callback, &mut verifier, &mut connector)
        .map_err(|_| GateFailure::CredentialRejected)?;
    if !replay.idempotent_replay() {
        return Err(GateFailure::DurableStateRejected);
    }
    let dispatches = framework
        .storage()
        .inbound_dispatches(&tenant_scope(), &integration, 0, 10)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    validate_single_control_plane_command(&dispatches, 7, expires_at)?;

    let blocked_marker = format!("{marker}-revoked");
    let blocked = outbound_request(
        &blocked_marker,
        "slack.attention.notify",
        inputs,
        &format!("{interaction_id}-revoked"),
        8,
        expires_at,
        wall_clock_millis()?,
    )?;
    framework
        .enqueue_outbound(&blocked)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    let authority = framework
        .storage()
        .authority(&tenant_scope(), &integration)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    framework
        .revoke_credential(
            &tenant_scope(),
            &integration,
            authority.revision(),
            wall_clock_millis()?,
        )
        .map_err(|_| GateFailure::DurableStateRejected)?;
    credentials.revoke();
    let revoked_result = framework.deliver_next(
        &tenant_scope(),
        &integration,
        wall_clock_millis()?,
        lease_id('Z'),
        wall_clock_millis()?
            .checked_add(60_000)
            .ok_or(GateFailure::InvalidConfiguration)?,
        &mut connector,
    );
    if revoked_result
        .as_ref()
        .err()
        .map(winwincode_integration::IntegrationError::kind)
        != Some(IntegrationErrorKind::CredentialRevoked)
    {
        return Err(GateFailure::DurableStateRejected);
    }
    let revoked_callback = framework.receive_webhook(&callback, &mut verifier, &mut connector);
    if revoked_callback
        .as_ref()
        .err()
        .map(winwincode_integration::IntegrationError::kind)
        != Some(IntegrationErrorKind::CredentialRevoked)
    {
        return Err(GateFailure::DurableStateRejected);
    }
    let post_revoke_history = api.history(&inputs.channel_id)?;
    if post_revoke_history
        .iter()
        .any(|message| message.get("text").and_then(Value::as_str) == Some(blocked_marker.as_str()))
    {
        return Err(GateFailure::DurableStateRejected);
    }
    let dispatches_after_revoke = framework
        .storage()
        .inbound_dispatches(&tenant_scope(), &integration, 0, 10)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    if dispatches_after_revoke.len() != 1 {
        return Err(GateFailure::DurableStateRejected);
    }

    cleanup.delete(&remote_timestamp)?;
    let audits = framework
        .storage()
        .audit_facts(&tenant_scope(), &integration, 0, 100)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    let audit_json = serde_json::to_vec(&audits).map_err(|_| GateFailure::DurableStateRejected)?;
    assert_omits_secrets(
        &audit_json,
        inputs.bot_token.as_bytes(),
        inputs.signing_secret.as_bytes(),
    )?;
    let redacted_diagnostic = format!("{headers:?}\n{redacted_credential_diagnostic}");
    assert_omits_secrets(
        redacted_diagnostic.as_bytes(),
        inputs.bot_token.as_bytes(),
        inputs.signing_secret.as_bytes(),
    )?;
    let report = GateReport {
        audit_facts: audits.len(),
        bootstrap_probe_cleaned: bootstrap.used_probe_message,
        callback_dispatches: dispatches_after_revoke.len(),
        files_scanned: 0,
        production_message_cleaned: true,
        revocation_closed: true,
    };
    let log = format!(
        "slack_live_gate tls=true callback_dispatches={} cleanup=true revoked=true\n{}",
        report.callback_dispatches, redacted_diagnostic
    );
    fs::write(directory.path().join("slack-live-gate.log"), log)
        .map_err(|_| GateFailure::DurableStateRejected)?;
    drop(connector);
    drop(verifier);
    drop(framework);
    let files_scanned = scan_directory(
        directory.path(),
        inputs.bot_token.as_bytes(),
        inputs.signing_secret.as_bytes(),
    )?;
    Ok(GateReport {
        files_scanned,
        ..report
    })
}

fn integration_id() -> EnterpriseIntegrationId {
    EnterpriseIntegrationId(portable_id("int", '1'))
}

fn credential_reference_id() -> CredentialReferenceId {
    CredentialReferenceId(portable_id("crd", '2'))
}

fn tenant_scope() -> AuditScope {
    AuditScope::repository(
        OrganizationId(portable_id("org", '3')),
        WorkspaceId(portable_id("wsp", '4')),
        ProjectId(portable_id("prj", '5')),
        RepositoryId(portable_id("rep", '6')),
    )
    .expect("static Slack live-gate scope")
}

fn portable_id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn outbound_request(
    marker: &str,
    operation: &str,
    inputs: &SlackLiveGateInputs,
    interaction_id: &str,
    expected_revision: u64,
    expires_at_millis: u64,
    enqueued_at_millis: u64,
) -> Result<OutboundRequest, GateFailure> {
    OutboundRequest::try_new(
        integration_id(),
        tenant_scope(),
        IntegrationOperationKey::derive(marker).map_err(|_| GateFailure::InvalidConfiguration)?,
        operation,
        serde_json::to_vec(&json!({
            "body": format!("Controlled Slack live-gate message {marker}"),
            "channelId": inputs.channel_id,
            "expiresAtMillis": expires_at_millis,
            "expectedRevision": expected_revision,
            "interactionId": interaction_id,
            "title": marker,
            "workspaceId": inputs.workspace_id,
        }))
        .map_err(|_| GateFailure::InvalidConfiguration)?,
        RetryPolicy::try_new(3, 1_000, 120_000).map_err(|_| GateFailure::InvalidConfiguration)?,
        enqueued_at_millis,
    )
    .map_err(|_| GateFailure::InvalidConfiguration)
}

fn deliver_until_terminal(
    framework: &mut IntegrationFramework,
    integration: &EnterpriseIntegrationId,
    connector: &mut SlackEnterpriseConnector<LiveCredentialPort, SystemSlackClock>,
) -> Result<(), GateFailure> {
    for attempt in 0..3_u8 {
        let now = wall_clock_millis()?;
        let result = framework
            .deliver_next(
                &tenant_scope(),
                integration,
                now,
                lease_id(char::from(b'A' + attempt)),
                now.checked_add(60_000)
                    .ok_or(GateFailure::InvalidConfiguration)?,
                connector,
            )
            .map_err(|_| GateFailure::DurableStateRejected)?
            .ok_or(GateFailure::DurableStateRejected)?;
        match result {
            OutboundAttemptResult::Delivered(receipt)
                if receipt.remote_write_performed() == Some(true) =>
            {
                return Ok(());
            }
            OutboundAttemptResult::RetryScheduled(operation) => {
                let delay = operation.eligible_at_millis().saturating_sub(now);
                if delay == 0 || delay > MAX_LIVE_RETRY_SECONDS * 1_000 {
                    return Err(GateFailure::LiveRetryExceeded);
                }
                thread::sleep(Duration::from_millis(delay));
            }
            OutboundAttemptResult::Delivered(_) | OutboundAttemptResult::DeadLettered(_) => {
                return Err(GateFailure::ApiRejected);
            }
        }
    }
    Err(GateFailure::LiveRetryExceeded)
}

fn lease_id(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(portable_id("igl", tail)).expect("static Slack live-gate lease")
}

fn find_message<'a>(messages: &'a [Value], marker: &str) -> Result<&'a Value, GateFailure> {
    let mut matches = messages
        .iter()
        .filter(|message| message.get("text").and_then(Value::as_str) == Some(marker));
    let message = matches.next().ok_or(GateFailure::InvalidRemoteMessage)?;
    if matches.next().is_some() {
        return Err(GateFailure::InvalidRemoteMessage);
    }
    Ok(message)
}

#[allow(clippy::too_many_arguments)]
fn validate_remote_message(
    message: &Value,
    installation: &SlackInstallationIdentity,
    channel: &str,
    marker: &str,
    interaction_id: &str,
    operation_key: &str,
    expected_revision: u64,
    expires_at_millis: u64,
) -> Result<String, GateFailure> {
    if message
        .get("channel")
        .and_then(Value::as_str)
        .is_some_and(|value| value != channel)
        || message.get("text").and_then(Value::as_str) != Some(marker)
        || message.get("app_id").and_then(Value::as_str) != Some(installation.app_id().as_str())
        || message.get("bot_id").and_then(Value::as_str) != Some(installation.bot_id().as_str())
        || message
            .pointer("/metadata/event_type")
            .and_then(Value::as_str)
            != Some("winwincode_notification")
        || message
            .pointer("/metadata/event_payload/operation_key")
            .and_then(Value::as_str)
            != Some(operation_key)
        || message
            .pointer("/metadata/event_payload/team_id")
            .and_then(Value::as_str)
            != Some(installation.workspace_id().as_str())
        || message
            .pointer("/metadata/event_payload/app_id")
            .and_then(Value::as_str)
            != Some(installation.app_id().as_str())
        || message
            .pointer("/metadata/event_payload/bot_id")
            .and_then(Value::as_str)
            != Some(installation.bot_id().as_str())
    {
        return Err(GateFailure::InvalidRemoteMessage);
    }
    let actions = action_elements(message)?;
    let action_ids = actions
        .iter()
        .filter_map(|action| action.get("action_id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if action_ids != ["approval.approve", "approval.reject", "control-plane.open"] {
        return Err(GateFailure::InvalidRemoteMessage);
    }
    let expected_deep_link = format!(
        "{}/interactions/{}",
        CONTROL_PLANE_BASE_URL.trim_end_matches('/'),
        percent_encode(interaction_id)
    );
    if actions[2].get("url").and_then(Value::as_str) != Some(expected_deep_link.as_str()) {
        return Err(GateFailure::InvalidRemoteMessage);
    }
    for action in &actions[..2] {
        let value = action
            .get("value")
            .and_then(Value::as_str)
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .ok_or(GateFailure::InvalidRemoteMessage)?;
        if value.get("interactionId").and_then(Value::as_str) != Some(interaction_id)
            || value.get("expectedRevision").and_then(Value::as_u64) != Some(expected_revision)
            || value.get("expiresAtMillis").and_then(Value::as_u64) != Some(expires_at_millis)
            || value.get("action").and_then(Value::as_str)
                != action.get("action_id").and_then(Value::as_str)
        {
            return Err(GateFailure::InvalidRemoteMessage);
        }
    }
    required_string(message, &["ts"])
}

fn action_elements(message: &Value) -> Result<&[Value], GateFailure> {
    let blocks = message
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or(GateFailure::InvalidRemoteMessage)?;
    let actions = blocks
        .iter()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("actions"))
        .and_then(|block| block.get("elements"))
        .and_then(Value::as_array)
        .ok_or(GateFailure::InvalidRemoteMessage)?;
    Ok(actions)
}

fn approval_action_value(message: &Value) -> Result<String, GateFailure> {
    action_elements(message)?
        .iter()
        .find(|action| action.get("action_id").and_then(Value::as_str) == Some("approval.approve"))
        .and_then(|action| action.get("value"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(GateFailure::InvalidRemoteMessage)
}

fn interaction_form(
    bootstrap: &BootstrapIdentity,
    channel: &str,
    message_timestamp: &str,
    approve_value: &str,
    received_at_millis: u64,
) -> Result<Vec<u8>, GateFailure> {
    let action_timestamp = format!(
        "{}.{:06}",
        received_at_millis / 1_000,
        (received_at_millis % 1_000) * 1_000
    );
    let interaction = json!({
        "actions": [{
            "action_id": "approval.approve",
            "action_ts": action_timestamp,
            "value": approve_value,
        }],
        "api_app_id": bootstrap.installation.app_id().as_str(),
        "channel": {"id": channel},
        "container": {"channel_id": channel, "message_ts": message_timestamp},
        "team": {"id": bootstrap.installation.workspace_id().as_str()},
        "type": "block_actions",
        "user": {"id": bootstrap.callback_user_id},
    });
    let interaction =
        serde_json::to_string(&interaction).map_err(|_| GateFailure::InvalidConfiguration)?;
    Ok(format!("payload={}", percent_encode(&interaction)).into_bytes())
}

fn signed_headers(
    secret: &[u8],
    body: &[u8],
    timestamp_seconds: u64,
) -> Result<SlackWebhookHeaders, GateFailure> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret).map_err(|_| GateFailure::InvalidConfiguration)?;
    mac.update(b"v0:");
    mac.update(timestamp_seconds.to_string().as_bytes());
    mac.update(b":");
    mac.update(body);
    SlackWebhookHeaders::try_new(
        timestamp_seconds,
        format!("v0={:x}", mac.finalize().into_bytes()),
    )
    .map_err(|_| GateFailure::InvalidConfiguration)
}

fn validate_single_control_plane_command(
    dispatches: &[winwincode_integration::InboundDispatch],
    expected_revision: u64,
    expires_at_millis: u64,
) -> Result<(), GateFailure> {
    let [dispatch] = dispatches else {
        return Err(GateFailure::DurableStateRejected);
    };
    if dispatch.command_name() != "slack.interaction.handle" {
        return Err(GateFailure::DurableStateRejected);
    }
    let command: Value = serde_json::from_slice(dispatch.command_payload())
        .map_err(|_| GateFailure::DurableStateRejected)?;
    if command.get("action").and_then(Value::as_str) != Some("approval.approve")
        || command.get("disposition").and_then(Value::as_str) != Some("active")
        || command.get("expectedRevision").and_then(Value::as_u64) != Some(expected_revision)
        || command.get("expiresAtMillis").and_then(Value::as_u64) != Some(expires_at_millis)
        || command.get("decision").is_some()
        || command.get("approved").is_some()
    {
        return Err(GateFailure::DurableStateRejected);
    }
    Ok(())
}

fn assert_omits_secrets(
    bytes: &[u8],
    bot_token: &[u8],
    signing_secret: &[u8],
) -> Result<(), GateFailure> {
    if find_bytes(bytes, bot_token).is_some() || find_bytes(bytes, signing_secret).is_some() {
        return Err(GateFailure::LeakDetected);
    }
    Ok(())
}

fn scan_directory(
    directory: &Path,
    bot_token: &[u8],
    signing_secret: &[u8],
) -> Result<usize, GateFailure> {
    let mut pending = vec![directory.to_path_buf()];
    let mut files = 0;
    while let Some(path) = pending.pop() {
        let entries = fs::read_dir(path).map_err(|_| GateFailure::DurableStateRejected)?;
        for entry in entries {
            let path = entry.map_err(|_| GateFailure::DurableStateRejected)?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                let bytes = fs::read(path).map_err(|_| GateFailure::DurableStateRejected)?;
                assert_omits_secrets(&bytes, bot_token, signing_secret)?;
                files += 1;
            }
        }
    }
    Ok(files)
}

fn wall_clock_millis() -> Result<u64, GateFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
        .ok_or(GateFailure::InvalidConfiguration)
}

fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

struct TlsSlackFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    request_lines: mpsc::Receiver<String>,
    server: thread::JoinHandle<()>,
}

impl TlsSlackFixture {
    fn start() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate live-gate TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("live-gate TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind live-gate TLS fixture");
        let address = listener.local_addr().expect("live-gate TLS address");
        let (sender, request_lines) = mpsc::channel();
        let messages = Arc::new(Mutex::new(Vec::<Value>::new()));
        let server_messages = Arc::clone(&messages);
        let server = thread::spawn(move || {
            for sequence in 1..=LIVE_REQUEST_COUNT_WITH_BOOTSTRAP {
                let (socket, _) = listener.accept().expect("accept live-gate TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                let request = read_http_request(&mut stream);
                let request_line = String::from_utf8_lossy(
                    request
                        .split(|byte| *byte == b'\n')
                        .next()
                        .unwrap_or_default(),
                )
                .trim_end_matches('\r')
                .to_owned();
                sender
                    .send(request_line)
                    .expect("record live-gate request line");
                let reply = fixture_reply(&request, sequence, &server_messages);
                write_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/", address.port()),
            certificate_der: cert.der().to_vec(),
            request_lines,
            server,
        }
    }

    fn finish(self) -> Vec<String> {
        self.server.join().expect("join live-gate TLS fixture");
        (0..LIVE_REQUEST_COUNT_WITH_BOOTSTRAP)
            .map(|_| {
                self.request_lines
                    .recv()
                    .expect("captured live-gate request line")
            })
            .collect()
    }
}

struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

fn fixture_reply(request: &[u8], sequence: usize, messages: &Arc<Mutex<Vec<Value>>>) -> HttpReply {
    let headers = String::from_utf8_lossy(request);
    let expected_authorization = format!("authorization: bearer {FIXTURE_BOT_TOKEN}");
    if !headers
        .to_ascii_lowercase()
        .contains(&expected_authorization.to_ascii_lowercase())
    {
        return json_reply(&json!({"error": "invalid_auth", "ok": false}));
    }
    let request_line = headers.lines().next().unwrap_or_default();
    if request_line.starts_with("POST /auth.test ") {
        return json_reply(&json!({
            "bot_id": FIXTURE_BOT_ID,
            "ok": true,
            "team_id": FIXTURE_WORKSPACE_ID,
            "user_id": FIXTURE_USER_ID,
        }));
    }
    if request_line.starts_with("POST /chat.postMessage ") {
        let Ok(mut body) = serde_json::from_slice::<Value>(http_request_body(request)) else {
            return json_reply(&json!({"error": "invalid_arguments", "ok": false}));
        };
        let Some(object) = body.as_object_mut() else {
            return json_reply(&json!({"error": "invalid_arguments", "ok": false}));
        };
        let timestamp = format!("1712345678.{sequence:06}");
        object.insert("app_id".to_owned(), json!(FIXTURE_APP_ID));
        object.insert("bot_id".to_owned(), json!(FIXTURE_BOT_ID));
        object.insert("ts".to_owned(), json!(timestamp));
        object.insert("type".to_owned(), json!("message"));
        messages
            .lock()
            .expect("lock live-gate messages")
            .push(body.clone());
        let mut response_message = body.clone();
        if sequence == 2 {
            response_message
                .as_object_mut()
                .expect("fixture message object")
                .remove("app_id");
        }
        return json_reply(&json!({
            "channel": FIXTURE_CHANNEL_ID,
            "message": response_message,
            "ok": true,
            "ts": timestamp,
        }));
    }
    if request_line.starts_with("POST /chat.delete ") {
        let Ok(body) = serde_json::from_slice::<Value>(http_request_body(request)) else {
            return json_reply(&json!({"error": "invalid_arguments", "ok": false}));
        };
        let Some(timestamp) = body.get("ts").and_then(Value::as_str) else {
            return json_reply(&json!({"error": "invalid_arguments", "ok": false}));
        };
        let mut messages = messages.lock().expect("lock live-gate messages");
        let before = messages.len();
        messages.retain(|message| message.get("ts").and_then(Value::as_str) != Some(timestamp));
        if messages.len() == before {
            return json_reply(&json!({"error": "message_not_found", "ok": false}));
        }
        return json_reply(&json!({
            "channel": FIXTURE_CHANNEL_ID,
            "ok": true,
            "ts": timestamp,
        }));
    }
    if request_line.starts_with("GET /conversations.history?") {
        let mut history = messages.lock().expect("lock live-gate messages").clone();
        history.reverse();
        return json_reply(&json!({
            "messages": history,
            "ok": true,
            "response_metadata": {"next_cursor": ""},
        }));
    }
    json_reply(&json!({"error": "invalid_request", "ok": false}))
}

fn json_reply(body: &Value) -> HttpReply {
    HttpReply {
        status: 200,
        body: serde_json::to_vec(body).expect("fixture response JSON"),
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let count = stream.read(&mut buffer).expect("read live-gate request");
        assert_ne!(count, 0, "live-gate request closed before body");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = http_content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return request;
        }
    }
}

fn http_content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn http_request_body(request: &[u8]) -> &[u8] {
    let header_end = find_bytes(request, b"\r\n\r\n").expect("HTTP header terminator");
    &request[header_end + 4..]
}

fn write_http_reply(stream: &mut StreamOwned<ServerConnection, TcpStream>, reply: &HttpReply) {
    let reason = if reply.status == 200 {
        "OK"
    } else {
        "Unprocessable Entity"
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.body.len()
    )
    .expect("write live-gate response headers");
    stream
        .write_all(&reply.body)
        .expect("write live-gate response body");
    stream.flush().expect("flush live-gate response");
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[test]
fn live_gate_configuration_stays_closed_until_all_four_inputs_are_present() {
    let SlackLiveGateConfiguration::Closed { missing } =
        SlackLiveGateConfiguration::inspect(|_| None)
    else {
        panic!("empty Slack live-gate configuration must remain closed");
    };
    assert_eq!(missing, REQUIRED_ENVIRONMENT);

    let configuration = SlackLiveGateConfiguration::inspect(|name| {
        (name == SLACK_BOT_TOKEN_ENV).then(|| FIXTURE_BOT_TOKEN.as_bytes().to_vec())
    });
    let SlackLiveGateConfiguration::Closed { missing } = configuration else {
        panic!("partial Slack live-gate configuration must remain closed");
    };
    assert_eq!(
        missing,
        [
            SLACK_SIGNING_SECRET_ENV,
            SLACK_TEST_WORKSPACE_ID_ENV,
            SLACK_TEST_CHANNEL_ID_ENV,
        ]
    );
    let description = missing.join(",");
    assert!(!description.contains(FIXTURE_BOT_TOKEN));

    let ready = SlackLiveGateConfiguration::inspect(|name| {
        Some(
            match name {
                SLACK_BOT_TOKEN_ENV => FIXTURE_BOT_TOKEN,
                SLACK_SIGNING_SECRET_ENV => FIXTURE_SIGNING_SECRET,
                SLACK_TEST_WORKSPACE_ID_ENV => FIXTURE_WORKSPACE_ID,
                SLACK_TEST_CHANNEL_ID_ENV => FIXTURE_CHANNEL_ID,
                _ => unreachable!("closed Slack live-gate environment set"),
            }
            .as_bytes()
            .to_vec(),
        )
    });
    assert!(matches!(ready, SlackLiveGateConfiguration::Ready(_)));
}

#[test]
fn deterministic_tls_live_gate_covers_callback_revocation_cleanup_and_leak_scan() {
    let fixture = TlsSlackFixture::start();
    let inputs = SlackLiveGateInputs::fixture();
    let report = run_slack_live_gate(
        &inputs,
        &fixture.endpoint,
        &LiveTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
        SlackTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    )
    .expect("deterministic Slack live gate");
    assert_eq!(report.callback_dispatches, 1);
    assert!(report.bootstrap_probe_cleaned);
    assert!(report.production_message_cleaned);
    assert!(report.revocation_closed);
    assert!(report.audit_facts >= 6);
    assert!(report.files_scanned >= 3);

    let requests = fixture.finish();
    assert_eq!(requests.len(), LIVE_REQUEST_COUNT_WITH_BOOTSTRAP);
    assert!(requests[0].starts_with("POST /auth.test "));
    assert!(requests[1].starts_with("POST /chat.postMessage "));
    assert!(requests[2].starts_with("GET /conversations.history?"));
    assert!(requests[3].starts_with("POST /chat.delete "));
    assert!(requests[4].starts_with("GET /conversations.history?"));
    assert!(requests[5].starts_with("POST /chat.postMessage "));
    assert!(requests[6].starts_with("GET /conversations.history?"));
    assert!(requests[7].starts_with("GET /conversations.history?"));
    assert!(requests[8].starts_with("POST /chat.delete "));
    let request_lines = requests.join("\n");
    assert!(!request_lines.contains(FIXTURE_BOT_TOKEN));
    assert!(!request_lines.contains(FIXTURE_SIGNING_SECRET));
}

#[test]
#[ignore = "requires SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET, SLACK_TEST_WORKSPACE_ID, and SLACK_TEST_CHANNEL_ID"]
fn live_slack_tls_callback_single_command_revocation_cleanup_and_leak_gate() {
    let inputs = match SlackLiveGateConfiguration::from_process_environment() {
        SlackLiveGateConfiguration::Ready(inputs) => inputs,
        SlackLiveGateConfiguration::Closed { missing } => {
            panic!(
                "missing required Slack live-gate environment names: {}",
                missing.join(",")
            );
        }
    };
    let report = run_slack_live_gate(
        &inputs,
        REAL_SLACK_API_BASE_URL,
        &LiveTlsRoots::WebPki,
        SlackTlsRoots::WebPki,
    )
    .expect("real Slack live gate failed at a secret-safe stage");
    assert_eq!(report.callback_dispatches, 1);
    assert!(report.production_message_cleaned);
    assert!(report.revocation_closed);
    assert!(report.files_scanned >= 3);
}
