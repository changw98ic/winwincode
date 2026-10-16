# 契约

本文档说明 WinWinCode v1 契约的事实来源、生成方式和运行时语义。系统边界见 [架构总览](architecture/overview.md)。浏览器消费方式见 [前端架构](architecture/frontend.md)。长期约束见 [不变量](invariants.md)。测试选择见 [测试](testing.md)。发布前检查见 [发布](operations/release.md)。贡献流程见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

## 事实来源

`schema/winwincode/v1/` 下的七个 `*.schema.json` 是 v1 产品协议的规范输入。它们使用 JSON Schema Draft 2020-12。`SchemaVersion` 的固定值是 `winwincode/v1`。

| 文件 | 负责的边界 |
| --- | --- |
| `domain.schema.json` | 共享标识、作用域、命令信封和领域投影 |
| `control-plane-http.schema.json` | HTTP 命令、查询、响应、错误和 OpenAPI 元数据 |
| `control-plane-events.schema.json` | WebSocket 订阅、事件、游标和重载语义 |
| `execution-port.schema.json` | Control Plane 与 Worker 之间的 ExecutionPort 消息 |
| `client-control.schema.json` | Control Plane 与 Device Client 之间的 ClientControlPort 消息 |
| `device-provider.schema.json` | Device 持有的 Provider 配置和加密控制消息 |
| `device-auth.schema.json` | 远程 Device 配对、挑战、绑定、吊销和请求签名 |

`domain.samples.json` 和 `examples/control-plane-http.examples.json` 是样例与测试数据。它们不是 `generate-contracts.mjs` 扫描的规范 Schema 输入。

消费者包括 Rust API、共享领域类型、ExecutionPort、浏览器 Client、生成的控制平面客户端、OpenAPI 和契约测试。协议变更必须同时检查这些消费者。

## 生成命令

根脚本按固定顺序运行两个生成器：

```text
pnpm contracts:generate
```

该命令先运行 `scripts/generate-network-policy.mjs`，再运行 `scripts/generate-contracts.mjs`。

只检查漂移时运行：

```text
pnpm contracts:check
```

两个生成器的 `--check` 模式只比较预期内容。它不写文件。普通模式会写入生成产物。`generate-contracts.mjs` 对变更使用同目录临时文件和原子重命名。

## `generate-contracts.mjs`

默认输入目录是 `schema/winwincode/v1/`。脚本只读取该目录第一层中以 `.schema.json` 结尾的文件。它会校验 `$schema`、稳定 `$id`、公开 `$defs` 名称、局部引用和可生成的类型结构。

默认输出如下：

| 输出 | 用途 |
| --- | --- |
| `crates/winwincode-api/src/generated.rs` | Rust 控制平面 API 类型 |
| `crates/winwincode-domain/src/generated.rs` | Rust 共享领域类型 |
| `crates/winwincode-execution-port/src/generated.rs` | Rust ExecutionPort 类型 |
| `apps/client/src/generated/contracts.ts` | Client TypeScript 契约类型 |
| `apps/client/src/generated/control-plane-client.ts` | HTTP 与 WebSocket 传输客户端 |
| `packages/contracts/src/device-provider.generated.ts` | 共享 Device Provider 类型 |
| `schema/winwincode/v1/schema-collection.generated.json` | 合并后的 Schema 集合 |
| `schema/winwincode/v1/openapi.generated.json` | OpenAPI 文档 |

脚本支持以下覆盖参数：

- `--schema-dir PATH`
- `--rust-out PATH`
- `--rust-domain-out PATH`
- `--rust-execution-port-out PATH`
- `--typescript-out PATH`
- `--typescript-client-out PATH`
- `--schema-collection-out PATH`
- `--openapi-out PATH`
- `--out-dir PATH`

`--out-dir` 用于隔离生成。它写出三个 Rust 文件、一个 TypeScript 文件、Schema 集合和 OpenAPI。该模式不生成控制平面客户端和 `device-provider.generated.ts`。

## `generate-network-policy.mjs`

输入是 `schema/winwincode/network-request-policy.json`。输出是 `packages/network-request/src/policy.generated.mjs`。

生成器要求 `version` 等于 `winwincode.network-request-policy.v1`。它还要求以下值都是正的安全整数：

- `maxAttempts`
- `initialDelayMs`
- `maxConnectionDelayMs`
- `maxImmediateWaitMs`
- `authorityCheckMs`
- `jitterMs`

`--check` 会把现有输出与预期字节比较。漂移错误是 `network policy generation drift`。

## 生成文件规则

不要手工修改生成文件。修改规范 Schema 或网络策略输入，然后运行 `pnpm contracts:generate`。

提交协议改动时，应包含规范输入和全部受影响的生成产物。提交前运行 `pnpm contracts:check`。`pnpm typecheck` 和 `pnpm build:ts` 也把该检查作为前置步骤。

建议按以下顺序处理协议改动：

1. 修改 `schema/winwincode/v1/*.schema.json` 或网络策略输入。
2. 运行 `pnpm contracts:generate`。
3. 审查 Schema、Rust、TypeScript、OpenAPI 和传输客户端的差异。
4. 更新运行时适配器、消费者和契约测试。
5. 运行 `pnpm contracts:check` 和受影响的定向测试。

## HTTP 命令与查询

HTTP 写入入口是 `POST /api/v1/commands`。`CommandEnvelope` 要求以下字段：

- `schemaVersion`
- `command`
- `actor`
- `scope`
- `requestId`
- `expectedRevision`
- `payload`

`requestId` 的格式是 `req_` 加 26 个 Crockford Base32 字符。幂等身份与重放约束见 [命令身份与修订](invariants.md#命令身份与修订)。内容冲突的机器错误码是 `IDEMPOTENCY_CONFLICT`。

`expectedRevision` 是变更前的预期修订号。修订冲突返回 `REVISION_CONFLICT`，并报告预期修订号和当前修订号。完成响应包含 `requestId`、`command`、`previousRevision`、`currentRevision` 和 `result`。异步接受响应包含 `requestId`、`command`、`currentRevision` 和 `acceptedAt`。写入前置条件见 [命令身份与修订](invariants.md#命令身份与修订)。

查询入口是 `POST /api/v1/queries`。`QueryEnvelope` 要求 `schemaVersion`、`requestId`、`query`、`actor`、`scope`、`parameters` 和 `page`。查询无业务写入副作用。分页游标绑定作用域、查询、筛选条件和快照。

## WebSocket 投影

WebSocket 入口是 `/api/v1/events`。它只传输订阅控制帧和服务端投影事件。业务命令必须走 HTTP 命令入口。

订阅帧 `ControlPlaneWebSocketSubscribeFrame` 要求 `type`、`subscriptionId`、`subscription` 和 `startAt`。事件帧 `ControlPlaneWebSocketEventFrame` 要求以下字段：

- `type`
- `subscriptionId`
- `eventId`
- `scope`
- `stream`
- `sequence`
- `occurredAt`
- `source`
- `authorizationEpoch`
- `event`

实时更新采用“失效通知后 HTTP 重载”。`runtime-projection.invalidated.v1` 触发 `delivery.get` 或 `runtime.projection.get`。HTTP 快照返回的 `eventCursor` 必须作为订阅的 `startAt`。如果服务端发送 `transport.reset-required.v1`，客户端要丢弃部分状态，并按原订阅流执行完整 HTTP 重载。

## ExecutionPort

`ExecutionPortMessage` 是 Control Plane 与 Worker 之间的消息联合。每种消息有自己的精确字段。不要为整个联合假设一个不存在的统一请求信封。

`JobDispatchMessage` 要求：

- `kind`
- `schemaVersion`
- `messageId`
- `sentAt`
- `requestId`
- `job`
- `lease`
- `replacementAuthority`

`ExecutionLeaseStamp` 要求 `leaseId`、`jobId`、`workerId`、`workerInstanceId`、`attempt`、`fencingToken`、`issuedAt` 和 `expiresAt`。`RuntimeEventMessage` 和 `JobOutcomeMessage` 都携带完整 `lease`。运行时事件还携带 `workerSessionId`、`codexThreadId`、`event` 和 `sessionIdentity`。执行结果携带 `workerSessionId`、`outcome` 和 `sessionIdentity`。

`FencingToken` 是十进制字符串。格式是 `^[1-9][0-9]{0,19}$`。字符串编码用于保留 64 位精度。接收约束见 [租约与 fencing](invariants.md#租约与-fencing) 和 [旧 Worker 消息](invariants.md#旧-worker-的迟到消息)。

远程交换响应 v1 和 v2 都要求 `serverTime`。该字段使用 UTC RFC3339 固定毫秒格式 `YYYY-MM-DDTHH:mm:ss.SSSZ`。缺失、无效或非规范格式均拒绝解码。Server 在 scheduler drive 完成后、读取队列与构造响应之前取得当前时间。该时间同时用于筛选仍有效的派发和续租帧，并作为响应的 `serverTime`。响应时间只属于本次传输元数据。持久化帧的 `sentAt`、`messageId` 和原始字节保持不变。

Worker 在请求序列化完成后、进入 relay 调用之前记录本地单调时钟 `request_started`。本地租约截止时间为 `request_started + max(expiresAt - serverTime, 0)`。该计算覆盖 relay 排队、Device 转发和本次网络耗时。控制入队前和执行取出时均检查截止时间。同一派发重放不延长权限；续租被接受后才更新权限。

## ClientControlPort

`ClientControlMessage` 是 Control Plane 与 Device Client 之间的消息联合。每条消息携带以下信封身份字段：

- `kind`
- `schemaVersion`
- `messageId`
- `clientNodeId`
- `clientInstanceId`
- `sequence`
- `occurredAt`

命令类消息还携带 `expectedRevision` 和 `idempotencyKey`。占用或执行命令携带 `occupancyLeaseId` 和 `occupancyFencingToken`。报告类消息不应被补成命令字段。例如，`ClientHeartbeatMessage` 报告 `presenceState`、`acceptingConnections`、`lockState`、`capacity` 和可空的 `occupancyLeaseId`，但没有 `expectedRevision` 或 `idempotencyKey`。

`ClientOccupancyForceFenceMessage` 要求完整信封身份字段，并要求 `occupancyLeaseId`、`occupancyFencingToken`、`supersededLeaseId`、`reason`、`expectedRevision` 和 `idempotencyKey`。

`OccupancyFencingToken` 也是十进制字符串。格式是 `^[1-9][0-9]{0,19}$`。不要把该字段改成 JavaScript number。占用校验见 [租约与 fencing](invariants.md#租约与-fencing)。

ExecutionPort 的 `fencingToken` 和 ClientControlPort 的 `occupancyFencingToken` 属于不同租约域。字段名、租约身份和校验上下文不能互换。

### Device 转发 Worker 消息

Device 通过 `POST /internal/v1/client/worker-exchange` 转发子 Worker 的请求。`DeviceWorkerRelayRequest` 携带 `clientNodeId`、`workerId`、`workerInstanceId` 和 `request`。

`WorkerRelayRequest` 是 `kind` 标记的联合。`exchange` 携带序列化的 `body`。`accounting` 携带可空的 `body` 和 `offset`。`WorkerRelayResponse` 返回 `status`、`body` 和可选的结构化 `failure`。类型定义见 `crates/winwincode-client-port/src/worker_relay.rs`。

Worker 与父 Device 之间使用 stdin/stdout JSON-lines。Server 核对 Device 与 Worker 的归属、当前 instance、启动授权、placement 和执行租约。动作授权与用量对账沿用同一转发边界。

Device 创建并独占 Worker 的管道，并把该管道固定到配置的 Worker 与 Worker instance。远程转发请求使用 Device 身份签名。内部 ExecutionPort 消息继续携带会话、任务、attempt 和 fencing 身份。Worker 与 Server 按当前执行权威核对这些字段。

Worker 的 managed config 由 Device supervisor 写入。它包含会话身份、occupancy fencing token、仓库、工作目录、Provider 目录、`helperExecutable` 和可选模型路由。

## Device 远程认证

`device-auth.schema.json` 定义以下入口。Browser 使用 Owner 会话。Device 使用 Ed25519 私钥签名。

| 方法与路径 | 请求与响应 |
| --- | --- |
| `POST /api/v1/devices/pairing` | Owner 创建有效期为 10 分钟的配对令牌。 |
| `POST /internal/v1/device/challenge` | Device 提交 `publicKey`，取得绑定该公钥的单次挑战。挑战有效期为 60 秒。 |
| `POST /internal/v1/device/bind` | Device 提交 `requestId`、`pairingToken`、`publicKey` 和 `intent`。成功响应返回原请求 ID、Client Node、公开设备编号、Owner 和绑定时间。 |
| `POST /api/v1/devices/{client_node_id}/revoke` | Owner 永久吊销该绑定。 |

`intent` 固定设备名称、平台、架构和版本。Device 在请求前持久化私钥、Server URL、稳定的 `requestId`、配对令牌和意图。Server 在一个事务内消费令牌、建立绑定和管理授权，并保存绑定回执。同一请求、原公钥、原令牌和原意图的重试返回原回执。每次恢复仍须用原私钥回答新挑战。绑定回执随 Server 数据保存，当前实现没有自动回收期限。

`401 DEVICE_PAIRING_REJECTED` 表示签名认证已通过，但配对令牌错误、过期或已被其他请求消费。该拒绝不使未绑定目录进入永久失败。用户在同一目录提供新令牌时，Device 先重放原绑定请求。只有原请求明确收到该拒绝后，Device 才持久化新令牌并再次请求。私钥、`requestId` 和意图保持不变。网络失败、普通认证失败和无效响应保留原 pending 请求。原请求已有回执时，Server 先返回原回执；吊销检查先于令牌替换。

公钥和签名使用不带 padding 的标准 Base64。签名头是 `x-winwincode-device-public-key`、`x-winwincode-device-challenge-id` 和 `x-winwincode-device-signature`。绑定后的请求还携带 `x-winwincode-client-node-id`。签名输入是以下紧凑 UTF-8 JSON 数组：

```text
["winwincode.device-auth.v1", challengeId, challenge, UPPER_METHOD, exact_path, clientNodeId_or_empty, sha256hex(raw_body)]
```

`sha256hex` 使用小写十六进制。Device 控制交换、Worker 中继和 preview WebSocket upgrade 都使用此签名。WebSocket upgrade 使用 `GET` 和空请求正文。绑定请求的 Client Node 为空。实现见 `crates/winwincode-client-port/src/device_auth.rs::signing_bytes`。

`401 DEVICE_AUTH_REQUIRED` 触发一次新挑战重试。再次认证失败或未明确吊销的 `403` 会结束当前 Device 服务并停止 Worker。后续正常启动可以重新签名认证。未绑定请求保留恢复状态。只有 `403 DEVICE_REVOKED` 进入永久失败路径。已吊销绑定的旧请求不能恢复授权。

## 候选应用与恢复

`LocalCandidateReceipt.baseCommit` 保存候选生成时的源码基线。Device 从冻结提交内的 `sourceCommitId` 源链解析该值。链缺损或成环时拒绝保留候选。Server 将绑定设备、仓库、候选、`baseCommit` 和候选摘要的授权与下行命令原子持久化。命令同时携带 `applicationId`、`approvalId`、`candidateDigest` 和 `reconciledRetry`。

`client.application.reconciled` 关联原审批和原 epoch，并携带当前占用授权。`applied`、`not_started` 和 `outcome_unknown` 分别表示已找到应用提交、确认尚未开始、结果无法确定。当前授权可结算恢复结果。旧 epoch 的普通应用结果只补充事实。状态规则见 [候选应用与恢复不变量](invariants.md#候选应用与恢复)。

## 当前数据库 Schema 策略

产品存储的适用边界、原子初始化与结构拒绝行为见 [当前数据库 schema](invariants.md#当前数据库-schema)。协议版本 `winwincode/v1` 与各库的 SQLite `user_version` 是不同概念。

## 兼容性检查

`winwincode/v1` 内的删除、重命名、收紧约束、改变必填性或改变错误语义都可能破坏现有消费者。增加字段也要检查 `additionalProperties: false`、生成类型、运行时校验和旧客户端行为。

协议变更至少检查以下事项：

- 请求的必填字段、格式和未知字段处理
- 响应字段的缺失与 `null` 语义
- HTTP 状态、机器错误码和 `retryable`
- `requestId` 重放与冲突行为
- `expectedRevision` 的并发冲突行为
- 分页顺序、游标绑定和过期处理
- HTTP 快照与 WebSocket `startAt` 的交接
- ExecutionPort 与 ClientControlPort 的租约和 fencing 规则
- Rust、TypeScript、OpenAPI 和生成客户端是否同时更新

契约变更的验收证据应来自 Schema、生成差异和定向测试。不要用手工修改生成文件来掩盖生成器或运行时不一致。
