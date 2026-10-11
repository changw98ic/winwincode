// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_device_client::ClientLockState;
use winwincode_device_client::connect_code;
use winwincode_device_client::{
    ConnectCodeStateRecord, DeviceServiceStatus, DeviceStore, device_service_status,
    load_device_identity,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectCodeView {
    pub connect_code_id: String,
    pub generation: u64,
    pub state: String,
    pub expires_at: String,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatusView {
    pub device_id: String,
    pub client_node_id: String,
    pub public_client_id: String,
    pub enrolled: bool,
    pub accepting_connections: bool,
    pub lock_state: String,
    pub running_worker_sessions: u64,
    pub connect_code: Option<ConnectCodeView>,
    pub service: DeviceServiceStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum DeviceAdminOutcome {
    Status {
        device: DeviceStatusView,
    },
    CodeRefreshed {
        code: ConnectCodeView,
        connect_code: String,
        valid_seconds: i64,
    },
    PolicyUpdated {
        accepting_connections: bool,
        lock_state: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceAdminError {
    NotInitialized,
    NotEnrolled,
    Failed { code: &'static str, message: String },
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
    let service = device_service_status(data_directory);
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
            service,
        },
    })
}

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

pub(crate) fn open_store(data_directory: &Path) -> Result<DeviceStore, DeviceAdminError> {
    DeviceStore::open(data_directory).map_err(|error| DeviceAdminError::Failed {
        code: "device-store-open",
        message: error.to_string(),
    })
}

pub(crate) fn store_failed(error: &winwincode_device_client::DeviceStoreError) -> DeviceAdminError {
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
