# 公网 Server 部署参考与安全默认值

本文是 WinWinCode Server 公网部署的运维参考：部署拓扑、安全默认值核查表、环境变量清单与上线前核查清单。全部行为性描述以当前源码为准，逐条标注代码位置（文件与符号）；本文不引入新连接协议，也不承诺代码未实现的行为。发布产物与版本冻结流程见 [releasing.md](releasing.md) 与 [release-gate.md](release-gate.md)。

代码位置缩写（均在仓库根目录下）：

| 缩写 | 完整路径 |
| --- | --- |
| `main.rs` | `crates/winwincode-server/src/main.rs` |
| `server.rs` | `crates/winwincode-server/src/server.rs` |
| `config.rs` | `crates/winwincode-server/src/config.rs` |
| `auth_session.rs` | `crates/winwincode-server/src/auth_session.rs` |
| `login_rate_limiter.rs` | `crates/winwincode-server/src/login_rate_limiter.rs` |
| `remote_worker_transport.rs` | `crates/winwincode-server/src/remote_worker_transport.rs` |
| `client_exchange.rs` | `crates/winwincode-server/src/client_exchange.rs` |
| `local_secret_store.rs` | `crates/winwincode-control-plane/src/local_secret_store.rs` |
| `device-client/lib.rs` | `crates/winwincode-device-client/src/lib.rs` |
| `device-client/daemon.rs` | `crates/winwincode-device-client/src/daemon.rs` |
| `device-client/http.rs` | `crates/winwincode-device-client/src/http.rs` |
| `control-plane-client.ts` | `apps/client/src/control-plane-client.ts` |

## 1. 部署拓扑：公网 Server + 本地 Device Client

### 1.1 出站 exchange 模型

只有 Server 暴露公网。Server 绑定一个地址（`main.rs` `environment_config` 读取 `WWC_SERVER_BIND`），以一条 HTTP/HTTPS origin 服务全部流量（`server.rs` `start_server` 的文档："Start the one public HTTP/HTTPS origin"；`crates/winwincode-server/README.md`："Worker execution and provider addresses are not part of this router"）。

Device Client 是本地常驻进程，没有任何入站监听：`crates/winwincode-device-client` 中不存在 `TcpListener`；它与 Server 的唯一通道是周期性向 `POST /internal/v1/client/exchange` 发起的出站 exchange（`device-client/lib.rs` `daemon` 模块文档；`device-client/http.rs` `HttpExchangeTransport`——"Minimal std HTTP/1.1 `POST` implementation"）。一次 exchange 是一个有界批次：上行帧 + 对下行流的确认游标，响应带回下行批次；断线按指数退避恢复（`device-client/daemon.rs` `DaemonConfig`：初始退避 1 秒、上限 30 秒；`device-client/http.rs`：单次操作 10 秒 socket 超时、32 MiB 响应上限）。

浏览器（DSH chat 面，`apps/client` 静态包）独立托管，通过 `runtime-config.js` 的 `serverUrl` 指向公网 Server（`docs/releasing.md` 第 4 节第 3 条）；页面内所有请求走该 origin，事件流由 `/api/v1/events` 升级为 WebSocket，`https:` 映射为 `wss:`（`control-plane-client.ts` `parseControlPlaneServerUrl`）。

```text
浏览器（apps/client 静态包，任意静态托管）
    │  HTTPS + WSS（cookie 会话；Origin 允许列表）
    ▼
公网 ──► winwincode-server（唯一公网入口，TLS 由 Server 终结）
             ▲
             │  出站 POST /internal/v1/client/exchange（Bearer Device Credential）
             │
本地 Device Client（零入站端口；本机 SQLite、本机 Worker）
```

### 1.2 公网路由面

| 路由 | 面 | 认证与准入 | 代码依据 |
| --- | --- | --- | --- |
| `GET /health` | 公开健康检查 | 无认证、无 Origin 要求 | `server.rs` `router`、`health` |
| `GET/POST/DELETE /api/v1/auth/session` | 浏览器 | Origin 必须在允许列表；Bearer 仅为一次性初始化；日常为用户名 + Argon2id 密码登录 | `server.rs` `create_auth_session`；`auth_session.rs` `initialize`、`login` |
| `GET /api/v1/server/initialization` | 浏览器 | Origin 允许列表；无凭据；只发布一个布尔值 | `server.rs` `server_initialization` |
| `/api/v1/commands`、`/queries`、`/events`、`/users`、`/users/state`、`/users/password`、`/clients/*`、`/repositories`、`/sessions` | 浏览器 | `wwc_session` cookie 会话 + Origin 允许列表；管理路由要求 Owner 角色 | `server.rs` `router`、`authorize`、`require_owner` |
| `POST /internal/v1/client/exchange` | Device Client | Bearer Device Credential；统一 401 | `server.rs` `client_control_exchange` |
| `POST /internal/v1/execution-port/exchange` | 远程 Worker | Bearer 注册凭据；统一 401 | `server.rs` `remote_worker_exchange`；`remote_worker_transport.rs` `FileRemoteWorkerAuthenticator` |

未挂载对应应用时 `/internal` 路由返回 404：本地组成模式（`WWC_SERVER_WORKER_MODE=local`，默认值）调用 `start_server` 时不附带 remote Worker 与 client exchange，两个 `/internal` 路由都不可达（`main.rs` `run_local_composition`、`serve_runtime`；`server.rs` `remote_worker_exchange`、`client_control_exchange` 的 `let Some(...) = ... else { 404 }`）。Device Client 拓扑要求 `WWC_SERVER_WORKER_MODE=remote`，此时两个 `/internal` 路由随同挂载（`main.rs` `run_remote_composition`）。

### 1.3 最小暴露端口

- 公网侧只需放行一个 TCP 端口：`WWC_SERVER_BIND` 的监听端口（`main.rs` `environment_config`；`server.rs` `spawn_listener`）。
- Device Client 零入站端口（见 1.1）；Worker 在 Device Client 本机或 Server 进程内执行，不产生额外公网监听（`device-client/lib.rs` `supervisor` 模块文档；`crates/winwincode-server/README.md`）。
- 防火墙/安全组默认拒绝其余全部入站端口。

### 1.4 TLS 终点必须放在 Server

公网 Device Client 部署下，TLS 终点必须就是 Server 进程，这是唯一受支持的形态：

- `WWC_SERVER_WORKER_MODE=remote` 且 TLS 关闭时启动直接失败："remote Worker exchange requires the Server TLS listener"（`server.rs` `start_server_with_remote_worker`）。
- TLS 证书与私钥必须成对出现，只配置其一会启动失败（`main.rs` `environment_config`）；`WWC_SERVER_PUBLIC_URL` 的 scheme 必须与 TLS 模式一致，公网部署即 `https://`（`config.rs` `ServerConfig::new`）。
- TLS 在启动时加载，证书或私钥无法加载即启动失败（`server.rs` `spawn_listener`，rustls PEM 加载）。
- 会话 cookie 恒为 `Secure`（`auth_session.rs` `IssuedBrowserSession::set_cookie_header`），且 `SameSite=None` 依赖 `Secure` 才会被浏览器接受，明文 HTTP 上浏览器会丢弃 cookie——TLS 因此对浏览器登录同样是硬要求。

因此不要在前置代理处终止 TLS 再以 HTTP 回源；若必须前置 LB/CDN，使用 TCP/TLS 透传并保持字节流不改写（Host、Origin、Authorization、Cookie 都不得被改写或剥离），否则 Origin 校验、CORS 与 cookie 语义会被破坏（见第 2 节第 6、7 行）。

### 1.5 参考进程守护（systemd）

进程在收到 SIGINT（Ctrl-C）后走优雅停机路径：停止监听、排水 Worker 与事件、关闭应用（`main.rs` `serve_runtime`、`serve_remote_runtime` 等待 `tokio::signal::ctrl_c`；`server.rs` `RunningServer::shutdown_listener`、`shutdown_application`）。停机宽限为 30 秒（`main.rs` `environment_config` 传入 `Duration::from_secs(30)`；`server.rs` `RunningServer` 的 `shutdown_grace`）。systemd 默认发送 SIGTERM，不会进入该路径，因此单元文件显式指定 `KillSignal=SIGINT`，并把 `TimeoutStopSec` 设为大于 30 秒：

```ini
[Unit]
Description=WinWinCode Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=winwincode
Group=winwincode
EnvironmentFile=/etc/winwincode/server.env
ExecStart=/opt/winwincode/bin/winwincode-server
# 进程只监听 SIGINT 进入优雅停机（main.rs tokio::signal::ctrl_c）
KillSignal=SIGINT
# 停机宽限 30 秒，超时上限必须大于宽限
TimeoutStopSec=45
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/winwincode

[Install]
WantedBy=multi-user.target
```

`EnvironmentFile` 中的变量清单见第 3 节；文件权限应为 0600，属主为运行账户。

### 1.6 数据、秘密与备份目录

- `WWC_SERVER_DATA_DIRECTORY`：全部持久状态——Control Plane SQLite、认证会话库（`auth-sessions/`，目录强制 0700、库文件强制 0600，`auth_session.rs` `SqliteAuthSessionManager::open`）、持久事件中枢（`event-hub/`，`main.rs` `open_production_application`）、Device Client 注册与游标、Worker 出站队列与 worker-runtime。
- `SECRET_DIRECTORY`：本地秘密库根目录，目录与引用目录强制仅属主可访问，秘密与锁文件 0600，文件名不含任何 ID、Provider 名或 scope（`local_secret_store.rs` `LocalSecretStoreAdapter` 文档与 `open`）。
- 认证会话库使用 `journal_mode=DELETE` 与 `synchronous=FULL`（`auth_session.rs` `open_with_dependencies` 的 `execute_batch`），无 WAL 边车文件；备份以整目录文件级快照为宜（停机窗口或一致性快照），同时覆盖上述两个目录。
- 认证会话库只存会话令牌的 SHA-256 摘要，不存原始 cookie（`auth_session.rs` `session_digest`；`issue_principal` 只落 `session_digest`），备份文件泄漏不直接等于会话劫持，但仍按秘密对待。

## 2. 安全默认值核查表

| # | 核查项 | 默认行为 | 代码依据 |
| --- | --- | --- | --- |
| 1 | 仅 Server 暴露公网 | Server 是唯一公网入口（一条 origin、一个端口）；Device Client 仅出站 exchange，零入站端口 | `server.rs` `start_server`；`device-client/lib.rs` `daemon`；`device-client/http.rs` `HttpExchangeTransport` |
| 2 | TLS 必开（Device Client 拓扑） | `remote` 模式 + TLS 关闭 → 拒绝启动；证书/私钥只配一个 → 启动失败；`publicUrl` scheme 必须匹配 TLS 模式；证书加载失败 → 启动失败 | `server.rs` `start_server_with_remote_worker`；`main.rs` `environment_config`；`config.rs` `ServerConfig::new`；`server.rs` `spawn_listener` |
| 3 | Cookie 属性 | `wwc_session=<43+ 字符随机值>; Path=/; HttpOnly; Secure; SameSite=None; Max-Age=<TTL>`；登出下发同属性空值 `Max-Age=0`；HttpOnly 阻断脚本读取，Secure 强制 HTTPS，SameSite=None 配合 CORS 允许源回显支持独立托管的浏览器面 | `auth_session.rs` `set_cookie_header`、`cleared_session_cookie_header`；`server.rs` `apply_cors` |
| 4 | 会话只存摘要 | 库中仅存令牌 SHA-256（64 位十六进制）与主体、时间、吊销状态；原始令牌与 bootstrap proof 不落库、不回显；会话目录 0700、库文件 0600 | `auth_session.rs` `session_digest`、`issue_principal`、`open_with_dependencies`；令牌不落库的测试 `initialization_persists_digest_and_secret_free_context_then_revokes` |
| 5 | Bootstrap 一次性 + 窗口 | 一次性 proof 仅在未初始化时可用：换取第一个 Owner 后初始化永久关闭（内存 + `server_initialization` 持久标记），重启后任何 proof 均被拒；窗口从进程启动起算，默认 600 秒，上限 86400 秒（`WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS` 可调，0 或超上限 → 启动失败）；proof 恒定时间比较，长度 1..=4096、禁空白/控制字符 | `auth_session.rs` `initialize`、`AuthSessionBootstrap::new`、`AuthSessionConfig::new`、`MAX_BOOTSTRAP_WINDOW_SECONDS`、`constant_time_equal`；持久关闭测试 `initialization_closes_permanently_and_login_survives_restart`；`main.rs` `load_production_startup` |
| 6 | 初始化状态端点暴露面 | `GET /api/v1/server/initialization` 无需凭据，但 Origin 必须在允许列表，URL 携带 query 即 400；响应只有 `schemaVersion` 与一个布尔 `initialized`，无其他信息；`Cache-Control: no-store` | `server.rs` `server_initialization`、`required_origin`、`prevent_auth_session_caching` |
| 7 | Origin 默认拒绝 | `WWC_SERVER_ALLOWED_ORIGINS` 必填且非空（空列表 → 启动失败）；请求携带的 Origin 不在允许列表 → 403 `ORIGIN_DENIED`；auth 端点与预检还要求必须携带 Origin（缺失 → 400 `ORIGIN_REQUIRED`）；CORS 仅回显允许列表中的源并带 `Access-Control-Allow-Credentials: true` 与 `Vary: Origin` | `main.rs` `environment_config`；`config.rs` `ServerConfig::new`（空允许列表拒绝）；`server.rs` `allowed_origin`、`required_origin`、`preflight`、`apply_cors` |
| 8 | `/internal` 面不面向浏览器 | 两个 `/internal` 路由不使用 cookie、不做 Origin 校验，专供设备与远程 Worker 出站调用：Device Credential 为注册时一次性下发的 32 字节随机值，服务端只存其 SHA-256 摘要，之后凭 `Authorization: Bearer <hex>` 交换，摘要恒定时间比较，任何失败统一 401 且不区分节点是否存在；client exchange 路由还拒绝重复 `Authorization` 头（401）；未挂载时 404。公网暴露决策：设备必须能到达 `/internal/v1/client/exchange`；若设备出口 IP 可枚举，建议防火墙再限制该路径来源；浏览器面永不调用 `/internal` | `server.rs` `remote_worker_exchange`、`client_control_exchange`；`client_exchange.rs` 模块文档（Credential model 一节）；`main.rs` `run_local_composition`（local 模式 404） |
| 9 | 登录限流 | 失败按（规范化用户名, 直连对端 IP）二元组计数：15 分钟固定窗口内 5 次失败后锁到窗口结束，返回 429 `RATE_LIMITED`；成功登录清零；互斥锁中毒时 fail-closed（视为拒绝）。注意 key 是直连 TCP 对端：经 TCP 透传时是设备真实地址；若经 L7 代理回源则退化为代理地址，限流近似按用户名全局生效。限流状态在内存中，进程重启后清零 | `login_rate_limiter.rs` `MAX_LOGIN_FAILURES`、`LOGIN_FAILURE_WINDOW_MILLIS`、`rejected`、`record_failure`；`server.rs` `create_auth_session`（以 `ConnectInfo` 对端 IP 为 client）、`auth_session_error` |
| 10 | 会话 TTL | 默认 28800 秒（8 小时），上限 31536000 秒（365 天），0 或超上限 → 启动失败；活跃会话滑动续期：已消耗超过四分之一 TTL 后续期回满 TTL（每四分之一周期至多一次），闲置会话到期即失效；账号禁用立即吊销其全部会话（kill switch）；WebSocket 每次事件下发前与每 250 毫秒复验会话，吊销后以 4403 关闭 | `auth_session.rs` `MAX_SESSION_TTL_SECONDS`、`AuthSessionConfig::new`、`read_session`（滑动续期与过期检查）、`revoke_actor_sessions`；`main.rs` `load_production_startup`；`server.rs` `SESSION_REVALIDATION_INTERVAL`、`principal_is_current`、`close_revoked_socket`、`set_user_state` |
| 11 | 请求边界 | 请求体上限 2 MiB（超出 413 `REQUEST_TOO_LARGE`）；除 `/api/v1/repositories`（clientId 经 query 传递，`server.rs` `list_repositories`）外，浏览器面一律拒绝 URL query（400 `QUERY_PARAMETERS_FORBIDDEN`），凭据只走头与体；Bearer 与 cookie 同时出现、重复 `Authorization` 头、重复 `wwc_session` cookie 均 400 `INVALID_AUTHENTICATION`；HTTP 响应与 WebSocket 帧都过凭据泄漏门，命中即替换为 `CREDENTIAL_OUTPUT_REJECTED` | `server.rs` `MAX_REQUEST_BYTES`、`parse_json_body`、`authorize`、`extract_credentials`、`session_cookie`、`json_response`、`send_value` |
| 12 | 配置缺失启动失败 | 任一必填环境变量缺失或为空 → 报错并以非零码退出；`ServerConfig::new` 拒绝非法/不匹配的 URL、空允许列表、空数据目录、零停机宽限；`AuthSessionConfig` 拒绝零/超上限时长；空 bootstrap proof 列表拒绝打开会话库 | `main.rs` `main`（`std::process::exit(1)`）、`required_environment`、`environment_config`；`config.rs` `ServerConfig::new`；`auth_session.rs` `AuthSessionConfig::new`、`open_with_dependencies` |

## 3. 环境变量清单

以下清单从 `main.rs` 的真实读取点生成（`environment_config`、`load_production_startup`、`local_production_configs`、`run_composed_server`、`run_remote_composition`、`open_production_codex`、`LocalModelRoute::from_environment`、`compose_enterprise_identity_protocol`、`enterprise_identity_mode_enabled`）。

### 3.1 必填（缺失或为空即启动失败）

| 变量 | 用途 | 读取点 |
| --- | --- | --- |
| `WWC_SERVER_BIND` | 监听地址（SocketAddr） | `main.rs` `environment_config` |
| `WWC_SERVER_PUBLIC_URL` | 对外 origin（仅 scheme://authority，须匹配 TLS 模式） | `main.rs` `environment_config`；`config.rs` `normalized_origin` |
| `WWC_SERVER_DATA_DIRECTORY` | 数据目录 | `main.rs` `environment_config` |
| `WWC_SERVER_ALLOWED_ORIGINS` | 浏览器 Origin 允许列表（逗号分隔，至少一个） | `main.rs` `environment_config` |
| `WWC_SERVER_CHECKOUT_REVISION` | 执行配置绑定的 checkout revision | `main.rs` `load_production_startup` |
| `WWC_SERVER_BOOTSTRAP_PROOF` | 一次性初始化凭据（初始化完成后可移除，见第 4 节） | `main.rs` `load_production_startup` |
| `WWC_SERVER_REPOSITORY_ROOT` | Delivery 仓库根 | `main.rs` `local_production_configs` |
| `WWC_SERVER_ORGANIZATION_ID` | 租户 scope：组织 | `main.rs` `local_production_configs` |
| `WWC_SERVER_WORKSPACE_ID` | 租户 scope：工作区 | `main.rs` `local_production_configs` |
| `WWC_SERVER_PROJECT_ID` | 租户 scope：项目 | `main.rs` `local_production_configs` |
| `WWC_SERVER_REPOSITORY_ID` | 租户 scope：仓库 | `main.rs` `local_production_configs` |
| `GITHUB_REPOSITORY` | Delivery 发布绑定的 GitHub 仓库 | `main.rs` `local_production_configs` |
| `GITHUB_CREDENTIAL_REFERENCE_ID` | Delivery 发布凭据引用 | `main.rs` `local_production_configs` |
| `GITHUB_API_BASE_URL` | GitHub API 基址 | `main.rs` `local_production_configs` |
| `SECRET_DIRECTORY` | 本地秘密库根目录 | `main.rs` `open_accounts_authority`、`open_local_model_execution`、`local_production_configs`、`compose_enterprise_identity_protocol` |
| `PUBLICATION_REQUESTERS` | 发布 requester（逗号分隔，至少一个） | `main.rs` `local_production_configs`、`comma_separated_environment` |
| `PUBLICATION_APPROVERS` | 发布 approver（逗号分隔，至少一个） | `main.rs` `local_production_configs` |
| `PUBLICATION_APPROVAL_MAX_AGE_MILLIS` | 发布批准最大有效期（毫秒） | `main.rs` `local_production_configs` |
| `WWC_SERVER_HELPER_EXECUTABLE` | 内部 Kernel helper 可执行文件路径（local 组成模式） | `main.rs` `open_production_codex` |
| `WWC_SERVER_HELPER_RELEASE_MANIFEST` | helper 签名清单文件路径（local 组成模式） | `main.rs` `open_production_codex` |

### 3.2 TLS（Device Client 公网拓扑必配）

| 变量 | 用途 | 读取点 |
| --- | --- | --- |
| `WWC_SERVER_TLS_CERTIFICATE` | PEM 证书路径 | `main.rs` `environment_config` |
| `WWC_SERVER_TLS_PRIVATE_KEY` | PEM 私钥路径 | `main.rs` `environment_config` |

两者必须同时配置或同时缺省，只配一个即启动失败；Device Client 拓扑（remote 模式）下必须配置（见第 2 节第 2 行）。

### 3.3 Device Client 拓扑（`WWC_SERVER_WORKER_MODE=remote`）附加

| 变量 | 必填性 | 用途 | 读取点 |
| --- | --- | --- | --- |
| `WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE` | 必填 | 远程 Worker 注册凭据文件：仅加载 SHA-256 指纹；要求 0600（group/other 无位）、非空且不超过 16 KiB，否则启动失败 | `main.rs` `run_remote_composition`；`remote_worker_transport.rs` `FileRemoteWorkerAuthenticator::open`、`read_private_credential` |
| `WWC_SERVER_REMOTE_WORKER_EXPIRES_AT` | 必填 | 凭据过期时间（RFC 3339），过期即启动失败，运行中到期即拒绝 | `main.rs` `run_remote_composition`；`remote_worker_transport.rs` `open`、`authenticate` |
| `WWC_SERVER_REMOTE_WORKER_ISSUER` | 可选，默认 `winwincode-server` | Worker 主体 issuer | `main.rs` `run_remote_composition` |
| `WWC_SERVER_REMOTE_WORKER_SUBJECT` | 可选，默认 `remote-worker` | Worker 主体 subject | `main.rs` `run_remote_composition` |
| `WWC_SERVER_REMOTE_WORKER_SECURITY_ZONE` | 可选，默认 `default` | Worker 安全域 | `main.rs` `run_remote_composition` |

### 3.4 可选变量与默认值

| 变量 | 默认值 | 用途 | 读取点 |
| --- | --- | --- | --- |
| `WWC_SERVER_WORKER_MODE` | `local`（仅允许 `local` 或 `remote`） | Device Client 拓扑必须为 `remote`，否则 `/internal` 路由 404 | `main.rs` `run_composed_server` |
| `WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS` | `600`（上限 86400） | 一次性初始化窗口 | `main.rs` `load_production_startup`；`auth_session.rs` `MAX_BOOTSTRAP_WINDOW_SECONDS` |
| `WWC_SERVER_SESSION_TTL_SECONDS` | `28800`（上限 31536000） | 浏览器会话 TTL（活跃滑动续期） | `main.rs` `load_production_startup`；`auth_session.rs` `MAX_SESSION_TTL_SECONDS` |
| `WWC_SERVER_EXECUTION_PROFILE` | `codex-chat` | 执行 profile | `main.rs` `load_production_startup` |
| `WWC_SERVER_MAX_RUNTIME_SECONDS` | `3600` | 单次运行时长上限 | `main.rs` `load_production_startup` |
| `WWC_SERVER_MAX_ARTIFACT_BYTES` | `1073741824` | 产物字节上限 | `main.rs` `load_production_startup` |
| `WWC_SERVER_SOURCE_ROOT` | Delivery 仓库根的父目录 | 受控源码根 | `main.rs` `load_production_startup` |
| `WWC_SERVER_WORKER_ID` | `wrk_00000000000000000000000001` | Server 内嵌 Worker 标识 | `main.rs` `run_composed_server` |
| `WWC_SERVER_WORKER_POOL_ID` | `wpl_00000000000000000000000001` | Worker 池标识 | `main.rs` `run_composed_server`、`run_remote_composition` |
| `WWC_SERVER_MODEL_PROVIDER_ID` | `winwincode-loopback` | 本地模型路由 Provider | `main.rs` `LocalModelRoute::from_environment` |
| `WWC_SERVER_MODEL_ID` | `loopback-model` | 本地模型路由模型 | `main.rs` `LocalModelRoute::from_environment` |
| `WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID` | `crd_00000000000000000000000001` | 模型凭据引用 | `main.rs` `LocalModelRoute::from_environment` |
| `WWC_SERVER_ACTION_SIGNING_KEY_HEX` | `1f` x 32（开发默认） | Action Enforcement 签名密钥，64 位十六进制（32 字节），长度或字符非法即启动失败；生产必须显式替换 | `main.rs` `configured_action_signing_key`、`parse_hex_key` |
| `WWC_SERVER_EXECUTION_ENVELOPE_DIGEST` | `sha256:` + 64 个 `a`（开发默认） | 执行信封摘要（local 组成模式读取） | `main.rs` `open_production_codex` |

### 3.5 进程内身份变量（默认每次启动随机，勿固定）

| 变量 | 缺省行为 | 读取点 |
| --- | --- | --- |
| `WWC_SERVER_WORKER_INSTANCE_ID` | 每次启动随机生成（`wki_` 前缀） | `main.rs` `run_composed_server`、`runtime_identity` |
| `WWC_SERVER_SCHEDULER_GENERATION` | 每次启动随机生成（`gen_` 前缀） | `main.rs` `run_composed_server`、`runtime_identity` |

这两个变量故意不提供稳定默认值：重启必须以新 Worker 实例与新 generation 进入调度器，否则会被当作前身进程并抑制仓库替换路径（`main.rs` `run_composed_server` 的注释）。部署时保持不设置即可。

### 3.6 企业身份模式（可选，全有或全无）

设置 `WWC_SERVER_ENTERPRISE_IDENTITY_MODE=https-verifier` 时，以下变量全部必填：`WWC_SERVER_IDENTITY_VERIFIER_ENDPOINT`、`WWC_SERVER_IDENTITY_VERIFIER_TLS_ROOT_DER_FILE`、`WWC_SERVER_IDENTITY_VERIFIER_CREDENTIAL_REFERENCE_ID`、`WWC_SERVER_OIDC_ISSUER`、`WWC_SERVER_OIDC_AUDIENCE`、`WWC_SERVER_SAML_ISSUER`、`WWC_SERVER_SAML_AUDIENCE`、`WWC_SERVER_SCIM_ISSUER`、`WWC_SERVER_SCIM_AUDIENCE`、`WWC_SERVER_IDENTITY_MAX_CLOCK_SKEW_MILLIS`、`WWC_SERVER_IDENTITY_MAX_ASSERTION_AGE_MILLIS`。出现任一配置变量而未设置模式，或模式值不是 `https-verifier`，均启动失败；企业身份模式还要求已完成 Owner 初始化（读取点：`main.rs` `enterprise_identity_mode_enabled`、`compose_enterprise_identity_protocol`）。

### 3.7 调试

| 变量 | 用途 | 读取点 |
| --- | --- | --- |
| `WWC_DEBUG_RUNTIME` | 存在时把 remote Worker / client exchange 错误输出到 stderr（不进入正常日志面） | `server.rs` `remote_worker_exchange`、`client_control_exchange` |

## 4. 上线前核查清单

按顺序执行；每项都对应第 2 节的核查项编号。

1. [ ] 入口收敛：防火墙/安全组只放行 `WWC_SERVER_BIND` 的一个公网 TCP 端口，其余入站端口默认拒绝（核查项 1）。
2. [ ] TLS：`WWC_SERVER_TLS_CERTIFICATE` 与 `WWC_SERVER_TLS_PRIVATE_KEY` 成对配置且加载成功；`WWC_SERVER_PUBLIC_URL` 为同域 `https://` origin；外部握手验证证书有效（核查项 2）。
3. [ ] Origin：`WWC_SERVER_ALLOWED_ORIGINS` 精确列出浏览器面 origin（协议 + 域名 + 端口）；用非列表 Origin 发起请求应得 403 `ORIGIN_DENIED`；对 `/api/v1/auth/session` 不带 Origin 应得 400 `ORIGIN_REQUIRED`（核查项 7）。
4. [ ] 初始化：启动后在窗口内用一次性 proof 完成 Owner 初始化（`POST /api/v1/auth/session`，`Authorization: Bearer <proof>`）；`GET /api/v1/server/initialization` 返回 `initialized: true`；随后从环境中移除 proof 并重启，验证再次提交任何 proof 得到 409 `WRONG_STATE`（核查项 5；初始化永久关闭后 proof 已无用途，留在环境中只增加暴露面）。
5. [ ] `/internal` 暴露面：确认 `/internal/v1/client/exchange` 仅对设备出口可达、浏览器面不调用它；设备出口 IP 可枚举时以防火墙限制来源；无凭据请求得到统一 401（核查项 8）。
6. [ ] 限流预期：同一（用户名，直连 IP）在一个 15 分钟窗口内 5 次失败后被锁到窗口结束并返回 429 `RATE_LIMITED`；确认部署链路不会把所有登录折叠到同一个直连地址（见核查项 9 的代理说明）。
7. [ ] 会话策略：确认 `WWC_SERVER_SESSION_TTL_SECONDS` 满足运维要求（默认 8 小时，活跃滑动续期）；验证禁用账号后其会话立即失效、WebSocket 以 4403 关闭（核查项 10）。
8. [ ] 配置缺失演练：移除任一必填变量后启动，进程必须以非零码退出且 systemd 显示 failed；修复后正常启动（核查项 12）。
9. [ ] 进程守护：`systemctl restart` 走 `KillSignal=SIGINT` 优雅停机，`TimeoutStopSec` 大于 30 秒停机宽限；重启后 Device Client 按退避自动恢复 exchange（`device-client/daemon.rs` `DaemonConfig`）（1.5 节）。
10. [ ] 秘钥与凭据：`WWC_SERVER_ACTION_SIGNING_KEY_HEX` 已显式设置为随机 64 位十六进制（默认值仅供开发）；远程 Worker 凭据文件 0600 且 `WWC_SERVER_REMOTE_WORKER_EXPIRES_AT` 有轮换计划；`SECRET_DIRECTORY` 仅属主可访问（核查项 4、3.3 节；`local_secret_store.rs`）。
11. [ ] 备份：`WWC_SERVER_DATA_DIRECTORY` 与 `SECRET_DIRECTORY` 已纳入备份与恢复演练（1.6 节）。
12. [ ] 产物与版本：使用按 `docs/releasing.md` 冻结、按 `docs/release-gate.md` 校验过的四平台 artifact；浏览器静态包的 `runtime-config.js` 仅通过 `serverUrl` 指向本 Server（`docs/releasing.md` 第 4 节）。

## 5. 边界说明

- 登录限流为单进程内存实现，进程重启即清零，也不在多实例间共享（`login_rate_limiter.rs` 模块文档）；当前产品形态为单 Server 进程。
- Server 未内置 IP 允许列表或 fail2ban 类设施；对 `/internal` 路径的来源限制应由部署侧防火墙实现（第 2 节第 8 行）。
- 本文所述拓扑中 Worker 执行地址不进入公网路由面（`crates/winwincode-server/README.md`）；远程 Worker 分离模式（`/internal/v1/execution-port/exchange`）的凭据来自 `WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE`（3.3 节）。
