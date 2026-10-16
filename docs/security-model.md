# WinWinCode 安全模型

漏洞报告方式见 [安全政策](../SECURITY.md)。

## 产品范围

产品范围和组件职责见 [架构总览](architecture/overview.md#产品范围)。下文说明这些组件之间的安全边界。

## 保护对象

主要保护对象如下：

- Owner 密码、浏览器会话和授权作用域。
- 工作区、仓库、候选分支和执行产物。
- Device Provider API key、自定义请求头、OAuth 凭据和 Device 签名私钥。
- 执行租约、进程归属和工具授权回执。
- 发布二进制、来源摘要和校验和。
- 审计、调用、用量和恢复记录的完整性。

## 信任边界

### Browser Client 到 Server

Server 是浏览器命令的安全权威。Browser Client 不能直接写产品数据库。Server 校验 Origin、会话、Actor 和 Scope，然后处理命令或查询。

### Server 到 Device Client

Server 只向具有有效管理授权的 Device 下发配置命令。Device 必须在线。执行操作还要求当前占用租约。Server 保存下行命令和回执状态。

Device Provider 配置在浏览器中加密。Browser Client 使用 Device 上报的 P-256 公钥执行 ECDH。它再使用 HKDF-SHA-256 派生密钥，并使用 AES-256-GCM 加密。AAD 绑定 Client Node、`requestId` 和 `expectedRevision`。Server 中继并持久化密文。Device Client 解密并应用配置。

该机制使明文 Provider 凭据不进入正常的 Server 持久化路径。它不把 Server 变成不可信中继。Browser Client 从 Server 获取 Device 公钥。被攻破的 Server 可以替换该公钥。

### Device Client 到 Worker

Device 创建并独占 Worker 的 stdin/stdout 管道，再启动 Worker。管道的创建、独占和受控继承关系确认通信对象。Device 把管道固定到配置的 Worker 与 Worker instance。内部 ExecutionPort 消息中的会话、任务、attempt 和 fencing 身份由 Worker 与 Server 核对。

Worker 通过私有管道发送交换和用量对账请求。Device 为远程 HTTP 中继和 preview WSS upgrade 签名。Device 私钥保存在 Device 的私有数据目录。Worker 仅接收本机执行配置和必要凭据。Server 核对 Device 归属、启动授权、placement、设备占用和执行租约。

### Worker 到内嵌 Codex

Worker 为每个任务固定工作区模式、工具目录、模型路由和执行身份。内嵌 Codex 发起文件、shell、网络或 MCP 动作前，必须通过 Worker 的 Action Gate。Server 是动作授权回执的签发方。

### 进程到外部服务

Provider、MCP、发布适配器和连接器可以访问外部服务。配置这些端点等于扩展信任边界。0.2.0 没有统一的产品级目标域名 allowlist。

## 登录和会话

Server 当前只保存一个 Owner 账户。密码使用 Argon2id 和随机盐。未知用户名也执行一次虚拟密码校验，以减小用户名枚举的计时差异。

认证子系统提供使用 `WWC_SERVER_BOOTSTRAP_PROOF` 初始化 Owner 的 API。默认 bootstrap 窗口为 Server 启动后的 10 分钟。初始化只允许成功一次。bootstrap proof 使用 Bearer 头传递。普通登录拒绝 Bearer 认证。

全新数据目录的 password 模式先启动 Owner 初始化监听。该监听复用正式服务的传输配置。远程初始化使用 HTTPS。Owner 向 `/api/v1/auth/session` 提交用户名、密码、允许的 Origin 和 bootstrap Bearer proof。初始化成功后，Server 切换到完整产品服务。初始化期间业务 API 返回 `503 SERVER_INITIALIZING`。后续普通登录使用用户名和密码。

`local-open` 会在本机首次启动时自动建立 Owner。它生成的随机密码不返回操作者。现有 local-open Owner 转换为 password 登录仍需要专门的账户运维流程。

默认会话 TTL 为 8 小时。会话令牌来自 32 字节系统随机数。Server 只在 `auth-sessions.sqlite3` 中保存令牌的 SHA-256 摘要。该目录模式为 `0700`。数据库文件模式为 `0600`。退出登录会撤销当前会话。

HTTP 会话 Cookie 使用 `HttpOnly` 和 `SameSite=Lax`。认证端点要求显式 Origin。Server 只回显配置的 `WWC_SERVER_ALLOWED_ORIGINS`，并允许凭据。其他 API 在 Origin 存在时也执行 allowlist 校验。URL 查询参数不能携带凭据或路由值。

登录失败限制按来源 IP 和规范化用户名计算。默认 15 分钟内允许 5 次失败。单个来源 IP 的总失败上限是 50 次。

当 Server 使用 loopback HTTP 且未显式配置认证模式时，默认模式是 `local-open`。该模式会创建本地 Owner，并允许没有 Cookie 的请求使用本地 Principal。远程模式默认使用 `password`，并拒绝 `local-open`。部署者须保护本机回环接口。

0.2.0 的密码修改路径不会自动撤销已经签发的浏览器会话。会话在退出、显式撤销或过期前仍然有效。

## 请求、修订和 fencing

幂等身份防止请求重放产生重复效果，修订检查防止覆盖并发修改，fencing 防止旧执行者继续写入。完整条件由 [命令身份与修订](invariants.md#命令身份与修订)、[租约与 fencing](invariants.md#租约与-fencing) 和 [旧 Worker 消息](invariants.md#旧-worker-的迟到消息) 维护。协议字段见 [契约](contracts.md)。

## Provider 凭据

### 当前生产模型路径

0.2.0 的生产模型路径是 Device-only。API 链路验证脚本拒绝 `WWC_SERVER_MODEL_API_KEY` 等集中式 Server 模型变量。Server 保存 Device Provider 的配置投影、密文命令和凭据是否已配置的布尔值。Server 不保存 Device Provider API key 明文。

### Device Provider

Device Client 将 Provider 状态保存在设备数据目录下的 `providers/providers.sqlite3`。目录模式为 `0700`。数据库文件模式为 `0600`。SQLite 使用 `secure_delete=ON`。

API key 和 ChatGPT Plan OAuth 凭据保存在 `providers` 表的 `secret` 字段。自定义请求头保存在 `provider_headers` 表。0.2.0 没有对这些字段再做应用层静态加密。文件系统权限和主机安全是静态保护边界。设备数据备份也应按凭据备份处理。

`codex_chatgpt` 配置保存本地 Codex 登录绑定。实际访问令牌由该绑定在 Device 上解析。Provider 状态投影只返回 `credentialConfigured`，不返回 secret。

### Server 侧 secret

Server 侧 Credential Reference 只保存标识、作用域、Provider、轮换版本和状态。需要 Server 侧 secret 的发布适配器使用 `SECRET_DIRECTORY`。`LocalSecretStoreAdapter` 要求 secret 目录模式为 `0700`，文件模式为 `0600`，并按作用域和 Credential Reference 分目录保存版本。

`StandaloneModelExecutionApplication` 也支持独立的 `secret_directory`，并要求它与产品数据目录互不包含。该适配器不是 0.2.0 当前生产 Server 的模型执行路径。

## 工具、MCP 和 sandbox

内嵌 Codex 不直接获得长期工具权限。Action Gate 将文件、Git、shell、进程输入、网络、MCP 工具和 MCP Resource 操作规范化。每个动作绑定以下信息：

- 当前 job、lease、attempt 和 fencing token。
- Worker Session 和 Codex Session Identity。
- 规范化资源、操作和请求摘要。
- 当前工具目录和 Worker capability。

MCP 工具使用 `mcp://<server>/<tool>` capability。MCP Resource 使用独立的 resource capability。未在当前能力目录中注册的 server、tool 或 resource 操作会被拒绝。目录、运行时和策略身份会写入 Tool Runtime Contract。恢复时按当前执行身份检查这些内容。

Server 核对当前执行权威后，生成动作授权回执。Worker 核对请求、当前租约、会话、动作和决策，并持久记录回执消费状态。回执只能消费一次。

只读角色仅允许明确的控制操作、结果轮询和受限的只读 shell 调用。其他动作直接拒绝。可写角色使用 workspace-write 权限，并把 sandbox 根固定到规范化工作区。权限配置使用 read-only 或 workspace-write profile。工具授权和 sandbox 是两个独立检查层。

MCP 自身的审批模式由内嵌 Codex 配置处理。启用 host action approvals 时，自动 MCP 调用会改为由 Host Action Gate 审批。单次批准仍受当前动作回执和租约约束。

## 网络边界

`schema/winwincode/network-request-policy.json` 定义重试、退避、取消检查和不确定结果处理。`scripts/network-entrypoints.json` 登记网络入口和唯一重试责任方。该策略不是目标域名 allowlist。

Device Provider 端点必须是规范化 HTTPS URL。实现启用证书验证和 SNI，默认使用 WebPKI 根，禁止 HTTP redirect，并限制超时、响应大小和 SSE 事件数量。显式 HTTP CONNECT proxy 可以被配置。代理地址会在 Debug 输出中隐藏。自定义请求头拒绝 hop-by-hop 头和 `Host`、`Authorization`、`Proxy-Authorization` 等敏感控制头。

本机 Server 默认监听 `127.0.0.1:8443` 并使用 HTTP。远程 Server 的对外 URL 必须使用 HTTPS。应用进程可以直接加载证书与私钥，也可以显式使用受保护的 loopback TLS 代理后端。代理模式要求监听地址为回环 IP，且对外 URL 为 HTTPS。Server 依据显式配置判断传输边界。

Device 仅主动连接 Server。Device 校验证书与主机名，默认信任 WebPKI 根。部署者可以在 Device 起点配置 CA 文件。Device 拒绝非回环 HTTP Server。控制交换、Worker 中继和 preview WSS 共享 Device 签名身份。部署步骤见 [远程 Server 与 Device](operations/deploy.md#远程-server-与-device)。

Device 私钥、稳定绑定请求与绑定状态保存在 `device-client.sqlite3`。数据目录权限为 `0700`，数据库权限为 `0600`。成功保存绑定后，Device 清除本地配对令牌。Server 保留公钥与绑定回执。配对令牌有效期为 10 分钟；请求挑战有效期为 60 秒，且只能消费一次。

匿名配对挑战最多保留 4096 条。已绑定公钥使用无状态挑战。Server 从该绑定的配对令牌摘要派生带域分离的 Ed25519 密钥，并签名公钥和挑战 ID。挑战 ID 包含到期时间和 24 字节随机数。签发已绑定挑战不写入存储。匿名申请只读取该绑定，不增加消费记录。

`device_auth_challenges` 同时保存匿名待消费挑战和已绑定挑战的消费记录。已绑定挑战使用 `bound_` ID 前缀。Server 仅在 Device 请求验签成功后，插入该挑战的消费记录。匿名容量只计算其他 ID。唯一键和立即事务保证同一挑战只能消费一次。不同挑战可以并发消费。匿名挑战签发事务和已绑定挑战消费事务会清理过期记录。签名继续绑定 method、path、Client Node 和原始请求体；吊销检查继续拒绝失效身份。

Device 对首次 `401 DEVICE_AUTH_REQUIRED` 换新挑战重试一次。再次认证失败或未明确吊销的 `403` 使当前服务停止 Worker。后续正常启动可以重新签名认证。只有 `403 DEVICE_REVOKED` 将 `DEVICE_REVOKED` 写入本地失败状态。旧版按 HTTP 状态码保存的认证失败记录在启动时清除，由 Server 用原身份重新判定。离线期间，Worker 的本地租约截止时间约束继续执行。重连不恢复旧的失效执行权威。

## 发布完整性

发布包记录 Server、Worker、`wwc` 和内核辅助进程的 SHA-256。发布验收检查源码、平台、文件清单和许可证。Device 为 Worker 指定内核辅助进程的绝对路径。Worker 按该路径直接执行程序。

macOS 发布二进制使用 `codesign --sign -` 进行 ad-hoc 签名。0.2.0 没有 Apple Developer ID 签名或 Apple 公证。ad-hoc 签名不能证明发布者身份。发布包还提供 SHA-256 校验和和发布验收材料。发布流程见[发布指南](operations/release.md)。

## 失败策略和剩余风险

关键身份、租约、回执、schema 或摘要不匹配时，组件应拒绝操作。恢复中的调用事实处理见 [崩溃恢复](invariants.md#崩溃恢复与新-attempt)。

以下风险仍需要部署者处理：

- 主机 root、同等系统权限或 Owner 被攻破后，可以读取工作区和本机凭据。
- Device Provider secret 和 Device 签名私钥依赖设备文件权限，没有应用层静态加密。
- `local-open` 把本机进程视为同一信任域。
- 配置的 MCP server、Provider endpoint、proxy 和自定义 CA 会扩展信任边界。
- 产品没有统一的外联目标 allowlist。
- macOS 包没有 Developer ID 身份和公证。
- 浏览器到 Device 的配置加密依赖 Server 转发的 Device 公钥。

部署者应把 Server 数据目录、Device 数据目录、secret 目录和备份视为敏感数据。不要把这些内容提交到仓库或附在公开问题中。

## 验证与维护

安全相关变更按 [协议修改流程](contracts.md#生成文件规则) 和 [定向验证](testing.md#定向验证) 检查受影响边界。上游 Codex 更新见 [上游维护](upstream.md)。

## 实现依据

以下文件和符号是本文的主要事实来源：

- `crates/winwincode-server/src/auth_session.rs`：`AuthSessionConfig`、`SqliteAuthSessionManager`、`IssuedBrowserSession`。
- `crates/winwincode-server/src/user_accounts.rs`：`UserAccountService`。
- `crates/winwincode-server/src/password_hash.rs`：`hash_password`、`verify_password`。
- `crates/winwincode-server/src/server.rs`：`authorize`、`allowed_origin`、`extract_credentials`。
- `crates/winwincode-server/src/device_providers.rs`：`node_for_user`、`apply`、`observe`。
- `apps/client/src/device-provider-encryption.ts`：`encryptConfiguration`。
- `crates/winwincode-provider/src/device_store.rs`：`DeviceProviderStore`、`valid_device_provider_config`。
- `crates/winwincode-control-plane/src/local_secret_store.rs`：`LocalSecretStoreAdapter`。
- `crates/winwincode-server/src/remote_worker_transport.rs`：`ProductionRemoteWorkerExchange`、`accept_ingress`。
- `crates/winwincode-device-client/src/fencing.rs`：`FencingGuard`。
- `crates/winwincode-device-client/src/remote_auth.rs`、`http.rs` 和 `preview.rs`：Device 密钥、持久绑定和签名传输。
- `crates/winwincode-server/src/device_auth.rs`、`crates/winwincode-storage/src/device_auth.rs`：挑战、绑定事务和吊销。
- `crates/winwincode-control-plane/src/worker_execution_lifecycle.rs`：`DurableWorkerExecutionLifecycle`。
- `crates/winwincode-control-plane/src/action_policy_enforcement.rs`：`issue_action_enforcement_receipt`。
- `crates/winwincode-execution-port/src/action_enforcement.rs`：`validate_action_receipt`、`FileActionReceiptUseStore`。
- `crates/winwincode-codex/src/action_bridge.rs`：`ExecutionPortActionGate`、`authorize_capability`。
- `crates/kernel/src/lib.rs`：`apply_workspace_permissions`、`apply_host_action_approvals`。
- `crates/winwincode-provider/src/provider_https_sse.rs`：`HttpsSseProviderConfig`、`canonical_https_endpoint`。
- `schema/winwincode/network-request-policy.json` 和 `scripts/network-entrypoints.json`。
- `crates/helper/src/main.rs`、`.github/workflows/release.yml` 和 `scripts/release-artifact-contract.mjs`。
