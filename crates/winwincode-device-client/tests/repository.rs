// SPDX-License-Identifier: Apache-2.0

//! Repository registry coverage (plan 8.1, 13.1–13.3, 13.5): the
//! registration check chain against real temporary Git repositories, the
//! path-free `client.repository.upsert` / `removed` / `status` frames,
//! launch-time revalidation (including directory removal and symlink
//! replacement), and concurrent registrations of different directories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use sha2::Digest as _;
use time::OffsetDateTime;
use winwincode_client_port::domain::{
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
};
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::{
    ClientRepositoryRemovedPayload, ClientRepositoryStatusPayload, ClientRepositoryUpsertPayload,
    ClientToServerEnvelope, ClientToServerMessage,
};
use winwincode_device_client::repository::{
    RegistrationOptions, RepositoryRegistryError, list_bindings, register_repository,
    remove_repository, repository_fingerprint, revalidate_repository,
};
use winwincode_device_client::{
    DeviceStore, IssuedEnrollment, PathMappingRecord, ensure_device_identity, load_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

const ASSIGNED_NODE: &str = "cnd_C1C1C1C1C1C1C1C1C1C1C1C1C1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "1029384756";
const ISSUED_SECRET: [u8; 32] = [0x5c; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-client-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn seed() -> winwincode_device_client::DeviceIdentitySeed {
    winwincode_device_client::DeviceIdentitySeed {
        display_name: "Repository Registry Tests".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Fails loudly unless the system git is on PATH: every scenario shells out
/// to real Git, so a missing binary must fail the suite, not skip it.
fn require_git() {
    let available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(available, "system git must be available on PATH");
}

/// Runs one git command with an isolated configuration and fails on error.
fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// One real temporary Git repository with a baseline commit.
struct GitRepository {
    root: PathBuf,
}

impl GitRepository {
    fn create(base: &Path, name: &str) -> Self {
        require_git();
        let root = base.join(name);
        fs::create_dir_all(&root).expect("repository directory");
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "registry@example.test"]);
        git(&root, &["config", "user.name", "Registry Tests"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-m", "baseline"]);
        Self { root }
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn branch(&self) -> String {
        git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
    }

    fn commit(&self, message: &str) -> String {
        git(&self.root, &["commit", "--allow-empty", "-m", message]);
        self.head()
    }

    fn make_dirty(&self) {
        fs::write(self.root.join("untracked.txt"), "local change".as_bytes()).expect("dirty file");
    }
}

/// One enrolled device with its own data directory and bound outbox stream.
struct TestDevice {
    data_directory: PathBuf,
}

impl TestDevice {
    fn new(name: &str) -> Self {
        let data_directory = temporary_directory(name);
        fs::create_dir_all(&data_directory).expect("data directory");
        Self { data_directory }
    }

    /// Opens the store in the state a first accepted exchange reaches:
    /// adopted enrollment plus a bound durable outbox stream.
    fn enrolled_store(&self) -> (DeviceStore, String, String) {
        let mut store = DeviceStore::open(&self.data_directory).expect("device store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        let device_id = load_device_identity(&store)
            .expect("identity read")
            .expect("fresh identity")
            .identity()
            .device_id()
            .to_owned();
        let mut material_hex = String::with_capacity(ISSUED_SECRET.len() * 2);
        for byte in ISSUED_SECRET {
            use std::fmt::Write as _;
            let _ = write!(material_hex, "{byte:02x}");
        }
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(ISSUED_SECRET));
        winwincode_device_client::adopt_enrollment(
            &mut store,
            &device_id,
            &IssuedEnrollment {
                client_node_id: ASSIGNED_NODE.to_owned(),
                public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
                credential_material: material_hex,
                credential_digest: digest,
            },
            STAMP,
        )
        .expect("enrollment adopts");
        let identity = ensure_device_identity(&mut store, &seed(), STAMP).expect("relaunch");
        let node = identity.identity().client_node_id().to_owned();
        let instance = identity.current_instance_id().to_owned();
        store
            .bind_outbox_stream(&node, &instance)
            .expect("outbox binds");
        (store, node, instance)
    }
}

fn register(
    store: &mut DeviceStore,
    node: &str,
    instance: &str,
    path: &Path,
    confirm_git_init: bool,
) -> Result<winwincode_device_client::repository::RepositoryRegistration, RepositoryRegistryError> {
    register_repository(
        store,
        node,
        instance,
        path,
        &RegistrationOptions { confirm_git_init },
        now(),
    )
}

fn revalidate(
    store: &mut DeviceStore,
    node: &str,
    instance: &str,
    binding_id: &str,
) -> Result<winwincode_device_client::repository::RepositoryRevalidation, RepositoryRegistryError> {
    revalidate_repository(store, node, instance, binding_id, now())
}

/// The `(kind, envelope)` pairs of every pending durable frame, in order.
fn pending_frames(store: &DeviceStore) -> Vec<(String, ClientToServerEnvelope)> {
    store
        .pending_outbox_envelopes()
        .expect("pending frames read")
        .into_iter()
        .map(|entry| {
            let envelope: ClientToServerEnvelope =
                serde_json::from_slice(&entry.payload).expect("pending frame decodes");
            assert_eq!(envelope.client_node_id, ASSIGNED_NODE);
            (entry.kind, envelope)
        })
        .collect()
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn upsert_payload(envelope: &ClientToServerEnvelope) -> &ClientRepositoryUpsertPayload {
    match &envelope.message {
        ClientToServerMessage::RepositoryUpsert(payload) => payload,
        other => panic!("expected a repository.upsert frame, got {other:?}"),
    }
}

fn removed_payload(envelope: &ClientToServerEnvelope) -> &ClientRepositoryRemovedPayload {
    match &envelope.message {
        ClientToServerMessage::RepositoryRemoved(payload) => payload,
        other => panic!("expected a repository.removed frame, got {other:?}"),
    }
}

fn status_payload(envelope: &ClientToServerEnvelope) -> &ClientRepositoryStatusPayload {
    match &envelope.message {
        ClientToServerMessage::RepositoryStatus(payload) => payload,
        other => panic!("expected a repository.status frame, got {other:?}"),
    }
}

#[test]
fn registration_of_git_repository_reports_full_projection() {
    let base = temporary_directory("repo-register");
    let repository = GitRepository::create(&base, "repo-alpha");
    let device = TestDevice::new("repo-register-device");
    let (mut store, node, instance) = device.enrolled_store();

    let registration = register(&mut store, &node, &instance, &repository.root, false)
        .expect("git repository registers");
    let binding_id = &registration.repository_binding_id;
    assert_eq!(binding_id.len(), "rbd_".len() + 26);
    assert!(binding_id.starts_with("rbd_"));
    assert!(
        binding_id.chars().skip(4).all(|c| c.is_ascii_digit()
            || matches!(c, 'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z')),
        "binding id must be canonical Crockford, got {binding_id}"
    );

    // The local path mapping carries the absolute facts.
    let mapping = store.path_mapping(binding_id).expect("mapping read");
    let expected: PathMappingRecord = PathMappingRecord {
        repository_binding_id: binding_id.clone(),
        canonical_path: fs::canonicalize(&repository.root)
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned(),
        git_common_directory: Some(
            fs::canonicalize(repository.root.join(".git"))
                .expect("common dir")
                .to_string_lossy()
                .into_owned(),
        ),
        last_canonicalized_at: Some(registration.projection.last_scanned_at.clone()),
        local_state: "available".to_owned(),
    };
    assert_eq!(mapping, Some(expected));

    // The durable scan projection carries the scan facts.
    let scan = store
        .repository_local_state(binding_id)
        .expect("scan read")
        .expect("scan exists");
    assert_eq!(scan.availability, RepositoryAvailability::Available);
    assert_eq!(scan.dirty_state, RepositoryDirtyState::Clean);
    assert_eq!(
        scan.head_commit.as_deref(),
        Some(repository.head().as_str())
    );

    // The upsert frame is the only pending frame and is path-free.
    let frames = pending_frames(&store);
    assert_eq!(frames.len(), 1);
    let (kind, envelope) = &frames[0];
    assert_eq!(kind, "client.repository.upsert");
    let payload = upsert_payload(envelope);
    let projection: &RepositoryBindingProjection = &payload.repository;
    assert_eq!(
        projection.repository_binding_id,
        registration.repository_binding_id
    );
    assert_eq!(projection.display_name, "repo-alpha");
    assert_eq!(
        projection.repository_kind,
        winwincode_client_port::domain::RepositoryKind::Git
    );
    assert_eq!(projection.default_branch, repository.branch());
    assert_eq!(projection.head_commit, repository.head());
    assert_eq!(projection.dirty_state, RepositoryDirtyState::Clean);
    assert_eq!(projection.availability, RepositoryAvailability::Available);
    assert_eq!(
        projection.repository_fingerprint,
        repository_fingerprint(&repository.head(), &repository.branch())
    );
    assert_eq!(payload.command.expected_revision, 0);
    assert!(
        payload
            .command
            .idempotency_key
            .starts_with("repository-upsert-")
    );

    // The encoded frame carries no absolute path anywhere, and its stored
    // payload digest convention still holds.
    let encoded = serde_json::to_string(envelope).expect("frame encodes");
    let root_text = fs::canonicalize(&repository.root)
        .expect("canonicalize")
        .to_string_lossy()
        .into_owned();
    let parent_text = Path::new(&root_text)
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned();
    assert!(
        !encoded.contains(&root_text),
        "frame carries the path: {encoded}"
    );
    assert!(
        !encoded.contains(&parent_text),
        "frame carries a parent path: {encoded}"
    );
    let identity = FrameCodec::envelope_identity(envelope).expect("frame digest");
    assert!(identity.payload_digest.starts_with("sha256:"));
    assert!(!identity.payload_digest.contains(&root_text));

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_refuses_non_git_directory_without_confirmation() {
    let base = temporary_directory("repo-not-git");
    let plain = base.join("plain-directory");
    fs::create_dir_all(&plain).expect("plain directory");
    let device = TestDevice::new("repo-not-git-device");
    let (mut store, node, instance) = device.enrolled_store();

    let rejection = register(&mut store, &node, &instance, &plain, false)
        .expect_err("non-git directory is refused");
    let RepositoryRegistryError::Rejected(rejection) = rejection else {
        panic!("expected a rejection, got {rejection:?}");
    };
    assert_eq!(rejection.availability, RepositoryAvailability::InvalidGit);

    // A refused registration persists nothing and reports nothing.
    assert!(store.path_mappings().expect("mappings").is_empty());
    assert!(pending_frames(&store).is_empty());

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_with_confirmation_initializes_git() {
    let base = temporary_directory("repo-init");
    let plain = base.join("init-directory");
    fs::create_dir_all(&plain).expect("plain directory");
    let device = TestDevice::new("repo-init-device");
    let (mut store, node, instance) = device.enrolled_store();

    let registration = register(&mut store, &node, &instance, &plain, true)
        .expect("confirmed registration initializes Git");
    assert!(registration.git_initialized_by_registration);
    let canonical = fs::canonicalize(&plain).expect("canonicalize");
    assert!(canonical.join(".git").is_dir(), "git init ran");

    // A freshly initialized repository has an unborn branch: a healthy
    // scan with an empty HEAD.
    let branch = git(&canonical, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    assert_eq!(registration.projection.default_branch, branch);
    assert_eq!(registration.projection.head_commit, "");
    assert_eq!(
        registration.projection.repository_fingerprint,
        repository_fingerprint("", &branch)
    );
    assert_eq!(
        registration.projection.availability,
        RepositoryAvailability::Available
    );
    assert_eq!(pending_frames(&store).len(), 1);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_of_missing_directory_maps_to_unavailable() {
    let base = temporary_directory("repo-missing");
    let device = TestDevice::new("repo-missing-device");
    let (mut store, node, instance) = device.enrolled_store();

    let rejection = register(
        &mut store,
        &node,
        &instance,
        &base.join("never-existed"),
        false,
    )
    .expect_err("missing directory is refused");
    let RepositoryRegistryError::Rejected(rejection) = rejection else {
        panic!("expected a rejection, got {rejection:?}");
    };
    assert_eq!(rejection.availability, RepositoryAvailability::Unavailable);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_of_dirty_working_tree_reports_dirty_state() {
    let base = temporary_directory("repo-dirty");
    let repository = GitRepository::create(&base, "repo-dirty-worktree");
    repository.make_dirty();
    let device = TestDevice::new("repo-dirty-device");
    let (mut store, node, instance) = device.enrolled_store();

    let registration = register(&mut store, &node, &instance, &repository.root, false)
        .expect("dirty repository registers");
    assert_eq!(
        registration.projection.availability,
        RepositoryAvailability::Dirty
    );
    assert_eq!(
        registration.projection.dirty_state,
        RepositoryDirtyState::Dirty
    );
    let scan = store
        .repository_local_state(&registration.repository_binding_id)
        .expect("scan read")
        .expect("scan exists");
    assert_eq!(scan.availability, RepositoryAvailability::Dirty);
    let mapping = store
        .path_mapping(&registration.repository_binding_id)
        .expect("mapping read")
        .expect("mapping exists");
    assert_eq!(mapping.local_state, "dirty");

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_refuses_duplicate_directory() {
    let base = temporary_directory("repo-duplicate");
    let repository = GitRepository::create(&base, "repo-duplicate-check");
    let device = TestDevice::new("repo-duplicate-device");
    let (mut store, node, instance) = device.enrolled_store();

    let first = register(&mut store, &node, &instance, &repository.root, false)
        .expect("first registration");
    let error = register(&mut store, &node, &instance, &repository.root, false)
        .expect_err("duplicate registration is refused");
    let RepositoryRegistryError::AlreadyRegistered {
        repository_binding_id,
    } = error
    else {
        panic!("expected AlreadyRegistered, got {error:?}");
    };
    assert_eq!(repository_binding_id, first.repository_binding_id);
    assert_eq!(pending_frames(&store).len(), 1);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_requires_adopted_enrollment() {
    let base = temporary_directory("repo-unenrolled");
    let repository = GitRepository::create(&base, "repo-unenrolled");
    let data_directory = temporary_directory("repo-unenrolled-device");
    let mut store = DeviceStore::open(&data_directory).expect("store opens");
    ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");

    let error = register(&mut store, "", "", &repository.root, false)
        .expect_err("placeholder stream is refused");
    assert!(matches!(error, RepositoryRegistryError::NotEnrolled));
    assert!(store.path_mappings().expect("mappings").is_empty());

    cleanup(&base);
    cleanup(&data_directory);
    store.close().expect("store closes");
}

#[test]
fn revalidate_reports_moved_after_directory_removal() {
    let base = temporary_directory("repo-moved");
    let repository = GitRepository::create(&base, "repo-vanishes");
    let device = TestDevice::new("repo-moved-device");
    let (mut store, node, instance) = device.enrolled_store();
    let registration =
        register(&mut store, &node, &instance, &repository.root, false).expect("registration");

    fs::remove_dir_all(&repository.root).expect("directory removed");
    let revalidated = revalidate(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
    )
    .expect("revalidation returns the state, not an error");
    assert_eq!(revalidated.availability, RepositoryAvailability::Moved);
    assert!(revalidated.status_reported);
    assert!(revalidated.status_outbox_sequence.is_some());
    assert!(!revalidated.detail.is_empty());

    // The durable projection moved to `moved` and a status report rode the
    // outbox.
    let mapping = store
        .path_mapping(&registration.repository_binding_id)
        .expect("mapping read")
        .expect("mapping survives a failed scan");
    assert_eq!(mapping.local_state, "moved");
    let scan = store
        .repository_local_state(&registration.repository_binding_id)
        .expect("scan read")
        .expect("scan exists");
    assert_eq!(scan.availability, RepositoryAvailability::Moved);
    assert_eq!(scan.head_commit, None);

    let frames = pending_frames(&store);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].0, "client.repository.status");
    let payload = status_payload(&frames[1].1);
    assert_eq!(
        payload.repository_binding_id,
        registration.repository_binding_id
    );
    assert_eq!(payload.availability, RepositoryAvailability::Moved);
    assert_eq!(payload.head_commit, "");
    assert_eq!(payload.dirty_state, RepositoryDirtyState::Clean);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn revalidate_detects_symlink_replacement() {
    let base = temporary_directory("repo-symlink-swap");
    let repository = GitRepository::create(&base, "repo-replaced");
    let device = TestDevice::new("repo-symlink-device");
    let (mut store, node, instance) = device.enrolled_store();
    let registration =
        register(&mut store, &node, &instance, &repository.root, false).expect("registration");
    // Replace the bound directory entry with a symlink to a twin directory.
    let twin = base.join("repo-relocated");
    fs::rename(&repository.root, &twin).expect("directory renamed");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&twin, &repository.root).expect("symlink created");
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&repository.root).expect("fallback directory");
    }

    let revalidated = revalidate(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
    )
    .expect("revalidation returns the state");
    assert_eq!(revalidated.availability, RepositoryAvailability::Moved);
    assert!(revalidated.status_reported);
    // The stored canonical path stays authoritative: nothing silently
    // follows the replacement.
    let mapping = store
        .path_mapping(&registration.repository_binding_id)
        .expect("mapping read")
        .expect("mapping exists");
    assert_eq!(
        mapping.canonical_path,
        registration.canonical_path.to_string_lossy()
    );
    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn registration_through_symlink_stores_resolved_target() {
    let base = temporary_directory("repo-via-symlink");
    let repository = GitRepository::create(&base, "repo-target");
    let link = base.join("repo-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repository.root, &link).expect("symlink created");
    #[cfg(not(unix))]
    {
        let _ = fs::copy(&repository.root, &link);
    }
    let device = TestDevice::new("repo-via-symlink-device");
    let (mut store, node, instance) = device.enrolled_store();

    let registration = register(&mut store, &node, &instance, &link, false)
        .expect("symlinked registration resolves");
    assert_eq!(
        registration.canonical_path,
        fs::canonicalize(&repository.root).expect("target canonicalized")
    );
    assert_ne!(registration.canonical_path, link);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn revalidate_without_changes_reports_nothing() {
    let base = temporary_directory("repo-stable");
    let repository = GitRepository::create(&base, "repo-stable-head");
    let device = TestDevice::new("repo-stable-device");
    let (mut store, node, instance) = device.enrolled_store();
    let registration =
        register(&mut store, &node, &instance, &repository.root, false).expect("registration");

    let revalidated = revalidate(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
    )
    .expect("revalidation");
    assert_eq!(revalidated.availability, RepositoryAvailability::Available);
    assert_eq!(revalidated.head_commit, repository.head());
    assert_eq!(revalidated.dirty_state, RepositoryDirtyState::Clean);
    assert!(!revalidated.status_reported);
    assert!(revalidated.status_outbox_sequence.is_none());
    assert_eq!(pending_frames(&store).len(), 1);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn revalidate_reports_new_head_as_status_change() {
    let base = temporary_directory("repo-head-move");
    let repository = GitRepository::create(&base, "repo-advances");
    let device = TestDevice::new("repo-head-move-device");
    let (mut store, node, instance) = device.enrolled_store();
    let registration =
        register(&mut store, &node, &instance, &repository.root, false).expect("registration");

    let new_head = repository.commit("second commit");
    let revalidated = revalidate(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
    )
    .expect("revalidation");
    assert_eq!(revalidated.availability, RepositoryAvailability::Available);
    assert_eq!(revalidated.head_commit, new_head);
    assert!(revalidated.status_reported);

    let frames = pending_frames(&store);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].0, "client.repository.status");
    let payload = status_payload(&frames[1].1);
    assert_eq!(payload.head_commit, new_head);
    assert_eq!(payload.availability, RepositoryAvailability::Available);

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn remove_deletes_binding_and_reports_removal() {
    let base = temporary_directory("repo-remove");
    let repository = GitRepository::create(&base, "repo-removed");
    let device = TestDevice::new("repo-remove-device");
    let (mut store, node, instance) = device.enrolled_store();
    let registration =
        register(&mut store, &node, &instance, &repository.root, false).expect("registration");

    let removal = remove_repository(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
        now(),
    )
    .expect("removal");
    assert_eq!(
        removal.repository_binding_id,
        registration.repository_binding_id
    );

    assert!(store.path_mappings().expect("mappings").is_empty());
    assert!(store.repository_local_states().expect("scans").is_empty());
    assert!(list_bindings(&store).expect("list").is_empty());

    let frames = pending_frames(&store);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].0, "client.repository.removed");
    let payload = removed_payload(&frames[1].1);
    assert_eq!(
        payload.repository_binding_id,
        registration.repository_binding_id
    );
    assert_eq!(payload.command.expected_revision, 0);
    assert!(
        payload
            .command
            .idempotency_key
            .starts_with("repository-removed-")
    );
    let encoded = serde_json::to_string(&frames[1].1).expect("frame encodes");
    assert!(
        !encoded.contains(
            &fs::canonicalize(&repository.root)
                .expect("canonicalize")
                .to_string_lossy()
                .to_string()
        ),
        "removal frame carries a path"
    );

    // Removing again finds no binding.
    let error = remove_repository(
        &mut store,
        &node,
        &instance,
        &registration.repository_binding_id,
        now(),
    )
    .expect_err("second removal is refused");
    assert!(matches!(error, RepositoryRegistryError::NotFound));

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn revalidate_of_unknown_binding_fails_cleanly() {
    let device = TestDevice::new("repo-unknown-binding");
    let (mut store, node, instance) = device.enrolled_store();
    let error = revalidate(
        &mut store,
        &node,
        &instance,
        "rbd_UNKNOWNUNKNOWNUNKNOWNUNKNOWN00",
    )
    .expect_err("unknown binding is refused");
    assert!(matches!(error, RepositoryRegistryError::NotFound));

    store.close().expect("store closes");
    cleanup(&device.data_directory);
}

#[test]
fn list_bindings_returns_mappings_with_scans() {
    let base = temporary_directory("repo-list");
    let first = GitRepository::create(&base, "repo-list-a");
    let second = GitRepository::create(&base, "repo-list-b");
    let device = TestDevice::new("repo-list-device");
    let (mut store, node, instance) = device.enrolled_store();

    let first_registration =
        register(&mut store, &node, &instance, &first.root, false).expect("first registration");
    let second_registration =
        register(&mut store, &node, &instance, &second.root, false).expect("second registration");

    let bindings = list_bindings(&store).expect("list");
    assert_eq!(bindings.len(), 2);
    // Binding-id order.
    let ids: Vec<&str> = bindings
        .iter()
        .map(|binding| binding.mapping.repository_binding_id.as_str())
        .collect();
    assert!(ids.contains(&first_registration.repository_binding_id.as_str()));
    assert!(ids.contains(&second_registration.repository_binding_id.as_str()));
    for binding in &bindings {
        assert!(!binding.mapping.canonical_path.is_empty());
        assert!(binding.scan.is_some());
    }

    store.close().expect("store closes");
    cleanup(&base);
    cleanup(&device.data_directory);
}

#[test]
fn concurrent_registrations_of_different_directories_succeed() {
    let base = temporary_directory("repo-concurrent");
    let device_base = temporary_directory("repo-concurrent-devices");
    fs::create_dir_all(&device_base).expect("device base");
    require_git();

    let handles: Vec<_> = (0..4)
        .map(|index| {
            let base = base.clone();
            thread::spawn(move || {
                let repository = GitRepository::create(&base, &format!("repo-parallel-{index}"));
                let device = TestDevice::new(&format!("repo-parallel-{index}"));
                let (mut store, node, instance) = device.enrolled_store();
                let registration = register(&mut store, &node, &instance, &repository.root, false)
                    .expect("concurrent registration");
                let mapping = store
                    .path_mapping(&registration.repository_binding_id)
                    .expect("mapping read")
                    .expect("mapping exists");
                (registration.repository_binding_id, mapping.canonical_path)
            })
        })
        .collect();
    let mut binding_ids = Vec::new();
    for handle in handles {
        let (binding_id, canonical_path) = handle.join().expect("thread joins");
        binding_ids.push(binding_id);
        assert!(canonical_path.contains("repo-parallel-"));
    }
    binding_ids.sort();
    let unique = binding_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), binding_ids.len(), "binding ids are unique");

    cleanup(&base);
    cleanup(&device_base);
}
