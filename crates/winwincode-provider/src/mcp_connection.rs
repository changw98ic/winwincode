// SPDX-License-Identifier: Apache-2.0

//! MCP discovery uses the same client and configuration parser as embedded Core.

use crate::DeviceProviderError;
use codex_config::types::{
    AuthKeyringBackendKind, McpServerConfig, McpServerTransportConfig, OAuthCredentialsStoreMode,
};
use codex_rmcp_client::{
    ElicitationAction, ElicitationResponse, LocalStdioServerLauncher, RmcpClient,
};
use futures::FutureExt;
use rmcp::model::{
    ClientCapabilities, Implementation, InitializeRequestParams, PaginatedRequestParams,
};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

pub(crate) fn validate(configuration: &str) -> Result<McpServerConfig, DeviceProviderError> {
    if configuration.len() > 32_768 {
        return Err(DeviceProviderError);
    }
    let config: McpServerConfig = serde_json::from_str(configuration)?;
    if !config.is_local_environment() {
        return Err(DeviceProviderError);
    }
    match &config.transport {
        McpServerTransportConfig::Stdio {
            command, args, cwd, ..
        } => {
            if command.trim().is_empty()
                || command.len() > 4096
                || args.len() > 128
                || cwd
                    .as_ref()
                    .is_some_and(|path| !Path::new(path.as_str()).is_absolute())
            {
                return Err(DeviceProviderError);
            }
        }
        McpServerTransportConfig::StreamableHttp {
            url,
            http_headers_helper,
            ..
        } => {
            // Header helper commands need their own lifecycle; explicit headers and env references
            // use Core's existing credential boundary.
            if !(url.starts_with("https://")
                || url.starts_with("http://127.0.0.1:")
                || url.starts_with("http://localhost:"))
                || http_headers_helper.is_some()
            {
                return Err(DeviceProviderError);
            }
        }
    }
    Ok(config)
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn discover(
    id: &str,
    configuration: &str,
    home: &Path,
) -> Result<Vec<String>, DeviceProviderError> {
    let config = validate(configuration)?;
    let enabled_tools = config.enabled_tools;
    let disabled_tools = config.disabled_tools.unwrap_or_default();
    let client = match config.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => RmcpClient::new_stdio_client(
            command.into(),
            args.into_iter().map(Into::into).collect(),
            env.map(|env| env.into_iter().map(|(k, v)| (k.into(), v.into())).collect()),
            &env_vars,
            cwd.map(|p| p.to_string()),
            Arc::new(LocalStdioServerLauncher::new(home.to_path_buf())),
        )
        .await
        .map_err(|_| DeviceProviderError)?,
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            ..
        } => {
            let bearer = bearer_token_env_var
                .map(|key| std::env::var(key).map_err(|_| DeviceProviderError))
                .transpose()?;
            let http = codex_exec_server::RouteAwareHttpClient::new(
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            );
            RmcpClient::new_streamable_http_client(
                id,
                &url,
                bearer,
                http_headers,
                env_http_headers,
                OAuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
                Arc::new(http),
                None,
            )
            .await
            .map_err(|_| DeviceProviderError)?
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        client
            .initialize(
                InitializeRequestParams::new(
                    ClientCapabilities::default(),
                    Implementation::new("winwincode", env!("CARGO_PKG_VERSION")),
                ),
                Some(Duration::from_secs(10)),
                Box::new(|_, _| {
                    async {
                        Ok(ElicitationResponse {
                            action: ElicitationAction::Decline,
                            content: None,
                            meta: None,
                        })
                    }
                    .boxed()
                }),
            )
            .await
            .map_err(|_| DeviceProviderError)?;
        let mut names = BTreeSet::new();
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        loop {
            let page = client
                .list_tools(
                    cursor.map(|cursor| {
                        let mut params = PaginatedRequestParams::default();
                        params.cursor = Some(cursor);
                        params
                    }),
                    Some(Duration::from_secs(10)),
                )
                .await
                .map_err(|_| DeviceProviderError)?;
            for tool in page.tools {
                let name = tool.name.to_string();
                // ponytail: keep model names identical to protocol names; a native identity map is needed for renamed/hashed tools.
                if !valid_tool(&name)
                    || id.len() + name.len() + 7 > 128
                    || names.len() >= 128
                    || names
                        .iter()
                        .any(|existing: &String| existing.eq_ignore_ascii_case(&name))
                {
                    return Err(DeviceProviderError);
                }
                names.insert(name);
            }
            cursor = page.next_cursor;
            match &cursor {
                Some(next) if cursors.len() < 16 && cursors.insert(next.clone()) => {}
                None => break,
                Some(_) => return Err(DeviceProviderError),
            }
        }
        Ok(names
            .into_iter()
            .filter(|name| {
                enabled_tools
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
                    && !disabled_tools.contains(name)
            })
            .collect())
    })
    .await
    .unwrap_or(Err(DeviceProviderError));
    client.shutdown().await;
    result
}

pub(crate) fn valid_tool(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
