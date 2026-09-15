// SPDX-License-Identifier: Apache-2.0

//! Device-owned Skills and MCP configuration. Only metadata leaves this store.

use crate::{DeviceProviderError, DeviceProviderStore, mcp_connection};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};
use winwincode_api::generated::{
    DeviceConfigurationEnvelope, DeviceExtensionMcp, DeviceExtensionMcpConnectionStatus,
    DeviceExtensionMcpTransport, DeviceExtensionOutcome, DeviceExtensionReceipt,
    DeviceExtensionSkill, DeviceExtensionSnapshot,
};

const CONTEXT: &str = "winwincode.device-extensions.v1";
const MAX_SKILL_BYTES: usize = 1_048_576;

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Mutation {
    SaveSkill {
        id: String,
        content: Option<String>,
        #[serde(rename = "sourcePath")]
        source_path: Option<String>,
        enabled: bool,
    },
    SaveMcp {
        id: String,
        configuration: String,
        enabled: bool,
    },
    SetEnabled {
        kind: Kind,
        id: String,
        enabled: bool,
    },
    Delete {
        kind: Kind,
        id: String,
    },
    TestMcp {
        id: String,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Skill,
    Mcp,
}
impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Mcp => "mcp",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SkillFiles {
    files: BTreeMap<String, SkillFile>,
}
#[derive(Serialize, Deserialize)]
struct SkillFile {
    data: String,
    executable: bool,
}

#[derive(Serialize, Deserialize)]
struct ExtensionEntry {
    kind: String,
    id: String,
    data: String,
    projection: String,
}

/// The exact tested MCP tools installed in the installed task configuration.
pub struct InstalledMcpTools {
    /// Configured MCP server identifier.
    pub server: String,
    /// Configuration digest, binding the capability version to its settings.
    pub digest: String,
    /// Only these discovered tools may be exposed and authorized.
    pub tools: Vec<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

impl DeviceProviderStore {
    fn extension_revision(&self) -> Result<i64, DeviceProviderError> {
        Ok(self.connection.query_row(
            "SELECT revision FROM extension_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    }

    /// Lists confirmed Device metadata, never MCP credentials or Skill file contents.
    ///
    /// # Errors
    /// Rejects corrupt stored configuration.
    pub fn extension_snapshot(
        &self,
        node: &str,
    ) -> Result<DeviceExtensionSnapshot, DeviceProviderError> {
        let mut skills = Vec::new();
        let mut mcp_servers = Vec::new();
        let mut query = self
            .connection
            .prepare("SELECT kind, projection FROM extensions ORDER BY kind,id")?;
        for row in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (kind, projection) = row?;
            match kind.as_str() {
                "skill" => skills.push(serde_json::from_str(&projection)?),
                "mcp" => mcp_servers.push(serde_json::from_str(&projection)?),
                _ => return Err(DeviceProviderError),
            }
        }
        Ok(DeviceExtensionSnapshot {
            client_node_id: node.to_owned(),
            revision: self.extension_revision()?,
            encryption_public_key: self.snapshot(node)?.encryption_public_key,
            skills,
            mcp_servers,
        })
    }

    /// Applies one encrypted, revision-bound operation and durably records its receipt.
    ///
    /// # Errors
    /// Rejects changed retries, wrong Device identities and unavailable storage.
    pub fn apply_extension(
        &mut self,
        node: &str,
        envelope: &DeviceConfigurationEnvelope,
    ) -> Result<DeviceExtensionReceipt, DeviceProviderError> {
        if envelope.client_node_id != node
            || envelope.request_id.len() < 8
            || envelope.request_id.len() > 200
        {
            return Err(DeviceProviderError);
        }
        let request_digest = digest(&serde_json::to_vec(envelope)?);
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.extension_transaction(envelope, &request_digest);
        let (receipt, replayed) = match result {
            Ok(result) => {
                self.connection.execute_batch("COMMIT")?;
                result
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        if !replayed && receipt.outcome == DeviceExtensionOutcome::Interrupted {
            let Mutation::TestMcp { id } = self.decrypt_configuration(envelope, CONTEXT)? else {
                return Err(DeviceProviderError);
            };
            return self.test_mcp(&id, receipt);
        }
        Ok(receipt)
    }

    fn extension_transaction(
        &self,
        envelope: &DeviceConfigurationEnvelope,
        request_digest: &str,
    ) -> Result<(DeviceExtensionReceipt, bool), DeviceProviderError> {
        let previous: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT digest,receipt FROM extension_receipts WHERE request_id=?1",
                [&envelope.request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((original, receipt)) = previous {
            if original != request_digest {
                return Err(DeviceProviderError);
            }
            return Ok((serde_json::from_str(&receipt)?, true));
        }
        let outcome = if envelope.expected_revision == self.extension_revision()? {
            match self.decrypt_configuration::<Mutation>(envelope, CONTEXT) {
                Ok(mutation) => self.mutate_extension(mutation)?,
                Err(_) => DeviceExtensionOutcome::InvalidRequest,
            }
        } else {
            DeviceExtensionOutcome::RevisionConflict
        };
        let receipt = DeviceExtensionReceipt {
            request_id: envelope.request_id.clone(),
            outcome,
            revision: self.extension_revision()?,
        };
        self.connection.execute(
            "INSERT INTO extension_receipts VALUES (?1,?2,?3)",
            params![
                envelope.request_id,
                request_digest,
                serde_json::to_string(&receipt)?
            ],
        )?;
        Ok((receipt, false))
    }

    fn entry(&self, kind: Kind, id: &str) -> Result<Option<(String, String)>, DeviceProviderError> {
        Ok(self
            .connection
            .query_row(
                "SELECT data,projection FROM extensions WHERE kind=?1 AND id=?2",
                params![kind.name(), id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    fn save_entry<T: Serialize>(
        &self,
        kind: Kind,
        id: &str,
        data: &str,
        projection: &T,
    ) -> Result<DeviceExtensionOutcome, DeviceProviderError> {
        let projection = serde_json::to_string(projection)?;
        // Leave room for the envelope/receipt within ClientControl's 256 KiB frame limit.
        let retained_bytes: i64 = self.connection.query_row(
            "SELECT coalesce(sum(length(CAST(projection AS BLOB))),0) FROM extensions WHERE kind!=?1 OR id!=?2",
            params![kind.name(), id], |row| row.get(0),
        )?;
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM extensions WHERE kind=?1 AND id!=?2",
            params![kind.name(), id],
            |r| r.get(0),
        )?;
        if count >= 100
            || retained_bytes + i64::try_from(projection.len()).map_err(|_| DeviceProviderError)?
                > 192 * 1024
        {
            return Ok(DeviceExtensionOutcome::InvalidRequest);
        }
        self.connection.execute("INSERT INTO extensions VALUES (?1,?2,?3,?4) ON CONFLICT(kind,id) DO UPDATE SET data=excluded.data,projection=excluded.projection",
            params![kind.name(),id,data,projection])?;
        self.connection.execute(
            "UPDATE extension_state SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        Ok(DeviceExtensionOutcome::Saved)
    }

    fn mutate_extension(
        &self,
        mutation: Mutation,
    ) -> Result<DeviceExtensionOutcome, DeviceProviderError> {
        let id = match &mutation {
            Mutation::SaveSkill { id, .. }
            | Mutation::SaveMcp { id, .. }
            | Mutation::SetEnabled { id, .. }
            | Mutation::Delete { id, .. }
            | Mutation::TestMcp { id } => id,
        };
        if !valid_id(id) {
            return Ok(DeviceExtensionOutcome::InvalidRequest);
        }
        match mutation {
            Mutation::SaveSkill {
                id,
                content,
                source_path,
                enabled,
            } => {
                let Ok((files, projection)) = read_skill(&id, content, source_path, enabled) else {
                    return Ok(DeviceExtensionOutcome::InvalidRequest);
                };
                self.save_entry(
                    Kind::Skill,
                    &id,
                    &serde_json::to_string(&files)?,
                    &projection,
                )
            }
            Mutation::SaveMcp {
                id,
                configuration,
                enabled,
            } => {
                let collision: bool = self.connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM extensions WHERE kind='mcp' AND lower(id)=lower(?1) AND id!=?1)", [&id], |row| row.get(0))?;
                if collision || id.starts_with("mcp__") {
                    return Ok(DeviceExtensionOutcome::InvalidRequest);
                }
                let Ok(config) = mcp_connection::validate(&configuration) else {
                    return Ok(DeviceExtensionOutcome::InvalidRequest);
                };
                let transport = match config.transport {
                    codex_config::types::McpServerTransportConfig::Stdio { .. } => {
                        DeviceExtensionMcpTransport::Stdio
                    }
                    codex_config::types::McpServerTransportConfig::StreamableHttp { .. } => {
                        DeviceExtensionMcpTransport::StreamableHttp
                    }
                };
                let projection = DeviceExtensionMcp {
                    id: id.clone(),
                    enabled,
                    transport,
                    tool_names: Vec::new(),
                    connection_status: DeviceExtensionMcpConnectionStatus::Untested,
                    digest: digest(configuration.as_bytes()),
                };
                self.save_entry(Kind::Mcp, &id, &configuration, &projection)
            }
            Mutation::SetEnabled { kind, id, enabled } => {
                let Some((data, projection)) = self.entry(kind, &id)? else {
                    return Ok(DeviceExtensionOutcome::InvalidRequest);
                };
                let mut projection: serde_json::Value = serde_json::from_str(&projection)?;
                projection["enabled"] = enabled.into();
                self.save_entry(kind, &id, &data, &projection)
            }
            Mutation::Delete { kind, id } => {
                self.connection.execute(
                    "DELETE FROM extensions WHERE kind=?1 AND id=?2",
                    params![kind.name(), id],
                )?;
                self.connection.execute(
                    "UPDATE extension_state SET revision=revision+1 WHERE singleton=1",
                    [],
                )?;
                Ok(DeviceExtensionOutcome::Deleted)
            }
            Mutation::TestMcp { id } => Ok(if self.entry(Kind::Mcp, &id)?.is_some() {
                DeviceExtensionOutcome::Interrupted
            } else {
                DeviceExtensionOutcome::InvalidRequest
            }),
        }
    }

    fn test_mcp(
        &mut self,
        id: &str,
        mut receipt: DeviceExtensionReceipt,
    ) -> Result<DeviceExtensionReceipt, DeviceProviderError> {
        let (configuration, projection) = self.entry(Kind::Mcp, id)?.ok_or(DeviceProviderError)?;
        let mut projection: DeviceExtensionMcp = serde_json::from_str(&projection)?;
        let home = PathBuf::from(self.connection.path().ok_or(DeviceProviderError)?)
            .parent()
            .ok_or(DeviceProviderError)?
            .to_path_buf();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(mcp_connection::discover(id, &configuration, &home));
        projection.connection_status = if result.is_ok() {
            DeviceExtensionMcpConnectionStatus::Ready
        } else {
            DeviceExtensionMcpConnectionStatus::Failed
        };
        projection.tool_names = result.unwrap_or_default();
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| {
            if self.extension_revision()? != receipt.revision
                || self.entry(Kind::Mcp, id)?.as_ref().map(|x| &x.0) != Some(&configuration)
            {
                receipt.outcome = DeviceExtensionOutcome::RevisionConflict;
            } else {
                receipt.outcome = self.save_entry(Kind::Mcp, id, &configuration, &projection)?;
                if receipt.outcome == DeviceExtensionOutcome::Saved {
                    receipt.outcome = if projection.connection_status
                        == DeviceExtensionMcpConnectionStatus::Ready
                    {
                        DeviceExtensionOutcome::Tested
                    } else {
                        DeviceExtensionOutcome::ConnectionFailed
                    };
                }
            }
            receipt.revision = self.extension_revision()?;
            self.connection.execute(
                "UPDATE extension_receipts SET receipt=?1 WHERE request_id=?2",
                params![serde_json::to_string(&receipt)?, receipt.request_id],
            )?;
            Ok::<_, DeviceProviderError>(receipt)
        })();
        match outcome {
            Ok(receipt) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Restores the last installed task snapshot after a Worker restart.
    /// New tasks must call `refresh_extensions` before opening their Core session.
    ///
    /// # Errors
    /// Rejects corrupt snapshots, symlink paths and unsafe filesystem permissions.
    pub fn restore_extensions(
        &self,
        home: &Path,
    ) -> Result<Vec<InstalledMcpTools>, DeviceProviderError> {
        let snapshot = home.join("device-extensions.json");
        let entries = if snapshot.exists() {
            checked_file(&snapshot)?;
            serde_json::from_slice(&fs::read(&snapshot)?)?
        } else {
            self.extension_entries()?
        };
        materialize_extensions(home, &entries)
    }

    /// Installs current Device extensions at an idle task boundary, removing old Skill files.
    /// In-flight tasks keep their installed snapshot until they finish, including on restart.
    ///
    /// # Errors
    /// Rejects corrupt configuration or unsafe filesystem paths.
    pub fn refresh_extensions(
        &self,
        home: &Path,
    ) -> Result<Vec<InstalledMcpTools>, DeviceProviderError> {
        let entries = self.extension_entries()?;
        materialize_extensions(home, &entries)
    }

    fn extension_entries(&self) -> Result<Vec<ExtensionEntry>, DeviceProviderError> {
        let mut query = self
            .connection
            .prepare("SELECT kind,id,data,projection FROM extensions ORDER BY kind,id")?;
        Ok(query
            .query_map([], |r| {
                Ok(ExtensionEntry {
                    kind: r.get(0)?,
                    id: r.get(1)?,
                    data: r.get(2)?,
                    projection: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
}

fn materialize_extensions(
    home: &Path,
    entries: &[ExtensionEntry],
) -> Result<Vec<InstalledMcpTools>, DeviceProviderError> {
    private_directory(home)?;
    // This directory belongs to the Worker. Rebuild it even on recovery so a
    // crash during the previous installation cannot leave stale Skill files.
    let skills = home.join("skills");
    private_directory(&skills)?;
    fs::remove_dir_all(&skills)?;
    private_directory(&skills)?;
    let mut servers = BTreeMap::new();
    let mut capabilities = Vec::new();
    for entry in entries {
        if !valid_id(&entry.id) {
            return Err(DeviceProviderError);
        }
        match entry.kind.as_str() {
            "skill" => {
                let projection: DeviceExtensionSkill = serde_json::from_str(&entry.projection)?;
                if !projection.enabled {
                    continue;
                }
                let bundle: SkillFiles = serde_json::from_str(&entry.data)?;
                let root = home.join("skills").join(&entry.id);
                for (path, file) in bundle.files {
                    if !relative_path(&path) {
                        return Err(DeviceProviderError);
                    }
                    private_directory(&home.join("skills"))?;
                    private_directory(&root)?;
                    let mut parent = root.clone();
                    for component in Path::new(&path)
                        .parent()
                        .ok_or(DeviceProviderError)?
                        .components()
                    {
                        parent.push(component.as_os_str());
                        private_directory(&parent)?;
                    }
                    let target = root.join(path);
                    write_private(
                        &target,
                        &STANDARD
                            .decode(file.data)
                            .map_err(|_| DeviceProviderError)?,
                    )?;
                    if file.executable {
                        fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
                    }
                }
            }
            "mcp" => {
                let projection: DeviceExtensionMcp = serde_json::from_str(&entry.projection)?;
                if !projection.enabled
                    || projection.connection_status != DeviceExtensionMcpConnectionStatus::Ready
                {
                    continue;
                }
                let mut config = mcp_connection::validate(&entry.data)?;
                config.enabled = true;
                config.enabled_tools = Some(projection.tool_names.clone());
                config.startup_timeout_sec = Some(std::time::Duration::from_secs(15));
                config.tool_timeout_sec = Some(std::time::Duration::from_mins(1));
                servers.insert(entry.id.clone(), config);
                capabilities.push(InstalledMcpTools {
                    server: entry.id.clone(),
                    digest: projection.digest,
                    tools: projection.tool_names,
                });
            }
            _ => return Err(DeviceProviderError),
        }
    }
    let mut config = toml::to_string(&BTreeMap::from([("mcp_servers", servers)]))
        .map_err(|_| DeviceProviderError)?;
    config.push_str("\n[skills.bundled]\nenabled = false\n");
    write_private(&home.join("config.toml"), config.as_bytes())?;
    write_private(
        &home.join("device-extensions.json"),
        &serde_json::to_vec(entries)?,
    )?;
    Ok(capabilities)
}

fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 2048
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn read_skill(
    id: &str,
    content: Option<String>,
    source: Option<String>,
    enabled: bool,
) -> Result<(SkillFiles, DeviceExtensionSkill), DeviceProviderError> {
    let mut bytes = BTreeMap::new();
    let source = match (content, source) {
        (Some(content), None) if content.len() <= 32_768 => {
            bytes.insert("SKILL.md".to_owned(), (content.into_bytes(), false));
            "SKILL.md".to_owned()
        }
        (None, Some(source)) if Path::new(&source).is_absolute() && source.len() <= 2048 => {
            let root = fs::canonicalize(&source)?;
            if fs::symlink_metadata(&source)?.file_type().is_symlink() {
                return Err(DeviceProviderError);
            }
            collect_skill(&root, &root, &mut bytes, &mut 0)?;
            source
        }
        _ => return Err(DeviceProviderError),
    };
    let content = std::str::from_utf8(&bytes.get("SKILL.md").ok_or(DeviceProviderError)?.0)
        .map_err(|_| DeviceProviderError)?;
    let metadata = codex_skills::parse_skill_frontmatter_metadata(content, || id.to_owned())
        .map_err(|_| DeviceProviderError)?;
    if metadata.name.is_empty()
        || metadata.name.len() > 64
        || metadata.description.is_empty()
        || metadata.description.len() > 1024
    {
        return Err(DeviceProviderError);
    }
    let files = SkillFiles {
        files: bytes
            .into_iter()
            .map(|(p, (b, executable))| {
                (
                    p,
                    SkillFile {
                        data: STANDARD.encode(b),
                        executable,
                    },
                )
            })
            .collect(),
    };
    let projection = DeviceExtensionSkill {
        id: id.to_owned(),
        name: metadata.name,
        description: metadata.description,
        source,
        enabled,
        digest: digest(&serde_json::to_vec(&files)?),
        file_count: i64::try_from(files.files.len()).map_err(|_| DeviceProviderError)?,
    };
    Ok((files, projection))
}

fn collect_skill(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, (Vec<u8>, bool)>,
    total: &mut usize,
) -> Result<(), DeviceProviderError> {
    if directory
        .strip_prefix(root)
        .map_err(|_| DeviceProviderError)?
        .components()
        .count()
        > 12
    {
        return Err(DeviceProviderError);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(DeviceProviderError);
        }
        if matches!(entry.file_name().to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        if kind.is_dir() {
            collect_skill(root, &path, files, total)?;
        } else if kind.is_file() {
            let size = usize::try_from(entry.metadata()?.len()).map_err(|_| DeviceProviderError)?;
            if files.len() >= 256 || size > MAX_SKILL_BYTES.saturating_sub(*total) {
                return Err(DeviceProviderError);
            }
            let bytes = fs::read(&path)?;
            *total += bytes.len();
            if *total > MAX_SKILL_BYTES {
                return Err(DeviceProviderError);
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| DeviceProviderError)?
                .to_str()
                .ok_or(DeviceProviderError)?
                .to_owned();
            if !relative_path(&relative) {
                return Err(DeviceProviderError);
            }
            files.insert(
                relative,
                (bytes, entry.metadata()?.permissions().mode() & 0o111 != 0),
            );
        } else {
            return Err(DeviceProviderError);
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), DeviceProviderError> {
    if path.exists() {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0
        {
            return Err(DeviceProviderError);
        }
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        private_directory(parent)?;
    }
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

fn checked_file(path: &Path) -> Result<(), DeviceProviderError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError);
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), DeviceProviderError> {
    private_directory(path.parent().ok_or(DeviceProviderError)?)?;
    if path.exists() {
        checked_file(path)?;
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| DeviceProviderError)?;
    let temporary = path.with_file_name(format!(".extension-{:x}", Sha256::digest(nonce)));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(path.parent().ok_or(DeviceProviderError)?)?.sync_all()?;
        Ok::<_, DeviceProviderError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_metadata_cannot_poison_the_device_control_stream() {
        let directory =
            std::env::temp_dir().join(format!("wwc-extension-frame-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let store = DeviceProviderStore::open(&directory).expect("store");
        assert_eq!(
            store
                .save_entry(Kind::Mcp, "oversized", "{}", &"x".repeat(192 * 1024))
                .expect("bounded write"),
            DeviceExtensionOutcome::InvalidRequest
        );
        let snapshot = store
            .extension_snapshot("device")
            .expect("unchanged snapshot");
        assert_eq!(snapshot.revision, 0);
        assert!(snapshot.mcp_servers.is_empty());
        drop(store);
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
