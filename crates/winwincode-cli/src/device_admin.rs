// SPDX-License-Identifier: Apache-2.0

//! Owner-facing Device Client local display and control surface for the
//! `wwc` CLI (plan 11.1 local display, §16.8: the CLI is always the
//! no-desktop fallback).
//!
//! The commands drive the canonical [`winwincode_device_client`] library —
//! the same connect-code lifecycle the daemon uses — and never re-implement
//! code generation, digesting, or policy persistence. Three boundaries are
//! deliberate:
//!
//! - The plaintext connect code exists only inside the process that
//!   generated it. `refresh-code` reveals it exactly once (the same
//!   reveal-once discipline as `user create`'s temporary password); `status`
//!   can only ever show the digest-era metadata.
//! - Publishing requires an adopted enrollment, so a pending publication
//!   frame can never be stranded on the placeholder stream the daemon could
//!   not re-key.
//! - One CLI run is one process launch: `refresh-code` rotates the
//!   `clientInstanceId` like every Device Client launch does.

use std::fmt;
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_device_client::ClientLockState;
use winwincode_device_client::connect_code;
use winwincode_device_client::{
    ConnectCodeStateRecord, DeviceStore, ensure_device_identity, load_device_identity,
};

/// Secret-free view of the published connect code (plan 11.1 display row).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectCodeView {
    /// Canonical `cct_` connect code id.
    pub connect_code_id: String,
    /// Monotonic publication generation.
    pub generation: u64,
    /// Lifecycle state (`active` or `revoked`).
    pub state: String,
    /// RFC 3339 expiry stamp.
    pub expires_at: String,
    /// Whole seconds until expiry; negative once the window passed.
    pub remaining_seconds: Option<i64>,
}

impl ConnectCodeView {
    fn of(record: &ConnectCodeStateRecord, now: OffsetDateTime) -> Self {
        let remaining_seconds = OffsetDateTime::parse(&record.expires_at, &Rfc3339)
            .ok()
            .map(|expires_at| (expires_at - now).whole_seconds());
        Self {
            connect_code_id: record.connect_code_id.clone(),
            generation: record.generation,
            state: connect_code_state_label(record.state).to_owned(),
            expires_at: record.expires_at.clone(),
            remaining_seconds,
        }
    }
}

const fn connect_code_state_label(
    state: winwincode_device_client::ConnectCodeState,
) -> &'static str {
    match state {
        winwincode_device_client::ConnectCodeState::Active => "active",
        winwincode_device_client::ConnectCodeState::Consumed => "consumed",
        winwincode_device_client::ConnectCodeState::Expired => "expired",
        winwincode_device_client::ConnectCodeState::Revoked => "revoked",
    }
}

const fn lock_state_label(lock_state: ClientLockState) -> &'static str {
    match lock_state {
        ClientLockState::Unlocked => "unlocked",
        ClientLockState::Locked => "locked",
    }
}

/// Secret-free Device Client status view (plan 11.1 local display).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatusView {
    /// Stable local device id.
    pub device_id: String,
    /// Server-assigned `clientNodeId`; empty before enrollment.
    pub client_node_id: String,
    /// Server-assigned public `Client ID`; empty before enrollment.
    pub public_client_id: String,
    /// Whether the enrollment was adopted.
    pub enrolled: bool,
    /// Whether the node currently accepts new connections.
    pub accepting_connections: bool,
    /// Machine-level lock state.
    pub lock_state: String,
    /// Worker sessions currently in the `running` state of the local
    /// worker-process registry (WORKER-100.2).
    pub running_worker_sessions: u64,
    /// The published connect code, if any.
    pub connect_code: Option<ConnectCodeView>,
}

/// Human- and JSON-readable result of one `wwc device` command.
///
/// The plaintext connect code appears only in `CodeRefreshed`; it is never
/// persisted or shown again by any later command.
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum DeviceAdminOutcome {
    /// The durable device status (no plaintext code material).
    Status {
        /// The device view.
        device: DeviceStatusView,
    },
    /// A new connect code was published; the plaintext is revealed once.
    CodeRefreshed {
        /// The published code's metadata.
        code: ConnectCodeView,
        /// The only plaintext reveal of this generation.
        connect_code: String,
        /// Whole seconds until the code expires.
        valid_seconds: i64,
    },
    /// The lock policy changed, or was already in the requested state.
    PolicyUpdated {
        /// Whether new connections are accepted.
        accepting_connections: bool,
        /// Machine-level lock state.
        lock_state: String,
    },
}

/// Failure of one `wwc device` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceAdminError {
    /// The data directory holds no device identity: the Device Client never
    /// ran here.
    NotInitialized,
    /// The device has not completed enrollment with a Server, so a published
    /// code could never be re-keyed onto the assigned node.
    NotEnrolled,
    /// The command cannot be completed. `code` is stable for scripting.
    Failed {
        /// Stable machine-readable failure code.
        code: &'static str,
        /// Human-readable explanation in the CLI language.
        message: String,
    },
}

impl fmt::Display for DeviceAdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => formatter.write_str("该数据目录还没有设备身份"),
            Self::NotEnrolled => formatter.write_str("设备尚未完成 enrollment 注册"),
            Self::Failed { code, message } => write!(formatter, "[{code}] {message}"),
        }
    }
}

/// Loads the durable device status (plan 11.1 display).
///
/// # Errors
///
/// Returns [`DeviceAdminError::NotInitialized`] before the first device
/// boot, and [`DeviceAdminError::Failed`] when the store cannot serve the
/// read.
pub fn device_status(data_directory: &Path) -> Result<DeviceAdminOutcome, DeviceAdminError> {
    if !data_directory.exists() {
        return Err(DeviceAdminError::NotInitialized);
    }
    let store = open_store(data_directory)?;
    let Some(identity) = load_device_identity(&store).map_err(|error| store_failed(&error))? else {
        return Err(DeviceAdminError::NotInitialized);
    };
    let now = OffsetDateTime::now_utc();
    let code = store
        .connect_code_state()
        .map_err(|error| store_failed(&error))?
        .as_ref()
        .map(|record| ConnectCodeView::of(record, now));
    let policy = connect_code::connection_policy(&store).map_err(|error| store_failed(&error))?;
    let running_worker_sessions = store
        .count_worker_processes_in_state(winwincode_device_client::WORKER_STATE_RUNNING)
        .map_err(|error| store_failed(&error))?;
    Ok(DeviceAdminOutcome::Status {
        device: DeviceStatusView {
            device_id: identity.identity().device_id().to_owned(),
            client_node_id: identity.identity().client_node_id().to_owned(),
            public_client_id: identity.identity().public_client_id().to_owned(),
            enrolled: identity.identity().is_enrolled(),
            accepting_connections: policy.accepting_connections,
            lock_state: lock_state_label(policy.lock_state).to_owned(),
            running_worker_sessions,
            connect_code: code,
        },
    })
}

/// Generates and publishes a fresh dynamic connect code (plan 11.3), then
/// reveals the plaintext exactly once.
///
/// The durable publication frame rides the outbox (persist-before-send) and
/// reaches the Server with the daemon's next exchange; the previous code
/// generation stops validating challenges immediately.
///
/// # Errors
///
/// Returns [`DeviceAdminError::NotInitialized`] before the first device
/// boot, [`DeviceAdminError::NotEnrolled`] before the enrollment adoption,
/// and [`DeviceAdminError::Failed`] for durable failures.
pub fn refresh_device_connect_code(
    data_directory: &Path,
) -> Result<DeviceAdminOutcome, DeviceAdminError> {
    let mut store = open_store(data_directory)?;
    let Some(identity) = load_device_identity(&store).map_err(|error| store_failed(&error))? else {
        return Err(DeviceAdminError::NotInitialized);
    };
    if !identity.identity().is_enrolled() {
        return Err(DeviceAdminError::NotEnrolled);
    }
    // One CLI run is one process launch: rotate the launch instance id.
    let identity = ensure_device_identity(&mut store, &rotation_seed(), &now_rfc3339())
        .map_err(|error| store_failed(&error))?;
    let client_node_id = identity.identity().client_node_id().to_owned();
    let client_instance_id = identity.current_instance_id().to_owned();
    store
        .bind_outbox_stream(&client_node_id, &client_instance_id)
        .map_err(|error| store_failed(&error))?;
    let now = OffsetDateTime::now_utc();
    let published = connect_code::publish_connect_code(
        &mut store,
        &client_instance_id,
        now,
        connect_code::CONNECT_CODE_TTL,
    )
    .map_err(|error| connect_code_failed(&error))?;
    connect_code::enqueue_published_frame(
        &mut store,
        &client_node_id,
        &client_instance_id,
        &published.record,
        now,
    )
    .map_err(|error| connect_code_failed(&error))?;
    let valid_seconds = i64::try_from(connect_code::CONNECT_CODE_TTL.as_secs()).unwrap_or(i64::MAX);
    Ok(DeviceAdminOutcome::CodeRefreshed {
        code: ConnectCodeView::of(&published.record, now),
        connect_code: published.plaintext.expose().to_owned(),
        valid_seconds,
    })
}

/// Locks or unlocks the Client locally (plan 11.1 `锁定 Client`): locking
/// sets `acceptingConnections = false` plus `lockState = locked`, durably.
/// While locked, every access challenge is refused.
///
/// Requires an existing device identity: a lock landing on a typo'd data
/// directory must fail loudly instead of silently locking a nonexistent
/// device.
///
/// # Errors
///
/// Returns [`DeviceAdminError::NotInitialized`] before the first device
/// boot, and [`DeviceAdminError::Failed`] when the store cannot serve the
/// write.
pub fn set_device_lock(
    data_directory: &Path,
    locked: bool,
) -> Result<DeviceAdminOutcome, DeviceAdminError> {
    let mut store = open_store(data_directory)?;
    if load_device_identity(&store)
        .map_err(|error| store_failed(&error))?
        .is_none()
    {
        return Err(DeviceAdminError::NotInitialized);
    }
    let lock_state = if locked {
        ClientLockState::Locked
    } else {
        ClientLockState::Unlocked
    };
    let policy = connect_code::set_connection_policy(
        &mut store,
        !locked,
        lock_state,
        OffsetDateTime::now_utc(),
    )
    .map_err(|error| store_failed(&error))?;
    Ok(DeviceAdminOutcome::PolicyUpdated {
        accepting_connections: policy.accepting_connections,
        lock_state: lock_state_label(policy.lock_state).to_owned(),
    })
}

fn open_store(data_directory: &Path) -> Result<DeviceStore, DeviceAdminError> {
    DeviceStore::open(data_directory).map_err(|error| DeviceAdminError::Failed {
        code: "device-store-open",
        message: error.to_string(),
    })
}

fn store_failed(error: &winwincode_device_client::DeviceStoreError) -> DeviceAdminError {
    DeviceAdminError::Failed {
        code: "device-store",
        message: error.to_string(),
    }
}

fn connect_code_failed(error: &connect_code::ConnectCodeError) -> DeviceAdminError {
    let code = match error {
        connect_code::ConnectCodeError::NotEnrolled => return DeviceAdminError::NotEnrolled,
        connect_code::ConnectCodeError::Store(_) => "device-connect-code-store",
        connect_code::ConnectCodeError::Protocol(_) => "device-connect-code",
    };
    DeviceAdminError::Failed {
        code,
        message: error.to_string(),
    }
}

/// The identity seed for the launch-instance rotation.
///
/// The rotation path of `ensure_device_identity` only validates the seed and
/// rewrites the `clientInstanceId` — the stored device description of an
/// existing identity is never overwritten, so these placeholder values exist
/// purely to pass that validation. `refresh-code` refuses to run before an
/// identity exists, so the fresh-boot write path is unreachable here.
fn rotation_seed() -> winwincode_device_client::DeviceIdentitySeed {
    winwincode_device_client::DeviceIdentitySeed {
        display_name: "wwc device refresh-code".to_owned(),
        platform: "cli".to_owned(),
        architecture: "cli".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
