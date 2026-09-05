// SPDX-License-Identifier: Apache-2.0

//! The `ProductSession` create/continue permission gate (FLOW-100.3).
//!
//! Every local execution session on a Device Client is bound to the caller's
//! *current* occupancy and to a repository that is authorized under the plan
//! 13.4 dual-authorization visibility (an `active` `ClientAccessGrant`
//! carrying `use` on the client node AND an `active` `RepositoryAccessGrant`
//! on the binding). The gate is the single precondition authority both
//! surfaces share:
//!
//! - the Worker launch surface (`POST /api/v1/sessions`) already carries the
//!   explicit `clientId` + `repositoryBindingId` pair and must pass the gate
//!   before a device session is created or continued;
//! - the generated `chat.submit` continue path resolves the same facts from
//!   the durable launch anchor of the session (its newest `WorkerLaunchGrant`)
//!   and enforces the gate whenever the session is device-anchored. Sessions
//!   without a device anchor (pure supervised local execution) pass through
//!   unchanged.
//!
//! Denials never confirm the existence of hidden resources: an unknown
//! binding and a binding of another client read identically.

use std::fmt;

use winwincode_storage::{OccupancyLeaseState, SqliteStorage};

use crate::client_launch_grant::{WorkerLaunchGrantService, WorkerLaunchGrantServiceErrorKind};
use crate::client_occupancy::ClientOccupancyService;
use crate::repository_binding::RepositoryBindingService;

/// The explicit facts one gated create/continue request must carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSessionGateInput<'a> {
    /// Acting user id (the authenticated browser actor).
    pub user_id: &'a str,
    /// Canonical client node identity the session targets.
    pub client_node_id: &'a str,
    /// Repository binding the session executes against.
    pub repository_binding_id: &'a str,
}

/// The occupancy and binding facts one approved request may stamp on the
/// downstream device execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSessionGateApproval {
    pub client_node_id: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: String,
    pub repository_name: String,
}

/// Stable gate failure categories. Each category maps to exactly one wire
/// error code of the central gate table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSessionGateDenialKind {
    /// The request carried missing or empty identities.
    InvalidRequest,
    /// The client has no usable occupancy (none, or not device-confirmed).
    OccupancyRequired,
    /// The caller is not the current occupancy holder.
    AccessDenied,
    /// The binding is unknown, foreign, or invisible to the holder.
    BindingNotVisible,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free gate denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSessionGateDenial {
    kind: DeviceSessionGateDenialKind,
    message: String,
}

impl DeviceSessionGateDenial {
    #[must_use]
    pub const fn kind(&self) -> DeviceSessionGateDenialKind {
        self.kind
    }

    fn new(kind: DeviceSessionGateDenialKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            DeviceSessionGateDenialKind::InvalidRequest,
            "gate request must carry a user, a client, and a repository binding identity",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            DeviceSessionGateDenialKind::Unavailable,
            "device session gate is temporarily unavailable",
        )
    }

    /// The central wire error code the surface must report.
    #[must_use]
    pub fn wire_code(&self) -> &'static str {
        match self.kind {
            DeviceSessionGateDenialKind::InvalidRequest => "INVALID_REQUEST",
            DeviceSessionGateDenialKind::OccupancyRequired => "OCCUPANCY_REQUIRED",
            DeviceSessionGateDenialKind::AccessDenied => "ACCESS_DENIED",
            DeviceSessionGateDenialKind::BindingNotVisible => "BINDING_NOT_VISIBLE",
            DeviceSessionGateDenialKind::Unavailable => "SERVICE_UNAVAILABLE",
        }
    }

    /// The HTTP status the surface must report for this denial.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self.kind {
            DeviceSessionGateDenialKind::InvalidRequest => 400,
            DeviceSessionGateDenialKind::OccupancyRequired => 409,
            DeviceSessionGateDenialKind::AccessDenied
            | DeviceSessionGateDenialKind::BindingNotVisible => 403,
            DeviceSessionGateDenialKind::Unavailable => 503,
        }
    }
}

impl fmt::Display for DeviceSessionGateDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeviceSessionGateDenial {}

/// Judges one explicit create/continue request against the caller's current
/// occupancy and the plan 13.4 dual-authorization visibility.
///
/// # Errors
///
/// Returns the stable gate denials; storage failures deny with
/// [`DeviceSessionGateDenialKind::Unavailable`] and decide nothing.
pub fn authorize_device_session(
    storage: &mut SqliteStorage,
    input: &DeviceSessionGateInput<'_>,
) -> Result<DeviceSessionGateApproval, DeviceSessionGateDenial> {
    if input.user_id.is_empty()
        || input.client_node_id.is_empty()
        || input.repository_binding_id.is_empty()
    {
        return Err(DeviceSessionGateDenial::invalid_request());
    }

    // Occupancy: the caller must hold the client's one active lease and it
    // must be device-confirmed (`occupied` or `draining`).
    let lease = ClientOccupancyService::new(storage)
        .active_lease_for_node(input.client_node_id)
        .map_err(|_| DeviceSessionGateDenial::unavailable())?;
    let Some(lease) = lease else {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::OccupancyRequired,
            "the client is not occupied; claim occupancy before continuing",
        ));
    };
    if lease.holder_user_id != input.user_id {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::AccessDenied,
            "only the current occupancy holder may use this client",
        ));
    }
    if !matches!(
        lease.state,
        OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
    ) {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::OccupancyRequired,
            "the occupancy is not confirmed by the device",
        ));
    }

    // Binding: it must exist, belong to the requested client, and be visible
    // to the holder under the dual-authorization projection.
    let binding = RepositoryBindingService::new(storage)
        .snapshot(input.repository_binding_id)
        .map_err(|_| DeviceSessionGateDenial::unavailable())?;
    let Some(binding) = binding else {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ));
    };
    if binding.client_node_id != input.client_node_id {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ));
    }
    let visible = RepositoryBindingService::new(storage)
        .visible_bindings(input.user_id, input.client_node_id)
        .map_err(|_| DeviceSessionGateDenial::unavailable())?;
    if !visible
        .iter()
        .any(|record| record.repository_binding_id == input.repository_binding_id)
    {
        return Err(DeviceSessionGateDenial::new(
            DeviceSessionGateDenialKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ));
    }

    Ok(DeviceSessionGateApproval {
        client_node_id: input.client_node_id.to_owned(),
        occupancy_lease_id: lease.occupancy_lease_id,
        occupancy_fencing_token: lease.fencing_token,
        repository_binding_id: binding.repository_binding_id,
        repository_name: binding.display_name,
    })
}

/// Judges one `ProductSession` continue (a new Chat turn) against the gate.
///
/// The session's device anchor is its newest durable launch grant, whatever
/// its lifecycle state: the anchor proves the session was bound to device
/// execution and stays the permission anchor after the grant ends. A session
/// without any device anchor is not a device session and passes through with
/// [`None`], so pure supervised local execution is unchanged.
///
/// # Errors
///
/// Returns the stable gate denials for a device-anchored session.
pub fn authorize_product_session_turn(
    storage: &mut SqliteStorage,
    user_id: &str,
    product_session_id: &str,
) -> Result<Option<DeviceSessionGateApproval>, DeviceSessionGateDenial> {
    if user_id.is_empty() || product_session_id.is_empty() {
        return Err(DeviceSessionGateDenial::invalid_request());
    }
    // A non-canonical session identity can never carry a launch anchor; it
    // passes through and the session service rejects it as before.
    let anchor = match WorkerLaunchGrantService::new(storage)
        .newest_grant_for_product_session(product_session_id)
    {
        Ok(anchor) => anchor,
        Err(error) if error.kind() == WorkerLaunchGrantServiceErrorKind::InvalidInput => {
            return Ok(None);
        }
        Err(_) => return Err(DeviceSessionGateDenial::unavailable()),
    };
    let Some(anchor) = anchor else {
        return Ok(None);
    };
    authorize_device_session(
        storage,
        &DeviceSessionGateInput {
            user_id,
            client_node_id: &anchor.client_node_id,
            repository_binding_id: &anchor.repository_binding_id,
        },
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_domain::Instant;
    use winwincode_storage::{
        AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
        GrantSource, GrantTrustMode, LaunchGrantIssuance, OccupancyClaim, OccupancyLeaseState,
        RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
        RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-device-session-gate-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn suffix(seed: u64) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut identity = String::with_capacity(26);
        let mut value = seed;
        for _ in 0..26 {
            let digit = usize::try_from(value % 32).expect("digit fits");
            identity.push(ALPHABET[digit] as char);
            value /= 32;
        }
        identity
    }

    fn open_storage(name: &str) -> SqliteStorage {
        let directory = temporary_directory(name);
        std::fs::create_dir_all(&directory).expect("test directory");
        SqliteStorage::open(&directory).expect("storage")
    }

    /// Registers one online client node and returns its identity.
    fn stage_node(storage: &mut SqliteStorage, seed: u64) -> String {
        let node = format!("cnd_{}", suffix(seed));
        let registration = ClientNodeRegistration::try_new(
            node.clone(),
            format!("{seed:010}"),
            "Gate Test Device".to_owned(),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(format!("cix_{}", suffix(seed + 40))),
            4,
        )
        .expect("registration");
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &instant("2026-01-01T00:00:00.000Z"))
            .expect("register");
        registry
            .update_presence(&node, ClientPresenceState::Online, 1)
            .expect("presence");
        node
    }

    /// Stages one active client `use` grant for the user.
    fn stage_client_grant(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) {
        let issuance = AccessGrantIssuance::try_new(
            format!("cag_{}", suffix(seed)),
            node,
            user,
            user,
            GrantTrustMode::Trusted,
            None,
        )
        .expect("issuance");
        storage
            .client_connect_ledger()
            .expect("ledger")
            .create_grant(
                &issuance,
                GrantSource::Administrator,
                GrantPermissions::USE,
                &instant("2026-01-01T00:00:10.000Z"),
            )
            .expect("grant");
    }

    /// Stages one visible repository binding on the node: an `active`
    /// repository access grant pairs with the client grant (plan 13.4).
    fn stage_visible_binding(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) {
        let binding = format!("rbd_{}", suffix(seed));
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
            node,
            "winwincode",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
            .expect("upsert");
        let issuance = RepositoryAccessGrantIssuance::try_new(
            format!("rag_{}", suffix(seed + 20)),
            &binding,
            user,
            user,
        )
        .expect("repo issuance");
        ledger
            .create_grant(
                &issuance,
                RepositoryGrantPermissions::Use,
                &instant("2026-01-01T00:00:31.000Z"),
            )
            .expect("repo grant");
    }

    /// Stages one repository binding without any repository access grant: it
    /// exists on the client but is invisible to every user.
    fn stage_invisible_binding(storage: &mut SqliteStorage, seed: u64, node: &str) -> String {
        let binding = format!("rbd_{}", suffix(seed));
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
            node,
            "unshared",
            None,
            None,
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
            .expect("upsert");
        binding
    }

    /// Claims occupancy and walks the lease to `occupied` (or `draining`
    /// when `active_sessions` is positive).
    fn stage_occupied_lease(
        storage: &mut SqliteStorage,
        seed: u64,
        node: &str,
        user: &str,
        active_sessions: u64,
    ) -> (String, u64) {
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let claim = OccupancyClaim::try_new(
            format!("ocl_{}", suffix(seed)),
            node,
            user,
            format!("req_{}", suffix(seed + 10)),
        )
        .expect("claim");
        let lease = occupancy
            .atomic_claim(&claim, &instant("2026-01-01T00:01:00.000Z"))
            .expect("claim");
        let occupied = occupancy
            .record_acknowledgement(
                &lease.occupancy_lease_id,
                lease.fencing_token,
                None,
                &instant("2026-01-01T00:01:01.000Z"),
            )
            .expect("ack");
        assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
        if active_sessions > 0 {
            let draining = occupancy
                .request_release(
                    &occupied.occupancy_lease_id,
                    occupied.fencing_token,
                    active_sessions,
                    &instant("2026-01-01T00:01:02.000Z"),
                )
                .expect("release");
            assert_eq!(draining.state, OccupancyLeaseState::Draining);
        }
        (occupied.occupancy_lease_id, occupied.fencing_token)
    }

    /// Anchors one launch grant carrying the product session identity.
    #[allow(clippy::too_many_arguments)]
    fn stage_anchor(
        storage: &mut SqliteStorage,
        seed: u64,
        node: &str,
        user: &str,
        lease_id: &str,
        token: u64,
        binding: &str,
        product_session_id: &str,
    ) -> String {
        let issuance = LaunchGrantIssuance::try_new(
            format!("wlg_{}", suffix(seed)),
            node,
            format!("cix_{}", suffix(seed + 40)),
            user,
            lease_id,
            token,
            binding,
            format!("ws_{}", suffix(seed + 50)),
            format!("wkr_{}", suffix(seed + 51)),
            format!("winst_{}", suffix(seed + 52)),
            "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
            Some(product_session_id.to_owned()),
            Some(format!("run_{}", suffix(seed + 53))),
            instant("2026-01-01T00:10:00.000Z"),
        )
        .expect("issuance");
        WorkerLaunchGrantService::new(storage)
            .issue(&issuance, &instant("2026-01-01T00:02:00.000Z"))
            .expect("issue")
            .worker_launch_grant_id
    }

    fn input<'a>(user: &'a str, node: &'a str, binding: &'a str) -> DeviceSessionGateInput<'a> {
        DeviceSessionGateInput {
            user_id: user,
            client_node_id: node,
            repository_binding_id: binding,
        }
    }

    #[test]
    fn the_current_holder_with_a_visible_binding_is_approved() {
        let mut storage = open_storage("approved");
        let node = stage_node(&mut storage, 1);
        let user = format!("usr_{}", suffix(2));
        stage_client_grant(&mut storage, 3, &node, &user);
        stage_visible_binding(&mut storage, 4, &node, &user);
        let (lease_id, token) = stage_occupied_lease(&mut storage, 5, &node, &user, 0);
        let binding = format!("rbd_{}", suffix(4));

        let approval = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect("approved");
        assert_eq!(approval.client_node_id, node);
        assert_eq!(approval.occupancy_lease_id, lease_id);
        assert_eq!(approval.occupancy_fencing_token, token);
        assert_eq!(approval.repository_binding_id, binding);
        assert_eq!(approval.repository_name, "winwincode");
    }

    #[test]
    fn a_draining_holder_stays_approved() {
        let mut storage = open_storage("draining");
        let node = stage_node(&mut storage, 11);
        let user = format!("usr_{}", suffix(12));
        stage_client_grant(&mut storage, 13, &node, &user);
        stage_visible_binding(&mut storage, 14, &node, &user);
        stage_occupied_lease(&mut storage, 15, &node, &user, 1);
        let binding = format!("rbd_{}", suffix(14));

        let approval = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect("draining holder stays approved");
        assert_eq!(approval.repository_name, "winwincode");
    }

    #[test]
    fn missing_ids_are_invalid_requests() {
        let mut storage = open_storage("shape");
        let denial = authorize_device_session(&mut storage, &input("", "cnd_a", "rbd_a"))
            .expect_err("empty user denied");
        assert_eq!(denial.kind(), DeviceSessionGateDenialKind::InvalidRequest);
        assert_eq!(denial.wire_code(), "INVALID_REQUEST");
        assert_eq!(denial.http_status(), 400);
    }

    #[test]
    fn a_client_without_occupancy_requires_occupancy() {
        let mut storage = open_storage("no-occupancy");
        let node = stage_node(&mut storage, 21);
        let user = format!("usr_{}", suffix(22));
        stage_client_grant(&mut storage, 23, &node, &user);
        stage_visible_binding(&mut storage, 24, &node, &user);
        let binding = format!("rbd_{}", suffix(24));

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect_err("unoccupied client denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::OccupancyRequired
        );
        assert_eq!(denial.wire_code(), "OCCUPANCY_REQUIRED");
        assert_eq!(denial.http_status(), 409);
    }

    #[test]
    fn an_unconfirmed_occupancy_requires_occupancy() {
        let mut storage = open_storage("reserving");
        let node = stage_node(&mut storage, 31);
        let user = format!("usr_{}", suffix(32));
        stage_client_grant(&mut storage, 33, &node, &user);
        stage_visible_binding(&mut storage, 34, &node, &user);
        // The lease stays `reserving`: the device never confirmed it.
        let claim = OccupancyClaim::try_new(
            format!("ocl_{}", suffix(35)),
            &node,
            &user,
            format!("req_{}", suffix(36)),
        )
        .expect("claim");
        storage
            .client_occupancy_ledger()
            .expect("ledger")
            .atomic_claim(&claim, &instant("2026-01-01T00:01:00.000Z"))
            .expect("claim");
        let binding = format!("rbd_{}", suffix(34));

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect_err("reserving lease denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::OccupancyRequired
        );
    }

    #[test]
    fn a_non_holder_is_access_denied() {
        let mut storage = open_storage("foreign-holder");
        let node = stage_node(&mut storage, 41);
        let holder = format!("usr_{}", suffix(42));
        let other = format!("usr_{}", suffix(43));
        stage_client_grant(&mut storage, 44, &node, &holder);
        stage_visible_binding(&mut storage, 45, &node, &holder);
        stage_occupied_lease(&mut storage, 46, &node, &holder, 0);
        let binding = format!("rbd_{}", suffix(45));

        let denial = authorize_device_session(&mut storage, &input(&other, &node, &binding))
            .expect_err("non-holder denied");
        assert_eq!(denial.kind(), DeviceSessionGateDenialKind::AccessDenied);
        assert_eq!(denial.wire_code(), "ACCESS_DENIED");
        assert_eq!(denial.http_status(), 403);
    }

    #[test]
    fn an_unknown_binding_reads_as_not_visible() {
        let mut storage = open_storage("unknown-binding");
        let node = stage_node(&mut storage, 51);
        let user = format!("usr_{}", suffix(52));
        stage_client_grant(&mut storage, 53, &node, &user);
        stage_occupied_lease(&mut storage, 55, &node, &user, 0);
        let unknown_binding = format!("rbd_{}", suffix(54));

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &unknown_binding))
            .expect_err("unknown binding denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::BindingNotVisible
        );
        assert_eq!(denial.wire_code(), "BINDING_NOT_VISIBLE");
        assert_eq!(denial.http_status(), 403);
    }

    #[test]
    fn a_binding_of_another_client_reads_as_not_visible() {
        let mut storage = open_storage("foreign-binding");
        let node = stage_node(&mut storage, 61);
        let other_node = stage_node(&mut storage, 71);
        let user = format!("usr_{}", suffix(62));
        stage_client_grant(&mut storage, 63, &node, &user);
        stage_visible_binding(&mut storage, 64, &node, &user);
        stage_occupied_lease(&mut storage, 65, &node, &user, 0);
        // The binding exists and is visible, but on the other client.
        stage_visible_binding(&mut storage, 74, &other_node, &user);
        let foreign_binding = format!("rbd_{}", suffix(74));

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &foreign_binding))
            .expect_err("foreign binding denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::BindingNotVisible
        );
    }

    #[test]
    fn a_binding_without_a_repository_grant_is_not_visible() {
        let mut storage = open_storage("invisible-binding");
        let node = stage_node(&mut storage, 81);
        let user = format!("usr_{}", suffix(82));
        stage_client_grant(&mut storage, 83, &node, &user);
        stage_occupied_lease(&mut storage, 85, &node, &user, 0);
        let binding = stage_invisible_binding(&mut storage, 84, &node);

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect_err("ungranted binding denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::BindingNotVisible
        );
    }

    #[test]
    fn a_revoked_repository_grant_denies_the_next_use() {
        let mut storage = open_storage("revoked-grant");
        let node = stage_node(&mut storage, 91);
        let user = format!("usr_{}", suffix(92));
        stage_client_grant(&mut storage, 93, &node, &user);
        stage_visible_binding(&mut storage, 94, &node, &user);
        stage_occupied_lease(&mut storage, 95, &node, &user, 0);
        let binding = format!("rbd_{}", suffix(94));

        let record = storage
            .repository_binding_ledger()
            .expect("ledger")
            .active_grants_for_binding(&binding)
            .expect("active grants")
            .into_iter()
            .next()
            .expect("one active grant");
        storage
            .repository_binding_ledger()
            .expect("ledger")
            .revoke_grant(&record.repository_access_grant_id, record.revision)
            .expect("revoke");

        let denial = authorize_device_session(&mut storage, &input(&user, &node, &binding))
            .expect_err("revoked grant denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::BindingNotVisible
        );
    }

    #[test]
    fn a_turn_without_a_device_anchor_passes_through() {
        let mut storage = open_storage("no-anchor");
        let session = format!("psn_{}", suffix(150));
        let approved = authorize_product_session_turn(&mut storage, "usr_anyone", &session)
            .expect("pass-through");
        assert!(approved.is_none());
    }

    #[test]
    fn an_anchored_turn_enforces_the_gate() {
        let mut storage = open_storage("anchored");
        let node = stage_node(&mut storage, 101);
        let user = format!("usr_{}", suffix(102));
        stage_client_grant(&mut storage, 103, &node, &user);
        stage_visible_binding(&mut storage, 104, &node, &user);
        let (lease_id, token) = stage_occupied_lease(&mut storage, 105, &node, &user, 0);
        let binding = format!("rbd_{}", suffix(104));
        let session = format!("psn_{}", suffix(154));
        stage_anchor(
            &mut storage,
            106,
            &node,
            &user,
            &lease_id,
            token,
            &binding,
            &session,
        );

        let approved = authorize_product_session_turn(&mut storage, &user, &session)
            .expect("anchored turn approved");
        let approved = approved.expect("device-anchored session");
        assert_eq!(approval_facts(&approved), (node.clone(), binding.clone()));

        // The holder releases the client: the next turn is refused even
        // though the anchor grant is still durable.
        storage
            .client_occupancy_ledger()
            .expect("ledger")
            .request_release(&lease_id, token, 0, &instant("2026-01-01T00:03:00.000Z"))
            .expect("release");
        let denial = authorize_product_session_turn(&mut storage, &user, &session)
            .expect_err("released occupancy denied");
        assert_eq!(
            denial.kind(),
            DeviceSessionGateDenialKind::OccupancyRequired
        );
    }

    #[test]
    fn the_newest_anchor_grant_wins() {
        let mut storage = open_storage("newest-anchor");
        let node = stage_node(&mut storage, 111);
        let user = format!("usr_{}", suffix(112));
        stage_client_grant(&mut storage, 113, &node, &user);
        stage_visible_binding(&mut storage, 114, &node, &user);
        let (lease_id, token) = stage_occupied_lease(&mut storage, 115, &node, &user, 0);
        let binding = format!("rbd_{}", suffix(114));
        let session = format!("psn_{}", suffix(155));
        stage_anchor(
            &mut storage,
            116,
            &node,
            &user,
            &lease_id,
            token,
            &binding,
            &session,
        );
        stage_anchor(
            &mut storage,
            126,
            &node,
            &user,
            &lease_id,
            token,
            &binding,
            &session,
        );

        let approved = authorize_product_session_turn(&mut storage, &user, &session)
            .expect("anchored turn approved")
            .expect("device-anchored session");
        assert_eq!(approval_facts(&approved), (node, binding));
    }

    fn approval_facts(approval: &DeviceSessionGateApproval) -> (String, String) {
        (
            approval.client_node_id.clone(),
            approval.repository_binding_id.clone(),
        )
    }
}
