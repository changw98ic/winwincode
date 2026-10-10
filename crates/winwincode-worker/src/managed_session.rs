use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ClientInstanceId, ClientNodeId, ClientOccupancyLeaseId, ProductSessionId, RepositoryBindingId,
    Sha256Digest, WorkRunId, WorkerId, WorkerInstanceId, WorkerSessionId, is_canonical_prefixed_id,
};
use winwincode_execution_port::generated::ModelGatewayRoute;

const MAX_CONFIG_BYTES: usize = 64 * 1024;

const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionConfigError {
    pub field: Option<&'static str>,
    pub reason: String,
}

impl ManagedSessionConfigError {
    fn field_error(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field: Some(field),
            reason: reason.into(),
        }
    }

    fn file_error(reason: impl Into<String>) -> Self {
        Self {
            field: None,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ManagedSessionConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.field {
            Some(field) => {
                write!(
                    formatter,
                    "managed session config field `{field}`: {}",
                    self.reason
                )
            }
            None => write!(formatter, "managed session config: {}", self.reason),
        }
    }
}

impl std::error::Error for ManagedSessionConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCredentialError {
    pub reason: String,
}

impl fmt::Display for WorkerCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "worker session credential: {}", self.reason)
    }
}

impl std::error::Error for WorkerCredentialError {}

#[derive(Debug, Clone, PartialEq)]
pub struct ManagedSessionConfig {
    pub client_node_id: ClientNodeId,
    pub client_instance_id: ClientInstanceId,
    pub occupancy_lease_id: ClientOccupancyLeaseId,
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: RepositoryBindingId,
    pub product_session_id: Option<ProductSessionId>,
    pub work_run_id: Option<WorkRunId>,
    pub worker_session_id: WorkerSessionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub source_directory: PathBuf,
    pub data_directory: PathBuf,
    pub execution_workspace_directory: Option<PathBuf>,
    pub provider_directory: PathBuf,
    pub server_origin: String,
    pub worker_credential_path: PathBuf,
    pub model_route: Option<ModelGatewayRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedSessionConfigFile {
    client_node_id: Option<String>,
    client_instance_id: Option<String>,
    occupancy_lease_id: Option<String>,
    occupancy_fencing_token: Option<String>,
    repository_binding_id: Option<String>,
    product_session_id: Option<String>,
    work_run_id: Option<String>,
    worker_session_id: Option<String>,
    worker_id: Option<String>,
    worker_instance_id: Option<String>,
    source_directory: Option<String>,
    data_directory: Option<String>,
    execution_workspace_directory: Option<String>,
    provider_directory: Option<String>,
    server_origin: Option<String>,
    worker_credential_path: Option<String>,
    model_route: Option<ModelRouteFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelRouteFile {
    capability: Option<String>,
    route: Option<String>,
}

impl ManagedSessionConfig {
    pub fn read(path: &Path) -> Result<Self, ManagedSessionConfigError> {
        let metadata = fs::metadata(path).map_err(|_| {
            ManagedSessionConfigError::file_error(format!(
                "config file {} is unavailable",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(ManagedSessionConfigError::file_error(format!(
                "config path {} is not a regular file",
                path.display()
            )));
        }
        require_exact_mode_0600(&metadata, path)?;
        let bytes = fs::read(path)
            .map_err(|_| ManagedSessionConfigError::file_error("config file is unreadable"))?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ManagedSessionConfigError::file_error(
                "config file exceeds the 64 KiB bound",
            ));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ManagedSessionConfigError::file_error("config file is not UTF-8"))?;
        let file: ManagedSessionConfigFile = serde_json::from_str(&text).map_err(|error| {
            ManagedSessionConfigError::file_error(format!("config is not valid JSON: {error}"))
        })?;
        file.resolve()
    }
}

impl ManagedSessionConfigFile {
    fn resolve(self) -> Result<ManagedSessionConfig, ManagedSessionConfigError> {
        let client_node_id = plain_id(
            "clientNodeId",
            require("clientNodeId", self.client_node_id)?,
        )?;
        let client_instance_id = ClientInstanceId(plain_id(
            "clientInstanceId",
            require("clientInstanceId", self.client_instance_id)?,
        )?);
        let occupancy_lease_id = ClientOccupancyLeaseId(plain_id(
            "occupancyLeaseId",
            require("occupancyLeaseId", self.occupancy_lease_id)?,
        )?);
        let occupancy_fencing_token =
            fencing_token("occupancyFencingToken", self.occupancy_fencing_token)?;
        let repository_binding_id = RepositoryBindingId(plain_id(
            "repositoryBindingId",
            require("repositoryBindingId", self.repository_binding_id)?,
        )?);
        let product_session_id =
            optional_plain_id("productSessionId", self.product_session_id)?.map(ProductSessionId);
        let work_run_id = optional_work_run_id(self.work_run_id)?;
        let worker_session_id = WorkerSessionId(plain_id(
            "workerSessionId",
            require("workerSessionId", self.worker_session_id)?,
        )?);
        let worker_id = WorkerId(plain_id("workerId", require("workerId", self.worker_id)?)?);
        let worker_instance_id = WorkerInstanceId(plain_id(
            "workerInstanceId",
            require("workerInstanceId", self.worker_instance_id)?,
        )?);
        let source_directory = local_path("sourceDirectory", self.source_directory)?;
        let data_directory = local_path("dataDirectory", self.data_directory)?;
        let execution_workspace_directory = self
            .execution_workspace_directory
            .map(|value| local_path("executionWorkspaceDirectory", Some(value)))
            .transpose()?;
        let provider_directory = local_path("providerDirectory", self.provider_directory)?;
        let server_origin = require("serverOrigin", self.server_origin)?;
        if crate::remote_transport::parse_origin(&server_origin).is_err() {
            return Err(ManagedSessionConfigError::field_error(
                "serverOrigin",
                "must be an https://HOST:PORT origin",
            ));
        }
        let worker_credential_path =
            local_path("workerCredentialPath", self.worker_credential_path)?;
        let model_route = model_route(self.model_route)?;
        Ok(ManagedSessionConfig {
            client_node_id: ClientNodeId(client_node_id),
            client_instance_id,
            occupancy_lease_id,
            occupancy_fencing_token,
            repository_binding_id,
            product_session_id,
            work_run_id,
            worker_session_id,
            worker_id,
            worker_instance_id,
            source_directory,
            data_directory,
            execution_workspace_directory,
            provider_directory,
            server_origin,
            worker_credential_path,
            model_route,
        })
    }
}

fn require(
    field: &'static str,
    value: Option<String>,
) -> Result<String, ManagedSessionConfigError> {
    value.ok_or_else(|| ManagedSessionConfigError::field_error(field, "required field is missing"))
}

fn plain_id(field: &'static str, value: String) -> Result<String, ManagedSessionConfigError> {
    if value.is_empty() {
        return Err(ManagedSessionConfigError::field_error(
            field,
            "must not be empty",
        ));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ManagedSessionConfigError::field_error(
            field,
            "must not contain whitespace or control characters",
        ));
    }
    Ok(value)
}

fn optional_work_run_id(
    value: Option<String>,
) -> Result<Option<WorkRunId>, ManagedSessionConfigError> {
    let Some(value) = optional_plain_id("workRunId", value)? else {
        return Ok(None);
    };
    if !is_canonical_prefixed_id(&value, "wrn_") {
        return Err(ManagedSessionConfigError::field_error(
            "workRunId",
            "must be a canonical WorkRun id",
        ));
    }
    Ok(Some(WorkRunId(value)))
}

fn optional_plain_id(
    field: &'static str,
    value: Option<String>,
) -> Result<Option<String>, ManagedSessionConfigError> {
    match value {
        None => Ok(None),
        Some(value) => plain_id(field, value).map(Some),
    }
}

fn fencing_token(
    field: &'static str,
    value: Option<String>,
) -> Result<u64, ManagedSessionConfigError> {
    let raw = require(field, value)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ManagedSessionConfigError::field_error(
            field,
            "must be a decimal string",
        ));
    }
    raw.parse::<u64>().map_err(|_| {
        ManagedSessionConfigError::field_error(field, "exceeds the unsigned 64-bit range")
    })
}

fn local_path(
    field: &'static str,
    value: Option<String>,
) -> Result<PathBuf, ManagedSessionConfigError> {
    let raw = require(field, value)?;
    if raw.is_empty() {
        return Err(ManagedSessionConfigError::field_error(
            field,
            "must not be empty",
        ));
    }
    if raw.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ManagedSessionConfigError::field_error(
            field,
            "must not contain control characters",
        ));
    }
    Ok(PathBuf::from(raw))
}

fn model_route(
    value: Option<ModelRouteFile>,
) -> Result<Option<ModelGatewayRoute>, ManagedSessionConfigError> {
    let Some(file) = value else {
        return Ok(None);
    };
    let capability = plain_id(
        "modelRoute.capability",
        require("modelRoute.capability", file.capability)?,
    )?;
    let route = plain_id("modelRoute.route", require("modelRoute.route", file.route)?)?;
    Ok(Some(ModelGatewayRoute { capability, route }))
}

#[cfg(unix)]
fn require_exact_mode_0600(
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<(), ManagedSessionConfigError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(ManagedSessionConfigError::file_error(format!(
            "config file {} must have exactly mode 0600 (found {:04o})",
            path.display(),
            mode
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSessionCredential {
    path: PathBuf,
    digest: Sha256Digest,
}

impl WorkerSessionCredential {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, WorkerCredentialError> {
        let path = path.into();
        let bytes = read_private_credential(&path)?;
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
        Ok(Self { path, digest })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

#[cfg(unix)]
fn read_private_credential(path: &Path) -> Result<Vec<u8>, WorkerCredentialError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|_| WorkerCredentialError {
        reason: format!("credential file {} is unavailable", path.display()),
    })?;
    if !metadata.is_file() {
        return Err(WorkerCredentialError {
            reason: format!("credential path {} is not a regular file", path.display()),
        });
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(WorkerCredentialError {
            reason: format!(
                "credential file {} is not private (mode {:04o} grants group or other access)",
                path.display(),
                mode & 0o777
            ),
        });
    }
    let bytes = fs::read(path).map_err(|_| WorkerCredentialError {
        reason: format!("credential file {} is unreadable", path.display()),
    })?;
    if bytes.is_empty() {
        return Err(WorkerCredentialError {
            reason: format!("credential file {} is empty", path.display()),
        });
    }
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(WorkerCredentialError {
            reason: format!(
                "credential file {} exceeds the 16 KiB bound",
                path.display()
            ),
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    fn config_json(extra: &[(&str, &str)]) -> String {
        let mut value = serde_json::json!({
            "clientNodeId": "cln_01J",
            "clientInstanceId": "cli_01J",
            "occupancyLeaseId": "ocq_01J",
            "occupancyFencingToken": "7",
            "repositoryBindingId": "rbn_01J",
            "workerSessionId": "wss_01J",
            "workerId": "wrk_01J",
            "workerInstanceId": "wri_01J",
            "sourceDirectory": "/repo/winwincode",
            "dataDirectory": "/data/wrk_01J",
            "providerDirectory": "/data/providers",
            "serverOrigin": "https://127.0.0.1:8443",
            "workerCredentialPath": "/secrets/worker-credential"
        });
        let object = value.as_object_mut().expect("config is an object");
        for (name, raw) in extra {
            object.insert(
                (*name).to_owned(),
                serde_json::from_str(raw).expect("test extra values are valid JSON"),
            );
        }
        value.to_string()
    }

    fn write_file(path: &Path, contents: &str, mode: u32) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test parent creates");
        }
        fs::write(path, contents).expect("test file writes");
        let mut permissions = fs::metadata(path).expect("test metadata").permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).expect("test chmod");
    }

    fn temp_root(name: &str) -> PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wwc-managed-session-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root creates");
        root
    }

    fn read_config(raw: &str) -> Result<ManagedSessionConfig, ManagedSessionConfigError> {
        let root = temp_root("read");
        let path = root.join("session.json");
        write_file(&path, raw, 0o600);
        let result = ManagedSessionConfig::read(&path);
        let _ = fs::remove_dir_all(&root);
        result
    }

    #[test]
    fn reads_the_full_managed_session_binding() {
        let raw = config_json(&[
            ("productSessionId", "\"psn_01J\""),
            ("workRunId", "\"wrn_01J00000000000000000000000\""),
            (
                "modelRoute",
                r#"{"capability": "reasoning", "route": "embedded-canonical-remote"}"#,
            ),
        ]);
        let config = read_config(&raw).expect("full config parses");
        assert_eq!(config.client_node_id, ClientNodeId("cln_01J".to_owned()));
        assert_eq!(
            config.client_instance_id,
            ClientInstanceId("cli_01J".to_owned())
        );
        assert_eq!(
            config.occupancy_lease_id,
            ClientOccupancyLeaseId("ocq_01J".to_owned())
        );
        assert_eq!(config.occupancy_fencing_token, 7);
        assert_eq!(
            config.repository_binding_id,
            RepositoryBindingId("rbn_01J".to_owned())
        );
        assert_eq!(
            config.product_session_id,
            Some(ProductSessionId("psn_01J".to_owned()))
        );
        assert_eq!(
            config.work_run_id,
            Some(WorkRunId("wrn_01J00000000000000000000000".to_owned()))
        );
        assert_eq!(
            config.worker_session_id,
            WorkerSessionId("wss_01J".to_owned())
        );
        assert_eq!(config.worker_id, WorkerId("wrk_01J".to_owned()));
        assert_eq!(
            config.worker_instance_id,
            WorkerInstanceId("wri_01J".to_owned())
        );
        assert_eq!(config.source_directory, PathBuf::from("/repo/winwincode"));
        assert_eq!(config.data_directory, PathBuf::from("/data/wrk_01J"));
        assert_eq!(config.server_origin, "https://127.0.0.1:8443");
        assert_eq!(
            config.worker_credential_path,
            PathBuf::from("/secrets/worker-credential")
        );
        assert_eq!(
            config.model_route,
            Some(ModelGatewayRoute {
                capability: "reasoning".to_owned(),
                route: "embedded-canonical-remote".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_noncanonical_work_run_identity() {
        let error = read_config(&config_json(&[(
            "workRunId",
            "\"run_01J00000000000000000000000\"",
        )]))
        .expect_err("legacy StageRun identity must be rejected");
        assert!(error.to_string().contains("workRunId"));
    }

    #[test]
    fn optional_fields_default_to_absent() {
        let config = read_config(&config_json(&[])).expect("minimal config parses");
        assert_eq!(config.product_session_id, None);
        assert_eq!(config.work_run_id, None);
        assert_eq!(config.model_route, None);
    }

    #[test]
    fn large_fencing_tokens_survive_as_decimal_strings() {
        let raw = config_json(&[("occupancyFencingToken", "\"18446744073709551615\"")]);
        let config = read_config(&raw).expect("max token parses");
        assert_eq!(config.occupancy_fencing_token, u64::MAX);
    }

    #[test]
    fn every_required_field_is_named_when_missing() {
        let required = [
            "clientNodeId",
            "clientInstanceId",
            "occupancyLeaseId",
            "occupancyFencingToken",
            "repositoryBindingId",
            "workerSessionId",
            "workerId",
            "workerInstanceId",
            "sourceDirectory",
            "dataDirectory",
            "providerDirectory",
            "serverOrigin",
            "workerCredentialPath",
        ];
        for field in required {
            let mut value: serde_json::Value =
                serde_json::from_str(&config_json(&[])).expect("base config parses");
            value
                .as_object_mut()
                .expect("config is an object")
                .remove(field)
                .expect("required field was present in the base config");
            let error = read_config(&value.to_string())
                .expect_err("missing required field must refuse startup");
            assert_eq!(error.field, Some(field), "{field}");
            assert!(error.reason.contains("missing"), "{field}: {error}");
        }
    }

    #[test]
    fn mis_shaped_fields_are_named_with_their_reason() {
        let cases: Vec<(&str, String, &str)> = vec![
            (
                "workerId",
                config_json(&[("workerId", "\"\"")]),
                "must not be empty",
            ),
            (
                "workerId",
                config_json(&[("workerId", "\"wrk 01J\"")]),
                "whitespace",
            ),
            (
                "occupancyFencingToken",
                config_json(&[("occupancyFencingToken", "\"+7\"")]),
                "decimal",
            ),
            (
                "occupancyFencingToken",
                config_json(&[("occupancyFencingToken", "\"7.0\"")]),
                "decimal",
            ),
            (
                "occupancyFencingToken",
                config_json(&[("occupancyFencingToken", "\"18446744073709551616\"")]),
                "64-bit",
            ),
            (
                "serverOrigin",
                config_json(&[("serverOrigin", "\"http://127.0.0.1:8443\"")]),
                "https://HOST:PORT",
            ),
            (
                "serverOrigin",
                config_json(&[("serverOrigin", "\"https://127.0.0.1\"")]),
                "https://HOST:PORT",
            ),
            (
                "sourceDirectory",
                config_json(&[("sourceDirectory", "\"\"")]),
                "must not be empty",
            ),
            (
                "productSessionId",
                config_json(&[("productSessionId", "\"\"")]),
                "must not be empty",
            ),
            (
                "modelRoute.capability",
                config_json(&[("modelRoute", r#"{"capability": "", "route": "r"}"#)]),
                "must not be empty",
            ),
            (
                "modelRoute.route",
                config_json(&[("modelRoute", r#"{"capability": "reasoning"}"#)]),
                "missing",
            ),
        ];
        for (field, raw, reason) in cases {
            let error = read_config(&raw).expect_err("mis-shaped field must refuse startup");
            assert_eq!(error.field, Some(field), "{field}: {error}");
            assert!(error.reason.contains(reason), "{field}: {error}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected_fail_closed() {
        let raw = config_json(&[("unexpectedServerHint", "\"https://example.invalid\"")]);
        let error = read_config(&raw).expect_err("unknown fields must refuse startup");
        assert_eq!(error.field, None);
        assert!(error.reason.contains("unknown field"), "{error}");
    }

    #[test]
    fn config_file_must_have_exactly_mode_0600() {
        let root = temp_root("mode");
        for mode in [0o644, 0o666, 0o700, 0o400, 0o640, 0o604] {
            let path = root.join(format!("session-{mode:o}.json"));
            write_file(&path, &config_json(&[]), mode);
            let error =
                ManagedSessionConfig::read(&path).expect_err("non-0600 config must refuse startup");
            assert_eq!(error.field, None);
            assert!(error.reason.contains("0600"), "{mode:o}: {error}");
            assert!(error.reason.contains(&format!("{mode:04o}")), "{error}");
        }
        let path = root.join("session-0600.json");
        write_file(&path, &config_json(&[]), 0o600);
        ManagedSessionConfig::read(&path).expect("0600 config is accepted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_file_level_failures_are_reported() {
        let root = temp_root("file");
        let missing = ManagedSessionConfig::read(&root.join("absent.json"))
            .expect_err("missing config must refuse startup");
        assert!(missing.reason.contains("unavailable"), "{missing}");

        let directory =
            ManagedSessionConfig::read(&root).expect_err("directory config must refuse startup");
        assert!(
            directory.reason.contains("not a regular file"),
            "{directory}"
        );

        let empty = root.join("empty.json");
        write_file(&empty, "", 0o600);
        let empty =
            ManagedSessionConfig::read(&empty).expect_err("empty config must refuse startup");
        assert!(empty.reason.contains("JSON"), "{empty}");

        let truncated = root.join("truncated.json");
        write_file(&truncated, "{\"workerId\": ", 0o600);
        let truncated = ManagedSessionConfig::read(&truncated)
            .expect_err("truncated config must refuse startup");
        assert!(truncated.reason.contains("JSON"), "{truncated}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn credential_digest_matches_sha256_hex_form() {
        let root = temp_root("credential");
        let path = root.join("worker-credential");
        write_file(&path, "wsc-test-token", 0o600);
        let credential = WorkerSessionCredential::load(&path).expect("credential loads");
        assert_eq!(credential.path(), path.as_path());
        let expected = Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"wsc-test-token")));
        assert_eq!(credential.digest(), &expected);

        write_file(&path, "wsc-test-token", 0o400);
        let reloaded = WorkerSessionCredential::load(&path).expect("0400 credential loads");
        assert_eq!(reloaded.digest(), credential.digest());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn credential_rejects_non_private_and_empty_files() {
        let root = temp_root("credential-bad");
        let path = root.join("worker-credential");
        write_file(&path, "token", 0o644);
        let error = WorkerSessionCredential::load(&path).expect_err("0644 must reject");
        assert!(error.to_string().contains("not private"), "{error}");
        assert!(error.to_string().contains("0644"), "{error}");

        write_file(&path, "", 0o600);
        let error = WorkerSessionCredential::load(&path).expect_err("empty must reject");
        assert!(error.to_string().contains("empty"), "{error}");

        let error = WorkerSessionCredential::load(root.join("absent"))
            .expect_err("missing credential must reject");
        assert!(error.to_string().contains("unavailable"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }
}
