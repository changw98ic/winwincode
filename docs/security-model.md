# WinWinCode 安全模型

漏洞报告方式见 [安全政策](../SECURITY.md)。

## 产品范围

Server 保存一个 Owner 账户。协议和存储使用 Organization、Workspace、Project 和 Repository 四级作用域，匹配资源身份与访问权限。

Browser Client 负责展示和协议消费。Server 持有产品状态和命令入口。Device Client 持有设备状态、仓库访问和 Device Provider 凭据。Worker 在租约内执行任务。内嵌 Codex 内核负责模型、工具和任务生命周期。

## 保护对象

主要保护对象如下：

- Owner 密码、浏览器会话和授权作用域。
- 工作区、仓库、候选分支和执行产物。
- Device Provider API key、自定义请求头和 OAuth 凭据。
- Worker 会话凭据、执行租约和工具授权回执。
- 发布二进制、helper 清单、来源摘要和校验和。
- 审计、调用、用量和恢复记录的完整性。

## 信任边界

### Browser Client 到 Server

Server 是浏览器命令的安全权威。Browser Client 不能直接写产品数据库。Server 校验 Origin、会话、Actor 和 Scope，然后处理命令或查询。

### Server 到 Device Client

Server 只向具有有效管理授权的 Device 下发配置命令。Device 必须在线。执行操作还要求当前占用租约。Server 保存下行命令和回执状态。

Device Provider 配置在浏览器中加密。Browser Client 使用 Device 上报的 P-256 公钥执行 ECDH。它再使用 HKDF-SHA-256 派生密钥，并使用 AES-256-GCM 加密。AAD 绑定 Client Node、`requestId` 和 `expectedRevision`。Server 中继并持久化密文。Device Client 解密并应用配置。

该机制使明文 Provider 凭据不进入正常的 Server 持久化路径。它不把 Server 变成不可信中继。Browser Client 从 Server 获取 Device 公钥。被攻破的 Server 可以替换该公钥。

### Device Client 到 Worker

每次 Worker 启动使用独立的 32 字节随机凭据。Server 只保存 SHA-256 摘要。明文凭据一次性下发到 Device，并写入 Worker 数据目录中的 `worker-credential` 文件。该文件模式为 `0600`。凭据默认有效期为 30 分钟。续期要求已接受的 heartbeat、当前 Worker 实例、有效启动授权、设备占用和执行租约完全匹配。

### Worker 到内嵌 Codex

Worker 为每个任务固定工作区模式、工具目录、模型路由和执行身份。内嵌 Codex 发起文件、shell、网络或 MCP 动作前，必须通过 Worker 的 Action Gate。Server 是动作授权回执的签发方。

### 进程到外部服务

Provider、MCP、发布适配器和连接器可以访问外部服务。配置这些端点等于扩展信任边界。0.2.0 没有统一的产品级目标域名 allowlist。

## 登录和会话

Server 当前只保存一个 Owner 账户。密码使用 Argon2id 和随机盐。未知用户名也执行一次虚拟密码校验，以减小用户名枚举的计时差异。

认证子系统提供使用 `WWC_SERVER_BOOTSTRAP_PROOF` 初始化 Owner 的 API。默认 bootstrap 窗口为 Server 启动后的 10 分钟。初始化只允许成功一次。bootstrap proof 使用 Bearer 头传递。普通登录拒绝 Bearer 认证。

生产 Server 组合仍要求 Owner 在开始提供服务前已经存在。因此，全新数据目录的 password 模式还没有完整的在线首次初始化流程。`local-open` 会在启动时自动建立 Owner。但是，它生成的随机 Owner 密码不会返回给操作者，而密码修改接口要求当前密码。0.2.0 的 `local-open` 到 password 模式转换流程也尚未完成。password 模式的生产首次初始化仍是待完成的组合工作。

默认会话 TTL 为 8 小时。会话令牌来自 32 字节系统随机数。Server 只在 `auth-sessions.sqlite3` 中保存令牌的 SHA-256 摘要。该目录模式为 `0700`。数据库文件模式为 `0600`。退出登录会撤销当前会话。

HTTP 会话 Cookie 使用 `HttpOnly` 和 `SameSite=Lax`。启用 TLS 后，Cookie 使用 `HttpOnly; Secure; SameSite=None`。认证端点要求显式 Origin。Server 只回显配置的 `WWC_SERVER_ALLOWED_ORIGINS`，并允许凭据。其他 API 在 Origin 存在时也执行 allowlist 校验。URL 查询参数不能携带凭据或路由值。

登录失败限制按来源 IP 和规范化用户名计算。默认 15 分钟内允许 5 次失败。单个来源 IP 的总失败上限是 50 次。

当 Server 绑定 loopback 地址且未显式配置认证模式时，默认模式是 `local-open`。该模式会创建本地 Owner，并允许没有 Cookie 的请求使用本地 Principal。只应在受信任的本机回环接口使用该模式。不要在共享主机或可被其他用户访问的代理后使用该模式。

0.2.0 的密码修改路径不会自动撤销已经签发的浏览器会话。会话在退出、显式撤销或过期前仍然有效。

## 请求、修订和 fencing

写命令使用三个互补机制：

- `requestId` 标识一次逻辑请求。相同请求和相同内容可以重放。相同标识和不同内容会冲突。
- `expectedRevision` 实现乐观并发控制。状态已经变化时，命令返回 revision conflict。
- fencing token 绑定当前占用或执行租约。只接受与当前 lease、attempt、Worker 和 Worker instance 完全匹配的消息。

Device Client 的 `FencingGuard` 要求 lease ID 和 fencing token 与本地 occupancy mirror 完全相等。较低 token、未见过的较高 token 和其他 lease 都会被拒绝。mirror revision 变化也会使已经签发的本地 ticket 失效。

Worker 凭据绑定 Worker Session、Worker、Worker Instance 和启动授权。远程连接还绑定当前 Worker placement。任务结果和用量结算再次核对 lease、attempt、fencing token、Worker Instance 和会话状态。因此，已被替换的 Worker 不能用旧凭据或旧消息写入当前执行。

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

MCP 工具使用 `mcp://<server>/<tool>` capability。MCP Resource 使用独立的 resource capability。未在当前能力目录中注册的 server、tool 或 resource 操作会被拒绝。目录、运行时、helper 和策略摘要会写入 Tool Runtime Contract。恢复时任一摘要变化都会拒绝继续原任务。

Server 核对当前执行权威后，生成 HMAC-SHA-256 Action Enforcement 回执。Worker 校验完整回执、当前 lease 和请求内容。回执只能消费一次。权限存储不可用、回执被修改或执行权威过期时，动作被拒绝。

只读角色仅允许明确的控制操作、结果轮询和受限的只读 shell 调用。其他动作直接拒绝。可写角色使用 workspace-write 权限，并把 sandbox 根固定到规范化工作区。权限配置使用 read-only 或 workspace-write profile。工具授权和 sandbox 是两个独立检查层。

MCP 自身的审批模式由内嵌 Codex 配置处理。启用 host action approvals 时，自动 MCP 调用会改为由 Host Action Gate 审批。单次批准仍受当前动作回执和租约约束。

## 网络边界

`schema/winwincode/network-request-policy.json` 定义重试、退避、取消检查和不确定结果处理。`scripts/network-entrypoints.json` 登记网络入口和唯一重试责任方。该策略不是目标域名 allowlist。

Device Provider 端点必须是规范化 HTTPS URL。实现启用证书验证和 SNI，默认使用 WebPKI 根，禁止 HTTP redirect，并限制超时、响应大小和 SSE 事件数量。显式 HTTP CONNECT proxy 可以被配置。代理地址会在 Debug 输出中隐藏。自定义请求头拒绝 hop-by-hop 头和 `Host`、`Authorization`、`Proxy-Authorization` 等敏感控制头。

Worker 到 Server 的执行通道只接受 HTTPS origin，并校验指定 TLS 根。Device Client 底层的 `HttpExchangeTransport` 支持 HTTP 和 HTTPS，但生产命令 `wwc device serve` 经过 `service.rs` 的 URL 校验，只接受 HTTPS。部署时应提供 TLS，并限制 Server bind 地址、防火墙和 allowed origins。部署要求见[部署指南](operations/deploy.md)。

## 发布完整性

发布流程为 `winwincode-kernel-helper` 生成 `winwincode-kernel-helper.release.json`。清单使用 Ed25519 签名。签名覆盖 schema、协议版本、产品版本、helper 源码 SHA-256、二进制 SHA-256、相对路径和文件模式。

Worker 启动时从 `WWC_WORKER_HELPER_RELEASE_MANIFEST` 加载清单。`HelperReleaseManifest::from_file` 使用编译进 `winwincode-codex` 的公钥验证签名。运行时还核对清单文件类型、大小、二进制名称、模式、摘要、package version 和 helper source digest。校验失败时不会使用该 helper。

helper 验证公钥的源码入口是 `crates/winwincode-codex/build.rs` 中的 `WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX`。发布 workflow 从同名 GitHub Actions variable 注入该值。

macOS 发布二进制使用 `codesign --sign -` 进行 ad-hoc 签名。0.2.0 没有 Apple Developer ID 签名或 Apple 公证。ad-hoc 签名不能证明发布者身份。发布包还提供 SHA-256 校验和和发布验收材料。发布流程见[发布指南](operations/release.md)。

## 失败策略和剩余风险

关键身份、租约、回执、schema 或摘要不匹配时，组件应拒绝操作。恢复逻辑保留已发生的 Provider 调用、工具调用和结算事实。它不会把未知结果改写为成功。

以下风险仍需要部署者处理：

- 主机 root、同等系统权限或 Owner 被攻破后，可以读取工作区和本机凭据。
- Device Provider secret 依赖设备文件权限，没有应用层静态加密。
- `local-open` 把本机进程视为同一信任域。
- 配置的 MCP server、Provider endpoint、proxy 和自定义 CA 会扩展信任边界。
- 产品没有统一的外联目标 allowlist。
- macOS 包没有 Developer ID 身份和公证。
- 浏览器到 Device 的配置加密依赖 Server 转发的 Device 公钥。

部署者应把 Server 数据目录、Device 数据目录、secret 目录、TLS 私钥、action signing key 和备份视为敏感数据。不要把这些内容提交到仓库或附在公开问题中。

## 验证与维护

安全相关变更必须同步检查 schema、生成代码、运行时和测试。测试策略见[测试指南](testing.md)。运行约束见[不变量](invariants.md)。上游 Codex 更新见[上游维护](upstream.md)。术语见[术语表](glossary.md)。架构决定见 [ADR 索引](adr/README.md)。

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
- `crates/winwincode-server/src/worker_session_credentials.rs`：`WorkerSessionCredentialService`。
- `crates/winwincode-server/src/remote_worker_transport.rs`：`WorkerSessionRemoteAuthenticator`、`accept_ingress`。
- `crates/winwincode-device-client/src/fencing.rs`：`FencingGuard`。
- `crates/winwincode-control-plane/src/worker_execution_lifecycle.rs`：`DurableWorkerExecutionLifecycle`。
- `crates/winwincode-control-plane/src/action_policy_enforcement.rs`：`issue_action_enforcement_receipt`。
- `crates/winwincode-execution-port/src/action_enforcement.rs`：`ActionEnforcementIssuer`、`ActionEnforcementVerifier`、`FileActionReceiptUseStore`。
- `crates/winwincode-codex/src/action_bridge.rs`：`ExecutionPortActionGate`、`authorize_capability`。
- `crates/kernel/src/lib.rs`：`apply_workspace_permissions`、`apply_host_action_approvals`。
- `crates/winwincode-provider/src/provider_https_sse.rs`：`HttpsSseProviderConfig`、`canonical_https_endpoint`。
- `schema/winwincode/network-request-policy.json` 和 `scripts/network-entrypoints.json`。
- `crates/winwincode-codex/src/helper_release.rs`：`HelperReleaseManifest`。
- `crates/winwincode-codex/build.rs`、`.github/workflows/release.yml` 和 `scripts/release-artifact-contract.mjs`。
